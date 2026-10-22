package com.pastestack.assetgraph

import java.io.InputStream
import java.nio.charset.StandardCharsets
import java.nio.file.{Files, Path, Paths}
import java.security.{DigestInputStream, MessageDigest}
import java.util.Locale
import scala.util.{Try, Using}
import com.sun.jna.Pointer
import spray.json.*
import spray.json.DefaultJsonProtocol.*

/**
 * Locates and loads the paste-assetgraph native library, in this order:
 *
 *  1. The `PASTE_ASSETGRAPH_LIB` environment variable. Set but not a
 *     regular file is an [[IllegalStateException]] naming the value —
 *     an explicit override never falls through silently.
 *  2. A bundled classpath resource `/native-lib/{platform}/{file}`,
 *     extracted once per JVM into a temporary directory and verified
 *     against the SHA-256 recorded in `/native-lib/manifest.json`.
 *
 * Throws [[IllegalStateException]] naming both locations and the platform
 * when no library resolves. All seams are parameters on `resolveWith`/
 * `extractBundled`, so the object carries no mutable state of its own.
 */
object PasteLibrary {

  private val ManifestResource = "/native-lib/manifest.json"

  /** The library's C ABI as JNA sees it: UTF-8 JSON in, a heap C string of
    * envelope JSON out; every returned pointer goes to `paste_free`. */
  private[assetgraph] trait Native extends com.sun.jna.Library {
    def paste_render_page(settings: String, events: String, generation: String): Pointer
    def paste_combo(settings: String, pathAndQuery: String): Pointer
    def paste_combo_conditional(settings: String, pathAndQuery: String, ifModifiedSince: String): Pointer
    def paste_free(p: Pointer): Unit
  }

  /** Every symbol [[Native]] binds; probed at load so a stale library
    * fails here rather than on the first call that needs the symbol. */
  private val Symbols =
    Seq("paste_render_page", "paste_combo", "paste_combo_conditional", "paste_free")

  /** The loaded library — resolved and loaded once per JVM. A library that
    * cannot be loaded or lacks a symbol raises [[PasteException]]: JNA's
    * UnsatisfiedLinkError is a fatal LinkageError, which a Future rethrows
    * on its dispatcher thread instead of failing the request. */
  private[assetgraph] lazy val native: Native = load(resolve().toString)

  /** Load and probe the library at `path`; see [[native]]. */
  private[assetgraph] def load(path: String): Native = {
    val options = java.util.Map.of(com.sun.jna.Library.OPTION_STRING_ENCODING, "UTF-8")
    try {
      val library = com.sun.jna.NativeLibrary.getInstance(path, options)
      Symbols.foreach(library.getFunction)
      com.sun.jna.Native.load(path, classOf[Native], options)
    } catch {
      case e: LinkageError =>
        throw new PasteException(
          s"paste-assetgraph native library $path failed to load: ${e.getMessage}",
          cause = e
        )
    }
  }

  /**
   * Render one page step under `settingsJson`, pinned to `generation`
   * when non-empty — an empty token asks for whatever the manifest
   * currently is. Answers `(html, generation)`: the step's markup and
   * the token the caller posts back on the next step. A token that no
   * longer matches the manifest raises a [[PasteException]] whose `kind`
   * is [[PasteException.ManifestGeneration]]; any other error envelope
   * raises PasteException with its message.
   */
  private[assetgraph] def renderPage(
    settingsJson: String,
    eventsJson: String,
    generation: String
  ): (String, String) =
    decodePage(
      unwrap(take(native.paste_render_page(settingsJson, eventsJson, generation)))
    )

  /**
   * The page payload must carry both fields as strings — an absent or
   * mistyped one is a malformed response, not a reason to default. An
   * empty `generation` here would read as the caller's "accept the current
   * manifest" sentinel, so only the library's own non-empty token returns.
   */
  private[assetgraph] def decodePage(payload: JsValue): (String, String) =
    payload match {
      case JsObject(fields) =>
        (fields.get("html"), fields.get("generation")) match {
          case (Some(JsString(html)), Some(JsString(token))) if token.nonEmpty =>
            (html, token)
          case _ =>
            throw new PasteException(
              "paste-assetgraph returned a malformed page response"
            )
        }
      case _ =>
        throw new PasteException(
          "paste-assetgraph returned a malformed page response"
        )
    }

