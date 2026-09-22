package com.pastestack.assetgraph

import java.nio.file.attribute.FileTime
import java.nio.file.{Files, Path}
import org.scalatest.BeforeAndAfterEach
import org.scalatest.funsuite.AnyFunSuite

/**
 * Extraction is exercised through extractBundled rather than through path,
 * because path deliberately prefers PASTE_ASSETGRAPH_BIN — a developer with that
 * variable set would otherwise never run the code under test.
 *
 * Paths are compared as Path values, so the assertions hold on a filesystem
 * whose separator is not "/".
 */
final class BinarySpec extends AnyFunSuite with BeforeAndAfterEach {
  private val originalOs = Option(System.getProperty("os.name"))
  private val originalArch = Option(System.getProperty("os.arch"))
  private val originalVersion = Option(System.getProperty("paste.assetgraph.version"))

  // The test resources carry one bundled platform, so the tests declare it.
  private val bundledPlatform = "darwin-aarch64"

  override def beforeEach(): Unit = {
    super.beforeEach()
    System.setProperty("os.name", "Mac OS X")
    System.setProperty("os.arch", "aarch64")
  }

  override def afterEach(): Unit = {
    restore("os.name", originalOs)
    restore("os.arch", originalArch)
    restore("paste.assetgraph.version", originalVersion)
    super.afterEach()
  }

  Seq(
    ("Linux", "amd64", "linux-amd64-musl"),
    ("Linux", "x86_64", "linux-amd64-musl"),
    ("Linux", "aarch64", "linux-aarch64-musl"),
    ("Linux", "arm64", "linux-aarch64-musl"),
    ("Mac OS X", "aarch64", "darwin-aarch64"),
    ("Mac OS X", "x86_64", "darwin-amd64"),
    ("Windows 11", "amd64", "windows-amd64")
  ).foreach { case (os, arch, expected) =>
    test(s"selects $expected for $os on $arch") {
      System.setProperty("os.name", os)
      System.setProperty("os.arch", arch)
      assert(Binary.platformName == expected)
    }
  }

  test("extracts the platform binary into a directory named for the plugin version") {
    val target = Files.createTempDirectory("paste-assetgraph-binary-spec")
    System.setProperty("paste.assetgraph.version", "0.1.0-develop-a1b2c3d")

    val extracted = Binary.extractBundled(target)

    assert(extracted.contains(expectedBinary(target, "0.1.0-develop-a1b2c3d")))
    assert(Files.isExecutable(extracted.get))
    assert(Files.size(extracted.get) > 0)
  }

  test("a second plugin version receives its own extraction directory") {
    val target = Files.createTempDirectory("paste-assetgraph-binary-spec")

    System.setProperty("paste.assetgraph.version", "0.1.0-develop-a1b2c3d")
    val first = Binary.extractBundled(target)
    System.setProperty("paste.assetgraph.version", "0.1.1-hotfix-e4f5a6b")
    val second = Binary.extractBundled(target)

    assert(first.contains(expectedBinary(target, "0.1.0-develop-a1b2c3d")))
    assert(second.contains(expectedBinary(target, "0.1.1-hotfix-e4f5a6b")))
    assert(first != second)
  }

  test("an already-extracted binary is reused instead of rewritten") {
    val target = Files.createTempDirectory("paste-assetgraph-binary-spec")
    System.setProperty("paste.assetgraph.version", "0.1.0-develop-a1b2c3d")
    val first = Binary.extractBundled(target).get
    val marker = FileTime.fromMillis(0L)
    Files.setLastModifiedTime(first, marker)

    val second = Binary.extractBundled(target)

    assert(second.contains(first))
    assert(Files.getLastModifiedTime(first) == marker, "a matching binary should not be written again")
  }

  test("a modified extracted binary is replaced rather than reused") {
    val target = Files.createTempDirectory("paste-assetgraph-binary-spec")
    System.setProperty("paste.assetgraph.version", "0.1.0-develop-a1b2c3d")
    val first = Binary.extractBundled(target).get
    val bundled = Files.readAllBytes(first)
    Files.write(first, "tampered".getBytes("UTF-8"))

    val second = Binary.extractBundled(target)

    assert(second.contains(first))
    assert(Files.readAllBytes(first).toSeq == bundled.toSeq, "the bundled bytes should be restored")
  }

  test("no bundled binary for the platform leaves resolution to the caller") {
    val target = Files.createTempDirectory("paste-assetgraph-binary-spec")
    System.setProperty("os.name", "Linux")
    System.setProperty("os.arch", "ppc64")

    assert(Binary.extractBundled(target).isEmpty)
  }

  test("refuses a checksum mismatch and names both checksums") {
    val target = Files.createTempFile("paste-assetgraph-tampered", ".bin")
    Files.write(target, Array[Byte](1, 2, 3))

    val error = intercept[IllegalArgumentException] {
      Binary.verifyChecksum(target, Some("0" * 64), bundledPlatform)
    }

    assert(error.getMessage.contains(s"expected ${"0" * 64}"))
    assert(error.getMessage.contains("actual"))
    assert(error.getMessage.contains(bundledPlatform))
  }

  test("refuses a bundled binary with no checksum entry") {
    val target = Files.createTempFile("paste-assetgraph-unlisted", ".bin")
    Files.write(target, Array[Byte](1, 2, 3))

    val error = intercept[IllegalArgumentException] {
      Binary.verifyChecksum(target, None, bundledPlatform)
    }

    assert(error.getMessage.contains("<missing>"))
  }

  private def expectedBinary(target: Path, version: String): Path =
    target.resolve(s"${Binary.BinaryName}-bin").resolve(version).resolve(Binary.BinaryName)

  private def restore(name: String, value: Option[String]): Unit =
    value.fold(System.clearProperty(name))(System.setProperty(name, _))
}
