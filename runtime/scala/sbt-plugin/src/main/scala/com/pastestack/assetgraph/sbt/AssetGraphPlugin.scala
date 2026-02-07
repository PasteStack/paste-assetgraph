package com.pastestack.assetgraph.sbt

import sbt._
import sbt.Keys._
import com.pastestack.assetgraph.Binary

/**
 * sbt plugin for paste-assetgraph.
 * 
 * For Scala projects, configure via sbt settings (no external config file needed).
 * The plugin generates a temp config and passes it to the Rust binary.
 * 
 * Usage in build.sbt:
 * {{{
 * enablePlugins(AssetGraphPlugin)
 * 
 * // Required settings (all prefixed with paste to avoid collisions)
 * pasteAssetGraphSourceDirs := Seq("src/main/assets/scss", "src/main/assets/js")
 * pasteAssetGraphOutputDir := "target/public"
 * pasteAssetGraphManifestPath := "target/paste-assetgraph-manifest.json"
 * 
 * // Optional settings
 * pasteAssetGraphIncludePaths := Seq("../paste-elements")
 * pasteAssetGraphMinify := true
 * pasteAssetGraphSourceMaps := true
 * }}}
 * 
 * For Python/Node/HTML projects, use external config file instead.
 */
object AssetGraphPlugin extends AutoPlugin {
  
  object autoImport {
    // Core settings - folder-based auto-discovery (old paste style)
    val pasteAssetGraphSourceDirs = settingKey[Seq[String]]("Source directories to scan for assets")
    val pasteAssetGraphOutputDir = settingKey[String]("Output directory for compiled assets")
    val pasteAssetGraphManifestPath = settingKey[String]("Path for generated manifest")
    val pasteAssetGraphIncludePaths = settingKey[Seq[String]]("Include paths for SCSS @import")
    
    // Build settings
    val pasteAssetGraphMinify = settingKey[Boolean]("Minify output")
    val pasteAssetGraphSourceMaps = settingKey[Boolean]("Generate source maps")
    val pasteAssetGraphDebug = settingKey[Boolean]("Build in debug mode")
    val pasteAssetGraphBuild = taskKey[Unit]("Build assets")
    val pasteAssetGraphWatch = taskKey[Unit]("Watch assets for changes")
  }
  
  import autoImport._
  
  override def trigger = noTrigger
  
