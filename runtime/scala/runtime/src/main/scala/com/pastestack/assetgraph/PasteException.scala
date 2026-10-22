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
