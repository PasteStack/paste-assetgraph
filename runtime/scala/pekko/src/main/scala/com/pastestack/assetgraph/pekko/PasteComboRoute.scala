// Copyright (c) 2017–present Tomshley LLC
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package com.pastestack.assetgraph.pekko

import scala.concurrent.{ExecutionContext, Future}
import scala.util.Try
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
 * header. Combo resolution reads files, so it runs on a blocking dispatcher:
 * Pekko's default blocking-IO dispatcher unless `dispatcherId` names one the
 * application configured to isolate and bound this work.
 */
final class PasteComboRoute(
    paste: Paste,
    dispatcherId: String = "pekko.actor.default-blocking-io-dispatcher"
) {

  // The prefix never changes for a given route — parse it once instead of
  // reconstructing the same Uri.Path on every request.
  private val comboPrefix: Uri.Path = Uri.Path(paste.basePath + "/")

  // The prefix is checked before the method, so a request this route does
  // not own falls through with an empty rejection (a 404 when nothing else
  // matches) instead of a GET method rejection on the caller's paths.
  val route: Route =
    extractUri { uri =>
      // Segment-anchored: a path that shares only a string prefix
      // ("/pastebin/x") or the bare basePath is rejected for sibling
      // routes; "/paste/" itself forwards and the library 404s it.
      if (!uri.path.startsWith(comboPrefix)) {
        reject
      } else (get | head) {
        (extractActorSystem & optionalHeaderValuePF(PasteComboRoute.rawPath)) { (system, raw) =>
          val blockingIo = system.dispatchers.lookup(dispatcherId)
          // The library's canonical-spelling check needs the path as the
          // client wrote it: Pekko's parsed path re-encodes on render, so
          // `app%2Bv1.0` would reach it as the canonical `app+v1.0` and
          // serve where the other runtimes redirect. The server's
          // Raw-Request-URI header carries the request target verbatim;
          // it is used only when it names the same path the server parsed.
          val path = raw
            .filter(target => Try(Uri(target).path).toOption.contains(uri.path))
            .getOrElse(uri.path.toString)
          val pathAndQuery = uri.rawQueryString match {
            case Some(query) => s"$path?$query"
            case None        => path
          }
          // The request's validator forwards to the same Rust decision
          // the other runtimes make — a 304 here is the library's, not
          // a route-computed one.
          optionalHeaderValueByName("If-Modified-Since") { ifModifiedSince =>
            optionalHeaderValueByName("If-None-Match") { ifNoneMatch =>
              // RFC 9110 §13.2.2: If-None-Match takes precedence — while
              // an entity-tag condition stands unevaluated (this API
              // evaluates none), the date condition must not answer on
              // its own. Suppressing it answers what an unsupported
              // validator is worth: nothing.
              val effectiveSince =
                ifModifiedSince.filter(_ => ifNoneMatch.isEmpty)
              complete(
                Future(paste.combo(pathAndQuery, effectiveSince))(blockingIo)
                  .map(toResponse)(ExecutionContext.parasitic)
              )
            }
          }
        }
      }
    }

  private def toResponse(response: ComboResponse): HttpResponse = {
    val contentType = PasteComboRoute.contentType(response.contentType)
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
        (response.location.map(target => Location(Uri(target))).toList ++
          response.lastModified.map(lm => RawHeader("Last-Modified", lm))),
      entity = entity
    )
  }
}

private object PasteComboRoute {

  /** The origin-form path of the server's `Raw-Request-URI` header — present
    * when the application sets `pekko.http.server.raw-request-uri-header = on`.
    * The modeled header is server-generated; a client-sent header of the same
    * name parses as a plain `RawHeader` and never matches. Anything but an
    * origin-form target falls back to the parsed path. */
  val rawPath: PartialFunction[HttpHeader, String] = {
    case headers.`Raw-Request-URI`(target) if target.startsWith("/") =>
      target.takeWhile(c => c != '?' && c != '#')
  }

  /** The library answers a handful of fixed content types — parse each
    * once instead of on every request; an unknown one still parses. */
  private val known: Map[String, ContentType] =
    Seq(
      "application/javascript; charset=utf-8",
      "text/css; charset=utf-8",
      "text/plain; charset=utf-8"
    ).flatMap(value => ContentType.parse(value).toOption.map(value -> _)).toMap

  def contentType(value: String): ContentType =
    known.getOrElse(
      value,
      ContentType.parse(value).getOrElse(ContentTypes.`text/plain(UTF-8)`)
    )
}
