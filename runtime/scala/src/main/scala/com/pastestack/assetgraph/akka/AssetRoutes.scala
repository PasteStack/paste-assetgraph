package com.pastestack.assetgraph.akka

import akka.http.scaladsl.model._
import akka.http.scaladsl.model.headers.CacheDirectives
import akka.http.scaladsl.server.Directives._
import akka.http.scaladsl.server.Route
import com.pastestack.assetgraph.Manifest

import java.nio.file.{Files, Path, Paths}
import scala.concurrent.ExecutionContext

/**
 * Akka HTTP routes for serving assets via paste-assetgraph manifest.
 * 
 * Usage:
 * {{{
 * val manifest = Manifest.load("public/manifest.json")
 * val assetRoutes = new AssetRoutes(manifest, Paths.get("public"))
 * 
 * val routes = assetRoutes.routes ~ otherRoutes
 * 
 * // In templates:
 * val cssPath = assetRoutes.resolve("css/main.css")
 * val jamUrl = assetRoutes.jamUrl(Seq("paste.dom", "paste.ui.heroscroll"))
 * }}}
 */
class AssetRoutes(
  val manifest: Manifest,
  publicDir: Path,
  debug: Boolean = false
)(implicit ec: ExecutionContext) {
  
  /**
   * All asset routes: JAM endpoint + static files.
   */
  def routes: Route = jamRoute ~ staticRoute
  
  /**
   * JAM endpoint: /paste/{version}/{checksum}/{modules}.min.js
   * 
   * Bundles and serves modules with dependencies resolved.
   */
  def jamRoute: Route = {
    pathPrefix("paste" / Segment / Segment / Remaining) { (version, checksum, modulesPath) =>
      get {
        val isMinified = modulesPath.endsWith(".min.js")
        val modulesStr = modulesPath
          .stripSuffix(".min.js")
          .stripSuffix(".js")
        
        val modules = modulesStr.split(",").toSeq
        
        // Resolve and concatenate modules with dependencies
        val contentParts = for {
          module <- modules
          depModule <- manifest.getDependencies(module)
          key = s"js/${depModule.replace(".", "/")}.js"
          entry <- manifest.assets.get(key)
          sourcePath = Paths.get(entry.source)
          if Files.exists(sourcePath)
        } yield Files.readString(sourcePath)
        
        val content = contentParts.mkString("\n")
        
        respondWithHeader(
          headers.`Cache-Control`(CacheDirectives.public, CacheDirectives.`max-age`(31536000))
        ) {
          complete(HttpEntity(ContentTypes.`text/plain(UTF-8)`, content))
        }
      }
    }
  }
  
  /**
   * Static file route: serves versioned assets from public directory.
   */
  def staticRoute: Route = {
    (pathPrefix("js") | pathPrefix("css") | pathPrefix("images")) {
      getFromDirectory(publicDir.toString)
    }
  }
  
  /**
   * Resolve an asset path using the manifest.
   * 
   * Use in templates:
   * {{{
   * <link rel="stylesheet" href="@assetRoutes.resolve("css/main.css")">
   * }}}
   */
  def resolve(sourcePath: String): Option[String] = {
    manifest.resolve(sourcePath, debug)
  }
  
  /**
   * Get JAM URL for modules.
   * 
   * Use in templates:
   * {{{
   * <script src="@assetRoutes.jamUrl(Seq("paste.dom", "paste.ui.heroscroll"))" defer></script>
   * }}}
   */
  def jamUrl(modules: Seq[String]): String = {
    manifest.jamUrl(modules, debug)
  }
}

object AssetRoutes {
  
  /**
   * Create from manifest file path.
   */
  def apply(
    manifestPath: Path,
    publicDir: Path,
    debug: Boolean = false
  )(implicit ec: ExecutionContext): AssetRoutes = {
    val manifest = Manifest.load(manifestPath)
    new AssetRoutes(manifest, publicDir, debug)
  }
  
  /**
   * Create from manifest file path strings.
   */
  def apply(
    manifestPath: String,
    publicDir: String,
    debug: Boolean
  )(implicit ec: ExecutionContext): AssetRoutes = {
    apply(Path.of(manifestPath), Path.of(publicDir), debug)
  }
  
  /**
   * Create from classpath resource.
   */
  def fromResource(
    manifestResource: String,
    publicDir: Path,
    debug: Boolean = false
  )(implicit ec: ExecutionContext): AssetRoutes = {
    val manifest = Manifest.loadFromResource(manifestResource)
    new AssetRoutes(manifest, publicDir, debug)
  }
}