  /**
   * Resolve `pathAndQuery` under `settingsJson` to a [[ComboResponse]]; an
   * error envelope raises [[PasteException]] with its message.
   */
  private[assetgraph] def combo(settingsJson: String, pathAndQuery: String): ComboResponse =
    unwrap(take(native.paste_combo(settingsJson, pathAndQuery)))
      .convertTo[ComboResponse](ComboResponse.format)

  /** The conditional form — `ifModifiedSince` is the request's raw
    * header value; `null` reaches the entry point as a NULL pointer
    * exactly like the no-validator call. */
  private[assetgraph] def comboConditional(
      settingsJson: String,
      pathAndQuery: String,
      ifModifiedSince: String
  ): ComboResponse =
    unwrap(take(native.paste_combo_conditional(settingsJson, pathAndQuery, ifModifiedSince)))
      .convertTo[ComboResponse](ComboResponse.format)

  /** Read the returned string and always release the native buffer. */
  private def take(pointer: Pointer): String = {
    if (pointer == null)
      throw new PasteException("paste-assetgraph returned no response")
    try pointer.getString(0, "UTF-8")
    finally native.paste_free(pointer)
  }

  /** The envelope's `ok` payload, or [[PasteException]] carrying `error`
    * — plus the machine `kind` when the library marks one. */
  private def unwrap(envelope: String): JsValue = {
    val fields = envelope.parseJson.asJsObject.fields
    fields.get("error") match {
      case Some(error) =>
        throw new PasteException(
          error.convertTo[String],
          fields.get("kind").map(_.convertTo[String])
        )
      case None =>
        fields.getOrElse(
          "ok",
          throw new PasteException(
            s"paste-assetgraph returned a malformed response: $envelope"
          )
        )
    }
  }

  def resolve(): Path =
    resolveWith(sys.env.get, bundledLibrary)

  /**
   * The bundled library, extracted and verified once per JVM — `lazy val`
   * gives the once-only, thread-safe memoization.
   */
  private lazy val bundledLibrary: Option[Path] =
    extractBundled(classpathResource)

  private def classpathResource(name: String): Option[InputStream] =
    Option(getClass.getResourceAsStream(name))

  private[assetgraph] def resolveWith(
    env: String => Option[String],
    bundled: => Option[Path]
  ): Path = {
    // Absolute before both the check and the load: a bare name is
    // searched on the library path by the loader, so the file checked
    // against the working directory need not be the one that loads.
    val envOverride =
      env("PASTE_ASSETGRAPH_LIB").map(value => Paths.get(value).toAbsolutePath)
    envOverride.foreach { path =>
      if (!Files.isRegularFile(path))
        throw new IllegalStateException(
          s"PASTE_ASSETGRAPH_LIB is set to $path but that is not a file"
        )
    }
    envOverride
      .orElse(bundled)
      .getOrElse {
        throw new IllegalStateException(
          s"paste-assetgraph native library not found for platform $currentPlatform. " +
            "Searched: the PASTE_ASSETGRAPH_LIB environment variable " +
            s"and the classpath resource ${libraryResourcePath()}"
        )
      }
  }

  /**
   * Extract the bundled library for this platform, or None when the
   * classpath carries no such resource. A bundled library whose bytes do
   * not match the checksum manifest fails loudly rather than loading.
   */
  private[assetgraph] def extractBundled(
    resources: String => Option[InputStream],
    tempRoot: Path = Paths.get(System.getProperty("java.io.tmpdir"))
  ): Option[Path] = {
    val platform = currentPlatform
    resources(s"/native-lib/$platform/${libraryFileName()}").map { stream =>
      val target = extract(stream, tempRoot)
      try verify(target, resources, platform)
      catch {
        case e: Throwable =>
          Files.deleteIfExists(target)
          Files.deleteIfExists(target.getParent)
          throw e
      }
      target
    }
  }

  private def libraryResourcePath(): String =
    s"/native-lib/$currentPlatform/${libraryFileName()}"

  private def libraryFileName(): String =
    osLabel(System.getProperty("os.name")) match {
      case "windows" => "paste_assetgraph_ffi.dll"
      case "darwin"  => "libpaste_assetgraph_ffi.dylib"
      case _         => "libpaste_assetgraph_ffi.so"
    }

