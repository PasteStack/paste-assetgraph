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

package com.pastestack.assetgraph.sbt

import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.{Files, Path}
import java.nio.file.attribute.FileTime
import java.util.zip.{ZipEntry, ZipOutputStream}
import org.scalatest.funsuite.AnyFunSuite
import _root_.sbt.{Attributed, IO, Logger}

final class AssetGraphPluginSpec extends AnyFunSuite {
  /** A BasicLogger that records every message, for asserting on warns. */
  private final class RecordingLogger extends sbt.internal.util.BasicLogger {
    val lines = new scala.collection.mutable.ListBuffer[String]
    override def ansiCodesSupported: Boolean = false
    def trace(t: => Throwable): Unit = ()
    def log(level: sbt.Level.Value, message: => String): Unit = lines += message
    def success(message: => String): Unit = ()
    def control(event: sbt.util.ControlEvent.Value, message: => String): Unit = ()
    def logAll(events: Seq[sbt.util.LogEvent]): Unit = events.foreach(e => lines += e.toString)
  }

  private def withDirectory(test: Path => Unit): Unit = {
    val directory = Files.createTempDirectory("assetgraph-webjar-spec")
    try test(directory) finally IO.delete(directory.toFile)
  }

  private def writeJar(path: Path, entries: Seq[(String, String)]): Unit = {
    val output = new ZipOutputStream(Files.newOutputStream(path))
    try {
      entries.foreach { case (entry, content) =>
        output.putNextEntry(new ZipEntry(s"META-INF/resources/webjars/$entry"))
        output.write(content.getBytes(UTF_8))
        output.closeEntry()
      }
    } finally output.close()
  }

  private def writeJar(path: Path, entry: String, content: String): Unit =
    writeJar(path, Seq(entry -> content))

  private def extract(jar: Path, root: Path): Seq[String] =
    AssetGraphPlugin.extractWebjars(Seq(Attributed.blank(jar.toFile)), root.toFile, Logger.Null)

  test("extracts ordinary assets and reuses an unchanged archive") {
    withDirectory { directory =>
      val jar = directory.resolve("assets.jar")
      val root = directory.resolve("extracted")
      writeJar(jar, "demo/1.0/css/main.css", "body {}")
      val paths = extract(jar, root)
      val asset = root.resolve("demo/1.0/css/main.css")
      assert(paths == Seq(root.resolve("demo/1.0").toFile.getCanonicalPath))
      assert(new String(Files.readAllBytes(asset), UTF_8) == "body {}")
      assert(Files.exists(root.resolve("demo/1.0/.extracted")))
      Files.setLastModifiedTime(asset, FileTime.fromMillis(0L))
      assert(extract(jar, root) == paths)
      assert(Files.getLastModifiedTime(asset) == FileTime.fromMillis(0L))
    }
  }

  test("refreshes a changed archive without retaining stale assets") {
    withDirectory { directory =>
      val jar = directory.resolve("assets.jar")
      val root = directory.resolve("extracted")
      writeJar(jar, "demo/1.0/css/old.css", "old")
      extract(jar, root)
      val marker = root.resolve("demo/1.0/.extracted")
      val updated = FileTime.fromMillis(Files.getLastModifiedTime(marker).toMillis + 2000L)
      writeJar(jar, "demo/1.0/css/new.css", "new")
      Files.setLastModifiedTime(jar, updated)
      extract(jar, root)
      assert(!Files.exists(root.resolve("demo/1.0/css/old.css")))
      assert(new String(Files.readAllBytes(root.resolve("demo/1.0/css/new.css")), UTF_8) == "new")
    }
  }

  test("a mid-extraction failure aborts the build without marking the group") {
    withDirectory { directory =>
      val jarA = directory.resolve("a.jar")
      val jarB = directory.resolve("b.jar")
      val root = directory.resolve("extracted")
      // x/y.js fails at extraction: its parent x lands as a file first.
      writeJar(jarA, Seq(
        "demo/1.0/a.js" -> "A",
        "demo/1.0/x" -> "not-a-dir",
        "demo/1.0/x/y.js" -> "B"
      ))
      writeJar(jarB, Seq("demo/1.0/b.js" -> "B"))

      intercept[java.io.IOException] {
        AssetGraphPlugin.extractWebjars(
          Seq(jarA, jarB).map(j => Attributed.blank(j.toFile)), root.toFile, Logger.Null
        )
      }
      // Partial output stays unmarked, so a later run re-extracts instead of
      // trusting the incomplete directory.
      assert(!Files.exists(root.resolve("demo/1.0/.extracted")))
    }
  }

