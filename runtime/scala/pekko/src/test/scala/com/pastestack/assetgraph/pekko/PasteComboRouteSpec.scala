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

import java.nio.file.Path
import scala.concurrent.duration.DurationInt
import org.apache.pekko.http.scaladsl.model.StatusCodes
import org.apache.pekko.http.scaladsl.server.Directives.*
import org.apache.pekko.http.scaladsl.testkit.{RouteTestTimeout, ScalatestRouteTest}
import org.scalatest.flatspec.AnyFlatSpec
import org.scalatest.matchers.should.Matchers
import com.pastestack.assetgraph.Paste
import com.pastestack.assetgraph.TestFiles.*

/**
 * The route is a pass-through: the paste-assetgraph library decides status,
 * cacheability, content type and body for every URL under `basePath`.
 */
final class PasteComboRouteSpec extends AnyFlatSpec with Matchers with ScalatestRouteTest {

  // The first served request pays the one-time native library load — the
  // testkit's 1 s default is tighter than that cost on a cold JVM.
  given RouteTestTimeout = RouteTestTimeout(10.seconds)

  private val ManifestJson = """{
    "version": "42",
    "generated": "2026-01-01T00:00:00Z",
    "modules": { "app": "js/app.js" },
    "assets": {
      "js/app.js": {
        "source": "src/app.js", "output": "/js/app.h.v1.min.js",
        "hash": "0bdbb2c325525e986d55ef50ca66d056",
        "version": 1, "timestamp": "2026-01-01T00:00:00Z",
        "dependencies": [], "history": []
      }
    },
    "sorted_deps": {}
  }"""

  private def routeFor(root: Path, basePath: String = "/paste"): PasteComboRoute = {
    val out = root.resolve("public")
    writeOutput(out, "/js/app.h.v1.min.js", "APP")
    val manifestPath = write(root.resolve("manifest.json"), ManifestJson)
    new PasteComboRoute(Paste(Paste.Settings(manifestPath, out, basePath = basePath)))
  }

  private def cacheControl: String =
    response.headers
      .find(_.lowercaseName() == "cache-control")
      .map(_.value())
      .getOrElse("")

  "PasteComboRoute" should "serve a versioned URL with a year-long cache header" in
    withTempDir("paste-route-versioned") { root =>
      // 1767225600 is the fixture stamp (2026-01-01T00:00:00Z).
      Get("/paste/1767225600/app+v1.0.min.js") ~> routeFor(root).route ~> check {
        status shouldBe StatusCodes.OK
        cacheControl shouldBe "public, max-age=31536000"
        responseAs[String] shouldBe "APP"
        contentType.mediaType.toString shouldBe "application/javascript"
      }
    }

  // The server layer drops a HEAD response's body; the route only has to
  // match the method and answer as it would for GET.
  it should "answer HEAD like GET" in
    withTempDir("paste-route-head") { root =>
      Head("/paste/1767225600/app+v1.0.min.js") ~> routeFor(root).route ~> check {
        status shouldBe StatusCodes.OK
        cacheControl shouldBe "public, max-age=31536000"
      }
    }

  it should "leave non-GET requests on foreign paths to the caller's routes" in
    withTempDir("paste-route-foreign") { root =>
      val routes = concat(routeFor(root).route, path("users") { post { complete("created") } })
      Post("/users") ~> routes ~> check {
        responseAs[String] shouldBe "created"
      }
      Delete("/nothing-here") ~> org.apache.pekko.http.scaladsl.server.Route.seal(routes) ~> check {
        status shouldBe StatusCodes.NotFound
      }
      Put("/users") ~> org.apache.pekko.http.scaladsl.server.Route.seal(routes) ~> check {
        status shouldBe StatusCodes.MethodNotAllowed
        header("Allow").map(_.value()) shouldBe Some("POST")
      }
    }

  it should "forward If-Modified-Since and answer the library's 304" in
    withTempDir("paste-route-conditional") { root =>
      val lastModified = "Thu, 01 Jan 2026 00:00:00 GMT" // the fixture stamp
      def lastModifiedHeader: Option[String] =
        response.headers
          .find(_.lowercaseName() == "last-modified")
          .map(_.value())

      // At the group stamp → the library's 304: no body, Last-Modified
      // written from the response record.
      Get("/paste/1767225600/app+v1.0.min.js")
        .addHeader(org.apache.pekko.http.scaladsl.model.headers.`If-Modified-Since`(
          org.apache.pekko.http.scaladsl.model.DateTime(1767225600000L))) ~>
        routeFor(root).route ~> check {
          status shouldBe StatusCodes.NotModified
          responseAs[String] shouldBe ""
          lastModifiedHeader shouldBe Some(lastModified)
          cacheControl shouldBe "public, max-age=31536000"
        }

      // A stale validator → the ordinary 200, still stamped.
      Get("/paste/1767225600/app+v1.0.min.js")
        .addHeader(org.apache.pekko.http.scaladsl.model.headers.`If-Modified-Since`(
          org.apache.pekko.http.scaladsl.model.DateTime(1735689600000L))) ~>
        routeFor(root).route ~> check {
          status shouldBe StatusCodes.OK
          responseAs[String] shouldBe "APP"
          lastModifiedHeader shouldBe Some(lastModified)
        }
    }

