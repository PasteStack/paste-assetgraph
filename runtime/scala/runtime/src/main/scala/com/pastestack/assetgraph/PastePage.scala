package com.pastestack.assetgraph

/**
 * A single page render's event list.
 *
 * Jams arrive from anywhere in a render — nested templates and components
 * each add their own — while dumps land where the layout emits them, so the
 * page accumulates the render's events. NOT thread-safe: create one per
 * render — `paste.page()` — and keep it on the request's thread. `jam` only
 * records an event (and returns `""` so a Twirl template can write
 * `@page.jam(...)`); each `dump` renders the full event list so far through
 * the native library, which is what lets the renderer skip modules an
 * earlier dump already delivered. That call renders synchronously, so run
 * the render on a blocking dispatcher, never the server's default one.
 */
final class PastePage private[assetgraph] (paste: Paste) {

  /** The page's single mutable cell: an immutable Vector replaced per event. */
  private var events: Vector[Paste.Event] = Vector.empty

  def jam(
    css: Seq[String] = Nil,
    js: Seq[String] = Nil,
    inline: Boolean = false,
    sync: Boolean = false,
    media: Option[String] = None
  ): String = {
    events :+= Paste.JamEvent(Jam(css, js, inline, sync, media))
    ""
  }

  def dumpCss(): String = dump(Paste.AssetType.Css)

  def dumpJs(): String = dump(Paste.AssetType.Js)

  private def dump(assetType: Paste.AssetType): String = {
    events :+= Paste.DumpEvent(assetType)
    paste.render(Paste.encodeEvents(events))
  }
}
