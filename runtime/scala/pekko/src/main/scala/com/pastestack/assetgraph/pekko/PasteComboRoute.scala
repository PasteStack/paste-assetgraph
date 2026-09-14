package com.pastestack.assetgraph.pekko

import org.apache.pekko.http.scaladsl.model._
import org.apache.pekko.http.scaladsl.model.headers.`Cache-Control`
import org.apache.pekko.http.scaladsl.model.headers.CacheDirectives._
import org.apache.pekko.http.scaladsl.server.Directives._
import org.apache.pekko.http.scaladsl.server.Route
import org.apache.pekko.util.ByteString
import java.nio.charset.StandardCharsets
import com.pastestack.assetgraph.{Manifest, PasteComboHandler}

/**
 * Ready-to-use Pekko HTTP route for JAM combo URLs.
 * 
 * Usage:
 * {{{
 * import com.pastestack.assetgraph.pekko.PasteComboRoute
 * 
 * val routes = concat(
 *   PasteComboRoute.route,
 *   // your other routes
 * )
 * }}}
 * 
 * Or with custom paths:
 * {{{
 * val comboRoute = new PasteComboRoute("../paste/src/js", "../paste-elements")
 * val routes = concat(comboRoute.route, ...)
 * }}}
 */
class PasteComboRoute(
  pastePath: String = sys.env.getOrElse("PASTE_PATH", "../paste/src/js"),
  pasteElementsPath: String = sys.env.getOrElse("PASTE_ELEMENTS_PATH", "../paste-elements"),
  siteJsPath: String = sys.env.getOrElse("SITE_JS_PATH", "src/main/assets/js"),
  siteCssPath: String = sys.env.getOrElse("SITE_CSS_PATH", "src/main/assets/scss"),
  sitePrefix: String = sys.env.getOrElse("SITE_PREFIX", "site"),
  manifest: Option[Manifest] = None,
  outputBasePath: String = sys.env.getOrElse("PASTE_OUTPUT_PATH", "target/public"),
  webjarBasePath: String = sys.env.getOrElse("PASTE_WEBJAR_PATH", "target/paste-assetgraph-webjars")
) {
  
  private val handler = new PasteComboHandler(pastePath, pasteElementsPath, siteJsPath, siteCssPath, sitePrefix, manifest, outputBasePath, webjarBasePath)
  
  private def contentTypeFor(ext: String): ContentType = ext match {
    case "js" => ContentType(MediaTypes.`application/javascript`, HttpCharsets.`UTF-8`)
    case "css" => ContentType(MediaTypes.`text/css`, HttpCharsets.`UTF-8`)
    case _ => ContentTypes.`text/plain(UTF-8)`
  }

  /**
   * Serve the modules named by a combo path.
   *
   * `cacheable` distinguishes the two URL shapes: a versioned path carries a
   * timestamp segment and is immutable, so it may be cached for a year; an
   * unversioned path has nothing to invalidate it and must not be.
   */
  private def serveModules(modulesWithExt: String, cacheable: Boolean): Route = get {
    val (modules, ext, isMinified) = handler.parseModulesUrl(modulesWithExt)
    val (content, foundCount) = handler.loadModulesWithCount(modules, ext, isMinified)
    val contentType = contentTypeFor(ext)
    val notFoundBody = if (modules.sizeIs > 1) "/* modules not found */" else "/* module not found */"

    if (foundCount == 0) {
      complete(HttpResponse(
        status = StatusCodes.NotFound,
        headers = List(`Cache-Control`(`no-cache`)),
        entity = HttpEntity(contentType, ByteString(notFoundBody, StandardCharsets.UTF_8))
      ))
    } else {
      val cacheControl =
        if (cacheable) `Cache-Control`(`public`, `max-age`(31536000)) else `Cache-Control`(`no-cache`)
      complete(HttpResponse(
        status = StatusCodes.OK,
        headers = List(cacheControl),
        entity = HttpEntity(contentType, ByteString(content, StandardCharsets.UTF_8))
      ))
    }
  }

  /**
   * Unversioned single-module path: /paste/{module}.{js|css}. Development
   * convenience for fetching one module without a combo timestamp.
   */
  private def standaloneModuleRoute: Route =
    path(Remaining) { moduleWithExt =>
      if (moduleWithExt.isEmpty || moduleWithExt.contains("/")) reject
      else serveModules(moduleWithExt, cacheable = false)
    }

  /** Versioned combo path: /paste/{timestamp}/{modules}.{min.}{js|css}. */
  private def versionedComboRoute: Route =
    path(Segment / Remaining) { (_, modulesWithExt) =>
      serveModules(modulesWithExt, cacheable = true)
    }

  val route: Route = pathPrefix("paste") {
    standaloneModuleRoute ~ versionedComboRoute
  }
}

object PasteComboRoute {
  private lazy val defaultRoute = new PasteComboRoute()
  
  /**
   * Default route using environment variables for paths.
   */
  def route: Route = defaultRoute.route
  
  /**
   * Create route with manifest for module lookup (preferred).
   */
  def withManifest(manifest: Manifest, outputBasePath: String = "target/public", webjarBasePath: String = "target/paste-assetgraph-webjars"): PasteComboRoute = 
    new PasteComboRoute(manifest = Some(manifest), outputBasePath = outputBasePath, webjarBasePath = webjarBasePath)
  
  /**
   * Create route with custom paths.
   */
  def apply(pastePath: String, pasteElementsPath: String): PasteComboRoute = 
    new PasteComboRoute(pastePath, pasteElementsPath)
  
  /**
   * Create route with custom paths and webjar base path.
   */
  def apply(pastePath: String, pasteElementsPath: String, webjarBasePath: String): PasteComboRoute = 
    new PasteComboRoute(pastePath, pasteElementsPath, webjarBasePath = webjarBasePath)
  
  /**
   * Create route with manifest and fallback paths.
   */
  def apply(manifest: Manifest, pastePath: String, pasteElementsPath: String): PasteComboRoute = 
    new PasteComboRoute(pastePath, pasteElementsPath, manifest = Some(manifest))
  
  /**
   * Create route with manifest, fallback paths, output path, and webjar path.
   * Note: For custom webjarBasePath only, use the 3-arg version with named parameters.
   */
  def apply(manifest: Manifest, pastePath: String, pasteElementsPath: String, outputBasePath: String, webjarBasePath: String = "target/paste-assetgraph-webjars"): PasteComboRoute = 
    new PasteComboRoute(pastePath, pasteElementsPath, manifest = Some(manifest), outputBasePath = outputBasePath, webjarBasePath = webjarBasePath)
}
