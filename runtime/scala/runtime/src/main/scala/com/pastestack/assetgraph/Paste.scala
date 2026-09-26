package com.pastestack.assetgraph

import java.io.InputStream
import java.nio.file.{Files, Path, Paths, StandardCopyOption}
import scala.util.Using
import spray.json.*

/**
 * Thin wrapper around the paste-assetgraph native library: every tag, URL
 * and combo answer comes from the library itself; this class only calls it.
 *
 * Safe to share across request threads: the library holds no per-caller
 * state and each [[page]] carries its own event state.
 */
final class Paste private[assetgraph] (
  private[assetgraph] val settings: Paste.Settings
) {

  /** The mount point the library generates and resolves URLs under. */
  def basePath: String = settings.basePath

  /** Start a fresh per-render event list — see [[PastePage]]. */
  def page(): PastePage = new PastePage(this)

  /**
   * Render an encoded event list — the last dump's HTML, or
   * [[PasteException]] when the library reports an error.
   */
  private[assetgraph] def render(eventsJson: String): String =
    PasteLibrary.render(settingsJson, eventsJson)

  /**
   * Resolve a combo path plus optional query (e.g.
   * `/paste/1/a+v1.0.min.js?x=y`) through the library.
   */
  def combo(pathAndQuery: String): ComboResponse = {
    // The path crosses the ABI as a C string: a NUL would silently
    // truncate it.
    if (pathAndQuery.indexOf('\u0000') >= 0)
      throw new PasteException("combo path must not contain NUL")
    PasteLibrary.combo(settingsJson, pathAndQuery)
  }

  /** The settings JSON the library expects, built once. */
  private val settingsJson: String = {
    val fields = Vector(
      "manifest" -> JsString(settings.manifest.toString),
      "outputBase" -> JsString(settings.outputBase.toString),
      "cdnBase" -> JsString(settings.cdnBase),
      "basePath" -> JsString(settings.basePath),
      "inlineThreshold" -> JsNumber(settings.inlineThreshold)
    ) ++ settings.outputJs.map(p => "outputJs" -> JsString(p.toString))
      ++ settings.outputCss.map(p => "outputCss" -> JsString(p.toString))
    JsObject(fields*).compactPrint
  }
}

object Paste {

  final case class Settings(
    manifest: Path,
    outputBase: Path,
    outputJs: Option[Path] = None,
    outputCss: Option[Path] = None,
    cdnBase: String = "",
    basePath: String = "/paste",
    inlineThreshold: Int = 1024
  )

  /**
   * Default build output directory for compiled assets — the single
   * `PASTE_OUTPUT_PATH`/`target/public` definition every runtime default
   * reads.
   */
  private[assetgraph] def defaultOutputBasePath: String =
    sys.env.getOrElse("PASTE_OUTPUT_PATH", "target/public")

  /**
   * An event in a page's render list: a jam, a dump — `Some(assetType)` for
   * one asset type, `None` for every type in one dump — or the terminal
   * finish, encoded as the bare string `"finish"`, that seals the page
   * against undumped modules.
   */
  private[assetgraph] sealed trait Event
  private[assetgraph] final case class JamEvent(jam: Jam) extends Event
  private[assetgraph] final case class DumpEvent(assetType: Option[AssetType]) extends Event
  private[assetgraph] case object FinishEvent extends Event

  /** Encode an event list to the library's render JSON: `{"events":[...]}`. */
  private[assetgraph] def encodeEvents(events: Seq[Event]): String =
    JsObject("events" -> JsArray(events.map {
      case JamEvent(jam) => JsObject("jam" -> jam.toJson(Jam.format))
      case DumpEvent(t)  => JsObject("dump" -> JsString(t.map(_.wire).getOrElse("all")))
      case FinishEvent   => JsString("finish")
    }.toVector)).compactPrint

  /** Context classloader first, then this jar's. */
  private def classpathResource(name: String): Option[InputStream] =
    Option(Thread.currentThread().getContextClassLoader)
      .flatMap(cl => Option(cl.getResourceAsStream(name.stripPrefix("/"))))
      .orElse(Option(getClass.getResourceAsStream(name)))

  def apply(settings: Settings): Paste =
    new Paste(settings)

  /**
   * A Paste whose manifest is a classpath resource, copied to a temp file so
   * the library can read it. Throws [[IllegalArgumentException]] when the
   * resource is absent.
   */
  def fromClasspath(
    resource: String = "/paste-assetgraph-manifest.json",
    outputBase: Path = Paths.get(defaultOutputBasePath),
    cdnBase: String = "",
    basePath: String = "/paste",
    inlineThreshold: Int = 1024
  ): Paste =
    fromClasspath(
      classpathResource, resource, outputBase, cdnBase, basePath, inlineThreshold,
      Paths.get(System.getProperty("java.io.tmpdir"))
    )

  private[assetgraph] def fromClasspath(
    resourceLoader: String => Option[InputStream],
    resource: String,
    outputBase: Path,
    cdnBase: String,
    basePath: String,
    inlineThreshold: Int,
    tempRoot: Path
  ): Paste = {
    val stream = resourceLoader(resource)
      .getOrElse(throw new IllegalArgumentException(s"Resource not found: $resource"))
    // The resource owns the stream until the copy finishes or fails: Using
    // closes it even when temp creation throws, and deleteOnExit is
    // registered inside the block so a failing close cannot strand the
    // temp file unregistered.
    val manifest = Using.resource(stream) { s =>
      val temp = Files.createTempFile(tempRoot, "paste-assetgraph-manifest-", ".json")
      temp.toFile.deleteOnExit()
      try {
        Files.copy(s, temp, StandardCopyOption.REPLACE_EXISTING)
        temp
      } catch {
        case e: Throwable =>
          Files.deleteIfExists(temp)
          throw e
      }
    }
    apply(
      Settings(manifest, outputBase, None, None, cdnBase, basePath, inlineThreshold)
    )
  }
}
