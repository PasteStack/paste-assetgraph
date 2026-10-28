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

ThisBuild / versionScheme := Some("semver-spec")

// The authoritative root legal texts ride inside every published JAR —
// binary, sources, and docs alike — at the JVM-conventional paths,
// sourced from the repo root so packaged copies can never drift from
// the files license review sees.
def legalMappings(repoRoot: File): Seq[(File, String)] = Seq(
  (repoRoot / "LICENSE") -> "META-INF/LICENSE",
  (repoRoot / "NOTICE") -> "META-INF/NOTICE",
  (repoRoot / "THIRD_PARTY_LICENSES.md") -> "META-INF/THIRD_PARTY_LICENSES"
)

// Runtime tests use the bundled library unless PASTE_ASSETGRAPH_LIB explicitly selects one.
ThisBuild / Test / fork := true

// Versions come from the repository VERSION file, plus the branch revision CI
// supplies: the release flow bumps VERSION, and nothing here restates it.
lazy val commonSettings = Seq(
  organization := "com.pastestack",
  publishMavenStyle := true,
  magicRootPublishGitLabProjectId := sys.env.get("TOMSHLEY_CICD_PUBLISH_PROJECT_ID").map(_.toInt).getOrElse(70289607),
  licenses := Seq("Apache-2.0" -> url("https://www.apache.org/licenses/LICENSE-2.0")),
  homepage := Some(url("https://github.com/PasteStack/paste-assetgraph")),
  Compile / packageBin / mappings ++=
    legalMappings((ThisBuild / baseDirectory).value / ".." / ".."),
  Compile / packageSrc / mappings ++=
    legalMappings((ThisBuild / baseDirectory).value / ".." / ".."),
  Compile / packageDoc / mappings ++=
    legalMappings((ThisBuild / baseDirectory).value / ".." / "..")
)

lazy val runtime = (project in file("runtime"))
  .enablePlugins(VersionFilePlugin, TomshleyCIBuildVersionPlugin, PublishGitLabPlugin)
  .settings(commonSettings)
  .settings(
    name := "paste-assetgraph-runtime",
    scalaVersion := "3.3.1",
    // Ships the bundled native libraries the runtime resolves at run time;
    // the build-rust CI job fills native-lib/ there before publish.
    Compile / unmanagedResourceDirectories +=
      (ThisBuild / baseDirectory).value / ".." / ".." / "runtime" / "native",
    libraryDependencies ++= Seq(
      "net.java.dev.jna" % "jna" % "5.17.0",
      "io.spray" %% "spray-json" % "1.3.6",
      "org.scalatest" %% "scalatest" % "3.2.17" % Test
    )
  )

lazy val pekko = (project in file("pekko"))
  .dependsOn(runtime % "compile->compile;test->test")
  .enablePlugins(VersionFilePlugin, TomshleyCIBuildVersionPlugin, PublishGitLabPlugin)
  .settings(commonSettings)
  .settings(
    name := "paste-assetgraph-pekko",
    scalaVersion := "3.3.1",
    libraryDependencies ++= Seq(
      "org.apache.pekko" %% "pekko-http" % "1.0.1",
      // Provided, as pekko-http declares it: the consumer chooses the Pekko
      // version, so this library never pins a mixed module set on its classpath.
      "org.apache.pekko" %% "pekko-stream" % "1.0.2" % Provided,
      "org.apache.pekko" %% "pekko-http-testkit" % "1.0.1" % Test,
      "org.apache.pekko" %% "pekko-stream-testkit" % "1.0.2" % Test,
      "org.scalatest" %% "scalatest" % "3.2.17" % Test
    )
  )

lazy val root = (project in file("."))
  .aggregate(runtime, pekko)
  .settings(
    publish / skip := true,
    publishArtifact := false
  )
