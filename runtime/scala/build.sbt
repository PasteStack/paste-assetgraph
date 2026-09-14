import com.tomshley.magicroot.sbt.projectsettings.{PublishGitLabPlugin, TomshleyCIBuildVersionPlugin, VersionFilePlugin}

ThisBuild / versionScheme := Some("semver-spec")

// Versions come from the repository VERSION file, plus the branch revision CI
// supplies: the release flow bumps VERSION, and nothing here restates it.
lazy val commonSettings = Seq(
  organization := "com.pastestack",
  publishMavenStyle := true,
  magicRootPublishGitLabProjectId := sys.env.get("TOMSHLEY_CICD_PUBLISH_PROJECT_ID").map(_.toInt).getOrElse(70289607),
  licenses := Seq("Apache-2.0" -> url("https://www.apache.org/licenses/LICENSE-2.0")),
  homepage := Some(url("https://github.com/PasteStack/paste-assetgraph"))
)

lazy val runtime = (project in file("runtime"))
  .enablePlugins(VersionFilePlugin, TomshleyCIBuildVersionPlugin, PublishGitLabPlugin)
  .settings(commonSettings)
  .settings(
    name := "paste-assetgraph-runtime",
    scalaVersion := "3.3.1",
    libraryDependencies ++= Seq(
      "io.spray" %% "spray-json" % "1.3.6",
      "org.scalatest" %% "scalatest" % "3.2.17" % Test
    )
  )

lazy val pekko = (project in file("pekko"))
  .dependsOn(runtime)
  .enablePlugins(VersionFilePlugin, TomshleyCIBuildVersionPlugin, PublishGitLabPlugin)
  .settings(commonSettings)
  .settings(
    name := "paste-assetgraph-pekko",
    scalaVersion := "3.3.1",
    libraryDependencies ++= Seq(
      "org.apache.pekko" %% "pekko-http" % "1.0.1",
      "org.apache.pekko" %% "pekko-actor-typed" % "1.0.2",
      "org.apache.pekko" %% "pekko-stream" % "1.0.2",
      "org.scalatest" %% "scalatest" % "3.2.17" % Test
    )
  )

lazy val root = (project in file("."))
  .aggregate(runtime, pekko)
  .settings(
    publish / skip := true,
    publishArtifact := false
  )
