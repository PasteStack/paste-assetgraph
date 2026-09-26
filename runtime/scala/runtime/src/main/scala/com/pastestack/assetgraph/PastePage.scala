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
 *
 * `finish` seals the page after the last dump: the library verifies every
 * jammed module was emitted — a jam queued after its type's final dump is
 * a [[PasteException]], not a silently dropped tag — and further
 * jams/dumps throw. A failed `finish` leaves the page open so the caller
 * can add the missing dump and retry.
 */
final class PastePage private[assetgraph] (paste: Paste) {

  /** The page's single mutable cell: an immutable Vector replaced per event. */
  private var events: Vector[Paste.Event] = Vector.empty

  /** Whether a successful `finish` closed the page. */
  def isFinished: Boolean = events.lastOption.contains(Paste.FinishEvent)

  private def requireOpen(): Unit =
    if (isFinished)
      throw new PasteException("page is finished — no more jams or dumps")

  def jam(
    css: Seq[String] = Nil,
    js: Seq[String] = Nil,
    inline: Boolean = false,
    sync: Boolean = false,
    media: Option[String] = None
  ): String = {
    requireOpen()
    events :+= Paste.JamEvent(Jam(css, js, inline, sync, media))
    ""
  }

  /**
   * Dump one asset type's pending modules as tags/URLs.
   */
  def dump(assetType: AssetType): String = dump(Some(assetType))

  /**
   * Dump every asset type's pending modules in one dump — CSS tags first,
   * then JS — equivalent to `dump(AssetType.Css)` then `dump(AssetType.Js)`
   * back to back.
   */
  def dump(): String = dump(None)

  /**
   * Seal the page: the library checks that every jammed module was dumped.
   * Returns `""` (so a Twirl template can write `@page.finish()`), and is
   * a no-op on an already-finished page.
   */
  def finish(): String =
    if (isFinished) ""
    else {
      val candidate = events :+ Paste.FinishEvent
      paste.render(Paste.encodeEvents(candidate))
      // Only a successful validation seals the page.
      events = candidate
      ""
    }

  private def dump(assetType: Option[AssetType]): String = {
    requireOpen()
    // Commit the dump event only when the render succeeds — a failed dump
    // must not count as delivery.
    val candidate = events :+ Paste.DumpEvent(assetType)
    val html = paste.render(Paste.encodeEvents(candidate))
    events = candidate
    html
  }
}
