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
 * Every render call is pinned to the manifest generation the page's first
 * call was answered with: a build landing mid-page can never make one
 * page mix two manifests' module maps — the library reports the
 * generation change instead, which retires the page permanently.
 *
 * `finish` seals the page after the last dump: the library verifies every
 * jammed module was emitted — a jam queued after its type's final dump is
 * a [[PasteException]], not a silently dropped tag — and further
 * jams/dumps throw. A failed `finish` leaves the page open so the caller
 * can add the missing dump and retry — unless the page is retired, in
 * which case every later call answers the recorded error.
 */
final class PastePage private[assetgraph] (paste: Paste) {

  /** The page's append-only event log — an immutable Vector replaced per
    * event so each render call posts a snapshot. */
  private var events: Vector[Paste.Event] = Vector.empty

  /** The manifest generation the render calls are pinned to: "" asks for
    * whatever the manifest is; every answer carries the token it was
    * planned against and each later call posts it back. */
  private var generation: String = ""

  /** A manifest-generation error retires the page permanently — earlier
    * dumps were planned against a manifest that no longer exists, so
    * every later call fails with it. */
  private var invalid: Option[PasteException] = None

  /** Whether a successful `finish` closed the page. */
  def isFinished: Boolean = events.lastOption.contains(Paste.FinishEvent)

  private def requireOpen(): Unit = {
    invalid.foreach(throw _)
    if (isFinished)
      throw new PasteException("page is finished — no more jams or dumps")
  }

  /** One page step: the call posts the page's generation token, the
    * answer refreshes it. A manifest-generation error retires the page,
    * then always propagates. */
  private def render(eventsJson: String): String =
    try {
      val (html, answered) = paste.renderPage(eventsJson, generation)
      generation = answered
      html
    } catch {
      case e: PasteException =>
        if (e.isManifestGeneration)
          invalid = Some(e)
        throw e
    }

  /** A Java caller can pass `null` or `Some(null)` past the Option type.
    * Matching on `Option[Any]` never unboxes, so a stored null is seen as
    * null — `exists`/`get` on an `Option[Boolean]` would read it as false. */
  private def checkOption(name: String, value: Option[Any]): Unit = value match {
    case null       => throw new PasteException(s"jam $name must be an Option, not null")
    case Some(null) => throw new PasteException(s"jam $name must not be Some(null)")
    case _          => ()
  }

  def jam(
    css: Seq[String] = Nil,
    js: Seq[String] = Nil,
    inline: Boolean = false,
    sync: Boolean = false,
    media: Option[String] = None,
    condition: Option[String] = None,
    perModule: Option[Boolean] = None
  ): String = {
    requireOpen()
    // Java callers can hand nulls past the signature. Every reference
    // boundary is checked before the event exists — a rejected jam changes
    // no page state, so a later valid jam on the same page is the
    // straightforward recovery. The module names themselves stay the
    // library's semantics; only nullness is the adapter's business.
    if (css == null) throw new PasteException("jam css must be a sequence, not null")
    if (js == null) throw new PasteException("jam js must be a sequence, not null")
    if (css.exists(_ == null))
      throw new PasteException("jam css entries must be non-null strings")
    if (js.exists(_ == null))
      throw new PasteException("jam js entries must be non-null strings")
    checkOption("media", media)
    checkOption("condition", condition)
    checkOption("perModule", perModule)
    events :+= Paste.JamEvent(Jam(css, js, inline, sync, media, condition, perModule))
    ""
  }

  /**
   * Dump one asset type's pending modules as tags/URLs.
   */
  def dump(assetType: AssetType): String = {
    // A Java caller can hand null past the signature — Some(null) would
    // otherwise NPE later inside encodeEvents' _.wire. Rejected before
    // the event exists, like every other null boundary on this class.
    if (assetType == null)
      throw new PasteException("dump assetType must be css or js, not null")
    dump(Some(assetType))
  }

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
    invalid match {
      case Some(error)      => throw error
      case None if isFinished => ""
      case None =>
        val candidate = events :+ Paste.FinishEvent
        render(Paste.encodeEvents(candidate))
        // Only a successful validation seals the page.
        events = candidate
        ""
    }

  private def dump(assetType: Option[AssetType]): String = {
    requireOpen()
    // Commit the dump event only when the render succeeds — a failed dump
    // must not count as delivery.
    val candidate = events :+ Paste.DumpEvent(assetType)
    val html = render(Paste.encodeEvents(candidate))
    events = candidate
    html
  }
}
