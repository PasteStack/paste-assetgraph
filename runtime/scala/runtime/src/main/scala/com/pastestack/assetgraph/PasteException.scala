package com.pastestack.assetgraph

/** A paste-assetgraph binary invocation failed; the message carries its stderr. */
final class PasteException(message: String) extends RuntimeException(message)
