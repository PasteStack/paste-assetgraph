import com.tomshley.magicroot.sbt.projectsettings.{PublishGitLabPlugin, TomshleyCIBuildVersionPlugin, VersionFilePlugin}

organization := "com.pastestack"
name := "paste-assetgraph-sbt"
scalaVersion := "2.12.18"
publishMavenStyle := true
licenses := Seq("Apache-2.0" -> url("https://www.apache.org/licenses/LICENSE-2.0"))
homepage := Some(url("https://github.com/PasteStack/paste-assetgraph"))

ThisBuild / versionScheme := Some("semver-spec")

// The version comes from the repository VERSION file, plus the branch revision
// CI supplies — the same contract as the runtime libraries in ../build.sbt.
enablePlugins(SbtPlugin, VersionFilePlugin, TomshleyCIBuildVersionPlugin, PublishGitLabPlugin)
magicRootPublishGitLabProjectId := sys.env.get("TOMSHLEY_CICD_PUBLISH_PROJECT_ID").map(_.toInt).getOrElse(70289607)

// Compatibility floor for consuming builds: compile the plugin against the
// oldest sbt 1.x it supports rather than whatever this build happens to run.
pluginCrossBuild / sbtVersion := "1.9.0"

libraryDependencies += "org.scalatest" %% "scalatest" % "3.2.19" % Test

// The plugin reads this at runtime to keep each version's extracted binary in
// its own directory, so a new plugin release cannot reuse an older binary.
Compile / resourceGenerators += Def.task {
  val output = (Compile / resourceManaged).value / "assetgraph-version.txt"
  IO.write(output, version.value)
  Seq(output)
}.taskValue

// CI supplies this project with the native binaries and native/manifest.json
// produced by the Rust build job before the plugin is packaged.
