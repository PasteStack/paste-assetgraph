package com.pastestack.assetgraph

import org.scalatest.flatspec.AnyFlatSpec
import org.scalatest.matchers.should.Matchers
import spray.json._

class ManifestSpec extends AnyFlatSpec with Matchers {

  // ============================================================
  // JAM Spec BDD: Timestamp Calculation
  // "The timestamp MUST be the maximum timestamp of all modules
  //  in the group" — JAM_SPEC.md §1
  // ============================================================

  // Helper: build a manifest that mirrors real paste-assetgraph output.
  // Modules map uses @module names → asset keys.
  // Assets map uses type/path keys → entries with RFC3339 timestamps.
  private def realWorldManifest = Manifest(
    version = "2026.02.07.1156",
    generated = "2026-02-07T11:56:00+00:00",
    modules = Map(
      "paste.dom"          -> "js/paste/dom.js",
      "paste.event"        -> "js/paste/event.js",
      "paste.ui.heroscroll"-> "js/paste/ui/heroscroll.js",
      "paste.ui.stickynav" -> "js/paste/ui/stickynav.js",
      "site.theme"  -> "css/theme.css"
    ),
    assets = Map(
      "js/paste/dom.js" -> AssetEntry(
        source = "src/paste/dom.js",
        output = "/js/paste/dom.aaa.v1.min.js",
        hash = "aaa", version = 1,
        timestamp = "2026-02-06T10:00:00+00:00",  // earliest
        dependencies = Seq.empty, history = Seq.empty
      ),
      "js/paste/event.js" -> AssetEntry(
        source = "src/paste/event.js",
        output = "/js/paste/event.bbb.v2.min.js",
        hash = "bbb", version = 2,
        timestamp = "2026-02-07T11:00:00+00:00",  // middle
        dependencies = Seq("paste.dom"), history = Seq.empty
      ),
      "js/paste/ui/heroscroll.js" -> AssetEntry(
        source = "src/paste/ui/heroscroll.js",
        output = "/js/paste/ui/heroscroll.ccc.v1.min.js",
        hash = "ccc", version = 1,
        timestamp = "2026-02-07T11:56:48+00:00",  // latest JS
        dependencies = Seq("paste.dom", "paste.event"), history = Seq.empty
      ),
      "js/paste/ui/stickynav.js" -> AssetEntry(
        source = "src/paste/ui/stickynav.js",
        output = "/js/paste/ui/stickynav.ddd.v1.min.js",
        hash = "ddd", version = 1,
        timestamp = "2026-02-07T11:56:48+00:00",
        dependencies = Seq("paste.dom", "paste.event"), history = Seq.empty
      ),
      "css/theme.css" -> AssetEntry(
        source = "src/theme.scss",
        output = "/css/theme.eee.v1.min.css",
        hash = "eee", version = 1,
        timestamp = "2026-02-07T11:56:49+00:00",  // latest overall
        dependencies = Seq.empty, history = Seq.empty
      )
    ),
    sortedDeps = Map.empty
  )

  // BDD Scenario: Generate combo URL for single module
  "JAM timestamp (BDD)" should "use module timestamp for single module" in {
    val m = realWorldManifest
    val url = m.jamJsUrl(Seq("paste.dom"), debug = false)

    url should startWith("/paste/")
    url should endWith(".min.js")
    url should include("paste.dom+v1.0")
    // 2026-02-06T10:00:00+00:00 = 1770372000
    url should include("1770372000")
  }

  // BDD Scenario: Generate combo URL for multiple modules — timestamp is max
  it should "use max timestamp across multiple modules" in {
    val m = realWorldManifest
    val url = m.jamJsUrl(Seq("paste.dom", "paste.event"), debug = false)

    url should include("paste.dom+v1.0")
    url should include("paste.event+v2.0")
    // max(2026-02-06T10:00:00, 2026-02-07T11:00:00) = 1770462000
    url should include("1770462000")
    url should not include "2026.02.07.1156"  // should NOT fall back to version
  }

