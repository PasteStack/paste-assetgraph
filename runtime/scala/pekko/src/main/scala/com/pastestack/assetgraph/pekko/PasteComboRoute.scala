package com.pastestack.assetgraph.pekko

import org.apache.pekko.http.scaladsl.model._
import org.apache.pekko.http.scaladsl.model.headers.`Cache-Control`
import org.apache.pekko.http.scaladsl.model.headers.CacheDirectives._
import org.apache.pekko.http.scaladsl.server.Directives._
import org.apache.pekko.http.scaladsl.server.Route
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
  
  val route: Route = pathPrefix("paste") {
    path(Segment / Remaining) { (timestamp, modulesWithExt) =>
      get {
        val (modules, ext, isMinified) = handler.parseModulesUrl(modulesWithExt)
        val (content, foundCount) = handler.loadModulesWithCount(modules, ext, isMinified)
        val contentType = ext match {
          case "js" => ContentType(MediaTypes.`application/javascript`, HttpCharsets.`UTF-8`)
          case "css" => ContentType(MediaTypes.`text/css`, HttpCharsets.`UTF-8`)
          case _ => ContentTypes.`text/plain(UTF-8)`
        }
        
        // Return 404 with no-cache for missing modules; cache immutable assets for 1 year
        if (foundCount == 0) {
          complete(HttpResponse(
            status = StatusCodes.NotFound,
            headers = List(`Cache-Control`(`no-cache`)),
            entity = HttpEntity(contentType, "/* modules not found */")
          ))
        } else {
          complete(HttpResponse(
            status = StatusCodes.OK,
            headers = List(`Cache-Control`(`public`, `max-age`(31536000))),
            entity = HttpEntity(contentType, content)
          ))
        }
      }
    }
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
