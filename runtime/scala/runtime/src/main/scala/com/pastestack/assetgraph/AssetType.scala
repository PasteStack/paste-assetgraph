package com.pastestack.assetgraph

/** The asset type a single-type [[PastePage.dump]] emits. */
enum AssetType(val wire: String) {
  case Css extends AssetType("css")
  case Js  extends AssetType("js")
}