  // BDD Scenario: Generate debug URL
  it should "use .js suffix in debug mode" in {
    val m = realWorldManifest
    val url = m.jamJsUrl(Seq("paste.dom"), debug = true)
    url should endWith(".js")
    url should not endWith ".min.js"
  }

  // Real-world: CSS URL where modules map points to JS paths (cross-type)
  it should "resolve cross-type modules via modules map for timestamp" in {
    val m = realWorldManifest
    // CSS URL for heroscroll, stickynav (JS modules), theme (CSS module)
    val url = m.jamCssUrl(Seq("paste.ui.heroscroll", "paste.ui.stickynav", "site.theme"))

    // All modules should be in URL
    url should include("paste.ui.heroscroll+v1.0")
    url should include("paste.ui.stickynav+v1.0")
    url should include("site.theme+v1.0")

    // Timestamp = max of ALL resolved entries (including cross-type JS lookups)
    // heroscroll JS: 2026-02-07T11:56:48 = 1770465408
    // stickynav JS:  2026-02-07T11:56:48 = 1770465408
    // theme CSS: 2026-02-07T11:56:49 = 1770465409
    // max = 1770465409
    url should include("1770465409")

    // Tilde separator
    url should include("~")
    url should not include ","
    url should endWith(".min.css")
  }

  // Modules not in manifest get v1.0 but don't break timestamp from known modules
  it should "include unknown modules without breaking timestamp from known ones" in {
    val m = realWorldManifest
    val url = m.jamJsUrl(Seq("paste.dom", "nonexistent.module"))

    url should include("paste.dom+v1.0")
    url should include("nonexistent.module+v1.0")
    // Timestamp should come from paste.dom (the known module)
    url should include("1770372000")
  }

  // All modules unknown — fall back to manifest version
  it should "fall back to manifest version when no modules have timestamps" in {
    val m = realWorldManifest
    val url = m.jamJsUrl(Seq("unknown.a", "unknown.b"))

    url should include("unknown.a+v1.0")
    url should include("unknown.b+v1.0")
    url should include("2026.02.07.1156")
  }

  // Empty timestamps in assets — fall back to manifest version
  it should "fall back to manifest version when timestamps are empty strings" in {
    val manifest = Manifest(
      version = "2024.01.15.1030",
      generated = "2024-01-15T10:30:00+00:00",
      modules = Map.empty,
      assets = Map(
        "js/paste/dom.js" -> AssetEntry(
          source = "src/paste/dom.js",
          output = "/js/paste/dom.abc123.v1.min.js",
          hash = "abc123", version = 1,
          timestamp = "",  // Empty timestamp
          dependencies = Seq.empty, history = Seq.empty
        )
      ),
      sortedDeps = Map.empty
    )

    val url = manifest.jamJsUrl(Seq("paste.dom"), debug = false)
    url should include("2024.01.15.1030")
  }

  // ============================================================
  // Manifest modules and sortedDeps serialization
  // Verify that new fields are correctly serialized/deserialized
  // ============================================================

  "Manifest JSON format" should "correctly parse modules field" in {
    val json = """
      {
        "version": "2024.01.15.1030",
        "generated": "2024-01-15T10:30:00+00:00",
        "modules": {
          "paste.dom": "js/paste/dom.js",
          "paste.event": "js/paste/event.js"
        },
        "assets": {},
        "sorted_deps": {}
      }
    """

    val manifest = json.parseJson.convertTo[Manifest]

    manifest.modules should have size 2
    manifest.modules("paste.dom") shouldBe "js/paste/dom.js"
    manifest.modules("paste.event") shouldBe "js/paste/event.js"
  }

  it should "correctly parse sorted_deps field" in {
    val json = """
      {
        "version": "2024.01.15.1030",
        "generated": "2024-01-15T10:30:00+00:00",
        "modules": {},
        "assets": {},
        "sorted_deps": {
          "js": [
            ["paste.dom", "/js/paste/dom.abc123.v1.min.js", 1],
            ["paste.event", "/js/paste/event.def456.v2.min.js", 2]
          ],
          "css": []
        }
      }
    """

    val manifest = json.parseJson.convertTo[Manifest]

    manifest.sortedDeps should have size 2
    manifest.sortedDeps("js") should have size 2
    manifest.sortedDeps("js").head shouldBe ("paste.dom", "/js/paste/dom.abc123.v1.min.js", 1)
    manifest.sortedDeps("css") shouldBe empty
  }