  test("a directory that cannot be extracted aborts the whole build") {
    withDirectory { directory =>
      val jar = directory.resolve("assets.jar")
      val root = directory.resolve("extracted")
      writeJar(jar, Seq(
        "good/1.0/a.js" -> "G",
        "bad/1.0/x" -> "not-a-dir",
        "bad/1.0/x/y.js" -> "B"
      ))

      intercept[java.io.IOException](extract(jar, root))

      // Earlier groups still completed; the failing one is unmarked.
      assert(new String(Files.readAllBytes(root.resolve("good/1.0/a.js")), UTF_8) == "G")
      assert(Files.exists(root.resolve("good/1.0/.extracted")))
      assert(!Files.exists(root.resolve("bad/1.0/.extracted")))
    }
  }

  test("rejects file traversal before overwriting a file outside the extraction root") {
    withDirectory { directory =>
      val jar = directory.resolve("assets.jar")
      val root = directory.resolve("extracted")
      val sentinel = directory.resolve("sentinel.txt")
      Files.write(sentinel, "preserve".getBytes(UTF_8))
      writeJar(jar, "demo/1.0/../../../sentinel.txt", "overwrite")
      intercept[IllegalArgumentException](extract(jar, root))
      assert(new String(Files.readAllBytes(sentinel), UTF_8) == "preserve")
    }
  }

  test("rejects artifact traversal before deleting a directory outside the extraction root") {
    withDirectory { directory =>
      val jar = directory.resolve("assets.jar")
      val root = Files.createDirectory(directory.resolve("extracted"))
      val outside = Files.createDirectory(directory.resolve("outside"))
      val sentinel = outside.resolve("sentinel.txt")
      Files.write(sentinel, "preserve".getBytes(UTF_8))
      writeJar(jar, "../outside/payload.txt", "payload")
      intercept[IllegalArgumentException](extract(jar, root))
      assert(new String(Files.readAllBytes(sentinel), UTF_8) == "preserve")
      assert(!Files.exists(outside.resolve("payload.txt")))
    }
  }

  Seq(
    "../outside/file", "demo/../file", "demo/1.0/../../../file",
    "demo/1.0/..\\..\\file", "/demo/1.0/file", "C:/1.0/file",
    "demo/1.0/C:\\file", "demo//file", "demo/1.0/", "demo/1.0/.",
    "demo/1.0/file:stream", "demo/1.0/line\nbreak",
    // reserved group-root marker name — a jar must never write it
    "demo/1.0/.extracted"
  ).zipWithIndex.foreach { case (entry, index) =>
    test(s"rejects unsafe archive path $index") {
      withDirectory { directory =>
        intercept[IllegalArgumentException] {
          AssetGraphPlugin.validatedWebjarPaths(directory.resolve("extracted").toFile, entry)
        }
      }
    }
  }

  test("rejects an existing artifact-directory symlink before cleanup") {
    withDirectory { directory =>
      val jar = directory.resolve("assets.jar")
      val root = Files.createDirectory(directory.resolve("extracted"))
      val artifact = Files.createDirectory(root.resolve("demo"))
      val outside = Files.createDirectory(directory.resolve("outside"))
      val sentinel = outside.resolve("sentinel.txt")
      Files.write(sentinel, "preserve".getBytes(UTF_8))
      Files.createSymbolicLink(artifact.resolve("1.0"), outside)
      writeJar(jar, "demo/1.0/payload.txt", "payload")
      intercept[IllegalArgumentException](extract(jar, root))
      assert(new String(Files.readAllBytes(sentinel), UTF_8) == "preserve")
      assert(!Files.exists(outside.resolve("payload.txt")))
    }
  }

