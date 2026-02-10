import scala.io.Source

// Common settings
val commonSettings = Seq(
  organization := "com.pastestack",
  version := {
    val base = "0.1.0"
    if (sys.env.contains("CI_COMMIT_TAG")) base
    else s"$base-SNAPSHOT"
  },
  publishMavenStyle := true,
  publishTo := Some("gitlab-maven" at
    s"https://gitlab.com/api/v4/projects/${sys.env.getOrElse("GITLAB_PUBLISH_PROJECT_ID", "70289607")}/packages/maven"),
  credentials ++= {
    val projectCredentials = file("../..") / ".secure_files" / ".credentials.gitlab"
    val homeCredentials = Path.userHome / ".sbt" / "gitlab-token"

    if (projectCredentials.exists()) {
      Seq(Credentials(projectCredentials))
    } else if (homeCredentials.exists()) {
      Seq(Credentials(
        "GitLab Packages Registry",
        "gitlab.com",
        "Private-Token",
        Source.fromFile(homeCredentials).getLines().mkString.trim
      ))
    } else {
      sys.env.get("GITLAB_TOKEN").map { token =>
        Credentials("GitLab Packages Registry", "gitlab.com", "Private-Token", token)
      }.toSeq
    }
  },
  licenses := Seq("MIT" -> url("https://opensource.org/licenses/MIT")),
  homepage := Some(url("https://github.com/PasteStack/paste-assetgraph-scala"))
)

// Runtime library - Scala 3
// Core library with no framework dependencies
lazy val runtime = (project in file("runtime"))
  .settings(commonSettings)
  .settings(
    name := "paste-assetgraph-runtime",
    scalaVersion := "3.3.1",
    
    libraryDependencies ++= Seq(
      "io.spray" %% "spray-json" % "1.3.6",
      "org.scalatest" %% "scalatest" % "3.2.17" % Test
    )
  )

// Pekko HTTP integration - optional module
// Add this dependency only if using Pekko HTTP
lazy val pekko = (project in file("pekko"))
  .dependsOn(runtime)
  .settings(commonSettings)
  .settings(
    name := "paste-assetgraph-pekko",
    scalaVersion := "3.3.1",
    // Disable cross-version conflict check: sbtPlugin (2.12) coexists in this
    // build but is an independent project with no dependency relationship.
    update / conflictWarning := ConflictWarning.disable,
    
    libraryDependencies ++= Seq(
      "org.apache.pekko" %% "pekko-http" % "1.0.1",
      "org.apache.pekko" %% "pekko-actor-typed" % "1.0.2",
      "org.apache.pekko" %% "pekko-stream" % "1.0.2",
      "org.scalatest" %% "scalatest" % "3.2.17" % Test
    )
  )

// sbt plugin lives in its own build (sbt-plugin/build.sbt) to avoid
// cross-version conflicts with the Scala 3 projects in this build.

lazy val root = (project in file("."))
  .aggregate(runtime, pekko)
  .settings(
    publish / skip := true
  )