  it should "handle empty optional fields" in {
    val json = """
      {
        "version": "2024.01.15.1030",
        "generated": "2024-01-15T10:30:00+00:00",
        "modules": {},
        "assets": {},
        "sorted_deps": {}
      }
    """

    val manifest = json.parseJson.convertTo[Manifest]

    manifest.modules shouldBe empty
    manifest.sortedDeps shouldBe empty
    manifest.assets shouldBe empty
  }

  it should "handle missing modules and sorted_deps fields (backward compat)" in {
    // Older manifests generated before the modules/sorted_deps fields were added
    // should deserialize without error, defaulting to empty maps.
    val json = """
      {
        "version": "2024.01.15.1030",
        "generated": "2024-01-15T10:30:00+00:00",
        "assets": {
          "js/paste/dom.js": {
            "source": "src/paste/dom.js",
            "output": "/js/paste/dom.abc123.v1.min.js",
            "hash": "abc123",
            "version": 1,
            "timestamp": "2024-01-15T10:30:00+00:00",
            "dependencies": [],
            "history": []
          }
        }
      }
    """

    val manifest = json.parseJson.convertTo[Manifest]

    manifest.modules shouldBe empty
    manifest.sortedDeps shouldBe empty
    manifest.assets should have size 1
  }

  it should "round-trip serialize/deserialize correctly" in {
    val original = Manifest(
      version = "2024.01.15.1030",
      generated = "2024-01-15T10:30:00+00:00",
      modules = Map("paste.dom" -> "js/paste/dom.js"),
      assets = Map(
        "js/paste/dom.js" -> AssetEntry(
          source = "src/paste/dom.js",
          output = "/js/paste/dom.abc123.v1.min.js",
          hash = "abc123",
          version = 1,
          timestamp = "2024-01-15T10:30:00+00:00",
          dependencies = Seq.empty,
          history = Seq.empty
        )
      ),
      sortedDeps = Map("js" -> Seq(("paste.dom", "/js/paste/dom.abc123.v1.min.js", 1)))
    )

    val json = original.toJson.prettyPrint
    val parsed = json.parseJson.convertTo[Manifest]

    parsed.version shouldBe original.version
    parsed.generated shouldBe original.generated
    parsed.modules shouldBe original.modules
    parsed.assets.keys shouldBe original.assets.keys
    parsed.sortedDeps shouldBe original.sortedDeps
  }

  // ============================================================
  // getVersion tests
  // ============================================================

  "Manifest.getVersion" should "return current version when no version specified" in {
    val manifest = Manifest(
      version = "2024.01.15.1030",
      generated = "2024-01-15T10:30:00+00:00",
      modules = Map.empty,
      assets = Map(
        "js/paste/dom.js" -> AssetEntry(
          source = "src/paste/dom.js",
          output = "/js/paste/dom.abc123.v2.min.js",
          hash = "abc123",
          version = 2,
          timestamp = "2024-01-15T10:30:00+00:00",
          dependencies = Seq.empty,
          history = Seq(
            AssetVersion("old123", 1, "/js/paste/dom.old123.v1.min.js", "2024-01-14T10:00:00+00:00")
          )
        )
      ),
      sortedDeps = Map.empty
    )

    manifest.getVersion("js/paste/dom.js", None) shouldBe Some("/js/paste/dom.abc123.v2.min.js")
  }

