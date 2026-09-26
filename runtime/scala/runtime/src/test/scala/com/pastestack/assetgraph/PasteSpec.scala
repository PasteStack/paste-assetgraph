package com.pastestack.assetgraph

import java.io.ByteArrayInputStream
import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.{Files, Path}
import java.security.MessageDigest
import org.scalatest.flatspec.AnyFlatSpec
import org.scalatest.matchers.should.Matchers
import TestFiles.*

class PasteSpec extends AnyFlatSpec with Matchers {

  private val ManifestJson = """{
    "version": "42",
    "generated": "2026-01-01T00:00:00Z",
    "modules": {
      "paste": "js/paste.js",
      "app": "js/app.js",
      "site.theme": "css/theme.css"
    },
    "assets": {
      "js/paste.js": {
        "source": "src/paste.js", "output": "/js/paste.h.v1.min.js", "hash": "h",
        "version": 1, "timestamp": "2026-01-01T00:00:00Z",
        "dependencies": [], "history": []
      },
      "js/app.js": {
        "source": "src/app.js", "output": "/js/app.h.v1.min.js", "hash": "h",
        "version": 1, "timestamp": "2026-01-01T00:00:00Z",
        "dependencies": [], "history": []
      },
      "css/theme.css": {
        "source": "src/theme.scss", "output": "/css/theme.h.v1.min.css", "hash": "h",
        "version": 1, "timestamp": "2026-01-01T00:00:00Z",
        "dependencies": [], "history": []
      }
    },
    "sorted_deps": {}
  }"""

  /** A manifest file plus its compiled outputs, in the layout the build writes. */
  private def e2eFixture(root: Path): (Path, Path) = {
    val out = root.resolve("public")
    writeOutput(out, "/js/paste.h.v1.min.js", "PASTE_RT")
    writeOutput(out, "/js/app.h.v1.min.js", "APP")
    writeOutput(out, "/css/theme.h.v1.min.css", "CSS")
    val manifest = write(root.resolve("manifest.json"), ManifestJson)
    (manifest, out)
  }

