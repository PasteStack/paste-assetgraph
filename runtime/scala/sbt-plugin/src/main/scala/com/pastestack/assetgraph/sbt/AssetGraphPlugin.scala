package com.pastestack.assetgraph.sbt

import sbt._
import sbt.Keys._
import com.pastestack.assetgraph.Binary
import java.util.zip.ZipFile
import java.io.File
import scala.collection.mutable

/**
 * sbt plugin for paste-assetgraph.
 * 
 * For Scala projects, configure via sbt settings (no external config file needed).
 * The plugin generates a temp config and passes it to the Rust binary.
 * 
 * Usage in build.sbt:
 * {{{
 * enablePlugins(AssetGraphPlugin)
 * 
 * // Required settings (all prefixed with paste to avoid collisions)
 * pasteAssetGraphSourceDirs := Seq("src/main/assets/scss", "src/main/assets/js")
 * pasteAssetGraphOutputDir := "target/public"
 * pasteAssetGraphManifestPath := "target/paste-assetgraph-manifest.json"
 * 
 * // Optional settings
 * pasteAssetGraphIncludePaths := Seq("../paste-elements")
 * pasteAssetGraphMinify := true
 * pasteAssetGraphSourceMaps := true
 * }}}
 * 
 * For Python/Node/HTML projects, use external config file instead.
 */
object AssetGraphPlugin extends AutoPlugin {
  
  object autoImport {
    // Core settings - folder-based auto-discovery (old paste style)
    val pasteAssetGraphSourceDirs = settingKey[Seq[String]]("Source directories to scan for assets")
    val pasteAssetGraphOutputDir = settingKey[String]("Output directory for compiled assets")
    val pasteAssetGraphManifestPath = settingKey[String]("Path for generated manifest")
    val pasteAssetGraphIncludePaths = settingKey[Seq[String]]("Include paths for SCSS @import")
    
    // Build settings
    val pasteAssetGraphMinify = settingKey[Boolean]("Minify output")
    val pasteAssetGraphSourceMaps = settingKey[Boolean]("Generate source maps")
    val pasteAssetGraphDebug = settingKey[Boolean]("Build in debug mode")
    val pasteAssetGraphBuild = taskKey[Unit]("Build assets")
    val pasteAssetGraphWatch = taskKey[Unit]("Watch assets for changes")
    val pasteAssetGraphWebJarPaths = taskKey[Seq[String]]("Auto-discovered webjar include paths")
  }
  
  import autoImport._
  
  override def trigger = noTrigger
  