  override def projectSettings: Seq[Setting[_]] = Seq(
    // Defaults - folder-based auto-discovery
    pasteAssetGraphSourceDirs := Seq("src/main/assets/scss", "src/main/assets/js"),
    pasteAssetGraphOutputDir := "target/public",
    pasteAssetGraphManifestPath := "", // Will be set from resourceManaged
    pasteAssetGraphIncludePaths := Seq.empty,
    pasteAssetGraphMinify := true,
    pasteAssetGraphSourceMaps := true,
    pasteAssetGraphDebug := false,
    
    pasteAssetGraphBuild := {
      val log = streams.value.log
      val base = baseDirectory.value
      val debug = pasteAssetGraphDebug.value
      val resourceManagedDir = (Compile / resourceManaged).value
      
      // Manifest goes to resourceManaged (sbt-web style)
      val manifestPath = if (pasteAssetGraphManifestPath.value.nonEmpty) {
        pasteAssetGraphManifestPath.value
      } else {
        (resourceManagedDir / "paste-assetgraph-manifest.json").relativeTo(base).map(_.getPath).getOrElse(
          resourceManagedDir.getPath + "/paste-assetgraph-manifest.json"
        )
      }
      
      // Ensure resourceManaged exists
      IO.createDirectory(resourceManagedDir)
      
      // Generate config from settings
      val configFile = base / "target" / "paste-assetgraph-generated.conf"
      val config = generateConfig(
        pasteAssetGraphSourceDirs.value,
        pasteAssetGraphOutputDir.value,
        manifestPath,
        pasteAssetGraphIncludePaths.value,
        pasteAssetGraphMinify.value,
        pasteAssetGraphSourceMaps.value,
        debug
      )
      
      // Write config
      IO.createDirectory(configFile.getParentFile)
      IO.write(configFile, config)
      
      log.info("Building assets with paste-assetgraph...")
      
      val targetDir = (base / "target").toPath
      val exitCode = Binary.build(targetDir, configFile.absolutePath, debug, Some(base.absolutePath))
      
      if (exitCode != 0) {
        throw new RuntimeException(s"paste-assetgraph failed with exit code $exitCode")
      }
      
      log.info("Assets built successfully")
    },
    
    pasteAssetGraphWatch := {
      val log = streams.value.log
      val base = baseDirectory.value
      val configFile = base / "target" / "paste-assetgraph-generated.conf"
      
      // Ensure config exists before watching (generate if missing)
      if (!configFile.exists()) {
        log.info("Generating config file before watch...")
        val resourceManagedDir = (Compile / resourceManaged).value
        val manifestPath = if (pasteAssetGraphManifestPath.value.nonEmpty) {
          pasteAssetGraphManifestPath.value
        } else {
          (resourceManagedDir / "paste-assetgraph-manifest.json").relativeTo(base).map(_.getPath).getOrElse(
            resourceManagedDir.getPath + "/paste-assetgraph-manifest.json"
          )
        }
        val config = generateConfig(
          pasteAssetGraphSourceDirs.value,
          pasteAssetGraphOutputDir.value,
          manifestPath,
          pasteAssetGraphIncludePaths.value,
          pasteAssetGraphMinify.value,
          pasteAssetGraphSourceMaps.value,
          pasteAssetGraphDebug.value
        )
        IO.createDirectory(configFile.getParentFile)
        IO.write(configFile, config)
      }
      
      log.info("Watching assets for changes...")
      val targetDir = (base / "target").toPath
      Binary.watch(targetDir, configFile.absolutePath, Some(base.absolutePath))
    },
    
    // Build assets and register manifest as a managed resource.
    // resourceGenerators runs before compile, ensuring:
    // 1. Assets are built first (manifest generated)
    // 2. Manifest appears on the classpath (Manifest.loadFromClasspath() works)
    Compile / resourceGenerators += Def.task {
      pasteAssetGraphBuild.value
      val resourceManagedDir = (Compile / resourceManaged).value
      val manifestFile = resourceManagedDir / "paste-assetgraph-manifest.json"
      if (manifestFile.exists()) Seq(manifestFile) else Seq.empty
    }.taskValue
  )
  
  private def escapeHoconValue(s: String): String =
    s.replace("\\", "\\\\").replace("\"", "\\\"")
  
  private def generateConfig(
    sourceDirs: Seq[String],
    outputDir: String,
    manifestPath: String,
    includePaths: Seq[String],
    minify: Boolean,
    sourceMaps: Boolean,
    debugMode: Boolean
  ): String = {
    // Flat key format for Rust parser
    val scssDir = escapeHoconValue(sourceDirs.find(_.contains("scss")).getOrElse("src/main/assets/scss"))
    val jsDir = escapeHoconValue(sourceDirs.find(_.contains("js")).getOrElse("src/main/assets/js"))
    val escapedOutputDir = escapeHoconValue(outputDir)
    val escapedManifestPath = escapeHoconValue(manifestPath)
    val escapedIncludes = includePaths.map(s => s""""${escapeHoconValue(s)}"""").mkString(", ")
    
    s"""# Auto-generated by sbt AssetGraphPlugin
# Do not edit - configure in build.sbt instead

source.scss = "$scssDir"
source.js = "$jsDir"
source.images = "src/main/assets/images"
output.base = "$escapedOutputDir"
output.js = "$escapedOutputDir/js"
output.css = "$escapedOutputDir/css"
output.images = "$escapedOutputDir/images"
manifest = "$escapedManifestPath"
include-paths = [$escapedIncludes]
debug-mode = $debugMode
minify = $minify
source-maps = $sourceMaps
jam.enabled = true
jam.base-path = "/paste"
"""
  }
}