  it should "return specific version from history" in {
    val manifest = Manifest(
      version = "2024.01.15.1030",
      generated = "2024-01-15T10:30:00+00:00",
      modules = Map.empty,
      assets = Map(
        "js/paste/dom.js" -> AssetEntry(
          source = "src/paste/dom.js",
          output = "/js/paste/dom.abc123.v2.min.js",
          hash = "abc123",
          version = 2,
          timestamp = "2024-01-15T10:30:00+00:00",
          dependencies = Seq.empty,
          history = Seq(
            AssetVersion("old123", 1, "/js/paste/dom.old123.v1.min.js", "2024-01-14T10:00:00+00:00")
          )
        )
      ),
      sortedDeps = Map.empty
    )

    manifest.getVersion("js/paste/dom.js", Some(1)) shouldBe Some("/js/paste/dom.old123.v1.min.js")
    manifest.getVersion("js/paste/dom.js", Some(2)) shouldBe Some("/js/paste/dom.abc123.v2.min.js")
    manifest.getVersion("js/paste/dom.js", Some(999)) shouldBe None
  }

  // ============================================================
  // getDependencies tests
  // ============================================================

  "Manifest.getDependencies" should "return transitive dependencies in order" in {
    val manifest = Manifest(
      version = "2024.01.15.1030",
      generated = "2024-01-15T10:30:00+00:00",
      modules = Map.empty,
      assets = Map(
        "js/paste/dom.js" -> AssetEntry(
          source = "src/paste/dom.js",
          output = "/js/paste/dom.abc123.v1.min.js",
          hash = "abc123",
          version = 1,
          timestamp = "2024-01-15T10:30:00+00:00",
          dependencies = Seq.empty,
          history = Seq.empty
        ),
        "js/paste/event.js" -> AssetEntry(
          source = "src/paste/event.js",
          output = "/js/paste/event.def456.v1.min.js",
          hash = "def456",
          version = 1,
          timestamp = "2024-01-15T10:30:00+00:00",
          dependencies = Seq("paste.dom"),
          history = Seq.empty
        ),
        "js/paste/ui/heroscroll.js" -> AssetEntry(
          source = "src/paste/ui/heroscroll.js",
          output = "/js/paste/ui/heroscroll.ghi789.v1.min.js",
          hash = "ghi789",
          version = 1,
          timestamp = "2024-01-15T10:30:00+00:00",
          dependencies = Seq("paste.dom", "paste.event"),
          history = Seq.empty
        )
      ),
      sortedDeps = Map.empty
    )

    val deps = manifest.getDependencies("paste.ui.heroscroll")

    // Should include all transitive deps, with heroscroll last
    deps should contain theSameElementsInOrderAs Seq("paste.dom", "paste.event", "paste.ui.heroscroll")
  }

  // ============================================================
  // readFile properly closes file handles
  // ============================================================

  "Manifest.readFile" should "read file content and close handles" in {
    val tmpFile = java.io.File.createTempFile("paste-test-", ".js")
    tmpFile.deleteOnExit()
    val writer = new java.io.PrintWriter(tmpFile)
    writer.print("var x = 1;")
    writer.close()

    val manifest = Manifest("v1", "2024-01-15T10:00:00+00:00", Map.empty,
      Map.empty, Map.empty)
    val content = manifest.readFile(tmpFile.getAbsolutePath)
    content shouldBe Some("var x = 1;")
  }

  // ============================================================
  // loadFromResource throws for missing resource
  // ============================================================

  "Manifest.loadFromResource" should "throw for missing resource" in {
    an [IllegalArgumentException] should be thrownBy {
      Manifest.loadFromResource("/nonexistent-manifest.json")
    }
  }

  // ============================================================
  // PasteComboHandler cache invalidation
  // ============================================================

  "PasteComboHandler" should "support clearCaches without error" in {
    val handler = PasteComboHandler()
    noException should be thrownBy handler.clearCaches()
  }

  it should "return not-found comment for unknown modules" in {
    val handler = PasteComboHandler()
    val content = handler.loadModules(Seq("nonexistent.module"), "js")
    content should include("not found")
  }

  it should "report zero found count for unknown modules" in {
    val handler = PasteComboHandler()
    val (content, foundCount) = handler.loadModulesWithCount(Seq("nonexistent.module"), "js")
    content should include("not found")
    foundCount shouldBe 0
  }

