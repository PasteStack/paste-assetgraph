import scala.io.Source

organization := "com.pastestack"
name := "paste-assetgraph-sbt"
scalaVersion := "2.12.18"
publishMavenStyle := true
licenses := Seq("MIT" -> url("https://opensource.org/licenses/MIT"))
homepage := Some(url("https://github.com/PasteStack/paste-assetgraph-scala"))

ThisBuild / versionScheme := Some("semver-spec")
version := {
  val base = "0.1.0"
  if (sys.env.contains("CI_COMMIT_TAG")) base
  else s"$base-SNAPSHOT"
}

enablePlugins(SbtPlugin)
pluginCrossBuild / sbtVersion := "1.9.0"

// --- GitLab Maven Registry → paste-registry (project 70289607) ---
publishTo := Some("gitlab-maven" at
  s"https://gitlab.com/api/v4/projects/${sys.env.getOrElse("GITLAB_PUBLISH_PROJECT_ID", "70289607")}/packages/maven")

credentials ++= {
  // .secure_files/ lives at the repo root, 3 levels up from runtime/scala/sbt-plugin/
  val projectCredentials = file("../../..") / ".secure_files" / ".credentials.gitlab"
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
}
