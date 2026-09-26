package com.pastestack.assetgraph

import spray.json.*

/**
 * A JAM event: modules requested by a page. Encoded to the binary's
 * `{"jam":{...}}` JSON by [[Paste]].
 */
final case class Jam(
  css: Seq[String] = Nil,
  js: Seq[String] = Nil,
  inline: Boolean = false,
  sync: Boolean = false,
  media: Option[String] = None
)

object Jam extends DefaultJsonProtocol {
  given format: RootJsonFormat[Jam] = jsonFormat5(Jam.apply)
}
