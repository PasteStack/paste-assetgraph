package com.pastestack.assetgraph

import spray.json.*

/** Response of `paste-assetgraph combo`, as emitted by the binary's JSON.
  * `location` is the canonical URL a non-canonical versioned request
  * redirects to — present only on a 302. */
final case class ComboResponse(
  status: Int,
  contentType: String,
  cacheControl: String,
  cacheable: Boolean,
  location: Option[String],
  body: String
)

object ComboResponse extends DefaultJsonProtocol {
  given format: RootJsonFormat[ComboResponse] = jsonFormat6(ComboResponse.apply)
}