  test("a same-size archive with new content refreshes even at an older mtime") {
    withDirectory { directory =>
      val jar = directory.resolve("assets.jar")
      val root = directory.resolve("extracted")
      writeJar(jar, "demo/1.0/css/main.css", "AAAA")
      extract(jar, root)
      val asset = root.resolve("demo/1.0/css/main.css")
      assert(new String(Files.readAllBytes(asset), UTF_8) == "AAAA")
      // Same length, different bytes, older mtime — an mtime cache would
      // keep the stale content; the checksum comparison cannot.
      writeJar(jar, "demo/1.0/css/main.css", "BBBB")
      Files.setLastModifiedTime(jar, FileTime.fromMillis(0L))
      extract(jar, root)
      assert(new String(Files.readAllBytes(asset), UTF_8) == "BBBB")
    }
  }

  test("a file deleted from an otherwise marked directory is restored") {
    withDirectory { directory =>
      val jar = directory.resolve("assets.jar")
      val root = directory.resolve("extracted")
      writeJar(jar, Seq("demo/1.0/a.js" -> "A", "demo/1.0/b.js" -> "B"))
      extract(jar, root)
      Files.delete(root.resolve("demo/1.0/a.js"))
      extract(jar, root)
      assert(new String(Files.readAllBytes(root.resolve("demo/1.0/a.js")), UTF_8) == "A")
      assert(new String(Files.readAllBytes(root.resolve("demo/1.0/b.js")), UTF_8) == "B")
    }
  }

  test("two jars contributing one root keep both sets, first jar wins a shared path") {
    withDirectory { directory =>
      val jarA = directory.resolve("a.jar")
      val jarB = directory.resolve("b.jar")
      val root = directory.resolve("extracted")
      writeJar(jarA, Seq("demo/1.0/shared.js" -> "A", "demo/1.0/a.js" -> "A"))
      writeJar(jarB, Seq("demo/1.0/shared.js" -> "B", "demo/1.0/b.js" -> "B"))

      val paths = AssetGraphPlugin.extractWebjars(
        Seq(jarA, jarB).map(j => Attributed.blank(j.toFile)), root.toFile, Logger.Null
      )
      assert(paths == Seq(root.resolve("demo/1.0").toFile.getCanonicalPath))
      assert(new String(Files.readAllBytes(root.resolve("demo/1.0/a.js")), UTF_8) == "A")
      assert(new String(Files.readAllBytes(root.resolve("demo/1.0/b.js")), UTF_8) == "B")
      // Classpath order decides the shared member's content.
      assert(new String(Files.readAllBytes(root.resolve("demo/1.0/shared.js")), UTF_8) == "A")

      // A second pass sees the merged group as fresh, jarB's losing member
      // does not make it stale.
      assert(AssetGraphPlugin.extractWebjars(
        Seq(jarA, jarB).map(j => Attributed.blank(j.toFile)), root.toFile, Logger.Null
      ) == paths)
    }
  }

  test("returned roots follow classpath order") {
    withDirectory { directory =>
      val jarAlpha = directory.resolve("alpha.jar")
      val jarBeta = directory.resolve("beta.jar")
      val root = directory.resolve("extracted")
      writeJar(jarAlpha, "alpha/1.0/a.js", "A")
      writeJar(jarBeta, "beta/1.0/b.js", "B")
      val paths = AssetGraphPlugin.extractWebjars(
        Seq(jarBeta, jarAlpha).map(j => Attributed.blank(j.toFile)), root.toFile, Logger.Null
      )
      assert(paths == Seq(
        root.resolve("beta/1.0").toFile.getCanonicalPath,
        root.resolve("alpha/1.0").toFile.getCanonicalPath
      ), paths)
    }
  }

  test("materializeManifest copies a configured external manifest to the managed name") {
    withDirectory { directory =>
      val base = directory.toFile
      val managed = new java.io.File(base, "target/resource_managed")
      val source = new java.io.File(base, "conf/history.json")
      IO.createDirectory(source.getParentFile)
      IO.write(source, "{\"a\":1}")
      val target = AssetGraphPlugin.materializeManifest(base, managed, "conf/history.json")
      assert(target == new java.io.File(managed, "paste-assetgraph-manifest.json"))
      assert(new String(Files.readAllBytes(target.toPath), UTF_8) == "{\"a\":1}")
      // The configured source stays on disk for CI inspection.
      assert(source.isFile)
    }
  }

