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

/** Response of `paste-assetgraph combo`, as emitted by the binary's JSON.
  * `location` is the canonical URL a non-canonical versioned request
  * redirects to — present only on a 302. */
final case class ComboResponse(
  status: Int,
  contentType: String,
  cacheControl: String,
  cacheable: Boolean,
  location: Option[String],
  lastModified: Option[String],
  body: String
)

object ComboResponse {
  // Imported, not inherited — see Jam's companion.
  import DefaultJsonProtocol.*
  given format: RootJsonFormat[ComboResponse] = jsonFormat7(ComboResponse.apply)
}
