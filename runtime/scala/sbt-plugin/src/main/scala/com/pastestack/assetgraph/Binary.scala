package com.pastestack.assetgraph

import java.io.FileOutputStream
import java.nio.file.{Files, Path, Paths}
import scala.sys.process._
import scala.util.Try

/**
 * Manages the paste-assetgraph Rust binary.
 *
 * The binary is bundled as a classpath resource inside the sbt plugin jar
 * at native/{os}-{arch}/paste-assetgraph. On first use it is extracted to
 * the project's target directory — nothing is written outside the project.
 *
 * Resolution order:
 *   1. PASTE_ASSETGRAPH_BIN environment variable (explicit override)
 *   2. Bundled resource extracted to {targetDir}/paste-assetgraph-bin/
 *   3. System PATH (fallback for manual installs)
 */
object Binary {

  val Version = "0.1.0"
  val BinaryName = "paste-assetgraph"

  /**
   * Get path to the binary, extracting into targetDir if needed.
   */
  def path(targetDir: Path): Path = {
    envOverride()
      .orElse(extractBundled(targetDir))
      .orElse(findInPath())
      .getOrElse {
        throw new RuntimeException(
          s"""paste-assetgraph binary not found.
             |
             |The binary should be bundled in the sbt plugin jar but no binary
             |was found for this platform (${osName()}-${archName()}).
             |
             |As a workaround, you can:
             |  1. Build from source: cargo build --release
             |  2. Set PASTE_ASSETGRAPH_BIN to the binary path
             |  3. Add paste-assetgraph to your PATH
             |""".stripMargin
        )
      }
  }

  /**
   * Run build command.
   */
  def build(targetDir: Path, configPath: String, debug: Boolean = false, cwd: Option[String] = None): Int = {
    val binary = path(targetDir).toString
    val args = Seq(binary, "build", "--config", configPath) ++
      (if (debug) Seq("--debug") else Seq.empty) ++
      cwd.map(d => Seq("--cwd", d)).getOrElse(Seq.empty)
    Process(args).!
  }

  /**
   * Run watch command (non-blocking).
   */
  def watch(targetDir: Path, configPath: String, cwd: Option[String] = None): Process = {
    val binary = path(targetDir).toString
    val args = Seq(binary, "watch", "--config", configPath) ++
      cwd.map(d => Seq("--cwd", d)).getOrElse(Seq.empty)
    Process(args).run()
  }

  // -- Resolution strategies --------------------------------------------------

  private def envOverride(): Option[Path] = {
    Option(System.getenv("PASTE_ASSETGRAPH_BIN"))
      .map(Paths.get(_))
      .filter(Files.isExecutable)
  }

  /**
   * Extract the platform-specific binary from jar resources into the
   * project's target directory. Nothing is written outside the project.
   */
  private def extractBundled(targetDir: Path): Option[Path] = {
    val resourcePath = s"/native/${osName()}-${archName()}/$BinaryName"
    val stream = getClass.getResourceAsStream(resourcePath)
    if (stream == null) return None

    val binDir = targetDir.resolve("paste-assetgraph-bin")
    val target = binDir.resolve(BinaryName)

    // Already extracted — reuse
    if (Files.isExecutable(target)) {
      stream.close()
      return Some(target)
    }

    Try {
      Files.createDirectories(binDir)

      // Extract to temp file, then atomic rename to avoid partial writes
      val tmp = Files.createTempFile(binDir, s".$BinaryName-", ".tmp")
      val out = new FileOutputStream(tmp.toFile)
      try {
        val buffer = new Array[Byte](8192)
        var n = 0
        while ({ n = stream.read(buffer); n != -1 }) {
          out.write(buffer, 0, n)
        }
      } finally {
        out.close()
        stream.close()
      }

      tmp.toFile.setExecutable(true)
      Files.move(tmp, target, java.nio.file.StandardCopyOption.REPLACE_EXISTING)
      target
    }.toOption.filter(Files.isExecutable)
  }

  private def findInPath(): Option[Path] = {
    Try {
      val result = Seq("which", BinaryName).!!.trim
      if (result.nonEmpty) Some(Paths.get(result)) else None
    }.getOrElse(None)
  }

  // -- Platform detection -----------------------------------------------------

  private def osName(): String = {
    val os = System.getProperty("os.name", "").toLowerCase
    if (os.contains("mac") || os.contains("darwin")) "darwin"
    else if (os.contains("linux")) "linux"
    else if (os.contains("win")) "windows"
    else os
  }

  private def archName(): String = {
    System.getProperty("os.arch", "") match {
      case a if a == "aarch64" || a == "arm64" => "aarch64"
      case a if a == "amd64" || a == "x86_64"  => "amd64"
      case a => a
    }
  }
}