  /**
   * The platform segment used by the packaged `native-lib/` directories:
   * `{os}-{arch}`, plus `-musl` on musl Linux — a musl process cannot load
   * a glibc build, so the Linux bundles split by libc.
   */
  private[assetgraph] def platformName(osName: String, osArch: String, musl: Boolean = false): String = {
    val os = osLabel(osName)
    val suffix = if (os == "linux" && musl) "-musl" else ""
    s"$os-${archLabel(osArch)}$suffix"
  }

  /** This host's platform segment, including the Linux libc split. */
  private[assetgraph] def currentPlatform: String =
    platformName(System.getProperty("os.name"), System.getProperty("os.arch"), linuxMusl)

  /** True on a musl Linux: glibc exports gnu_get_libc_version, musl does
    * not. The lookup is a symbol probe on the running process, not a file
    * check — only a genuine linkage miss means no glibc. */
  private lazy val linuxMusl: Boolean =
    osLabel(System.getProperty("os.name")) == "linux" && {
      try {
        com.sun.jna.NativeLibrary.getProcess.getFunction("gnu_get_libc_version")
        false
      } catch {
        case _: UnsatisfiedLinkError => true
      }
    }

  private def osLabel(osName: String): String =
    Option(osName).map(_.toLowerCase(Locale.ROOT)).collect {
      case os if os.contains("mac") || os.contains("darwin") => "darwin"
      case os if os.contains("linux")                      => "linux"
      case os if os.contains("win")                        => "windows"
    }.getOrElse("unknown")

  private def archLabel(osArch: String): String =
    Option(osArch).map(_.toLowerCase(Locale.ROOT)).collect {
      case "aarch64" | "arm64"  => "aarch64"
      case "amd64" | "x86_64"   => "amd64"
      case other                => other
    }.getOrElse("unknown")

  /**
   * Copy the bundled library into a fresh temporary directory; the copy and
   * its directory are removed at JVM exit (deleteOnExit runs in reverse
   * registration order, so the file goes first). The stream owns the whole
   * allocation: a failed copy deletes target and directory, and Using
   * closes the stream even when the temp creation itself throws.
   */
  private def extract(stream: InputStream, tempRoot: Path): Path =
    Using.resource(stream) { s =>
      val dir = Files.createTempDirectory(tempRoot, "paste-assetgraph-")
      val target = dir.resolve(libraryFileName())
      try {
        Files.copy(s, target)
      } catch {
        case e: Throwable =>
          Files.deleteIfExists(target)
          Files.deleteIfExists(dir)
          throw e
      }
      dir.toFile.deleteOnExit()
      target.toFile.deleteOnExit()
      target
    }

  /**
   * Check the extracted library against the flat checksum manifest
   * `{"<platform>": "<sha256>"}` — a library whose platform is missing from
   * the manifest, or whose bytes differ, fails loudly.
   */
  private def verify(
    target: Path,
    resources: String => Option[InputStream],
    platform: String
  ): Unit = {
    val content = resources(ManifestResource)
      .map(stream => Using.resource(stream)(s => decodeUtf8(s.readAllBytes())))
      .getOrElse(throw new IllegalStateException(
        s"Bundled paste-assetgraph library present but $ManifestResource is missing"
      ))
    val expected = checksumFor(content, platform)
      .getOrElse(throw new IllegalStateException(
        s"Bundled paste-assetgraph checksum manifest has no entry for $platform"
      ))
    val actual = sha256(target)
    if (actual != expected)
      throw new IllegalStateException(
        s"Bundled paste-assetgraph checksum mismatch at $target: expected $expected, got $actual"
      )
  }

  private def checksumFor(content: String, platform: String): Option[String] =
    Try(content.parseJson).toOption
      .collect { case obj: JsObject => obj }
      .flatMap(_.fields.get(platform))
      .collect { case JsString(value) => value.toLowerCase(Locale.ROOT) }
      .filter(_.matches("[0-9a-f]{64}"))

  private def sha256(path: Path): String = {
    val digest = MessageDigest.getInstance("SHA-256")
    Using.resource(new DigestInputStream(Files.newInputStream(path), digest)) { stream =>
      stream.transferTo(java.io.OutputStream.nullOutputStream())
    }
    digest.digest().map(b => f"${b & 0xff}%02x").mkString
  }

  /** Strict UTF-8 decode: malformed input throws rather than substituting U+FFFD. */
  private def decodeUtf8(bytes: Array[Byte]): String =
    StandardCharsets.UTF_8.newDecoder().decode(java.nio.ByteBuffer.wrap(bytes)).toString
}