  it should "report positive found count for modules that exist on disk" in {
    // Create a temp JS file to simulate a real module
    val tmpDir = java.nio.file.Files.createTempDirectory("paste-combo-test")
    val jsFile = tmpDir.resolve("testmod.js")
    java.nio.file.Files.write(jsFile, "/* only a comment */".getBytes)

    val handler = new PasteComboHandler(
      pastePath = tmpDir.toString,
      pasteElementsPath = tmpDir.toString,
      siteJsPath = tmpDir.toString,
      siteCssPath = tmpDir.toString,
      sitePrefix = "test"
    )
    val (content, foundCount) = handler.loadModulesWithCount(Seq("test.testmod"), "js")
    // Module exists and has content (even though it's only a comment), so foundCount > 0
    foundCount shouldBe >(0)
    content should include("/* only a comment */")

    // Cleanup
    java.nio.file.Files.delete(jsFile)
    java.nio.file.Files.delete(tmpDir)
  }

  // ============================================================
  // expandDependencies CSS asset type support
  // ============================================================

  "Manifest.expandDependencies" should "work for CSS asset type" in {
    val manifest = Manifest(
      version = "1.0",
      generated = "2024-01-15T10:00:00+00:00",
      modules = Map.empty,
      assets = Map(
        "css/base.css" -> AssetEntry(
          source = "src/base.scss",
          output = "/css/base.v1.min.css",
          hash = "h1",
          version = 1,
          timestamp = "2024-01-15T10:00:00+00:00",
          dependencies = Seq.empty,
          history = Seq.empty
        ),
        "css/theme.css" -> AssetEntry(
          source = "src/theme.scss",
          output = "/css/theme.v1.min.css",
          hash = "h2",
          version = 1,
          timestamp = "2024-01-15T10:00:00+00:00",
          dependencies = Seq("base"),
          history = Seq.empty
        )
      ),
      sortedDeps = Map.empty
    )

    val deps = manifest.expandDependencies(Seq("theme"), "css")
    deps should contain inOrderOnly ("base", "theme")
  }

  it should "include non-manifest modules without expansion (Bug #8 consistency fix)" in {
    // expandDependencies should include unknown modules (without expanding deps),
    // matching jamUrlForType behavior which also includes them with default v1.0.
    val manifest = Manifest(
      version = "1.0",
      generated = "2024-01-15T10:00:00+00:00",
      modules = Map.empty,
      assets = Map(
        "js/paste/dom.js" -> AssetEntry(
          source = "src/paste/dom.js",
          output = "/js/paste/dom.v1.min.js",
          hash = "h1",
          version = 1,
          timestamp = "2024-01-15T10:00:00+00:00",
          dependencies = Seq.empty,
          history = Seq.empty
        )
      ),
      sortedDeps = Map.empty
    )

    val deps = manifest.expandDependencies(Seq("paste.dom", "nonexistent.module"), "js")
    deps should contain ("paste.dom")
    deps should contain ("nonexistent.module")
  }

  // ============================================================
  // Cross-type resolution: CSS requests must not resolve to JS files
  // Regression test for bug where paste.ui.heroscroll (JS module)
  // was served as CSS because resolveModule returned JS path
  // ============================================================

