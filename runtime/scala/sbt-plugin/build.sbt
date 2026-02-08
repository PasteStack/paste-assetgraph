organization := "com.pastestack"
name := "paste-assetgraph-sbt"
version := "0.1.0"
scalaVersion := "2.12.18"
publishMavenStyle := true
licenses := Seq("MIT" -> url("https://opensource.org/licenses/MIT"))
homepage := Some(url("https://github.com/PasteStack/paste-assetgraph-scala"))

enablePlugins(SbtPlugin)
pluginCrossBuild / sbtVersion := "1.9.0"