  test("materializeManifest overwrites a stale managed default with the configured source") {
    withDirectory { directory =>
      val base = directory.toFile
      val managed = new java.io.File(base, "target/resource_managed")
      IO.createDirectory(managed)
      val stale = new java.io.File(managed, "paste-assetgraph-manifest.json")
      IO.write(stale, "stale")
      val source = new java.io.File(base, "conf/history.json")
      IO.createDirectory(source.getParentFile)
      IO.write(source, "{\"a\":2}")
      val target = AssetGraphPlugin.materializeManifest(base, managed, "conf/history.json")
      assert(new String(Files.readAllBytes(target.toPath), UTF_8) == "{\"a\":2}")
    }
  }

  test("materializeManifest with no configured path uses the managed file in place") {
    withDirectory { directory =>
      val base = directory.toFile
      val managed = new java.io.File(base, "target/resource_managed")
      IO.createDirectory(managed)
      val existing = new java.io.File(managed, "paste-assetgraph-manifest.json")
      IO.write(existing, "{\"a\":3}")
      val target = AssetGraphPlugin.materializeManifest(base, managed, "")
      assert(target.getCanonicalFile == existing.getCanonicalFile)
      assert(new String(Files.readAllBytes(target.toPath), UTF_8) == "{\"a\":3}")
    }
  }

  test("materializeManifest fails when the manifest was never generated") {
    withDirectory { directory =>
      val base = directory.toFile
      val managed = new java.io.File(base, "target/resource_managed")
      intercept[IllegalArgumentException] {
        AssetGraphPlugin.materializeManifest(base, managed, "conf/missing.json")
      }
      intercept[IllegalArgumentException] {
        AssetGraphPlugin.materializeManifest(base, managed, "")
      }
    }
  }

