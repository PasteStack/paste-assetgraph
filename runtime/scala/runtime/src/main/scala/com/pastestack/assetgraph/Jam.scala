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