  "PasteComboHandler" should "not serve JS content when CSS is requested for a JS-mapped module" in {
    // Create temp dirs with a JS file but a separate CSS file
    val tmpDir = java.nio.file.Files.createTempDirectory("paste-crosstype-test")
    val jsDir = tmpDir.resolve("js/paste/ui")
    java.nio.file.Files.createDirectories(jsDir)
    val jsFile = jsDir.resolve("heroscroll.js")
    java.nio.file.Files.write(jsFile, "paste['define']('paste.ui.heroscroll', function() {});".getBytes)

    // Create CSS in paste-elements convention path: modules/heroscroll/heroscroll.css
    val elemDir = tmpDir.resolve("modules/heroscroll")
    java.nio.file.Files.createDirectories(elemDir)
    val cssFile = elemDir.resolve("heroscroll.css")
    java.nio.file.Files.write(cssFile, ".heroscroll { background: red; }".getBytes)

    // Manifest maps paste.ui.heroscroll → JS file only
    val manifest = Manifest(
      version = "1.0",
      generated = "2024-01-15T10:00:00+00:00",
      modules = Map("paste.ui.heroscroll" -> "js/paste/ui/heroscroll.js"),
      assets = Map(
        "js/paste/ui/heroscroll.js" -> AssetEntry(
          source = "src/js/paste/ui/heroscroll.js",
          output = "/js/paste/ui/heroscroll.v1.min.js",
          hash = "h1", version = 1,
          timestamp = "2024-01-15T10:00:00+00:00",
          dependencies = Seq.empty, history = Seq.empty
        )
      ),
      sortedDeps = Map.empty
    )

    val handler = new PasteComboHandler(
      pastePath = tmpDir.toString,
      pasteElementsPath = tmpDir.toString,
      siteJsPath = tmpDir.toString,
      siteCssPath = tmpDir.toString,
      sitePrefix = "test",
      manifest = Some(manifest),
      outputBasePath = tmpDir.toString
    )

    val css = handler.loadModules(Seq("paste.ui.heroscroll"), "css")
    // Must NOT contain JS module wrapper
    css should not include "paste['define']"
    // Must contain actual CSS
    css should include(".heroscroll")

    // Cleanup
    java.nio.file.Files.walk(tmpDir).sorted(java.util.Comparator.reverseOrder())
      .forEach(java.nio.file.Files.delete(_))
  }

  // ============================================================
  // Combo URL separator: tilde (~) instead of comma
  // Pekko HTTP rejects commas in URI path segments
  // ============================================================

  "PasteComboHandler.parseModulesUrl" should "parse tilde-separated modules" in {
    val handler = PasteComboHandler()
    val (modules, ext, isMinified) = handler.parseModulesUrl(
      "paste.dom+v1.0~paste.event+v2.0.min.js"
    )
    modules shouldBe Seq("paste.dom", "paste.event")
    ext shouldBe "js"
    isMinified shouldBe true
  }

  it should "parse comma-separated modules for backwards compatibility" in {
    val handler = PasteComboHandler()
    val (modules, ext, isMinified) = handler.parseModulesUrl(
      "paste.dom+v1.0,paste.event+v2.0.min.css"
    )
    modules shouldBe Seq("paste.dom", "paste.event")
    ext shouldBe "css"
    isMinified shouldBe true
  }

  it should "parse single module without separator" in {
    val handler = PasteComboHandler()
    val (modules, ext, isMinified) = handler.parseModulesUrl(
      "site.theme+v1.0.min.css"
    )
    modules shouldBe Seq("site.theme")
    ext shouldBe "css"
    isMinified shouldBe true
  }

  // ============================================================
  // Null byte sanitization in version field
  // ============================================================

  "Manifest version sanitization" should "handle null bytes in version field" in {
    val manifest = Manifest(
      version = "\u0000\u0000\u00002024.01.15",
      generated = "2024-01-15T10:30:00+00:00",
      modules = Map.empty,
      assets = Map.empty,
      sortedDeps = Map.empty
    )

    val url = manifest.jamJsUrl(Seq("nonexistent.module"))

    // Should strip null bytes and use cleaned version
    url should include("2024.01.15")
    url should not include "\u0000"
  }

  it should "default to JS asset type" in {
    val manifest = Manifest(
      version = "1.0",
      generated = "2024-01-15T10:00:00+00:00",
      modules = Map.empty,
      assets = Map(
        "js/paste/dom.js" -> AssetEntry(
          source = "src/paste/dom.js",
          output = "/js/paste/dom.v1.min.js",
          hash = "h1",
          version = 1,
          timestamp = "2024-01-15T10:00:00+00:00",
          dependencies = Seq.empty,
          history = Seq.empty
        )
      ),
      sortedDeps = Map.empty
    )

    // No assetType arg — should default to "js"
    val deps = manifest.expandDependencies(Seq("paste.dom"))
    deps should contain ("paste.dom")
  }
}
