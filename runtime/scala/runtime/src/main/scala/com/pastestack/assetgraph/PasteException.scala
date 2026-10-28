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
 * A paste-assetgraph library call failed; the message is the library's.
 *
 * `kind` carries the library's machine classification when one failure
 * must be told apart from the rest — currently
 * [[PasteException.ManifestGeneration]], a manifest rewrite that retired
 * a page mid-render.
 */
final class PasteException(
    message: String,
    val kind: Option[String] = None,
    cause: Throwable = null
) extends RuntimeException(message, cause) {

  /** Whether a mid-page manifest rewrite retired the page — the one kind
    * an adapter must tell apart. */
  def isManifestGeneration: Boolean = kind.contains(PasteException.ManifestGeneration)
}

object PasteException {

  /** The kind a mid-page manifest rewrite reports: the page's stored
    * generation token no longer matches the manifest on disk. */
  val ManifestGeneration: String = "manifest-generation"
}
