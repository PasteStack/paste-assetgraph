package com.pastestack.assetgraph

import java.nio.file.{Files, Paths, Path}
import java.util.concurrent.atomic.AtomicBoolean
import scala.util.Try
import scala.util.matching.Regex
import java.util.concurrent.ConcurrentHashMap
import scala.collection.mutable

/**
 * JAM combo handler for paste-style URLs.
 * 
 * URL format: /paste/{timestamp}/{module1+v1,module2+v1}.min.js
 * 
 * Reads and concatenates modules from configured source paths.
 * This is the reference implementation per JAM_SPEC.md and jam_serving.feature.
 * 
 * For Pekko HTTP:
 * {{{
 * import com.pastestack.assetgraph.PasteComboHandler
 * 
 * val handler = new PasteComboHandler(
 *   pastePath = "../paste/src/js",
 *   pasteElementsPath = "../paste-elements"
 * )
 * 
 * val route = handler.createRoute()
 * }}}
 * 
 * For other frameworks, use the core methods directly:
 * - parseModulesUrl(url): Extract modules and extension from URL
 * - loadModules(modules, ext): Load and concatenate module content
 */
class PasteComboHandler(
  pastePath: String = sys.env.getOrElse("PASTE_PATH", "../paste/src/js"),
  pasteElementsPath: String = sys.env.getOrElse("PASTE_ELEMENTS_PATH", "../paste-elements"),
  siteJsPath: String = sys.env.getOrElse("SITE_JS_PATH", "src/main/assets/js"),
  siteCssPath: String = sys.env.getOrElse("SITE_CSS_PATH", "src/main/assets/scss"),
  sitePrefix: String = sys.env.getOrElse("SITE_PREFIX", "site"),
  manifest: Option[Manifest] = None,
  outputBasePath: String = sys.env.getOrElse("PASTE_OUTPUT_PATH", "target/public"),
  webjarBasePath: String = sys.env.getOrElse("PASTE_WEBJAR_PATH", "target/paste-assetgraph-webjars")
) {
  
  // Regex to parse @requires comments: @requires paste/dom -> paste.dom
  private val RequiresPattern: Regex = """@requires\s+([\w/-]+)""".r
  
  // Cache webjar SCSS paths at construction time to avoid filesystem traversal on every compile
  private lazy val webjarIncludePaths: Seq[String] = computeWebjarScssPaths()
  
  // Cache for parsed dependencies: module -> (deps, fileLastModified)
  // Thread-safe: accessed concurrently from Pekko HTTP request handlers
  private val dependencyCache: ConcurrentHashMap[String, (Seq[String], Long)] = new ConcurrentHashMap()
  
  // Cache for webjar file lookups: (artifactId, relativePath) -> Option[(basePath, relativePath)]
  // Avoids repeated filesystem traversal on the hot path for every JS module request
  private val webjarFileCache: ConcurrentHashMap[(String, String), Option[(String, String)]] = new ConcurrentHashMap()
  
  private val sassWarningShown: AtomicBoolean = new AtomicBoolean(false)
  
  /** Clear all caches. Call when source files change (e.g., from a file watcher). */
  def clearCaches(): Unit = {
    dependencyCache.clear()
    scssCache.clear()
    webjarFileCache.clear()
  }
  
  /**
   * Parse combo URL to extract modules, extension, and minified flag.
   * 
   * @param url "paste.dom+v1,paste.event+v2.min.js"
   * @return (Seq("paste.dom", "paste.event"), "js", isMinified)
   */
  def parseModulesUrl(url: String): (Seq[String], String, Boolean) = {
    val isMinified = url.contains(".min.")
    
    // Remove the trailing .min suffix (only the last occurrence, not greedy)
    val cleanUrl = if (isMinified) {
      val lastMin = url.lastIndexOf(".min.")
      if (lastMin >= 0) url.substring(0, lastMin) + "." + url.substring(lastMin + 5)
      else url
    } else {
      url
    }
    
    val dotIdx = cleanUrl.lastIndexOf('.')
    if (dotIdx < 0) return (Seq.empty, "", false)
    
    val ext = cleanUrl.substring(dotIdx + 1)
    val modulesPart = cleanUrl.substring(0, dotIdx)
    
    val modules = modulesPart.split("[~,]").map { m =>
      // Remove version suffix: "paste.dom+v1" -> "paste.dom"
      val vIdx = m.lastIndexOf("+v")
      if (vIdx > 0) {
        m.substring(0, vIdx)
      } else {
        // Also handle old format: "paste.dom.v1" -> "paste.dom"
        val dotVIdx = m.lastIndexOf(".v")
        if (dotVIdx > 0 && m.substring(dotVIdx + 2).forall(c => c.isDigit || c == '.')) {
          m.substring(0, dotVIdx)
        } else {
          m
        }
      }
    }.toSeq
    
    (modules, ext, isMinified)
  }
  
  /**
   * Load and concatenate module content with full dependency expansion.
   * 
   * @param modules Seq of module names (e.g., "paste.dom", "paste.ui.heroscroll")
   * @param ext File extension ("js" or "css")
   * @param minified Whether to serve minified content (from .min suffix in URL)
   * @return Concatenated content
   */
  def loadModules(modules: Seq[String], ext: String, minified: Boolean = false): String = {
    loadModulesWithCount(modules, ext, minified)._1
  }
  
  /**
   * Load and concatenate module content, returning (content, foundCount).
   * foundCount is the number of modules that resolved to real content
   * (not "not found" placeholders). Use this to distinguish true 404s
   * from modules whose content happens to be only comments.
   */
  def loadModulesWithCount(modules: Seq[String], ext: String, minified: Boolean = false): (String, Int) = {
    // Expand all transitive dependencies
    val expandedModules = expandDependencies(modules, ext)
    
    var foundCount = 0
    val content = expandedModules.map { module =>
      val result = loadModuleResult(module, ext, minified)
      if (result.found) {
        foundCount += 1
      }
      result.content
    }.mkString("\n")
    (content, foundCount)
  }
  
  /**
   * Expand modules to include all transitive dependencies (topologically sorted).
   * Dependencies come before dependents.
   */
  def expandDependencies(modules: Seq[String], ext: String): Seq[String] = {
    val visited = mutable.Set.empty[String]
    val result = mutable.ListBuffer.empty[String]
    
    def visit(m: String): Unit = {
      if (visited.contains(m)) return
      visited += m
      
      // Get dependencies for this module
      val deps = getDependencies(m, ext)
      deps.foreach(visit)
      
      result += m
    }
    
    modules.foreach(visit)
    result.toSeq
  }
  
  /**
   * Get dependencies for a module by parsing @requires comments from the source file.
   */
  private def getDependencies(module: String, ext: String): Seq[String] = {
    // Check manifest first for pre-computed dependencies (works for all asset types)
    manifest.foreach { m =>
      val key = s"$ext/${module.replace('.', '/')}.$ext"
      m.assets.get(key).foreach { entry =>
        if (entry.dependencies.nonEmpty) return entry.dependencies
      }
    }
    
    // Only JS source files have @requires for runtime parsing
    if (ext != "js") return Seq.empty
    
    val path = resolveModulePath(module, ext)
    val lastMod = Try(Files.getLastModifiedTime(path).toMillis).getOrElse(0L)
    
    Option(dependencyCache.get(module)) match {
      case Some((deps, cachedMod)) if cachedMod == lastMod => deps
      case _ =>
        val deps = Try {
          val content = new String(Files.readAllBytes(path))
          parseDependencies(content)
        }.getOrElse(Seq.empty)
        dependencyCache.put(module, (deps, lastMod))
        deps
    }
  }
  
  /**
   * Parse @requires comments from file content.
   * @requires paste/dom -> paste.dom
   */
  private def parseDependencies(content: String): Seq[String] = {
    RequiresPattern.findAllMatchIn(content).map { m =>
      // Convert paste/dom to paste.dom
      m.group(1).replace('/', '.')
    }.toSeq
  }
  
  /**
   * Get content type for extension.
   */
  def contentTypeFor(ext: String): String = ext match {
    case "js" => "application/javascript"
    case "css" => "text/css"
    case _ => "text/plain"
  }
  
  // Cache for compiled SCSS: path -> (compiledCss, fileLastModified)
  // Thread-safe: accessed concurrently from Pekko HTTP request handlers
  private val scssCache: ConcurrentHashMap[Path, (String, Long)] = new ConcurrentHashMap()
  
  /** Result of loading a single module: found content or not-found placeholder. */
  case class ModuleResult(content: String, found: Boolean)
  
  private def loadModule(module: String, ext: String, minified: Boolean = false): String = {
    loadModuleResult(module, ext, minified).content
  }
  
  private def loadModuleResult(module: String, ext: String, minified: Boolean = false): ModuleResult = {
    // 1. Try pre-compiled output from manifest (preferred for CSS)
    resolveCompiledOutput(module, ext) match {
      case Some(compiled) => return ModuleResult(compiled, found = true)
      case None => // fall through to source resolution
    }
    
    val path = resolveModulePath(module, ext)
    
    // For CSS, try .scss if .css doesn't exist
    val actualPath = if (ext == "css" && !Files.exists(path)) {
      val pathStr = path.toString
      val scssPath = if (pathStr.endsWith(".css")) Paths.get(pathStr.dropRight(4) + ".scss") else path
      if (Files.exists(scssPath)) scssPath else path
    } else {
      path
    }
    
    Try {
      val content = new String(Files.readAllBytes(actualPath))
      
      val result = if (actualPath.toString.endsWith(".scss")) {
        // Try on-the-fly SCSS compilation via sass CLI
        compileScssToCss(actualPath, content).getOrElse {
          s"/* SCSS source (not compiled - install sass CLI): $module */\n/* $actualPath */"
        }
      } else {
        content
      }
      ModuleResult(result, found = true)
    }.getOrElse {
      // Module not found in manifest or filesystem - may be bundled in another module
      ModuleResult(
        s"/* Module $module: not found as standalone $ext (may be bundled in site $ext) */",
        found = false
      )
    }
  }
  
  /**
   * Try to find pre-compiled output for a CSS module via manifest.
   */
  private def resolveCompiledOutput(module: String, ext: String): Option[String] = {
    if (ext != "css") return None
    
    manifest.flatMap { m =>
      // module name → asset key via modules map
      // Only use if the resolved key matches the requested extension;
      // e.g., paste.ui.heroscroll resolves to js/paste/ui/heroscroll.js
      // which is wrong when we want CSS.
      val assetKey = m.resolveModule(module)
        .filter(_.endsWith(s".$ext"))
        .orElse {
          // Fallback: try constructing the key directly
          val key = s"$ext/${module.replace('.', '/')}.$ext"
          if (m.assets.contains(key)) Some(key) else None
        }
      
      assetKey.flatMap { key =>
        // asset key → AssetEntry
        m.assets.get(key).flatMap { entry =>
          // Read compiled file from output directory
          val outputFile = Paths.get(outputBasePath, entry.output.stripPrefix("/"))
          Try {
            if (Files.exists(outputFile)) {
              Some(new String(Files.readAllBytes(outputFile)))
            } else {
              None
            }
          }.getOrElse(None)
        }
      }
    }
  }
  
  /**
   * Compile SCSS to CSS on-the-fly using the sass CLI.
   * Caches results to avoid recompilation on every request.
   * 
   * NOTE: Cache invalidation is based on the main SCSS file's lastModified timestamp.
   * If webjar SCSS dependencies change (e.g., dependency version update), the cache
   * will not automatically invalidate. Call clearCaches() to force recompilation.
   */
  private def compileScssToCss(scssPath: Path, content: String): Option[String] = {
    val lastMod = Try(Files.getLastModifiedTime(scssPath).toMillis).getOrElse(0L)
    
    Option(scssCache.get(scssPath)) match {
      case Some((cached, cachedMod)) if cachedMod == lastMod => Some(cached)
      case _ =>
        Try {
          import scala.sys.process._
          val parentPath = Option(Paths.get(pasteElementsPath).getParent).map(_.toString)
          
          val includePaths = (Seq(
            pasteElementsPath,
            siteCssPath
          ) ++ parentPath.toSeq ++ webjarIncludePaths).flatMap(p => Seq("--load-path", p))
          
          val cmd = Seq("sass", "--stdin", "--no-source-map", "--style=expanded") ++ includePaths
          val result = (cmd #< new java.io.ByteArrayInputStream(content.getBytes("UTF-8"))).!!
          scssCache.put(scssPath, (result, lastMod))
          Some(result)
        }.getOrElse {
          if (sassWarningShown.compareAndSet(false, true)) {
            System.err.println("[paste-assetgraph] SCSS compilation failed: 'sass' CLI not found or returned an error. Install sass (npm install -g sass) for on-the-fly SCSS compilation.")
          }
          None
        }
    }
  }
  
  /**
   * Resolve module path using manifest or convention.
   */
  private def resolveModulePath(module: String, ext: String): Path = {
    // 1. Try manifest lookup first (preferred)
    // Only use manifest path if it matches the requested extension;
    // modules like paste.ui.heroscroll map to JS but also have CSS via convention.
    val manifestPath = manifest.flatMap(_.resolveModule(module))
      .filter(_.endsWith(s".$ext"))
    
    manifestPath match {
      case Some(relPath) =>
        // Manifest provides relative path like "js/site.js" or "css/site.css"
        // Resolve against output base first (for compiled assets), then source dirs
        val outputResolved = Paths.get(outputBasePath, relPath)
        if (Files.exists(outputResolved)) outputResolved
        else {
          // Strip the type prefix (js/ or css/) and look in source dirs
          val stripped = relPath.replaceFirst("^(js|css)/", "")
          val sourceBase = if (ext == "css" || relPath.startsWith("css/")) siteCssPath else siteJsPath
          val sourceResolved = Paths.get(sourceBase, stripped)
          if (Files.exists(sourceResolved)) sourceResolved
          else {
            // Final fallback: try convention-based resolution
            val (basePath, filePath) = resolveModulePathByConvention(module, ext)
            Paths.get(basePath, filePath)
          }
        }
      case None =>
        // 2. Fallback to convention-based resolution
        val (basePath, filePath) = resolveModulePathByConvention(module, ext)
        Paths.get(basePath, filePath)
    }
  }
  
  /**
   * Resolve module path using naming conventions (fallback when not in manifest).
   * 
   * NOTE: paste.ui.* modules are resolved from pasteElementsPath only.
   * If using paste-elements-webjar, JS modules must be in the local pasteElementsPath.
   * Webjar support for paste.ui.* can be added if needed in the future.
   */
  private def resolveModulePathByConvention(module: String, ext: String): (String, String) = {
    // Site modules: site.home -> siteAssetsPath/site.js
    // paste.ui.* -> paste-elements/modules/
    // paste.* -> paste/src/js/ (checks webjar first)
    
    module match {
      case "paste" => 
        // Try webjar first, then fallback to pastePath
        val webjarRelPath = if (ext == "css") s"src/scss/paste.$ext" else s"src/js/paste.$ext"
        val webjarPath = findInWebjars("paste-webjar", webjarRelPath)
        webjarPath.getOrElse((pastePath, s"paste.$ext"))
      case m if m.startsWith(s"$sitePrefix.") =>
        // Site-specific module: site.home -> site.js or site.scss
        val component = m.stripPrefix(s"$sitePrefix.")
        val basePath = if (ext == "css") siteCssPath else siteJsPath
        (basePath, s"$component.$ext")
      case m if m.startsWith("paste.") =>
        // Cascade: paste-elements first (by filesystem), then paste core / webjar
        val component = m.split('.').last
        val elementsRelPath = s"modules/$component/$component.$ext"
        if (Files.exists(Paths.get(pasteElementsPath, elementsRelPath))) {
          (pasteElementsPath, elementsRelPath)
        } else {
          val sub = m.stripPrefix("paste.")
          val webjarPath = findInWebjars("paste-webjar", s"src/js/$sub.$ext")
          webjarPath.getOrElse((pastePath, s"$sub.$ext"))
        }
      case m if m.startsWith("polyfills.") =>
        // polyfills.getcomputedstyle -> paste/src/js/polyfills/getcomputedstyle.js
        // Try webjar first, then fallback to pastePath
        val sub = m.stripPrefix("polyfills.")
        val webjarPath = findInWebjars("paste-webjar", s"src/js/polyfills/$sub.$ext")
        webjarPath.getOrElse((pastePath, s"polyfills/$sub.$ext"))
      case _ => 
        (pasteElementsPath, s"$module.$ext")
    }
  }
  
  /**
   * Compute SCSS include paths from extracted webjars (called once at construction).
   */
  private def computeWebjarScssPaths(): Seq[String] = {
    val webjarDir = Paths.get(webjarBasePath)
    if (!Files.exists(webjarDir)) {
      return Seq.empty
    }
    
    val paths = mutable.ArrayBuffer[String]()
    
    Option(webjarDir.toFile.listFiles()).foreach { files =>
      files.filter(_.isDirectory).foreach { artifactDir =>
        Option(artifactDir.listFiles()).foreach { versionFiles =>
          versionFiles.filter(_.isDirectory).foreach { versionDir =>
            val scssDir = versionDir.toPath.resolve("src/scss")
            if (Files.exists(scssDir)) {
              paths += scssDir.toString
            }
          }
        }
      }
    }
    
    paths.toSeq
  }
  
  /**
   * Find a file in extracted webjars directory.
   * Returns Option[(basePath, relativePath)] if found.
   */
  private def findInWebjars(artifactId: String, relativePath: String): Option[(String, String)] = {
    val cacheKey = (artifactId, relativePath)
    val cached = webjarFileCache.get(cacheKey)
    if (cached != null) return cached
    
    val result = {
      val webjarDir = Paths.get(webjarBasePath, artifactId)
      if (!Files.exists(webjarDir)) None
      else {
        // Find version directory (one level deep)
        Option(webjarDir.toFile.listFiles()).flatMap { files =>
          files.filter(_.isDirectory).collectFirst {
            case versionDir if Files.exists(versionDir.toPath.resolve(relativePath)) =>
              (versionDir.getAbsolutePath, relativePath)
          }
        }
      }
    }
    
    webjarFileCache.put(cacheKey, result)
    result
  }
}

object PasteComboHandler {
  /**
   * Create handler with default paths from environment.
   */
  def apply(): PasteComboHandler = new PasteComboHandler()
  
  /**
   * Create handler with manifest for module lookup.
   * This is the preferred method - uses @module comments from manifest.
   */
  def withManifest(
    manifest: Manifest, 
    outputBasePath: String = "target/public",
    webjarBasePath: String = "target/paste-assetgraph-webjars"
  ): PasteComboHandler = 
    new PasteComboHandler(manifest = Some(manifest), outputBasePath = outputBasePath, webjarBasePath = webjarBasePath)
  
  /**
   * Create handler with custom paths (fallback convention-based resolution).
   */
  def apply(pastePath: String, pasteElementsPath: String): PasteComboHandler = 
    new PasteComboHandler(pastePath, pasteElementsPath)
  
  /**
   * Create handler with custom paths and webjar base path.
   */
  def apply(pastePath: String, pasteElementsPath: String, webjarBasePath: String): PasteComboHandler = 
    new PasteComboHandler(pastePath, pasteElementsPath, webjarBasePath = webjarBasePath)
  
  /**
   * Create handler with all paths including site assets.
   */
  def apply(pastePath: String, pasteElementsPath: String, siteJsPath: String, siteCssPath: String, sitePrefix: String): PasteComboHandler = 
    new PasteComboHandler(pastePath, pasteElementsPath, siteJsPath, siteCssPath, sitePrefix)
  
  /**
   * Create handler with all paths including site assets and webjar base path.
   */
  def apply(
    pastePath: String, 
    pasteElementsPath: String, 
    siteJsPath: String, 
    siteCssPath: String, 
    sitePrefix: String,
    webjarBasePath: String
  ): PasteComboHandler = 
    new PasteComboHandler(pastePath, pasteElementsPath, siteJsPath, siteCssPath, sitePrefix, webjarBasePath = webjarBasePath)
  
  /**
   * Create handler with manifest and fallback paths.
   */
  def apply(manifest: Manifest, pastePath: String, pasteElementsPath: String): PasteComboHandler = 
    new PasteComboHandler(pastePath, pasteElementsPath, manifest = Some(manifest))
  
  /**
   * Create handler with manifest, fallback paths, output path, and webjar path.
   * Note: For custom webjarBasePath only, use the 3-arg version with named parameters.
   */
  def apply(
    manifest: Manifest, 
    pastePath: String, 
    pasteElementsPath: String, 
    outputBasePath: String,
    webjarBasePath: String = "target/paste-assetgraph-webjars"
  ): PasteComboHandler = 
    new PasteComboHandler(pastePath, pasteElementsPath, manifest = Some(manifest), outputBasePath = outputBasePath, webjarBasePath = webjarBasePath)
}
