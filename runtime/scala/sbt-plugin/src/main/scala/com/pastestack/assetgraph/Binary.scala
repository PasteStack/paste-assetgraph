package com.pastestack.assetgraph

import java.io.{InputStream, OutputStream}
import java.nio.file.{Files, Path, Paths, StandardCopyOption}
import java.security.{DigestInputStream, MessageDigest}
import java.util.Locale
import scala.sys.process._
import scala.util.control.NonFatal
import scala.util.{Try, Using}
import sjsonnew.shaded.scalajson.ast.unsafe.{JField, JObject, JString}
import sjsonnew.support.scalajson.unsafe.Parser

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
    envOverride(sys.env.get)
      .orElse(extractBundled(targetDir))
      .orElse(findInPath(sys.env.get))
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

  /**
   * An explicit PASTE_ASSETGRAPH_BIN override wins over everything else — so a
   * value that is set but does not name an executable file fails loudly instead
   * of silently falling through to the bundled binary.
   */
  private[assetgraph] def envOverride(env: String => Option[String]): Option[Path] = {
    val configured = env("PASTE_ASSETGRAPH_BIN").map(Paths.get(_))
    configured.foreach { candidate =>
      if (!isExecutableFile(candidate)) {
        throw new IllegalStateException(
          s"PASTE_ASSETGRAPH_BIN is set to $candidate but that is not an executable file"
        )
      }
    }
    configured
  }

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

    if (isExecutableFile(binaryTarget) && expected.contains(sha256(binaryTarget))) {
      Some(binaryTarget)
    } else {
      Option(getClass.getResourceAsStream(s"/native/$platform/$binaryFileName")).map { input =>
        settle(binaryTarget, expected, {
          val extracted = Using.resource(input)(extract(_, binaryTarget))
          verifyChecksum(extracted, expected, platform)
        })
      }
    }
  }

  /**
   * On Windows a running .exe cannot be replaced: rewriting `target` fails even
   * though the file already on disk is the binary this version would write.
   * When `attempt` throws, keep `target` only if it is an executable file that
   * still verifies against the checksum manifest; anything else rethrows the
   * original failure.
   */
  private[assetgraph] def settle(target: Path, expected: Option[String], attempt: => Path): Path =
    try attempt
    catch {
      case NonFatal(error) =>
        if (isExecutableFile(target) && Try(sha256(target)).toOption.exists(expected.contains)) {
          target
        } else {
          throw error
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
    Parser.parseFromString(content).toOption
      .collect { case obj: JObject => obj }
      .flatMap(_.value.collectFirst { case JField(`platform`, JString(value)) => value })
      .map(_.toLowerCase(Locale.ROOT))
      .filter(_.matches("[0-9a-f]{64}"))

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
    Using.resource(new DigestInputStream(Files.newInputStream(path), digest)) { stream =>
      stream.transferTo(OutputStream.nullOutputStream())
    }
    digest.digest().map(byte => f"${byte & 0xff}%02x").mkString
  }

  private def isExecutableFile(path: Path): Boolean =
    Files.isRegularFile(path) && Files.isExecutable(path)

  private[assetgraph] def findInPath(env: String => Option[String]): Option[Path] =
    env("PATH").toSeq
      .flatMap(_.split(java.io.File.pathSeparator).toSeq)
      .map(Paths.get(_).resolve(binaryFileName))
      .find(isExecutableFile)

  private def artifactVersion: String =
    Option(System.getProperty("paste.assetgraph.version"))
      .filter(_.nonEmpty)
      .orElse(resourceText("/assetgraph-version.txt").map(_.trim).filter(_.nonEmpty))
      .orElse(Option(getClass.getPackage.getImplementationVersion).filter(_.nonEmpty))
      .getOrElse(Version)

  private def resourceText(name: String): Option[String] =
    Option(getClass.getResourceAsStream(name)).flatMap { stream =>
      Using.resource(stream)(readResource)
    }

  private[assetgraph] def platformName: String = osName match {
    case "linux" => s"linux-$archName-musl"
    case os => s"$os-$archName"
  }

  private def osName: String =
    Option(System.getProperty("os.name"))
      .map(_.toLowerCase(Locale.ROOT))
      .collect {
        case os if os.contains("mac") || os.contains("darwin") => "darwin"
        case os if os.contains("linux") => "linux"
        case os if os.contains("win") => "windows"
      }
      .getOrElse("unknown")

  private def archName: String =
    Option(System.getProperty("os.arch")).map(_.toLowerCase(Locale.ROOT)).collect {
      case "aarch64" | "arm64" => "aarch64"
      case "amd64" | "x86_64" => "amd64"
      case other => other
    }.getOrElse("unknown")

  private def binaryFileName: String = s"$BinaryName$executableSuffix"

  private def executableSuffix: String =
    if (osName == "windows") ".exe" else ""
}