  it should "let an unevaluated If-None-Match suppress the date condition" in
    withTempDir("paste-route-precedence") { root =>
      // RFC 9110 §13.2.2: If-None-Match outranks If-Modified-Since — the
      // library evaluates no entity tags, so the date must not 304 on
      // its own. A nonmatching tag plus a matching date answers 200.
      Get("/paste/1767225600/app+v1.0.min.js")
        .addHeader(org.apache.pekko.http.scaladsl.model.headers.`If-None-Match`(
          org.apache.pekko.http.scaladsl.model.headers.EntityTag("nonexistent-tag")))
        .addHeader(org.apache.pekko.http.scaladsl.model.headers.`If-Modified-Since`(
          org.apache.pekko.http.scaladsl.model.DateTime(1767225600000L))) ~>
        routeFor(root).route ~> check {
          status shouldBe StatusCodes.OK
          responseAs[String] shouldBe "APP"
        }
    }

  it should "write the library's location as the Location header on a 302" in
    withTempDir("paste-route-redirect") { root =>
      Get("/paste/1767225599/app+v1.0.min.js") ~> routeFor(root).route ~> check {
        status shouldBe StatusCodes.Found
        header[org.apache.pekko.http.scaladsl.model.headers.Location]
          .map(_.uri.toString) shouldBe Some("/paste/1767225600/app+v1.0.min.js")
        cacheControl shouldBe "no-cache"
        responseAs[String] shouldBe ""
      }
    }

  it should "forward the raw request spelling so a non-canonical encoding redirects" in
    withTempDir("paste-route-raw") { root =>
      import org.apache.pekko.http.scaladsl.model.headers.{`Raw-Request-URI`, Location, RawHeader}
      val encoded = "/paste/1767225600/app%2Bv1.0.min.js"
      val canonical = "/paste/1767225600/app+v1.0.min.js"
      val route = routeFor(root).route
      // With the server's Raw-Request-URI header the library sees the
      // client's own spelling and answers what Node and Python answer.
      Get(encoded).addHeader(`Raw-Request-URI`(encoded)) ~> route ~> check {
        status shouldBe StatusCodes.Found
        header[Location].map(_.uri.toString) shouldBe Some(canonical)
        cacheControl shouldBe "no-cache"
      }
      // A raw target naming another path is not this request's spelling.
      Get(canonical).addHeader(`Raw-Request-URI`("/paste/1767225600/ghost+v1.0.min.js")) ~>
        route ~> check {
          status shouldBe StatusCodes.OK
          responseAs[String] shouldBe "APP"
        }
      // A client-sent header of the same name is not the server's.
      Get(canonical).addHeader(RawHeader("Raw-Request-URI", encoded)) ~> route ~> check {
        status shouldBe StatusCodes.OK
        responseAs[String] shouldBe "APP"
      }
      // Without the header the parsed path is all there is: Pekko renders
      // it canonically.
      Get(encoded) ~> route ~> check {
        status shouldBe StatusCodes.OK
        responseAs[String] shouldBe "APP"
      }
    }

  it should "write a percent-encoded Location for a Unicode module name" in
    withTempDir("paste-route-unicode") { root =>
      // The library percent-encodes names outside the URL-safe set in a
      // Location; a raw non-ASCII value would be rejected by Uri.
      val out = root.resolve("public")
      writeOutput(out, "/js/site/café.h.v1.min.js", "C")
      val manifestPath = write(
        root.resolve("manifest.json"),
        """{
          "version": "42",
          "generated": "2026-01-01T00:00:00Z",
          "modules": {},
          "assets": {
            "js/site/café.js": {
              "source": "src/site/café.js", "output": "/js/site/café.h.v1.min.js",
              "hash": "0d61f8370cad1d412f80b84d143e1257",
              "version": 1, "timestamp": "2026-01-01T00:00:00Z",
              "dependencies": [], "history": []
            }
          },
          "sorted_deps": {}
        }"""
      )
      val route = new PasteComboRoute(Paste(Paste.Settings(manifestPath, out))).route
      Get("/paste/1767225599/site.caf%C3%A9+v1.0.min.js") ~> route ~> check {
        status shouldBe StatusCodes.Found
        header[org.apache.pekko.http.scaladsl.model.headers.Location]
          .map(_.uri.toString) shouldBe Some("/paste/1767225600/site.caf%C3%A9+v1.0.min.js")
      }
    }