  test("generated config lists every source dir and the layout keys") {
    val config = AssetGraphPlugin.generateConfig(
      Seq("src/main/web", "lib/assets"),
      "target/public",
      "target/paste-assetgraph-manifest.json",
      Seq.empty,
      minify = true,
      sourceMaps = true,
      debugMode = false,
      storageBackend = "local",
      keepVersions = 10,
      outputLayout = "flat",
      buildDirName = "_paste_build"
    )

    // Every entry lands verbatim and in order — a dir that holds JS and
    // styles together is a source root like any other.
    assert(config.contains("""source.dirs = ["src/main/web", "lib/assets"]"""))
    assert(config.contains("""output.layout = "flat""""))
    assert(config.contains("""output.build-dir-name = "_paste_build""""))
    assert(!config.contains("source.js"), config)
    assert(!config.contains("source.scss"), config)
    // `source.images` was never used by the build — the generated config no
    // longer carries it.
    assert(!config.contains("source.images"), config)
    // Hash-qualified URLs default on; the setting feeds the generated file.
    assert(config.contains("jam.hash-urls = true"), config)
  }

  test("generated config spells unbounded retention as all and a bound as a number") {
    def generate(keepVersions: Int): String =
      AssetGraphPlugin.generateConfig(
        Seq("src/main/web"),
        "target/public",
        "target/paste-assetgraph-manifest.json",
        Seq.empty,
        minify = true,
        sourceMaps = true,
        debugMode = false,
        storageBackend = "dist",
        keepVersions = keepVersions,
        outputLayout = "flat",
        buildDirName = "_paste_build"
      )

    // -1 is the plugin's keep-all sentinel — it lands as `all` because a
    // negative number is not a valid retention count in the shared parser.
    assert(generate(-1).contains("storage.keep-versions = all"))
    assert(generate(0).contains("storage.keep-versions = 0"))
    assert(generate(3).contains("storage.keep-versions = 3"))
    // A value below the sentinel is a configuration error, never a
    // silently-reset policy.
    intercept[IllegalArgumentException] {
      generate(-2)
    }
  }

  test("generated config carries the hash-urls opt-out when set") {
    val config = AssetGraphPlugin.generateConfig(
      Seq("src/main/web"),
      "target/public",
      "target/paste-assetgraph-manifest.json",
      Seq.empty,
      minify = true,
      sourceMaps = true,
      debugMode = false,
      storageBackend = "local",
      keepVersions = 10,
      outputLayout = "flat",
      buildDirName = "_paste_build",
      hashUrls = false
    )
    assert(config.contains("jam.hash-urls = false"), config)
  }

  test("generated config carries the cdn origin only when set") {
    def generate(cdnUrl: String): String =
      AssetGraphPlugin.generateConfig(
        Seq("src/main/web"),
        "target/public",
        "target/paste-assetgraph-manifest.json",
        Seq.empty,
        minify = true,
        sourceMaps = true,
        debugMode = false,
        storageBackend = "local",
        keepVersions = 10,
        outputLayout = "flat",
        buildDirName = "_paste_build",
        cdnUrl = cdnUrl
      )

    val withCdn = generate("https://cdn.example")
    assert(withCdn.contains("cdn.enabled = true"), withCdn)
    assert(withCdn.contains("""cdn.url = "https://cdn.example""""), withCdn)
    // Empty is disabled: no cdn keys land in the generated file at all.
    val without = generate("")
    assert(!without.contains("cdn.enabled"), without)
    assert(!without.contains("cdn.url"), without)
  }

  test("generated config escapes quotes, backslashes and control characters in paths") {
    // The shared parser decodes quoted values as JSON strings, so a value
    // must survive the round trip byte-identically — a raw newline would
    // split the line and corrupt the file rather than fail loudly.
    val config = AssetGraphPlugin.generateConfig(
      Seq("""src/odd"quoted"""", "src/back\\slash", "src/with\nnewline"),
      "target/public",
      "target/paste-assetgraph-manifest.json",
      Seq("include\tpath"),
      minify = true,
      sourceMaps = false,
      debugMode = false,
      storageBackend = "local",
      keepVersions = 10,
      outputLayout = "flat",
      buildDirName = "_paste_build"
    )
    assert(config.contains(
      """source.dirs = ["src/odd\"quoted\"", "src/back\\slash", "src/with\nnewline"]"""
    ), config)
    assert(config.contains("""include-paths = ["include\tpath"]"""), config)
  }

  test("writeGeneratedConfig feeds hash-urls through the shared config path") {
    withDirectory { directory =>
      val base = directory.toFile
      val configFile = new java.io.File(base, "target/paste-assetgraph-generated.conf")
      val resourceManaged = new java.io.File(base, "target/resource_managed")

      // Both pasteAssetGraphBuild and pasteAssetGraphWatch generate through
      // writeGeneratedConfig, so one check covers the setting on both.
      AssetGraphPlugin.writeGeneratedConfig(
        Logger.Null,
        base,
        configFile,
        resourceManaged,
        "",
        Seq("src/main/web"),
        "target/public",
        Seq.empty,
        minify = true,
        sourceMaps = true,
        debug = false,
        storageBackend = "local",
        keepVersions = 10,
        "flat",
        "_paste_build",
        Seq.empty,
        hashUrls = false
      )

      val generated = new String(Files.readAllBytes(configFile.toPath), UTF_8)
      assert(generated.contains("jam.hash-urls = false"), generated)
    }
  }

  test("writeGeneratedConfig overwrites a stale config with changed settings") {
    withDirectory { directory =>
      val base = directory.toFile
      val configFile = new java.io.File(base, "target/paste-assetgraph-generated.conf")
      val resourceManaged = new java.io.File(base, "target/resource_managed")

      def write(sourceDirs: Seq[String], outputLayout: String): Unit =
        AssetGraphPlugin.writeGeneratedConfig(
          Logger.Null,
          base,
          configFile,
          resourceManaged,
          "",
          sourceDirs,
          "target/public",
          Seq.empty,
          minify = true,
          sourceMaps = true,
          debug = false,
          storageBackend = "local",
          keepVersions = 10,
          outputLayout,
          "_paste_build",
          Seq.empty
        )

      // Watch used to generate only when the file was missing — a second run
      // must pick up the changed settings, not the first config's contents.
      write(Seq("stale/dir"), "flat")
      write(Seq("src/main/web", "lib/assets"), "beside-source")

      val regenerated = new String(Files.readAllBytes(configFile.toPath), UTF_8)
      assert(regenerated.contains("""source.dirs = ["src/main/web", "lib/assets"]"""), regenerated)
      assert(regenerated.contains("""output.layout = "beside-source""""), regenerated)
      assert(!regenerated.contains("stale/dir"), regenerated)
      assert(regenerated.contains("""manifest = "target/resource_managed/paste-assetgraph-manifest.json""""), regenerated)
    }
  }

  test("writeGeneratedConfig warns when the manifest path cannot be made relative") {
    withDirectory { directory =>
      val base = directory.resolve("base").toFile
      val configFile = new java.io.File(base, "target/paste-assetgraph-generated.conf")
      // resourceManaged outside the project base cannot be relativized —
      // the generated config takes the absolute path and warns about it.
      val resourceManaged = directory.resolve("elsewhere/resource_managed").toFile
      val log = new RecordingLogger

      AssetGraphPlugin.writeGeneratedConfig(
        log,
        base,
        configFile,
        resourceManaged,
        "",
        Seq("src/main/web"),
        "target/public",
        Seq.empty,
        minify = true,
        sourceMaps = true,
        debug = false,
        storageBackend = "local",
        keepVersions = 10,
        "flat",
        "_paste_build",
        Seq.empty
      )

      val absolute = s"${resourceManaged.getPath}/paste-assetgraph-manifest.json"
      val generated = new String(Files.readAllBytes(configFile.toPath), UTF_8)
      assert(generated.contains(s"""manifest = "$absolute""""), generated)
      assert(
        log.lines.exists(line => line.contains("absolute") && line.contains("manifest")),
        log.lines.mkString("\n")
      )
    }
  }

  test("writeGeneratedConfig leaves an unchanged file's timestamp alone and rewrites only changed content") {
    withDirectory { directory =>
      val base = directory.toFile
      val configFile = new java.io.File(base, "target/paste-assetgraph-generated.conf")
      val resourceManaged = new java.io.File(base, "target/resource_managed")

      def write(sourceDirs: Seq[String]): Unit =
        AssetGraphPlugin.writeGeneratedConfig(
          Logger.Null,
          base,
          configFile,
          resourceManaged,
          "",
          sourceDirs,
          "target/public",
          Seq.empty,
          minify = true,
          sourceMaps = true,
          debug = false,
          storageBackend = "local",
          keepVersions = 10,
          "flat",
          "_paste_build",
          Seq.empty
        )

      write(Seq("src/main/web"))
      val marker = FileTime.fromMillis(0L)
      Files.setLastModifiedTime(configFile.toPath, marker)

      // Same settings: the destination must not be touched at all.
      write(Seq("src/main/web"))
      assert(Files.getLastModifiedTime(configFile.toPath) == marker)

      // Changed settings: the file is rewritten, so its stamp moves.
      write(Seq("src/main/web", "lib/assets"))
      assert(Files.getLastModifiedTime(configFile.toPath) != marker)
      assert(
        new String(Files.readAllBytes(configFile.toPath), UTF_8)
          .contains("""source.dirs = ["src/main/web", "lib/assets"]""")
      )
    }
  }

  test("materializeManifest leaves a byte-identical managed file untouched and copies changed content") {
    withDirectory { directory =>
      val base = directory.toFile
      val managed = new java.io.File(base, "target/resource_managed")
      val source = new java.io.File(base, "conf/history.json")
      IO.createDirectory(source.getParentFile)
      IO.write(source, "{\"a\":1}")

      val target = AssetGraphPlugin.materializeManifest(base, managed, "conf/history.json")
      val marker = FileTime.fromMillis(0L)
      Files.setLastModifiedTime(target.toPath, marker)

      // Identical content: no copy, timestamp holds.
      AssetGraphPlugin.materializeManifest(base, managed, "conf/history.json")
      assert(Files.getLastModifiedTime(target.toPath) == marker)

      // Changed content: the copy lands and the stamp moves.
      IO.write(source, "{\"a\":2}")
      AssetGraphPlugin.materializeManifest(base, managed, "conf/history.json")
      assert(Files.getLastModifiedTime(target.toPath) != marker)
      assert(new String(Files.readAllBytes(target.toPath), UTF_8) == "{\"a\":2}")
    }
  }

  test("rejects existing asset symlinks even when they remain inside the extraction root") {
    withDirectory { directory =>
      val root = Files.createDirectory(directory.resolve("extracted"))
      val version = Files.createDirectories(root.resolve("demo/1.0"))
      val sentinel = version.resolve("sentinel.txt")
      Files.write(sentinel, "preserve".getBytes(UTF_8))
      Files.createSymbolicLink(version.resolve("linked.txt"), sentinel)
      intercept[IllegalArgumentException] {
        AssetGraphPlugin.validatedWebjarPaths(root.toFile, "demo/1.0/linked.txt")
      }
      assert(new String(Files.readAllBytes(sentinel), UTF_8) == "preserve")
    }
  }
}
