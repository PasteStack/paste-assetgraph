package com.pastestack.assetgraph

import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.{Files, Path}
import org.scalatest.flatspec.AnyFlatSpec
import org.scalatest.matchers.should.Matchers

/**
 * Serving behaviour of the combo handler against a manifest that mirrors real
 * paste-assetgraph output: `modules` names asset keys, and each asset entry
 * carries its source path and its content-hashed output path.
 */
final class PasteComboHandlerSpec extends AnyFlatSpec with Matchers {

  private def write(path: Path, content: String): Path = {
    Files.createDirectories(path.getParent)
    Files.write(path, content.getBytes(UTF_8))
  }

  private def entry(source: String, output: String, dependencies: Seq[String] = Seq.empty): AssetEntry =
    AssetEntry(
      source = source,
      output = output,
      hash = "hash",
      version = 1,
      timestamp = "2026-02-07T11:56:00+00:00",
      dependencies = dependencies,
      history = Seq.empty
    )

  private def handlerFor(root: Path, manifest: Manifest): PasteComboHandler =
    new PasteComboHandler(
      pastePath = root.resolve("paste/src/js").toString,
      pasteElementsPath = root.resolve("paste-elements").toString,
      siteJsPath = root.resolve("src/js").toString,
      siteCssPath = root.resolve("src/scss").toString,
      sitePrefix = "site",
      manifest = Some(manifest),
      outputBasePath = root.resolve("output").toString,
      webjarBasePath = root.resolve("webjars").toString
    )

  "PasteComboHandler" should "serve compiled output for a minified JS request" in {
    val root = Files.createTempDirectory("paste-combo-compiled")
    val source = write(root.resolve("src/js/home.js"), "var home = 'source';")
    write(root.resolve("output/js/site/home.hash.v1.min.js"), "var home='compiled';")
    val manifest = Manifest(
      version = "1", generated = "",
      modules = Map("site.home" -> "js/site/home.js"),
      assets = Map("js/site/home.js" -> entry(source.toString, "/js/site/home.hash.v1.min.js"))
    )

    val (content, found) = handlerFor(root, manifest).loadModulesWithCount(Seq("site.home"), "js", minified = true)

    found shouldBe 1
    content should include("compiled")
    content should not include "source"
  }

  it should "serve module source for a JS request without the minified marker" in {
    // The unversioned/debug URL exists so an edited file is visible without a
    // rebuild; serving the compiled artifact there would hide the edit.
    val root = Files.createTempDirectory("paste-combo-debug")
    val source = write(root.resolve("src/js/home.js"), "var home = 'source';")
    write(root.resolve("output/js/site/home.hash.v1.min.js"), "var home='compiled';")
    val manifest = Manifest(
      version = "1", generated = "",
      modules = Map("site.home" -> "js/site/home.js"),
      assets = Map("js/site/home.js" -> entry(source.toString, "/js/site/home.hash.v1.min.js"))
    )

    val (content, found) = handlerFor(root, manifest).loadModulesWithCount(Seq("site.home"), "js", minified = false)

    found shouldBe 1
    content should include("'source'")
    content should not include "compiled"
  }

  it should "not answer a module whose mapping names no asset" in {
    // A modules entry that does not name a key of `assets` carries no usable
    // information: resolution falls back to the convention key for the SAME
    // name, never to some other module's asset.
    val root = Files.createTempDirectory("paste-combo-mismatch")
    write(root.resolve("output/js/other.hash.v1.min.js"), "var other='other module';")
    val manifest = Manifest(
      version = "1", generated = "",
      modules = Map("wanted" -> "js/missing.js", "other" -> "js/other.js"),
      assets = Map("js/other.js" -> entry("src/js/other.js", "/js/other.hash.v1.min.js"))
    )

    val (content, found) = handlerFor(root, manifest).loadModulesWithCount(Seq("wanted"), "js", minified = true)

    found shouldBe 0
    content should not include "other module"
  }

  it should "expand CSS dependencies declared through the modules map" in {
    // theme.scss declares site.theme, so its asset key is css/theme.css — a key
    // the convention cannot derive from the name. Its dependency must still load.
    val root = Files.createTempDirectory("paste-combo-css-deps")
    write(root.resolve("output/css/base.hash.v1.min.css"), ".base{color:red}")
    write(root.resolve("output/css/theme.hash.v1.min.css"), ".theme{color:blue}")
    val manifest = Manifest(
      version = "1", generated = "",
      modules = Map("site.theme" -> "css/theme.css", "site.base" -> "css/base.css"),
      assets = Map(
        "css/theme.css" -> entry("src/scss/theme.scss", "/css/theme.hash.v1.min.css", Seq("site.base")),
        "css/base.css" -> entry("src/scss/base.scss", "/css/base.hash.v1.min.css")
      )
    )

    val (content, found) = handlerFor(root, manifest).loadModulesWithCount(Seq("site.theme"), "css", minified = true)

    found shouldBe 2
    content should include(".base")
    content should include(".theme")
    content.indexOf(".base") should be < content.indexOf(".theme")
  }

  it should "terminate each JS module before the next one begins" in {
    // Each module is valid alone; concatenated without a terminator the second
    // module's leading parenthesis continues the first module's expression.
    val root = Files.createTempDirectory("paste-combo-separator")
    write(root.resolve("output/js/first.hash.v1.min.js"), "var first=(function(){return 1})()")
    write(root.resolve("output/js/second.hash.v1.min.js"), "(function(){var x=2})()")
    val manifest = Manifest(
      version = "1", generated = "",
      modules = Map("first" -> "js/first.js", "second" -> "js/second.js"),
      assets = Map(
        "js/first.js" -> entry("src/js/first.js", "/js/first.hash.v1.min.js"),
        "js/second.js" -> entry("src/js/second.js", "/js/second.hash.v1.min.js")
      )
    )

    val (content, found) = handlerFor(root, manifest).loadModulesWithCount(Seq("first", "second"), "js", minified = true)

    found shouldBe 2
    content should include(Manifest.JsConcatSeparator)
  }

  "Manifest bootstrap" should "produce no markup for an empty module list" in {
    val manifest = Manifest(version = "1", generated = "", assets = Map.empty)

    manifest.jamBootstrap(Seq.empty) shouldBe ""
    manifest.jamBootstrap(Seq.empty, inlineThreshold = 1024) shouldBe ""
    manifest.jamCssBootstrap(Seq.empty) shouldBe ""
  }

  it should "assign isLoading for every module it loads" in {
    val manifest = Manifest(
      version = "1", generated = "",
      modules = Map("first" -> "js/first.js"),
      assets = Map("js/first.js" -> entry("src/js/first.js", "/js/first.hash.v1.min.js"))
    )

    val bootstrap = manifest.jamBootstrap(Seq("first"))

    bootstrap should include("d('first').isLoading=!0")
    bootstrap should not include "{=!0"
  }
}
