package com.pastestack.assetgraph

import java.io.ByteArrayInputStream
import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.{Files, Path, Paths}
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
        "source": "src/paste.js", "output": "/js/paste.h.v1.min.js",
        "hash": "7e9e92b0857c477a6e8aebb0ab2bd924",
        "version": 1, "timestamp": "2026-01-01T00:00:00Z",
        "dependencies": [], "history": []
      },
      "js/app.js": {
        "source": "src/app.js", "output": "/js/app.h.v1.min.js",
        "hash": "0bdbb2c325525e986d55ef50ca66d056",
        "version": 1, "timestamp": "2026-01-01T00:00:00Z",
        "dependencies": [], "history": []
      },
      "css/theme.css": {
        "source": "src/theme.scss", "output": "/css/theme.h.v1.min.css",
        "hash": "2c56c360580420d293172f42d85dfbed",
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
      jsTags shouldBe paste.renderPage(eventsJson, "")._1

      // A complete page seals cleanly — and isFinished reports it.
      page.isFinished shouldBe false
      page.finish() shouldBe ""
      page.isFinished shouldBe true
      page.finish() shouldBe ""
    }

  it should "raise PasteException, not a fatal LinkageError, for an unloadable library" in
    withTempDir("paste-bad-lib") { root =>
      val notALibrary = write(root.resolve("not-a-library.so"), "plain text")
      val error = intercept[PasteException](PasteLibrary.load(notALibrary.toString))
      error.getMessage should include("failed to load")
      error.getCause shouldBe a[LinkageError]
    }

  it should "retire the page when a build lands mid-page" in
    withTempDir("paste-generation") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))
      val page = paste.page()
      page.jam(css = Seq("site.theme"), js = Seq("app"))
      page.dump(AssetType.Css)

      // A build lands between the page's dumps — the manifest's bytes
      // move and so does its generation token (the mtime bump moves the
      // stamp past filesystem granularity — the rewrite is the same
      // length).
      write(
        manifest,
        ManifestJson.replace("\"version\": \"42\"", "\"version\": \"43\"")
      )
      val future = java.nio.file.attribute.FileTime.fromMillis(
        System.currentTimeMillis() + 60000
      )
      Files.setLastModifiedTime(manifest, future)

      val ex = the[PasteException] thrownBy page.dump(AssetType.Js)
      ex.kind shouldBe Some(PasteException.ManifestGeneration)
      ex.getMessage should include("generation")

      // The failure is terminal: every later step answers the recorded
      // error rather than rendering against a manifest the page's
      // earlier dumps were not planned with.
      for (step <- Seq(
          () => page.jam(js = Seq("app")),
          () => page.dump(),
          () => page.finish()
      )) {
        val again = the[PasteException] thrownBy step()
        again.kind shouldBe Some(PasteException.ManifestGeneration)
      }
    }

  it should "request each versioned file under perModule, overridable per jam" in
    withTempDir("paste-per-module") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0, perModule = true))

      // Global on: a js dump names the versioned file per module — one
      // loader statement each, no combo token.
      val page = paste.page()
      page.jam(js = Seq("app"))
      val perModuleTags = page.dump(AssetType.Js)
      perModuleTags should include("/js/app.h.v1.min.js")
      perModuleTags should include("paste.require")
      perModuleTags should not include "app+v1.0"
      page.finish() shouldBe ""

      // A jam's perModule overrides the setting in either direction —
      // Some(false) renders the combo group the setting would suppress.
      val combo = paste.page()
      combo.jam(js = Seq("app"), perModule = Some(false))
      val comboTags = combo.dump(AssetType.Js)
      comboTags should include("app+v1.0")
      comboTags should not include "/js/app.h.v1.min.js"
      combo.finish() shouldBe ""

      // And Some(true) flips a combo-setting page to per-module.
      val comboSetting = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))
      val flipped = comboSetting.page()
      flipped.jam(js = Seq("app"), perModule = Some(true))
      flipped.dump(AssetType.Js) should include("/js/app.h.v1.min.js")
      flipped.finish() shouldBe ""

      // The wire form posts perModule only when it is set — Some(false)
      // still posts, so a jam can override an enabled setting off.
      val posted = Paste.encodeEvents(Vector(
        Paste.JamEvent(Jam(js = Seq("app"), perModule = Some(true)))
      ))
      posted should include("\"perModule\":true")
      Paste.encodeEvents(Vector(
        Paste.JamEvent(Jam(js = Seq("app"), perModule = Some(false)))
      )) should include("\"perModule\":false")
      Paste.encodeEvents(Vector(
        Paste.JamEvent(Jam(js = Seq("app")))
      )) should not include "perModule"

      // The codec round-trips a full jam for every perModule state.
      import spray.json.*
      for (flag <- Seq(None, Some(false), Some(true))) {
        val jam = Jam(css = Seq("site.theme"), js = Seq("app"), inline = true,
          sync = true, media = Some("print"), perModule = flag)
        jam.toJson(Jam.format).convertTo[Jam](Jam.format) shouldBe jam
      }
    }

  it should "reject null jam inputs before the event is recorded" in
    withTempDir("paste-jam-nulls") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))
      val page = paste.page()

      // Java interop hands nulls past the signature — each must reject as
      // a PasteException naming the argument, before an event exists.
      for ((bad, arg) <- Seq(
          (() => page.jam(js = null), "jam js"),
          (() => page.jam(css = null), "jam css"),
          (() => page.jam(js = Seq(null.asInstanceOf[String])), "jam js"),
          (() => page.jam(css = Seq(null.asInstanceOf[String])), "jam css"),
          (() => page.jam(media = null), "jam media"),
          (() => page.jam(media = Some(null.asInstanceOf[String])), "jam media"),
          (() => page.jam(condition = null), "jam condition"),
          (() => page.jam(condition = Some(null.asInstanceOf[String])), "jam condition"),
          (() => page.jam(perModule = null), "jam perModule"),
          // A `Some[Boolean]` holding null is only constructible through
          // Java's erased `Some(Object)` — `null.asInstanceOf[Boolean]`
          // would produce a legitimate `Some(false)`.
          (() => page.jam(perModule =
            Some(null.asInstanceOf[AnyRef]).asInstanceOf[Option[Boolean]]),
           "jam perModule")
      )) {
        val ex = the[PasteException] thrownBy bad()
        ex.getMessage should include(arg)
      }

      // The rejected calls recorded nothing: a valid jam — with a real
      // Option value — still renders and finishes on the same page.
      page.jam(js = Seq("app"), perModule = Some(true)) shouldBe ""
      page.dump(AssetType.Js) should include("/js/app.h.v1.min.js")
      page.finish() shouldBe ""
    }

  it should "wrap conditional tags and not mark conditional js loaded" in
    withTempDir("paste-conditional") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))
      val page = paste.page()

      page.jam(js = Seq("app"), condition = Some("lt IE 9"))
      val first = page.dump(AssetType.Js)
      first should include("<!--[if lt IE 9]>\n")
      first should include("\n<![endif]-->")
      // A conditional group cannot carry the unconditional bootstrap.
      first should not include "paste.config"

      // js stays unloaded — the unconditional jam serves it again, with
      // the bootstrap this time.
      page.jam(js = Seq("app"))
      val second = page.dump(AssetType.Js)
      second should include("paste.config")
      second should include("app+v1.0")
      second should not include "<!--[if"
      page.finish() shouldBe ""
    }

  it should "still mark a conditional stylesheet loaded" in
    withTempDir("paste-conditional-css") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))
      val page = paste.page()

      page.jam(css = Seq("site.theme"), condition = Some("lt IE 9"))
      val first = page.dump(AssetType.Css)
      first should include("<!--[if lt IE 9]>")
      first should include("<link rel=\"stylesheet\"")

      // The stylesheet counted as delivered — nothing pending on re-jam.
      page.jam(css = Seq("site.theme"))
      page.dump(AssetType.Css) shouldBe ""
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

  it should "answer 304 or 200 from the request's validator by the recorded stamp" in
    withTempDir("paste-combo-conditional") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out))
      val url = "/paste/1767225600/app+v1.0.min.js"

      // At or after the group stamp → 304, empty body, stamped header.
      for (ims <- Seq("Thu, 01 Jan 2026 00:00:00 GMT", "Thu, 01 Jan 2026 00:00:01 GMT")) {
        val r = paste.combo(url, Some(ims))
        withClue(s"validator $ims: ") {
          r.status shouldBe 304
          r.body shouldBe ""
          r.lastModified shouldBe Some("Thu, 01 Jan 2026 00:00:00 GMT")
          r.cacheControl shouldBe "public, max-age=31536000"
        }
      }

      // Before it, absent, or unparsable → the ordinary 200.
      for (ims <- Seq(Some("Wed, 01 Jan 2025 00:00:00 GMT"), Some("not a date"), None)) {
        val r = paste.combo(url, ims)
        withClue(s"validator $ims: ") {
          r.status shouldBe 200
          r.body shouldBe "APP"
          r.lastModified shouldBe Some("Thu, 01 Jan 2026 00:00:00 GMT")
        }
      }

      // A non-canonical URL still redirects first — the validator never
      // suppresses canonicalization.
      val r = paste.combo("/paste/1767225599/app+v1.0.min.js", Some("Thu, 01 Jan 2026 00:00:01 GMT"))
      r.status shouldBe 302
      r.location shouldBe Some(url)

      // A NUL in the validator is rejected at the adapter.
      the[PasteException] thrownBy paste.combo(url, Some("Thu\u0000"))
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

  it should "reject a null combo path as a PasteException, not an NPE" in
    withTempDir("paste-combo-null") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out))
      // A null dereference at indexOf would be an NPE; the contract is a
      // PasteException naming the argument before any native call.
      val ex = the[PasteException] thrownBy paste.combo(null)
      ex.getMessage shouldBe "combo path must not be null"
    }

  it should "reject a negative inlineThreshold at settings construction" in
    withTempDir("paste-negative-threshold") { root =>
      val (manifest, out) = e2eFixture(root)
      // The type cannot constrain the range; construction must — a
      // negative threshold used to surface mid-render at the native
      // deserializer instead.
      val ex = intercept[IllegalArgumentException] {
        Paste.Settings(manifest, out, inlineThreshold = -1)
      }
      ex.getMessage should include("inlineThreshold")
    }

  it should "reject null and unusable settings references at construction" in
    withTempDir("paste-null-settings") { root =>
      val (manifest, out) = e2eFixture(root)
      val good = out.toAbsolutePath
      // Java interop can hand nulls past the signature — every reference
      // field is named at the constructor, before settingsJson's first
      // toString can NPE. Some(null) is inspected for the same reason;
      // and Paths.get("") or "   " spells an unusable path that must not
      // reach the serializer. (A NUL cannot be constructed via Paths.get
      // on this platform — the spelling check stays as defense in depth.)
      for ((make, name) <- Seq[(() => Paste.Settings, String)](
        (() => Paste.Settings(null, good), "manifest"),
        (() => Paste.Settings(good, null), "outputBase"),
        (() => Paste.Settings(good, good, outputJs = null), "outputJs"),
        (() => Paste.Settings(good, good, outputCss = null), "outputCss"),
        (() => Paste.Settings(good, good, outputJs = Some(null)), "outputJs"),
        (() => Paste.Settings(good, good, outputCss = Some(null)), "outputCss"),
        (() => Paste.Settings(good, good, cdnBase = null), "cdnBase"),
        (() => Paste.Settings(good, good, basePath = null), "basePath"),
        (() => Paste.Settings(Paths.get(""), good), "manifest"),
        (() => Paste.Settings(Paths.get("   "), good), "manifest"),
        (() => Paste.Settings(good, Paths.get("   ")), "outputBase"),
        (() => Paste.Settings(good, good, outputJs = Some(Paths.get("   "))), "outputJs"),
        (() => Paste.Settings(good, good, outputCss = Some(Paths.get("   "))), "outputCss"),
      )) {
        val ex = intercept[IllegalArgumentException](make())
        ex.getMessage should include(name)
      }
      // And valid settings still construct and render.
      val page = Paste(Paste.Settings(manifest, out, inlineThreshold = 0)).page()
      page.jam(js = Seq("app"))
      page.dump(AssetType.Js) should include("app+v1.0")
      page.finish() shouldBe ""
    }

  it should "reject null settings at the public Paste boundary" in
    withTempDir("paste-null-apply") { root =>
      val (manifest, out) = e2eFixture(root)
      // Java interop can hand null past the signature; without a guard it
      // reaches the eager settingsJson dereference as a bare NPE — the
      // contract names the argument instead, before any native call.
      val ex = intercept[IllegalArgumentException](Paste(null))
      ex.getMessage should include("settings")
      // A valid reference still constructs and serves a page.
      Paste(Paste.Settings(manifest, out)).page().finish() shouldBe ""
    }

  it should "reject a null dump asset type without recording an event" in
    withTempDir("paste-null-dump") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out, inlineThreshold = 0))
      val page = paste.page()
      val ex = the[PasteException] thrownBy page.dump(null.asInstanceOf[AssetType])
      ex.getMessage should include("assetType")
      // The rejected call changed nothing — the page still dumps and seals.
      page.jam(js = Seq("app"))
      page.dump(AssetType.Js) should include("app+v1.0")
      page.finish() shouldBe ""
    }

  it should "raise PasteException naming the module for an unknown module" in
    withTempDir("paste-unknown") { root =>
      val (manifest, out) = e2eFixture(root)
      val paste = Paste(Paste.Settings(manifest, out))
      val eventsJson = Paste.encodeEvents(Vector(
        Paste.JamEvent(Jam(js = Seq("ghost"))),
        Paste.DumpEvent(Some(AssetType.Js))
      ))
      val ex = the[PasteException] thrownBy paste.renderPage(eventsJson, "")
      ex.getMessage should include("ghost")
    }

  it should "copy a classpath manifest and refuse a missing resource" in
    withTempDir("paste-from-classpath") { root =>
      val loader: String => Option[java.io.InputStream] = name =>
        Option.when(name == "/paste-assetgraph-manifest.json")(getClass.getResourceAsStream(name))
      val paste = Paste.fromClasspath(
        loader, "/paste-assetgraph-manifest.json", root, "", "/paste", 1024, false, root
      )
      Files.exists(paste.settings.manifest) shouldBe true
      new String(Files.readAllBytes(paste.settings.manifest), UTF_8) should include("spec")
      an[IllegalArgumentException] shouldBe thrownBy(
        Paste.fromClasspath(
          loader, "/no-such-manifest.json", root, "", "/paste", 1024, false, root
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
          loader, "/paste-assetgraph-manifest.json", root, "", "/paste", 1024, false, root
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

  it should "reject malformed page payloads instead of defaulting fields" in {
    import spray.json.*
    // Every shape short of {html: string, generation: non-empty string}
    // is a malformed response — a defaulted empty generation would read
    // as the "accept the current manifest" sentinel and silently unpin
    // the page.
    val bad = Seq(
      """{"ok":{}}""".parseJson,
      """{"html":"<p>x</p>"}""".parseJson,
      """{"generation":"gen-1"}""".parseJson,
      """{"html":"<p>x</p>","generation":""}""".parseJson,
      """{"html":42,"generation":"gen-1"}""".parseJson,
      """{"html":"<p>x</p>","generation":7}""".parseJson,
      JsString("not an object")
    )
    for (payload <- bad) {
      val ex = the[PasteException] thrownBy PasteLibrary.decodePage(payload)
      ex.getMessage should include("malformed page response")
    }
    PasteLibrary.decodePage(
      """{"html":"<p>x</p>","generation":"gen-1"}""".parseJson
    ) shouldBe ("<p>x</p>", "gen-1")
  }

  it should "match the shared CLI-built distribution" in {
    import spray.json.*
    import spray.json.DefaultJsonProtocol.*
    import scala.jdk.CollectionConverters.*
    import scala.util.Using

    def digest(bytes: Array[Byte]): String =
      MessageDigest.getInstance("SHA-256").digest(bytes).map(b => f"${b & 0xff}%02x").mkString
    def textDigest(value: String): String = digest(value.getBytes(UTF_8))
    def tree(root: Path): Map[String, String] = Using.resource(Files.walk(root)) { paths =>
      paths.iterator().asScala
        .filter(p => Files.isRegularFile(p))
        .map(p => root.relativize(p).toString.replace('\\', '/') -> digest(Files.readAllBytes(p)))
        .toMap
    }
    def objects(value: JsValue): Vector[JsObject] = value match {
      case JsArray(elements) => elements.map(_.asJsObject)
      case other             => fail(s"expected a JSON array, got $other")
    }

    // The shared CLI-built fixture every runner checks: one built
    // distribution whose frozen reference.json holds the expected response
    // and page-output digests — adapter consistency, not legacy parity.
    val root = java.nio.file.Paths.get(sys.env.getOrElse(
      "PASTE_ASSETGRAPH_TEST_FIXTURE",
      fail("Build tests/build_shared_fixture.py once and set PASTE_ASSETGRAPH_TEST_FIXTURE")
    )).toRealPath()
    // Fixture metadata paths must resolve beneath the selected root —
    // absolute members, "..", and symlink escapes are rejected. The
    // operator-chosen root itself is the intended input.
    def fixturePath(relative: String): Path = {
      require(!java.nio.file.Paths.get(relative).isAbsolute, "fixture paths must be relative")
      val candidate = root.resolve(relative).toRealPath()
      require(candidate.startsWith(root), "fixture path escapes its root")
      candidate
    }
    intercept[IllegalArgumentException](fixturePath(".."))
    val reference = new String(Files.readAllBytes(fixturePath("reference.json")), UTF_8).parseJson.asJsObject.fields
    reference("schema") shouldBe JsNumber(1)
    objects(reference("cases")).foreach { itemObject =>
      val item = itemObject.fields
      val output = fixturePath(item("outputBase").convertTo[String])
      val manifestSource = fixturePath(item("manifest").convertTo[String])
      val expectedFiles = item("files").convertTo[Map[String, String]]
      tree(output) shouldBe expectedFiles
      val paste = Paste(Paste.Settings(manifestSource, output, inlineThreshold = 0))
      objects(item("responses")).foreach { request =>
        val response = paste.combo(request.fields("path").convertTo[String])
        val fields = response.toJson(ComboResponse.format).asJsObject.fields
        // spray-json omits an absent Option on write; the reference JSON
        // spells it `location: null`.
        JsObject(
          (fields - "body") +
            ("bodySha256" -> JsString(textDigest(response.body))) +
            ("location" -> fields.getOrElse("location", JsNull)) +
            ("lastModified" -> fields.getOrElse("lastModified", JsNull))
        ) shouldBe request.fields("expected")
      }
      objects(item("pages")).foreach { scenario =>
        withTempDir("paste-shared") { temp =>
          val manifest = temp.resolve("manifest.json")
          val held = temp.resolve("held.json")
          // The scenario's recorded manifest default, applied exactly as
          // the generator wrote it — only when it names one.
          val manifestJson = new String(Files.readAllBytes(manifestSource), UTF_8).parseJson.asJsObject
          scenario.fields.get("manifestCdnBase") match {
            case Some(JsString(cdnBase)) =>
              Files.write(manifest, JsObject(manifestJson.fields + ("cdn_base" -> JsString(cdnBase))).compactPrint.getBytes(UTF_8))
            case _ => Files.copy(manifestSource, manifest)
          }
          // Only the known render settings are mapped through — no
          // arbitrary scenario keys reach the settings record.
          val settings = scenario.fields.get("settings").map(_.asJsObject.fields).getOrElse(Map.empty)
          val page = Paste(Paste.Settings(
            manifest, output,
            cdnBase = settings.get("cdnBase").map(_.convertTo[String]).getOrElse(""),
            inlineThreshold = settings.get("inlineThreshold").map(_.convertTo[Int]).getOrElse(0),
            perModule = settings.get("perModule").exists(_.convertTo[Boolean])
          )).page()
          objects(scenario.fields("steps")).foreach { stepObject =>
            val step = stepObject.fields
            if (step.contains("hideManifest")) Files.move(manifest, held)
            else if (step.contains("restoreManifest")) Files.move(held, manifest)
            else {
              def operation(): String = step.get("jam") match {
                case Some(value) =>
                  val jam = value.asJsObject.fields
                  page.jam(
                    css = jam.get("css").map(_.convertTo[Vector[String]]).getOrElse(Vector.empty),
                    js = jam.get("js").map(_.convertTo[Vector[String]]).getOrElse(Vector.empty),
                    inline = jam.get("inline").exists(_.convertTo[Boolean]),
                    sync = jam.get("sync").exists(_.convertTo[Boolean]),
                    media = jam.get("media").map(_.convertTo[String]),
                    condition = jam.get("condition").map(_.convertTo[String]),
                    // Forwarded as an Option so an explicit false stays a
                    // real override rather than reading as absent.
                    perModule = jam.get("perModule").map(_.convertTo[Boolean])
                  )
                case None if step.contains("finish") => page.finish()
                case None => step("dump").convertTo[String] match {
                  case "all" => page.dump()
                  case "css" => page.dump(AssetType.Css)
                  case "js"  => page.dump(AssetType.Js)
                  case other => fail(s"Unknown fixture dump: $other")
                }
              }
              if (step.get("error").contains(JsBoolean(true)) || step.contains("errorContains")) {
                val error = intercept[PasteException](operation())
                step.get("errorContains").foreach(v => error.getMessage should include(v.convertTo[String]))
              } else
                textDigest(operation()) shouldBe step("valueSha256").convertTo[String]
            }
          }
        }
      }
      tree(output) shouldBe expectedFiles
    }
  }
}
