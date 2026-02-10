organization := "com.pastestack"
name := "paste-assetgraph-sbt"
version := "0.1.0"
scalaVersion := "2.12.18"
publishMavenStyle := true
licenses := Seq("MIT" -> url("https://opensource.org/licenses/MIT"))
homepage := Some(url("https://github.com/PasteStack/paste-assetgraph-scala"))

enablePlugins(SbtPlugin)
pluginCrossBuild / sbtVersion := "1.9.0"

// --- GitLab Maven Registry → paste-registry (project 70289607) ---
publishTo := Some("gitlab-maven" at
  s"https://gitlab.com/api/v4/projects/${sys.env.getOrElse("GITLAB_PUBLISH_PROJECT_ID", "70289607")}/packages/maven")

credentials ++= {
  sys.env.get("CI_JOB_TOKEN").map { token =>
    Credentials("GitLab Packages Registry", "gitlab.com", "Job-Token", token)
  }.toSeq
}
