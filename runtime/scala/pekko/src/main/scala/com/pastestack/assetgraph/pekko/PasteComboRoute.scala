package com.pastestack.assetgraph.pekko

import scala.concurrent.{ExecutionContext, Future}
import org.apache.pekko.http.scaladsl.model.*
import org.apache.pekko.http.scaladsl.model.headers.{Location, RawHeader}
import org.apache.pekko.http.scaladsl.server.Directives.*
import org.apache.pekko.http.scaladsl.server.Route
import com.pastestack.assetgraph.{ComboResponse, Paste}

/**
 * Pekko HTTP route that delegates every URL under `paste.basePath` to the
 * paste-assetgraph library via [[Paste.combo]].
 *
 * Usage:
 * {{{
 * val paste = Paste.fromClasspath()
 * val routes = concat(new PasteComboRoute(paste).route, ...)
 * }}}
 *
 * The match runs against the full request path — not the remaining path —
 * so the route composes under an enclosing `pathPrefix`: set `basePath` to
 * the complete prefix (e.g. `"/app/paste"` under `pathPrefix("app")`).
 *
 * The library decides status, cacheability, content type and body; this
 * route only forwards them — including a 302's `location` as the `Location`
 * header. Combo resolution runs on the default blocking-IO dispatcher since
 * the library call reads files.
 */
final class PasteComboRoute(paste: Paste) {

  val route: Route = get {
    extractUri { uri =>
      // Segment-anchored: a path that shares only a string prefix
      // ("/pastebin/x") or the bare basePath is rejected for sibling
      // routes; "/paste/" itself forwards and the library 404s it.
      if (!uri.path.startsWith(Uri.Path(paste.basePath + "/"))) {
        reject
      } else {
        extractActorSystem { system =>
          val blockingIo =
            system.dispatchers.lookup("pekko.actor.default-blocking-io-dispatcher")
          val path = uri.path.toString
          val pathAndQuery = uri.rawQueryString match {
            case Some(query) => s"$path?$query"
            case None        => path
          }
          complete(
            Future(paste.combo(pathAndQuery))(blockingIo)
              .map(toResponse)(ExecutionContext.parasitic)
          )
        }
      }
    }
  }

  private def toResponse(response: ComboResponse): HttpResponse = {
    val contentType = ContentType
      .parse(response.contentType)
      .getOrElse(ContentTypes.`text/plain(UTF-8)`)
    val entity = contentType match {
      case nonBinary: ContentType.NonBinary =>
        HttpEntity(nonBinary, response.body)
      case other =>
        HttpEntity(other, org.apache.pekko.util.ByteString(response.body))
    }
    HttpResponse(
      status = StatusCodes
        .getForKey(response.status)
        .getOrElse(StatusCodes.InternalServerError),
      headers = RawHeader("Cache-Control", response.cacheControl) ::
        response.location.map(target => Location(Uri(target))).toList,
      entity = entity
    )
  }
}
