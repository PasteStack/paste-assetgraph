package com.pastestack.assetgraph

import java.io.InputStream
import java.nio.file.{Files, Path, Paths, StandardCopyOption}
import java.security.MessageDigest
import scala.sys.process._
import scala.util.Try

/**
 * Resolves the platform-specific paste-assetgraph binary bundled in the sbt plugin.
 *
 * Each plugin version is extracted into its own target directory and verified against
 * the checksum manifest shipped in the same jar. A bad or stale resource cannot be
 * silently reused by a later build.
 */
object Binary {
  /** Last-resort label for the extraction directory when the packaged version
    * resource and the jar manifest are both unavailable (an unpackaged build). */
  val Version = "0.1.0"
  val BinaryName = "paste-assetgraph"
  private val ExtractionDirectory = s"$BinaryName-bin"

  def path(targetDir: Path): Path =
    envOverride()
      .orElse(extractBundled(targetDir))
      .orElse(findInPath())
      .getOrElse {
        throw new RuntimeException(
          s"""paste-assetgraph binary not found for $platformName.
             |The sbt plugin does not contain a binary for this platform.
             |Install a matching standalone release or use a plugin release that contains it.
             |""".stripMargin
        )
      }

  def build(targetDir: Path, configPath: String, debug: Boolean = false, cwd: Option[String] = None): Int =
    run(Seq(path(targetDir).toString, "build", "--config", configPath) ++ debugFlag(debug) ++ cwdFlag(cwd))

  def watch(targetDir: Path, configPath: String, cwd: Option[String] = None): Process =
    Process(Seq(path(targetDir).toString, "watch", "--config", configPath) ++ cwdFlag(cwd)).run()

  private def run(arguments: Seq[String]): Int = Process(arguments).!

  private def debugFlag(debug: Boolean): Seq[String] =
    if (debug) Seq("--debug") else Seq.empty

  private def cwdFlag(cwd: Option[String]): Seq[String] =
    cwd.toSeq.flatMap(directory => Seq("--cwd", directory))

  private def envOverride(): Option[Path] =
    Option(System.getenv("PASTE_ASSETGRAPH_BIN"))
      .map(Paths.get(_))
      .filter(Files.isExecutable)

  /**
   * Extract this plugin version's bundled binary into `targetDir` and verify it
   * against the checksum manifest packaged beside it.
   *
   * Returns None when the jar carries no binary for the platform, so the caller
   * can look elsewhere. A binary that IS bundled but does not match its
   * checksum fails loudly instead: that is a broken artifact, not a reason to
   * run something else. An already-extracted binary is reused only while its
   * bytes still match, so a truncated or tampered file cannot survive.
   */
  private[assetgraph] def extractBundled(targetDir: Path): Option[Path] = {
    val platform = platformName
    val expected = resourceText("/native/manifest.json").flatMap(checksumFor(_, platform))
    val binaryTarget = targetDir
      .resolve(ExtractionDirectory)
      .resolve(artifactVersion)
      .resolve(binaryFileName)

    if (Files.isExecutable(binaryTarget) && expected.contains(sha256(binaryTarget))) {
      Some(binaryTarget)
    } else {
      Option(getClass.getResourceAsStream(s"/native/$platform/$binaryFileName")).map { input =>
        val extracted = try extract(input, binaryTarget) finally input.close()
        verifyChecksum(extracted, expected, platform)
      }
    }
  }

  private def readResource(input: InputStream): Option[String] =
    Try(new String(input.readAllBytes(), java.nio.charset.StandardCharsets.UTF_8)).toOption

  private def extract(input: InputStream, target: Path): Path = {
    val parent = target.getParent
    Files.createDirectories(parent)
    val temporary = Files.createTempFile(parent, s".$BinaryName-", ".tmp")
    try {
      Files.copy(input, temporary, StandardCopyOption.REPLACE_EXISTING)
      temporary.toFile.setExecutable(true)
      Files.move(temporary, target, StandardCopyOption.REPLACE_EXISTING)
      target
    } catch {
      case error: Throwable =>
        Files.deleteIfExists(temporary)
        throw error
    }
  }

  private def checksumFor(content: String, platform: String): Option[String] =
    ("\"" + java.util.regex.Pattern.quote(platform) + "\"\\s*:\\s*\"([0-9a-fA-F]{64})\"").r
      .findFirstMatchIn(content)
      .map(_.group(1).toLowerCase)

  private[assetgraph] def verifyChecksum(path: Path, expected: Option[String], platform: String): Path = {
    val actual = sha256(path)
    require(
      expected.contains(actual),
      s"paste-assetgraph checksum mismatch for $platform: expected ${expected.getOrElse("<missing>")}, actual $actual"
    )
    path
  }

  private def sha256(path: Path): String = {
    val digest = MessageDigest.getInstance("SHA-256")
    val buffer = new Array[Byte](64 * 1024)
    val input = Files.newInputStream(path)
    try {
      var read = input.read(buffer)
      while (read != -1) {
        digest.update(buffer, 0, read)
        read = input.read(buffer)
      }
    } finally {
      input.close()
    }
    digest.digest().map(byte => f"$byte%02x").mkString
  }

  private def findInPath(): Option[Path] =
    Option(System.getenv("PATH"))
      .toSeq
      .flatMap(_.split(java.io.File.pathSeparator).toSeq)
      .map(Paths.get(_).resolve(binaryFileName))
      .find(Files.isExecutable)

  private def artifactVersion: String =
    Option(System.getProperty("paste.assetgraph.version"))
      .filter(_.nonEmpty)
      .orElse(resourceText("/assetgraph-version.txt").map(_.trim).filter(_.nonEmpty))
      .orElse(Option(getClass.getPackage.getImplementationVersion).filter(_.nonEmpty))
      .getOrElse(Version)

  private def resourceText(name: String): Option[String] =
    Option(getClass.getResourceAsStream(name)).flatMap { stream =>
      try readResource(stream) finally stream.close()
    }

  private def platformName: String = s"$osName-$archName"

  private def osName: String =
    Option(System.getProperty("os.name"))
      .map(_.toLowerCase)
      .collect {
        case os if os.contains("mac") || os.contains("darwin") => "darwin"
        case os if os.contains("linux") => "linux"
        case os if os.contains("win") => "windows"
      }
      .getOrElse("unknown")

  private def archName: String =
    Option(System.getProperty("os.arch")).map(_.toLowerCase).collect {
      case "aarch64" | "arm64" => "aarch64"
      case "amd64" | "x86_64" => "amd64"
      case other => other
    }.getOrElse("unknown")

  private def binaryFileName: String = s"$BinaryName$executableSuffix"

  private def executableSuffix: String =
    if (osName == "windows") ".exe" else ""
}