  "Paste" should "render pages end-to-end through the library" in
    withTempDir("paste-e2e") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))
      val page = paste.page()

      page.jam(css = Seq("site.theme"), js = Seq("app")) shouldBe ""

      val cssTag = page.dump(AssetType.Css)
      cssTag should include("stylesheet")
      cssTag should include("site.theme+v1.0")

      val jsTags = page.dump(AssetType.Js)
      jsTags should startWith("<script type=\"text/javascript\">PASTE_RT")
      jsTags should include("(function(d,r){")
      jsTags should include("app+v1.0")

      // Identical to rendering the same event list directly.
      val eventsJson = Paste.encodeEvents(Vector(
        Paste.JamEvent(Jam(css = Seq("site.theme"), js = Seq("app"))),
        Paste.DumpEvent(Some(AssetType.Css)),
        Paste.DumpEvent(Some(AssetType.Js))
      ))
      jsTags shouldBe paste.render(eventsJson)

      // A complete page seals cleanly — and isFinished reports it.
      page.isFinished shouldBe false
      page.finish() shouldBe ""
      page.isFinished shouldBe true
      page.finish() shouldBe ""
    }

  it should "dump every type in one call, equal to the two typed dumps" in
    withTempDir("paste-e2e-all") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))

      val reference = paste.page()
      reference.jam(css = Seq("site.theme"), js = Seq("app"))
      val expected = reference.dump(AssetType.Css) + "\n" + reference.dump(AssetType.Js)

      val page = paste.page()
      page.jam(css = Seq("site.theme"), js = Seq("app"))
      page.dump() shouldBe expected
      page.finish() shouldBe ""
      page.isFinished shouldBe true
    }

  it should "refuse finish while a jam's type was never dumped, then recover" in
    withTempDir("paste-finish") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))
      val page = paste.page()

      // The css dump already ran when the jam lands: site.theme is queued
      // for a dump that never comes, and finishing on the js dump must
      // refuse — not drop the stylesheet silently.
      page.dump(AssetType.Css)
      page.jam(css = Seq("site.theme"), js = Seq("app"))
      page.dump(AssetType.Js)
      val ex = intercept[PasteException](page.finish())
      ex.getMessage should include("site.theme")

      // A failed finish leaves the page open: deliver the missing dump,
      // then finish — and finishing again is a no-op.
      page.isFinished shouldBe false
      page.dump(AssetType.Css)
      page.finish() shouldBe ""
      page.isFinished shouldBe true

      // A sealed page accepts no more events.
      intercept[PasteException](page.jam(js = Seq("app")))
      intercept[PasteException](page.dump(AssetType.Js))
    }

  it should "serve a versioned combo with a year cache and a 404 with no-cache" in
    withTempDir("paste-combo") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out))

      // 1767225600 is the fixture stamp (2026-01-01T00:00:00Z).
      val versioned = paste.combo("/paste/1767225600/app+v1.0.min.js")
      versioned.status shouldBe 200
      versioned.cacheable shouldBe true
      versioned.cacheControl shouldBe "public, max-age=31536000"
      versioned.location shouldBe None
      versioned.body shouldBe "APP"

      val missing = paste.combo("/paste/1767225600/ghost+v1.0.min.js")
      missing.status shouldBe 404
      missing.cacheable shouldBe false
      missing.cacheControl shouldBe "no-cache"
    }

  it should "carry the canonical URL in location for a stale stamp" in
    withTempDir("paste-combo-redirect") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out))

      val redirected = paste.combo("/paste/1767225599/app+v1.0.min.js")
      redirected.status shouldBe 302
      redirected.location shouldBe Some("/paste/1767225600/app+v1.0.min.js")
      redirected.cacheControl shouldBe "no-cache"
      redirected.body shouldBe ""
    }

  /** The content hashes the contract records for this fixture's bytes —
    * golden vectors; hash calculation is the compiler's, tested in Rust. */
  private val HashApp = "0bdbb2c325525e986d55ef50ca66d056"
  private val HashPasteRt = "7e9e92b0857c477a6e8aebb0ab2bd924"
  private val HashCss = "2c56c360580420d293172f42d85dfbed"
  private val HashBogus = "90affbd9a1954ec9ff029b7ad7183a16"

  /** The fixture manifest with `hash_urls` on and recorded hashes — the
    * contract stays in the library; the wrapper only forwards paths. */
  private def hashedManifestJson: String =
    s"""{
      "version": "42",
      "generated": "2026-01-01T00:00:00Z",
      "hash_urls": true,
      "modules": { "paste": "js/paste.js", "app": "js/app.js", "site.theme": "css/theme.css" },
      "assets": {
        "js/paste.js": {
          "source": "src/paste.js", "output": "/js/paste.h.v1.min.js", "hash": "$HashPasteRt",
          "version": 1, "timestamp": "2026-01-01T00:00:00Z",
          "dependencies": [], "history": []
        },
        "js/app.js": {
          "source": "src/app.js", "output": "/js/app.h.v1.min.js", "hash": "$HashApp",
          "version": 1, "timestamp": "2026-01-01T00:00:00Z",
          "dependencies": [], "history": []
        },
        "css/theme.css": {
          "source": "src/theme.scss", "output": "/css/theme.h.v1.min.css", "hash": "$HashCss",
          "version": 1, "timestamp": "2026-01-01T00:00:00Z",
          "dependencies": [], "history": []
        }
      },
      "sorted_deps": {}
    }"""

  it should "render, serve, redirect, and validate hash-qualified URLs" in
    withTempDir("paste-combo-hash") { root =>
      val appHash = HashApp

      val out = root.resolve("public")
      writeOutput(out, "/js/paste.h.v1.min.js", "PASTE_RT")
      writeOutput(out, "/js/app.h.v1.min.js", "APP")
      writeOutput(out, "/css/theme.h.v1.min.css", "CSS")
      val manifestPath = write(root.resolve("manifest.json"), hashedManifestJson)
      val paste = Paste(Paste.Settings(manifestPath, out, inlineThreshold = 0))

      // Render emits the qualified token.
      val page = paste.page()
      page.jam(js = Seq("app"))
      page.dump(AssetType.Js) should include(s"app+v1.0-h$appHash")

      // The qualified combo serves 200.
      val qualified = paste.combo(s"/paste/1767225600/app+v1.0-h$appHash.min.js")
      qualified.status shouldBe 200
      qualified.body shouldBe "APP"
      qualified.cacheControl shouldBe "public, max-age=31536000"

      // A legacy request redirects to the qualified canonical URL.
      val legacy = paste.combo("/paste/1767225600/app+v1.0.min.js")
      legacy.status shouldBe 302
      legacy.location shouldBe Some(s"/paste/1767225600/app+v1.0-h$appHash.min.js")

      // A modified hash is a 404 — never a redirect onto different bytes.
      val wrong = paste.combo(s"/paste/1767225600/app+v1.0-h$HashBogus.min.js")
      wrong.status shouldBe 404
      wrong.location shouldBe None
      wrong.cacheControl shouldBe "no-cache"

      // Opted out, the same manifest serves and renders the legacy form —
      // the version unchanged — while a qualified URL still resolves.
      val optedOutPath = write(
        root.resolve("manifest-legacy.json"),
        hashedManifestJson.replace("\"hash_urls\": true", "\"hash_urls\": false")
      )
      val optedOut = Paste(Paste.Settings(optedOutPath, out, inlineThreshold = 0))
      optedOut.combo("/paste/1767225600/app+v1.0.min.js").status shouldBe 200
      optedOut.combo(s"/paste/1767225600/app+v1.0-h$appHash.min.js").status shouldBe 200
      val page2 = optedOut.page()
      page2.jam(js = Seq("app"))
      val tags = page2.dump(AssetType.Js)
      tags should include("app+v1.0.min.js")
      tags should not include "-h"
    }

  it should "reject a combo path containing NUL rather than truncating it" in
    withTempDir("paste-combo-nul") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out))
      val ex = the[PasteException] thrownBy
        paste.combo("/paste/app.js\u0000/paste/1767225600/app+v1.0.min.js")
      ex.getMessage shouldBe "combo path must not contain NUL"
    }

  it should "raise PasteException naming the module for an unknown module" in
    withTempDir("paste-unknown") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out))
      val eventsJson = Paste.encodeEvents(Vector(
        Paste.JamEvent(Jam(js = Seq("ghost"))),
        Paste.DumpEvent(Some(AssetType.Js))
      ))
      val ex = the[PasteException] thrownBy paste.render(eventsJson)
      ex.getMessage should include("ghost")
    }

  it should "copy a classpath manifest and refuse a missing resource" in
    withTempDir("paste-from-classpath") { root =>
      val loader: String => Option[java.io.InputStream] = name =>
        Option.when(name == "/paste-assetgraph-manifest.json")(getClass.getResourceAsStream(name))
      val paste = Paste.fromClasspath(
        loader, "/paste-assetgraph-manifest.json", root, "", "/paste", 1024, root
      )
      Files.exists(paste.settings.manifest) shouldBe true
      new String(Files.readAllBytes(paste.settings.manifest), UTF_8) should include("spec")
      an[IllegalArgumentException] shouldBe thrownBy(
        Paste.fromClasspath(
          loader, "/no-such-manifest.json", root, "", "/paste", 1024, root
        )
      )
    }

  it should "close the manifest stream and remove the temp file when the copy fails" in
    withTempDir("paste-fail") { root =>
      val closed = scala.concurrent.Promise[Unit]()
      val failing = new java.io.InputStream {
        override def read(): Int = throw new java.io.IOException("boom")
        override def close(): Unit = { closed.trySuccess(()); super.close() }
      }
      val loader: String => Option[java.io.InputStream] = _ => Some(failing)

      an[java.io.IOException] shouldBe thrownBy(
        Paste.fromClasspath(
          loader, "/paste-assetgraph-manifest.json", root, "", "/paste", 1024, root
        )
      )
      closed.isCompleted shouldBe true
      // The failed copy deleted its temp file under the test-owned root.
      scala.util.Using.resource(Files.list(root))(
        _.filter(_.getFileName.toString.startsWith("paste-assetgraph-manifest-")).count()
      ) shouldBe 0
    }

  /** The library file name this platform's bundle carries. */
  private def libraryFileName: String =
    System.getProperty("os.name").toLowerCase(java.util.Locale.ROOT) match {
      case os if os.contains("win")                        => "paste_assetgraph_ffi.dll"
      case os if os.contains("mac") || os.contains("darwin") => "libpaste_assetgraph_ffi.dylib"
      case _                                               => "libpaste_assetgraph_ffi.so"
    }

  /** Count of the loader's own temp directories under a test-owned root. */
  private def extractionTemps(root: Path): Long =
    scala.util.Using.resource(Files.list(root))(
      _.filter(_.getFileName.toString.startsWith("paste-assetgraph-")).count()
    )

  "PasteLibrary" should "prefer the PASTE_ASSETGRAPH_LIB override" in
    withTempDir("paste-lib-env") { root =>
      val lib = write(root.resolve("fake-lib.dylib"), "x")
      val resolved = PasteLibrary.resolveWith(
        name => if (name == "PASTE_ASSETGRAPH_LIB") Some(lib.toString) else None,
        None
      )
      resolved shouldBe lib
    }

  it should "fail loudly when PASTE_ASSETGRAPH_LIB is set but not a regular file" in
    withTempDir("paste-lib-bad-env") { root =>
      val missing = root.resolve("missing")
      val ex = the[IllegalStateException] thrownBy PasteLibrary.resolveWith(
        name => if (name == "PASTE_ASSETGRAPH_LIB") Some(missing.toString) else None,
        None
      )
      ex.getMessage should include(missing.toString)

      val dir = Files.createDirectory(root.resolve("dir"))
      val ex2 = the[IllegalStateException] thrownBy PasteLibrary.resolveWith(
        name => if (name == "PASTE_ASSETGRAPH_LIB") Some(dir.toString) else None,
        None
      )
      ex2.getMessage should include(dir.toString)
    }

  it should "map os.name and os.arch to the packaged platform names" in {
    PasteLibrary.platformName("Linux", "amd64") shouldBe "linux-amd64"
    PasteLibrary.platformName("Linux", "x86_64") shouldBe "linux-amd64"
    PasteLibrary.platformName("Linux", "aarch64") shouldBe "linux-aarch64"
    PasteLibrary.platformName("Mac OS X", "aarch64") shouldBe "darwin-aarch64"
    PasteLibrary.platformName("Mac OS X", "x86_64") shouldBe "darwin-amd64"
    PasteLibrary.platformName("Windows 11", "amd64") shouldBe "windows-amd64"
    PasteLibrary.platformName("FreeBSD", "riscv64") shouldBe "unknown-riscv64"
  }

  it should "carry the libc on Linux platform names only" in {
    PasteLibrary.platformName("Linux", "amd64", musl = true) shouldBe "linux-amd64-musl"
    PasteLibrary.platformName("Mac OS X", "aarch64", musl = true) shouldBe "darwin-aarch64"
    // The running JVM's own segment resolves through the host probe.
    PasteLibrary.currentPlatform shouldBe PasteLibrary.platformName(
      System.getProperty("os.name"),
      System.getProperty("os.arch"),
      PasteLibrary.currentPlatform.contains("-musl")
    )
  }

  it should "extract a bundled library verified against the flat checksum manifest" in
    withTempDir("paste-extract") { root =>
      val platform = PasteLibrary.currentPlatform
      val sha = MessageDigest.getInstance("SHA-256")
        .digest("LIBRARY".getBytes(UTF_8))
        .map(b => f"${b & 0xff}%02x").mkString
      val resources: String => Option[java.io.InputStream] = name =>
        if (name == "/native-lib/manifest.json")
          Some(new ByteArrayInputStream(s"""{"$platform": "$sha"}""".getBytes(UTF_8)))
        else if (name == s"/native-lib/$platform/$libraryFileName")
          Some(new ByteArrayInputStream("LIBRARY".getBytes(UTF_8)))
        else None
      val extracted = PasteLibrary.extractBundled(resources, root)
      extracted shouldBe defined
      new String(Files.readAllBytes(extracted.get), UTF_8) shouldBe "LIBRARY"
    }

  it should "close the stream and leave no temp directory when extraction fails" in
    withTempDir("paste-extract-fail") { root =>
      val closed = scala.concurrent.Promise[Unit]()
      val failing = new java.io.InputStream {
        override def read(): Int = throw new java.io.IOException("boom")
        override def close(): Unit = { closed.trySuccess(()); super.close() }
      }
      val platform = PasteLibrary.currentPlatform
      val resources: String => Option[java.io.InputStream] = name =>
        Option.when(name == s"/native-lib/$platform/$libraryFileName")(failing)

      an[java.io.IOException] shouldBe thrownBy(PasteLibrary.extractBundled(resources, root))
      closed.isCompleted shouldBe true
      extractionTemps(root) shouldBe 0
    }

  it should "fail loudly when a bundled library's checksum mismatches, removing the extraction" in
    withTempDir("paste-extract-bad-sha") { root =>
      val platform = PasteLibrary.currentPlatform
      val resources: String => Option[java.io.InputStream] = name =>
        if (name == "/native-lib/manifest.json")
          Some(new ByteArrayInputStream(s"""{"$platform": "${"0" * 64}"}""".getBytes(UTF_8)))
        else
          Some(new ByteArrayInputStream("LIBRARY".getBytes(UTF_8)))
      val ex = the[IllegalStateException] thrownBy PasteLibrary.extractBundled(resources, root)
      ex.getMessage should include("checksum")
      extractionTemps(root) shouldBe 0
    }

  it should "fail loudly when the checksum manifest lacks the platform, removing the extraction" in
    withTempDir("paste-extract-no-entry") { root =>
      val resources: String => Option[java.io.InputStream] = name =>
        if (name == "/native-lib/manifest.json")
          Some(new ByteArrayInputStream(s"""{"other-platform": "${"0" * 64}"}""".getBytes(UTF_8)))
        else
          Some(new ByteArrayInputStream("LIBRARY".getBytes(UTF_8)))
      val ex = the[IllegalStateException] thrownBy PasteLibrary.extractBundled(resources, root)
      ex.getMessage should include("no entry")
      extractionTemps(root) shouldBe 0
    }

  it should "name both search locations and the platform when nothing resolves" in {
    val ex = the[IllegalStateException] thrownBy
      PasteLibrary.resolveWith(_ => None, None)
    ex.getMessage should include("PASTE_ASSETGRAPH_LIB")
    ex.getMessage should include("/native-lib/")
    ex.getMessage should include(PasteLibrary.currentPlatform)
  }
}
