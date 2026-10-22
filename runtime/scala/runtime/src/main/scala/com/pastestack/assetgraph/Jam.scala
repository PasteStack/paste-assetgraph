package com.pastestack.assetgraph

import spray.json.*

/**
 * A JAM event: modules requested by a page. Encoded to the binary's
 * `{"jam":{...}}` JSON by [[Paste]]. `condition` wraps the group's tags
 * in a downlevel conditional comment and keeps a conditional js group
 * out of the loaded set — `None` posts no key. `perModule` overrides
 * the render's per-module setting for this event's groups in either
 * direction — `None` inherits the setting and posts no key.
 */
final case class Jam(
  css: Seq[String] = Nil,
  js: Seq[String] = Nil,
  inline: Boolean = false,
  sync: Boolean = false,
  media: Option[String] = None,
  condition: Option[String] = None,
  perModule: Option[Boolean] = None
)

object Jam {
  // The protocol is imported, not inherited: inheriting would publish all
  // of spray's formats as members of this public companion.
  import DefaultJsonProtocol.*
  given format: RootJsonFormat[Jam] = jsonFormat7(Jam.apply)
}
