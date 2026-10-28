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

  test("an empty directory at the extraction path is replaced by the binary") {
    val target = Files.createTempDirectory("paste-assetgraph-binary-spec")
    System.setProperty("paste.assetgraph.version", "0.1.0-develop-a1b2c3d")
    val binaryTarget = expectedBinary(target, "0.1.0-develop-a1b2c3d")
    Files.createDirectories(binaryTarget)

    val extracted = Binary.extractBundled(target)

    assert(extracted.contains(binaryTarget))
    assert(Files.isRegularFile(binaryTarget))
    assert(Files.isExecutable(binaryTarget))
    assert(Files.size(binaryTarget) > 0)
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

  test("settle keeps the on-disk binary when replacement fails and it still verifies") {
    val target = Files.createTempDirectory("paste-assetgraph-binary-spec")
    System.setProperty("paste.assetgraph.version", "0.1.0-develop-a1b2c3d")
    val binary = Binary.extractBundled(target).get
    val failure = new java.io.IOException("cannot replace a running binary")

    // A Windows host refuses to overwrite a running .exe; the bytes already
    // in place are the ones this version would write, so they stand.
    val result = Binary.settle(binary, Some(digest(binary)), throw failure)

    assert(result == binary)
  }

  test("settle rethrows the replacement failure when the on-disk binary does not verify") {
    val binary = Files.createTempFile("paste-assetgraph-settle-mismatch", ".bin")
    Files.write(binary, "other".getBytes("UTF-8"))
    binary.toFile.setExecutable(true)
    val failure = new java.io.IOException("cannot replace a running binary")

    val thrown = intercept[java.io.IOException] {
      Binary.settle(binary, Some("0" * 64), throw failure)
    }
    assert(thrown eq failure)
  }

  test("settle rethrows the replacement failure when no binary is on disk") {
    val missing = Files.createTempDirectory("paste-assetgraph-settle-missing").resolve(Binary.BinaryName)
    val failure = new java.io.IOException("cannot replace a running binary")

    val thrown = intercept[java.io.IOException] {
      Binary.settle(missing, Some("0" * 64), throw failure)
    }
    assert(thrown eq failure)
  }

  test("settle returns the replacement when the attempt succeeds") {
    val replacement = Files.createTempFile("paste-assetgraph-settle-ok", ".bin")
    val unused = replacement.resolveSibling("paste-assetgraph-old")

    assert(Binary.settle(unused, Some("0" * 64), replacement) == replacement)
  }

  test("an executable PASTE_ASSETGRAPH_BIN override wins") {
    val binary = Files.createTempFile("paste-assetgraph-override", ".bin")
    binary.toFile.setExecutable(true)

    assert(Binary.envOverride(_ => Some(binary.toString)).contains(binary))
  }

  test("a set-but-invalid PASTE_ASSETGRAPH_BIN fails loudly instead of falling through") {
    val missing = "/nonexistent/paste-assetgraph"
    val missingError = intercept[IllegalStateException] {
      Binary.envOverride(_ => Some(missing))
    }
    assert(missingError.getMessage.contains(missing))

    // A file that exists but is not executable fails the same way.
    val plain = Files.createTempFile("paste-assetgraph-plain", ".bin")
    val plainError = intercept[IllegalStateException] {
      Binary.envOverride(_ => Some(plain.toString))
    }
    assert(plainError.getMessage.contains(plain.toString))
  }

  test("an unset PASTE_ASSETGRAPH_BIN leaves resolution to the bundled binary") {
    assert(Binary.envOverride(_ => None).isEmpty)
  }

  test("PATH lookup skips a directory and finds the executable file") {
    val dirA = Files.createTempDirectory("paste-path-a")
    Files.createDirectory(dirA.resolve(Binary.BinaryName))
    val dirB = Files.createTempDirectory("paste-path-b")
    val bin = Files.write(
      dirB.resolve(Binary.BinaryName),
      "#!/bin/sh\nexit 0\n".getBytes("UTF-8")
    )
    bin.toFile.setExecutable(true)

    assert(
      Binary.findInPath(_ => Some(s"$dirA${java.io.File.pathSeparator}$dirB")).contains(bin)
    )
  }

  test("a version property resolving outside the extraction root is refused before any file is written") {
    val target = Files.createTempDirectory("paste-assetgraph-binary-spec")
    val outside = target.resolve("outside")
    System.setProperty("paste.assetgraph.version", "../../outside")

    val error = intercept[IllegalArgumentException] {
      Binary.extractBundled(target)
    }

    assert(error.getMessage.contains("Invalid assetgraph version directory"))
    assert(
      !Files.exists(outside),
      "the escaping version must be refused before it creates a directory"
    )
  }

  test("safeVersion keeps ordinary version spellings") {
    assert(Binary.safeVersion("0.3.1") == "0.3.1")
    assert(Binary.safeVersion("0.1.0-develop-a1b2c3d") == "0.1.0-develop-a1b2c3d")
    Seq("..", "../../outside", "a/b", "a\\b", " leading", "trailing ", "", ".").foreach { value =>
      assertThrows[IllegalArgumentException](Binary.safeVersion(value), s"version $value")
    }
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

  private def digest(path: Path): String = {
    val md = java.security.MessageDigest.getInstance("SHA-256")
    md.update(Files.readAllBytes(path))
    md.digest().map(byte => f"${byte & 0xff}%02x").mkString
  }

  private def expectedBinary(target: Path, version: String): Path =
    target.resolve(s"${Binary.BinaryName}-bin").resolve(version).resolve(Binary.BinaryName)

  private def restore(name: String, value: Option[String]): Unit =
    value.fold(System.clearProperty(name))(System.setProperty(name, _))
}