  it should "pass through the qualified Location target and serve a qualified URL" in
    withTempDir("paste-route-hash") { root =>
      // The route only forwards paths and response fields — hash validation
      // is the library's. The fixture pins the content hash the contract
      // records for the body (a golden vector — hash calculation is the
      // compiler's, tested in Rust).
      val appHash = "0bdbb2c325525e986d55ef50ca66d056" // APP
      val bogusHash = "90affbd9a1954ec9ff029b7ad7183a16"

      val out = root.resolve("public")
      writeOutput(out, "/js/app.h.v1.min.js", "APP")
      val manifestPath = write(
        root.resolve("manifest.json"),
        s"""{
          "version": "42",
          "generated": "2026-01-01T00:00:00Z",
          "hash_urls": true,
          "modules": { "app": "js/app.js" },
          "assets": {
            "js/app.js": {
              "source": "src/app.js", "output": "/js/app.h.v1.min.js", "hash": "$appHash",
              "version": 1, "timestamp": "2026-01-01T00:00:00Z",
              "dependencies": [], "history": []
            }
          },
          "sorted_deps": {}
        }"""
      )
      val route = new PasteComboRoute(Paste(Paste.Settings(manifestPath, out))).route

      Get(s"/paste/1767225600/app+v1.0-h$appHash.min.js") ~> route ~> check {
        status shouldBe StatusCodes.OK
        responseAs[String] shouldBe "APP"
      }

      // A legacy request's 302 carries the qualified target verbatim.
      Get("/paste/1767225600/app+v1.0.min.js") ~> route ~> check {
        status shouldBe StatusCodes.Found
        header[org.apache.pekko.http.scaladsl.model.headers.Location]
          .map(_.uri.toString) shouldBe Some(s"/paste/1767225600/app+v1.0-h$appHash.min.js")
      }

      Get(s"/paste/1767225600/app+v1.0-h$bogusHash.min.js") ~> route ~> check {
        status shouldBe StatusCodes.NotFound
        cacheControl shouldBe "no-cache"
      }
    }

  it should "mark an unversioned URL no-cache" in
    withTempDir("paste-route-unversioned") { root =>
      Get("/paste/app.js") ~> routeFor(root).route ~> check {
        status shouldBe StatusCodes.OK
        cacheControl shouldBe "no-cache"
        responseAs[String] shouldBe "APP"
      }
    }

  it should "pass through a 404 for a missing module" in
    withTempDir("paste-route-missing") { root =>
      Get("/paste/1767225600/ghost+v1.0.min.js") ~> routeFor(root).route ~> check {
        status shouldBe StatusCodes.NotFound
        cacheControl shouldBe "no-cache"
        responseAs[String] shouldBe "/* modules not found */"
      }
    }

  it should "reject a path outside basePath" in
    withTempDir("paste-route-outside") { root =>
      Get("/other/app.js") ~> routeFor(root).route ~> check {
        handled shouldBe false
      }
    }

  it should "reject a path that shares only a string prefix with basePath" in
    withTempDir("paste-route-prefix") { root =>
      Get("/pastebin/x") ~> routeFor(root).route ~> check {
        handled shouldBe false
      }
    }

  it should "reject the bare basePath" in
    withTempDir("paste-route-bare") { root =>
      Get("/paste") ~> routeFor(root).route ~> check {
        handled shouldBe false
      }
    }

  it should "forward the bare trailing-slash basePath to the library's 404" in
    withTempDir("paste-route-slash") { root =>
      Get("/paste/") ~> routeFor(root).route ~> check {
        status shouldBe StatusCodes.NotFound
        cacheControl shouldBe "no-cache"
      }
    }

  it should "compose under an enclosing pathPrefix when basePath includes it" in
    withTempDir("paste-route-nested") { root =>
      val route = pathPrefix("app") { routeFor(root, "/app/paste").route }
      Get("/app/paste/1767225600/app+v1.0.min.js") ~> route ~> check {
        status shouldBe StatusCodes.OK
        responseAs[String] shouldBe "APP"
      }
    }

  it should "reject a nested request when basePath lacks the prefix" in
    withTempDir("paste-route-nested-default") { root =>
      val route = pathPrefix("app") { routeFor(root).route }
      Get("/app/paste/1767225600/app+v1.0.min.js") ~> route ~> check {
        handled shouldBe false
      }
    }
}