  override def projectSettings: Seq[Setting[_]] = Seq(
    // Defaults - folder-based auto-discovery
    pasteAssetGraphSourceDirs := Seq("src/main/assets/scss", "src/main/assets/js"),
    pasteAssetGraphOutputDir := "target/public",
    pasteAssetGraphManifestPath := "", // Will be set from resourceManaged
    pasteAssetGraphIncludePaths := Seq.empty,
    pasteAssetGraphMinify := true,
    pasteAssetGraphSourceMaps := true,
    pasteAssetGraphDebug := false,
    
    pasteAssetGraphWebJarPaths := {
      val cp = (Compile / dependencyClasspath).value
      val extractDir = target.value / "paste-assetgraph-webjars"
      extractWebjars(cp, extractDir, streams.value.log)
    },
    
    pasteAssetGraphBuild := {
      val log = streams.value.log
      val base = baseDirectory.value
      val debug = pasteAssetGraphDebug.value
      val resourceManagedDir = (Compile / resourceManaged).value
      
      // Manifest goes to resourceManaged (sbt-web style)
      val manifestPath = if (pasteAssetGraphManifestPath.value.nonEmpty) {
        pasteAssetGraphManifestPath.value
      } else {
        (resourceManagedDir / "paste-assetgraph-manifest.json").relativeTo(base).map(_.getPath).getOrElse(
          resourceManagedDir.getPath + "/paste-assetgraph-manifest.json"
        )
      }
      
      // Ensure resourceManaged exists
      IO.createDirectory(resourceManagedDir)
      
      // Merge manual include paths with auto-discovered webjar paths
      val webjarPaths = pasteAssetGraphWebJarPaths.value
      val allIncludes = pasteAssetGraphIncludePaths.value ++ webjarPaths
      
      // Log webjar paths for debugging
      if (webjarPaths.nonEmpty) {
        log.info(s"Auto-discovered webjar paths: ${webjarPaths.mkString(", ")}")
      }
      
      // Generate config from settings
      val configFile = base / "target" / "paste-assetgraph-generated.conf"
      val config = generateConfig(
        pasteAssetGraphSourceDirs.value,
        pasteAssetGraphOutputDir.value,
        manifestPath,
        allIncludes,
        pasteAssetGraphMinify.value,
        pasteAssetGraphSourceMaps.value,
        debug
      )
      
      // Write config
      IO.createDirectory(configFile.getParentFile)
      IO.write(configFile, config)
      
      log.info("Building assets with paste-assetgraph...")
      
      val targetDir = (base / "target").toPath
      val exitCode = Binary.build(targetDir, configFile.absolutePath, debug, Some(base.absolutePath))
      
      if (exitCode != 0) {
        throw new RuntimeException(s"paste-assetgraph failed with exit code $exitCode")
      }
      
      log.info("Assets built successfully")
    },
    
    pasteAssetGraphWatch := {
      val log = streams.value.log
      val base = baseDirectory.value
      val configFile = base / "target" / "paste-assetgraph-generated.conf"
      
      // Task and setting values are resolved before the branch below: a regular
      // task evaluates every dependency it names regardless of any `if`, so
      // reading them here is what actually happens.
      val resourceManagedDir = (Compile / resourceManaged).value
      val configuredManifestPath = pasteAssetGraphManifestPath.value
      val webjarPaths = pasteAssetGraphWebJarPaths.value
      val allIncludes = pasteAssetGraphIncludePaths.value ++ webjarPaths
      val sourceDirs = pasteAssetGraphSourceDirs.value
      val outputDir = pasteAssetGraphOutputDir.value
      val minify = pasteAssetGraphMinify.value
      val sourceMaps = pasteAssetGraphSourceMaps.value
      val debug = pasteAssetGraphDebug.value

      // Ensure config exists before watching (generate if missing)
      if (!configFile.exists()) {
        log.info("Generating config file before watch...")
        val manifestPath = if (configuredManifestPath.nonEmpty) {
          configuredManifestPath
        } else {
          (resourceManagedDir / "paste-assetgraph-manifest.json").relativeTo(base).map(_.getPath).getOrElse(
            resourceManagedDir.getPath + "/paste-assetgraph-manifest.json"
          )
        }
        
        val config = generateConfig(
          sourceDirs,
          outputDir,
          manifestPath,
          allIncludes,
          minify,
          sourceMaps,
          debug
        )
        IO.createDirectory(configFile.getParentFile)
        IO.write(configFile, config)
        
        // Log webjar paths for debugging
        if (webjarPaths.nonEmpty) {
          log.info(s"Auto-discovered webjar paths: ${webjarPaths.mkString(", ")}")
        }
      }
      
      log.info("Watching assets for changes...")
      val targetDir = (base / "target").toPath
      Binary.watch(targetDir, configFile.absolutePath, Some(base.absolutePath))
    },
    
    // Build assets and register manifest as a managed resource.
    // resourceGenerators runs before compile, ensuring:
    // 1. Assets are built first (manifest generated)
    // 2. Manifest appears on the classpath (Manifest.loadFromClasspath() works)
    Compile / resourceGenerators += Def.task {
      pasteAssetGraphBuild.value
      val resourceManagedDir = (Compile / resourceManaged).value
      val manifestFile = resourceManagedDir / "paste-assetgraph-manifest.json"
      if (manifestFile.exists()) Seq(manifestFile) else Seq.empty
    }.taskValue
  )
  
  private def escapeHoconValue(s: String): String =
    s.replace("\\", "\\\\").replace("\"", "\\\"")
  
