package com.pastestack.assetgraph

import java.io.{File, FileOutputStream}
import java.net.URL
import java.nio.file.{Files, Path, Paths}
import scala.sys.process._
import scala.util.{Failure, Success, Try}

/**
 * Manages the paste-assetgraph Rust binary.
 * 
 * Downloads the binary automatically if not found.
 */
object Binary {
  
  val Version = "0.1.0"
  val BinaryName = "paste-assetgraph"
  
  private val GithubReleasesUrl = s"https://github.com/PasteStack/paste-assetgraph/releases/download/v$Version"
  
  /**
   * Get path to the binary, downloading if necessary.
   */
  def path: Path = {
    // Check if already in PATH
    findInPath().getOrElse {
      // Download to ~/.pastestack/bin/
      val localBin = localBinaryPath
      if (!Files.exists(localBin)) {
        download(localBin)
      }
      localBin
    }
  }
  
  /**
   * Run the binary with arguments.
   */
  def run(args: String*): Int = {
    val binary = path.toString
    Process(binary +: args).!
  }
  
  /**
   * Run build command.
   */
  def build(configPath: String, debug: Boolean = false): Int = {
    val args = Seq("build", "--config", configPath) ++ (if (debug) Seq("--debug") else Seq.empty)
    run(args: _*)
  }
  
  /**
   * Run watch command (non-blocking).
   */
  def watch(configPath: String): Process = {
    val binary = path.toString
    Process(Seq(binary, "watch", "--config", configPath)).run()
  }
  
  private def findInPath(): Option[Path] = {
    Try {
      val result = Seq("which", BinaryName).!!.trim
      if (result.nonEmpty) Some(Paths.get(result)) else None
    }.getOrElse(None)
  }
  
  private def localBinaryPath: Path = {
    val home = System.getProperty("user.home")
    Paths.get(home, ".pastestack", "bin", BinaryName)
  }
  
  private def download(target: Path): Unit = {
    val os = System.getProperty("os.name").toLowerCase
    val arch = System.getProperty("os.arch").toLowerCase
    
    val platform = (os, arch) match {
      case (o, a) if o.contains("mac") && a.contains("aarch64") => "darwin-arm64"
      case (o, _) if o.contains("mac") => "darwin-amd64"
      case (o, a) if o.contains("linux") && a.contains("aarch64") => "linux-arm64"
      case (o, _) if o.contains("linux") => "linux-amd64"
      case (o, _) if o.contains("win") => "windows-amd64.exe"
      case _ => throw new RuntimeException(s"Unsupported platform: $os $arch")
    }
    
    val url = s"$GithubReleasesUrl/paste-assetgraph-$platform"
    
    println(s"Downloading paste-assetgraph from $url...")
    
    // Create parent directories
    Files.createDirectories(target.getParent)
    
    // Download
    val connection = new URL(url).openConnection()
    val in = connection.getInputStream
    val out = new FileOutputStream(target.toFile)
    
    try {
      val buffer = new Array[Byte](8192)
      var read = 0
      while ({ read = in.read(buffer); read != -1 }) {
        out.write(buffer, 0, read)
      }
    } finally {
      in.close()
      out.close()
    }
    
    // Make executable
    target.toFile.setExecutable(true)
    
    println(s"Downloaded to ${target.toAbsolutePath}")
  }
}
