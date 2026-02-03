lazy val root = (project in file("."))
  .enablePlugins(SbtPlugin)
  .settings(
    name := "paste-assetgraph-scala",
    organization := "com.pastestack",
    version := "0.1.0",
    scalaVersion := "2.12.18",  // sbt plugins need 2.12
    
    // Also publish for 2.13/3.x as a library (not plugin)
    crossScalaVersions := Seq("2.12.18", "2.13.12", "3.3.1"),
    
    libraryDependencies ++= Seq(
      "com.typesafe.akka" %% "akka-http" % "10.5.3" % Optional,
      "com.typesafe.akka" %% "akka-stream" % "2.8.5" % Optional,
      "io.spray" %% "spray-json" % "1.3.6",
      
      "org.scalatest" %% "scalatest" % "3.2.17" % Test
    ),
    
    // Publish settings
    publishMavenStyle := true,
    licenses := Seq("MIT" -> url("https://opensource.org/licenses/MIT")),
    homepage := Some(url("https://github.com/PasteStack/paste-assetgraph-scala")),
    
    // sbt plugin settings
    pluginCrossBuild / sbtVersion := "1.9.0"
  )
