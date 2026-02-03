package com.pastestack.assetgraph.sbt

import sbt._
import sbt.Keys._
import com.pastestack.assetgraph.Binary

/**
 * sbt plugin for paste-assetgraph.
 * 
 * Automatically downloads the Rust binary and runs it before compile.
 * 
 * Usage in build.sbt:
 * {{{
 * enablePlugins(AssetGraphPlugin)
 * 
 * // Optional: customize config path
 * assetGraphConfig := baseDirectory.value / "config" / "paste.conf"
 * }}}
 */
object AssetGraphPlugin extends AutoPlugin {
  
  object autoImport {
    val assetGraphConfig = settingKey[File]("Path to paste-surface.conf")
    val assetGraphDebug = settingKey[Boolean]("Build in debug mode")
    val assetGraphBuild = taskKey[Unit]("Build assets")
    val assetGraphWatch = taskKey[Unit]("Watch assets for changes")
  }
  
  import autoImport._
  
  override def trigger = noTrigger
  
  override def projectSettings: Seq[Setting[_]] = Seq(
    assetGraphConfig := baseDirectory.value / "src" / "main" / "resources" / "paste-surface.conf",
    assetGraphDebug := false,
    
    assetGraphBuild := {
      val log = streams.value.log
      val config = assetGraphConfig.value
      val debug = assetGraphDebug.value
      
      if (!config.exists()) {
        log.warn(s"Config not found: $config")
        log.warn("Run 'paste-assetgraph init -t scala' to create one")
      } else {
        log.info("Building assets with paste-assetgraph...")
        
        val exitCode = Binary.build(config.absolutePath, debug)
        
        if (exitCode != 0) {
          throw new RuntimeException(s"paste-assetgraph failed with exit code $exitCode")
        }
        
        log.info("Assets built successfully")
      }
    },
    
    assetGraphWatch := {
      val log = streams.value.log
      val config = assetGraphConfig.value
      
      log.info("Watching assets for changes...")
      Binary.watch(config.absolutePath)
    },
    
    // Run asset build before compile (if config exists)
    Compile / compile := {
      val config = assetGraphConfig.value
      if (config.exists()) {
        assetGraphBuild.value
      }
      (Compile / compile).value
    }
  )
}
