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

// The specs set JVM-wide properties (os.name, os.arch, the plugin version);
// a forked test JVM keeps them out of the sbt server's own process.
Test / fork := true

// The plugin reads this at runtime to keep each version's extracted binary in
// its own directory, so a new plugin release cannot reuse an older binary.
Compile / resourceGenerators += Def.task {
  val output = (Compile / resourceManaged).value / "assetgraph-version.txt"
  val content = version.value
  // An unchanged version must not rewrite the resource — a fresh timestamp
  // alone would propagate a spurious rebuild to everything that watches it.
  if (!output.isFile || IO.read(output) != content) {
    IO.write(output, content)
  }
  Seq(output)
}.taskValue

// The authoritative root legal texts ride inside every published JAR —
// binary, sources, and docs alike — at the JVM-conventional paths,
// sourced from the repo root so packaged copies can never drift from
// the files license review sees.
def legalMappings(repoRoot: File): Seq[(File, String)] = Seq(
  (repoRoot / "LICENSE") -> "META-INF/LICENSE",
  (repoRoot / "NOTICE") -> "META-INF/NOTICE",
  (repoRoot / "THIRD_PARTY_LICENSES.md") -> "META-INF/THIRD_PARTY_LICENSES"
)

Compile / packageBin / mappings ++=
  legalMappings((ThisBuild / baseDirectory).value / ".." / ".." / "..")
Compile / packageSrc / mappings ++=
  legalMappings((ThisBuild / baseDirectory).value / ".." / ".." / "..")
Compile / packageDoc / mappings ++=
  legalMappings((ThisBuild / baseDirectory).value / ".." / ".." / "..")

// CI supplies this project with the native binaries and native/manifest.json
// produced by the Rust build job before the plugin is packaged.