  private def generateConfig(
    sourceDirs: Seq[String],
    outputDir: String,
    manifestPath: String,
    includePaths: Seq[String],
    minify: Boolean,
    sourceMaps: Boolean,
    debugMode: Boolean
  ): String = {
    // Flat key format for Rust parser
    val scssDir = escapeHoconValue(sourceDirs.find(_.contains("scss")).getOrElse("src/main/assets/scss"))
    val jsDir = escapeHoconValue(sourceDirs.find(_.contains("js")).getOrElse("src/main/assets/js"))
    val escapedOutputDir = escapeHoconValue(outputDir)
    val escapedManifestPath = escapeHoconValue(manifestPath)
    val escapedIncludes = includePaths.map(s => s""""${escapeHoconValue(s)}"""").mkString(", ")
    
    s"""# Auto-generated by sbt AssetGraphPlugin
# Do not edit - configure in build.sbt instead

source.scss = "$scssDir"
source.js = "$jsDir"
source.images = "src/main/assets/images"
output.base = "$escapedOutputDir"
output.js = "$escapedOutputDir/js"
output.css = "$escapedOutputDir/css"
output.images = "$escapedOutputDir/images"
manifest = "$escapedManifestPath"
include-paths = [$escapedIncludes]
debug-mode = $debugMode
minify = $minify
source-maps = $sourceMaps
jam.enabled = true
jam.base-path = "/paste"
"""
  }
  
  private[sbt] def validatedWebjarPaths(extractDir: File, relativePath: String): (File, File) = {
    val parts = relativePath.split("/", -1).toSeq
    val safeSegments = parts.size >= 3 && parts.forall { part =>
      part.nonEmpty && part != "." && part != ".." &&
        !part.exists(character => character == '\\' || character == ':' || Character.isISOControl(character))
    }
    require(safeSegments, "Unsafe WebJar archive entry")

    val root = extractDir.getCanonicalFile.toPath
    val directory = root.resolve(parts(0)).resolve(parts(1))
    val file = parts.drop(2).foldLeft(directory)((path, part) => path.resolve(part))
    val canonicalDirectory = directory.toFile.getCanonicalFile.toPath
    val canonicalFile = file.toFile.getCanonicalFile.toPath
    require(
      canonicalDirectory == directory && canonicalFile == file &&
        canonicalDirectory.startsWith(root) && canonicalDirectory != root &&
        canonicalFile.startsWith(canonicalDirectory) && canonicalFile != canonicalDirectory,
      "Unsafe WebJar archive entry"
    )
    (canonicalDirectory.toFile, canonicalFile.toFile)
  }

  /**
   * Extract webjars from classpath JARs to target directory.
   * Returns Seq of extracted top-level directories.
   */
  private[sbt] def extractWebjars(classpath: Classpath, extractDir: File, log: Logger): Seq[String] = {
    val webjarPrefix = "META-INF/resources/webjars/"
    val extractedPaths = mutable.Set[String]()
    val extractedDirs = mutable.Set[String]()
    val cleanedDirs = mutable.Set[String]()
    
    classpath.foreach { entry =>
      val jar = entry.data
      if (jar.isFile && jar.getName.endsWith(".jar")) {
        var zipFile: ZipFile = null
        try {
          zipFile = new ZipFile(jar)
          val entries = zipFile.entries()
          
          while (entries.hasMoreElements) {
            val zipEntry = entries.nextElement()
            val name = zipEntry.getName
            
            if (name.startsWith(webjarPrefix) && !zipEntry.isDirectory) {
              // Parse: META-INF/resources/webjars/{artifactId}/{version}/{path}
              val relativePath = name.substring(webjarPrefix.length)
              val parts = relativePath.split("/", 3)
              
              // Require artifactId, version, AND a non-empty file path
              if (parts.length >= 3 && parts(2).nonEmpty) {
                val (targetDir, targetFile) = validatedWebjarPaths(extractDir, relativePath)
                
                // Check if already extracted and up-to-date
                val markerFile = targetDir / ".extracted"
                if (!markerFile.exists() || markerFile.lastModified() < jar.lastModified()) {
                  // Clean stale directory on first stale entry to remove old files
                  if (cleanedDirs.add(targetDir.getAbsolutePath)) {
                    IO.delete(targetDir)
                  }
                  IO.createDirectory(targetFile.getParentFile)
                  val in = zipFile.getInputStream(zipEntry)
                  try {
                    IO.transfer(in, targetFile)
                    log.debug(s"Extracted webjar: $name -> $targetFile")
                  } finally {
                    in.close()
                  }
                  
                  // Mark this directory for extraction marker
                  extractedDirs += targetDir.getAbsolutePath
                }
                
                if (!extractedPaths.contains(targetDir.getAbsolutePath)) {
                  extractedPaths += targetDir.getAbsolutePath
                }
              }
            }
          }
          
          // Create marker files after processing all entries
          extractedDirs.foreach { dir =>
            val markerFile = new File(dir) / ".extracted"
            IO.touch(markerFile)
          }
          extractedDirs.clear()
          
        } catch {
          case e: IllegalArgumentException => throw e
          case e: Exception =>
            log.warn(s"Failed to extract webjars from ${jar.getName}: ${e.getMessage}")
        } finally {
          if (zipFile != null) {
            try {
              zipFile.close()
            } catch {
              case e: Exception => log.debug(s"Failed to close zip file ${jar.getName}: ${e.getMessage}")
            }
          }
        }
      }
    }
    
    extractedPaths.toSeq
  }
  
  /**
   * Convenience helpers for common paste setup.
   */
  def withPaste(version: String = "2.0.1"): Seq[Setting[_]] = Seq(
    libraryDependencies += "com.pastestack" % "paste-webjar" % version
  )
  
  def withPasteElements(version: String = "0.1.0"): Seq[Setting[_]] = Seq(
    libraryDependencies += "com.pastestack" % "paste-elements-webjar" % version
  )
  
  def withPasteStack(pasteVersion: String = "2.0.1", elementsVersion: String = "0.1.0"): Seq[Setting[_]] =
    withPaste(pasteVersion) ++ withPasteElements(elementsVersion)
}
