package com.pastestack.assetgraph.sbt

import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.{Files, Path}
import java.nio.file.attribute.FileTime
import java.util.zip.{ZipEntry, ZipOutputStream}
import org.scalatest.funsuite.AnyFunSuite
import _root_.sbt.{Attributed, IO, Logger}

final class AssetGraphPluginSpec extends AnyFunSuite {
  private def withDirectory(test: Path => Unit): Unit = {
    val directory = Files.createTempDirectory("assetgraph-webjar-spec")
    try test(directory) finally IO.delete(directory.toFile)
  }

  private def writeJar(path: Path, entry: String, content: String): Unit = {
    val output = new ZipOutputStream(Files.newOutputStream(path))
    try {
      output.putNextEntry(new ZipEntry(s"META-INF/resources/webjars/$entry"))
      output.write(content.getBytes(UTF_8))
      output.closeEntry()
    } finally output.close()
  }

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
    "demo/1.0/file:stream", "demo/1.0/line\nbreak"
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
