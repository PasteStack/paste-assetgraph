package com.pastestack.assetgraph.sbt

import sbt._
import sbt.Keys._
import sbt.io.Using
import com.pastestack.assetgraph.Binary
import java.io.File
import java.nio.file.Files
import java.util.zip.CRC32
import scala.collection.JavaConverters._

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
 * pasteAssetGraphIncludePaths := Seq("vendor/assets")
 * pasteAssetGraphMinify := true
 * pasteAssetGraphOutputLayout := "flat"
 * pasteAssetGraphBuildDirName := "_paste_build"
 * pasteAssetGraphCdnUrl := "https://cdn.example"  // records cdn_base in the manifest
 * pasteAssetGraphStorageBackend := "dist"         // the default; "local" keeps only the current version
 * pasteAssetGraphKeepVersions := -1               // keep all; 0 current only; n bounds history
 * }}}
 *
 * Every directory in pasteAssetGraphSourceDirs is walked recursively and each
 * file classified by extension — one directory may hold JS and styles together.
 * With pasteAssetGraphOutputLayout := "beside-source", compiled files land in a
 * <build-dir-name> directory beside their source instead of under the output
 * directory, and every manifest output is relative to the project base, which
 * is then also the runtime's output root.
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
    val pasteAssetGraphOutputLayout = settingKey[String]("Compiled output layout: flat or beside-source")
    val pasteAssetGraphBuildDirName = settingKey[String]("Directory name beside-source output is written into")
    val pasteAssetGraphStorageBackend = settingKey[String]("Storage backend for compiled output: dist (the default) retains history, local keeps only the current version")
    val pasteAssetGraphKeepVersions = settingKey[Int]("Versions the dist backend retains per asset; -1 (the default) keeps every version ever built, 0 keeps the current output only")
    val pasteAssetGraphSourceMaps = settingKey[Boolean]("Generate source maps (not implemented — setting true makes the build warn)")
    val pasteAssetGraphDebug = settingKey[Boolean]("Build in debug mode")
    val pasteAssetGraphHashUrls = settingKey[Boolean]("Emit content-hash-qualified combo URLs (jam.hash-urls); false keeps the legacy +vN.0 token shape")
    val pasteAssetGraphCdnUrl = settingKey[String]("Origin prefix written into the manifest for emitted asset URLs; empty leaves the manifest without one")
    val pasteAssetGraphBuild = taskKey[Unit]("Build assets")
    val pasteAssetGraphWatch = taskKey[Unit]("Watch assets for changes")
    val pasteAssetGraphWebJarPaths = taskKey[Seq[String]]("Auto-discovered webjar include paths")
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
    pasteAssetGraphOutputLayout := "flat",
    pasteAssetGraphBuildDirName := "_paste_build",
    pasteAssetGraphSourceMaps := false,
    pasteAssetGraphDebug := false,
    pasteAssetGraphHashUrls := true,
    pasteAssetGraphCdnUrl := "",
    pasteAssetGraphStorageBackend := "dist",
    pasteAssetGraphKeepVersions := -1,

    pasteAssetGraphWebJarPaths := {
      val cp = (Compile / dependencyClasspath).value
      val extractDir = target.value / "paste-assetgraph-webjars"
      extractWebjars(cp, extractDir, streams.value.log)
    },

    pasteAssetGraphBuild := {
      val log = streams.value.log
      val base = baseDirectory.value
      val debug = pasteAssetGraphDebug.value
      val resourceManagedDir = (Compile / resourceManaged).value

      // Ensure resourceManaged exists
      IO.createDirectory(resourceManagedDir)

      // Merge manual include paths with auto-discovered webjar paths
      val webjarPaths = pasteAssetGraphWebJarPaths.value
      val allIncludes = pasteAssetGraphIncludePaths.value ++ webjarPaths

      // Generate config from settings
      val configFile = target.value / "paste-assetgraph-generated.conf"
      writeGeneratedConfig(
        log,
        base,
        configFile,
        resourceManagedDir,
        pasteAssetGraphManifestPath.value,
        pasteAssetGraphSourceDirs.value,
        pasteAssetGraphOutputDir.value,
        allIncludes,
        pasteAssetGraphMinify.value,
        pasteAssetGraphSourceMaps.value,
        debug,
        pasteAssetGraphStorageBackend.value,
        pasteAssetGraphKeepVersions.value,
        pasteAssetGraphOutputLayout.value,
        pasteAssetGraphBuildDirName.value,
        webjarPaths,
        pasteAssetGraphHashUrls.value,
        pasteAssetGraphCdnUrl.value
      )

      log.info("Building assets with paste-assetgraph...")

      val targetDir = target.value.toPath
      val exitCode = Binary.build(
        targetDir,
        configFile.absolutePath,
        debug,
        Some(base.absolutePath),
        Some(scala.sys.process.ProcessLogger(line => log.info(line), line => log.warn(line)))
      )

      if (exitCode != 0) {
        throw new RuntimeException(s"paste-assetgraph failed with exit code $exitCode")
      }

      log.info("Assets built successfully")
    },

    pasteAssetGraphWatch := {
      val log = streams.value.log
      val base = baseDirectory.value
      val configFile = target.value / "paste-assetgraph-generated.conf"
      val resourceManagedDir = (Compile / resourceManaged).value
      val webjarPaths = pasteAssetGraphWebJarPaths.value
      val allIncludes = pasteAssetGraphIncludePaths.value ++ webjarPaths

      // Always regenerate: settings changed since the last build (layout,
      // source dirs, backend) must reach watch too, not just a missing file.
      writeGeneratedConfig(
        log,
        base,
        configFile,
        resourceManagedDir,
        pasteAssetGraphManifestPath.value,
        pasteAssetGraphSourceDirs.value,
        pasteAssetGraphOutputDir.value,
        allIncludes,
        pasteAssetGraphMinify.value,
        pasteAssetGraphSourceMaps.value,
        pasteAssetGraphDebug.value,
        pasteAssetGraphStorageBackend.value,
        pasteAssetGraphKeepVersions.value,
        pasteAssetGraphOutputLayout.value,
        pasteAssetGraphBuildDirName.value,
        webjarPaths,
        pasteAssetGraphHashUrls.value,
        pasteAssetGraphCdnUrl.value
      )

      log.info("Watching assets for changes...")
      val targetDir = target.value.toPath
      val process = Binary.watch(
        targetDir,
        configFile.absolutePath,
        Some(base.absolutePath),
        Some(scala.sys.process.ProcessLogger(line => log.info(line), line => log.warn(line)))
      )
      try {
        val exitCode = process.exitValue()
        if (exitCode != 0) sys.error(s"paste-assetgraph watch failed with exit code $exitCode")
      } finally process.destroy()
    },

    // Build assets and register the manifest as a managed resource.
    // A resource generator runs whenever resources are needed — `run`,
    // `test`, `package`, `fullClasspath` — not on a bare `compile`; then:
    // 1. Assets are built first (manifest generated)
    // 2. Manifest appears on the classpath (Paste.fromClasspath() works)
    // The managed copy is the one canonical resource wherever the build
    // actually wrote its history JSON.
    Compile / resourceGenerators += Def.task {
      pasteAssetGraphBuild.value
      Seq(materializeManifest(
        baseDirectory.value,
        (Compile / resourceManaged).value,
        pasteAssetGraphManifestPath.value
      ))
    }.taskValue,

    // A configured manifest that lives under unmanaged resources would also be
    // picked up verbatim; the managed copy is the canonical resource, so the
    // on-disk original stays out of the jar.
    Compile / unmanagedResources / excludeFilter := {
      val configured = pasteAssetGraphManifestPath.value
      val existing = (Compile / unmanagedResources / excludeFilter).value
      if (configured.isEmpty) existing
      else {
        val manifestSource = configuredManifest(
          baseDirectory.value,
          (Compile / resourceManaged).value,
          configured
        ).getCanonicalFile
        // The local backend's bincode cache sits beside the manifest; it is
        // build state, never a resource to ship.
        val cacheSource = new File(manifestSource.getPath.stripSuffix(".json") + ".bin")
        existing || new SimpleFileFilter(file =>
          Set(manifestSource, cacheSource).contains(file.getCanonicalFile)
        )
      }
    }
  )

  /** The file the build writes its manifest to: the managed resource name
    * when no path is configured, else the configured path resolved against
    * the project base. */
  private[sbt] def configuredManifest(base: File, managed: File, configured: String): File =
    if (configured.isEmpty) managed / "paste-assetgraph-manifest.json"
    else { val path = new File(configured); if (path.isAbsolute) path else base / configured }

  /** Guarantee the managed manifest resource exists: require the configured
    * source was generated, then copy it under the canonical managed name
    * unless source and target are already the same file — or the copy would
    * be byte-identical, in which case the target's timestamp must not move. */
  private[sbt] def materializeManifest(base: File, managed: File, configured: String): File = {
    val source = configuredManifest(base, managed, configured)
    require(source.isFile, s"Asset manifest was not generated: $source")
    val target = managed / "paste-assetgraph-manifest.json"
    if (source.getCanonicalFile != target.getCanonicalFile) {
      IO.createDirectory(managed)
      val unchanged = target.isFile &&
        java.util.Arrays.equals(Files.readAllBytes(source.toPath), Files.readAllBytes(target.toPath))
      if (!unchanged) {
        IO.copyFile(source, target)
      }
    }
    target
  }

  /** Escape for the generated config's quoted strings — the shared parser
    * decodes them as JSON strings, so the full JSON escape set round-trips
    * where a raw control character would corrupt the line. */
  private def escapeHoconValue(s: String): String =
    s.flatMap {
      case '"'           => "\\\""
      case '\\'          => "\\\\"
      case '\b'          => "\\b"
      case '\f'          => "\\f"
      case '\n'          => "\\n"
      case '\r'          => "\\r"
      case '\t'          => "\\t"
      case c if c < ' '  => f"\\u${c.toInt}%04x"
      case c             => c.toString
    }

  /** Resolve the manifest path, regenerate `configFile` from the current
    * settings and write it — the regeneration is unconditional so a setting
    * changed since the last build (layout, source dirs, backend) reaches
    * watch as well, but the write itself only lands when the bytes differ:
    * an unchanged file keeps its timestamp instead of forcing downstream
    * consumers to see a spurious rebuild. Both pasteAssetGraphBuild and
    * pasteAssetGraphWatch go through here so the two paths cannot drift. */
  private[sbt] def writeGeneratedConfig(
    log: Logger,
    base: File,
    configFile: File,
    resourceManagedDir: File,
    configuredManifestPath: String,
    sourceDirs: Seq[String],
    outputDir: String,
    allIncludes: Seq[String],
    minify: Boolean,
    sourceMaps: Boolean,
    debug: Boolean,
    storageBackend: String,
    keepVersions: Int,
    outputLayout: String,
    buildDirName: String,
    webjarPaths: Seq[String],
    hashUrls: Boolean = true,
    cdnUrl: String = ""
  ): Unit = {
    // Manifest goes to resourceManaged (sbt-web style)
    val manifestPath = if (configuredManifestPath.nonEmpty) {
      configuredManifestPath
    } else {
      (resourceManagedDir / "paste-assetgraph-manifest.json").relativeTo(base).map(_.getPath).getOrElse {
        log.warn(
          s"manifest path ${resourceManagedDir.getPath}/paste-assetgraph-manifest.json is absolute; " +
            "it makes the generated config machine-specific"
        )
        resourceManagedDir.getPath + "/paste-assetgraph-manifest.json"
      }
    }

    // Log webjar paths for debugging
    if (webjarPaths.nonEmpty) {
      log.info(s"Auto-discovered webjar paths: ${webjarPaths.mkString(", ")}")
    }

    val config = generateConfig(
      sourceDirs,
      outputDir,
      manifestPath,
      allIncludes,
      minify,
      sourceMaps,
      debug,
      storageBackend,
      keepVersions,
      outputLayout,
      buildDirName,
      hashUrls,
      cdnUrl
    )
    IO.createDirectory(configFile.getParentFile)
    if (!configFile.isFile || IO.read(configFile) != config) {
      IO.write(configFile, config)
    }
  }

  private[sbt] def generateConfig(
    sourceDirs: Seq[String],
    outputDir: String,
    manifestPath: String,
    includePaths: Seq[String],
    minify: Boolean,
    sourceMaps: Boolean,
    debugMode: Boolean,
    storageBackend: String,
    keepVersions: Int,
    outputLayout: String,
    buildDirName: String,
    hashUrls: Boolean = true,
    cdnUrl: String = ""
  ): String = {
    // -1 is the keep-all sentinel, spelled `all` in the generated config —
    // negative numbers are invalid there, so the boundary maps it rather
    // than serializing a policy the parser would reject.
    require(
      keepVersions >= -1,
      s"pasteAssetGraphKeepVersions must be -1 (keep all) or non-negative, got $keepVersions"
    )
    // Flat key format for Rust parser. Every configured source dir goes in
    // verbatim and in order — each is walked recursively and classified by
    // extension, so a dir like src/main/web that holds JS and styles
    // together is not silently dropped.
    val escapedSourceDirs = sourceDirs.map(s => s""""${escapeHoconValue(s)}"""").mkString(", ")
    val escapedOutputDir = escapeHoconValue(outputDir)
    val escapedManifestPath = escapeHoconValue(manifestPath)
    val escapedIncludes = includePaths.map(s => s""""${escapeHoconValue(s)}"""").mkString(", ")

    s"""# Auto-generated by sbt AssetGraphPlugin
# Do not edit - configure in build.sbt instead

source.dirs = [$escapedSourceDirs]
output.base = "$escapedOutputDir"
output.js = "$escapedOutputDir/js"
output.css = "$escapedOutputDir/css"
output.images = "$escapedOutputDir/images"
output.layout = "${escapeHoconValue(outputLayout)}"
output.build-dir-name = "${escapeHoconValue(buildDirName)}"
manifest = "$escapedManifestPath"
include-paths = [$escapedIncludes]
debug-mode = $debugMode
minify = $minify
${if (sourceMaps) "source-maps = true\n" else ""}storage.backend = "${escapeHoconValue(storageBackend)}"
storage.keep-versions = ${if (keepVersions == -1) "all" else keepVersions.toString}
jam.hash-urls = $hashUrls
${if (cdnUrl.nonEmpty) s"""cdn.enabled = true
cdn.url = "${escapeHoconValue(cdnUrl)}"
""" else ""}"""
  }

  private[sbt] def validatedWebjarPaths(extractDir: File, relativePath: String): (File, File) = {
    val parts = relativePath.split("/", -1).toSeq
    val safeSegments = parts.size >= 3 && parts.forall { part =>
      part.nonEmpty && part != "." && part != ".." &&
        !part.exists(character => character == '\\' || character == ':' || Character.isISOControl(character))
    }
    require(safeSegments, "Unsafe WebJar archive entry")
    // A jar-shipped `.extracted` at the group root would overwrite the
    // freshness marker and make an incomplete extraction look complete.
    require(parts.drop(2) != Seq(".extracted"), "Unsafe WebJar archive entry")

    val root = extractDir.getCanonicalFile.toPath
    val directory = root.resolve(parts(0)).resolve(parts(1))
    val file = parts.drop(2).foldLeft(directory)((path, part) => path.resolve(part))
    val canonicalDirectory = directory.toFile.getCanonicalFile.toPath
    val canonicalFile = file.toFile.getCanonicalFile.toPath
    require(
      canonicalDirectory == directory && canonicalFile == file &&
        canonicalDirectory.startsWith(root) && canonicalDirectory != root &&
        canonicalFile.startsWith(canonicalDirectory) && canonicalFile != canonicalDirectory,
      "Unsafe WebJar archive entry"
    )
    (canonicalDirectory.toFile, canonicalFile.toFile)
  }

  /** A single webjar member resolved against the extraction root: the jar it
    * came from, its archive name, the artifact-version directory it belongs
    * to, and its validated target file with the archive-recorded size and
    * checksum used for freshness validation. */
  private final case class WebjarEntry(
    jar: File,
    name: String,
    directory: File,
    file: File,
    size: Long,
    crc: Long
  )

  /**
   * Extract webjars from classpath JARs to target directory.
   * Returns the extracted artifact-version directories in classpath order —
   * the first root on the classpath wins, so include-path precedence matches
   * dependency order. Any unreadable archive or extraction failure aborts the
   * build; a dependency is never silently omitted.
   */
  private[sbt] def extractWebjars(classpath: Classpath, extractDir: File, log: Logger): Seq[String] = {
    val jars = classpath.toList
      .map(_.data)
      .filter(jar => jar.isFile && jar.getName.endsWith(".jar"))

    // Describe every member up front, closing each archive before any write —
    // a jar unreadable at scan time fails before directories are deleted.
    val entries = jars.flatMap(jar => webjarEntries(jar, extractDir))

    // Group by artifact-version directory in first-seen order. When several
    // jars contribute to the same directory the first jar's entry wins for a
    // shared path, matching classpath precedence.
    val byDirectory = entries.groupBy(_.directory)
    val groups = entries.map(_.directory).distinct.map { directory =>
      val expected = byDirectory(directory)
        .foldLeft((Vector.empty[WebjarEntry], Set.empty[File])) {
          case ((unique, seen), entry) =>
            if (seen(entry.file)) (unique, seen)
            else (unique :+ entry, seen + entry.file)
        }
        ._1
      directory -> expected
    }

    groups.map { case (directory, expected) =>
      if (!webjarGroupIsFresh(directory, expected)) {
        extractWebjarGroup(directory, expected, log)
      }
      directory.getCanonicalPath
    }
  }

  /** Read one jar's webjar members into descriptors without writing anything. */
  private def webjarEntries(jar: File, extractDir: File): List[WebjarEntry] = {
    val webjarPrefix = "META-INF/resources/webjars/"
    Using.zipFile(jar) { zipFile =>
      // Collect: META-INF/resources/webjars/{artifactId}/{version}/{path}
      // entries, requiring artifactId, version, AND a non-empty file path.
      zipFile.entries().asScala.toList.flatMap { zipEntry =>
        val name = zipEntry.getName
        if (name.startsWith(webjarPrefix) && !zipEntry.isDirectory) {
          val relativePath = name.substring(webjarPrefix.length)
          val parts = relativePath.split("/", 3)
          if (parts.length >= 3 && parts(2).nonEmpty) {
            val (directory, file) = validatedWebjarPaths(extractDir, relativePath)
            List(WebjarEntry(jar, name, directory, file, zipEntry.getSize, zipEntry.getCrc))
          } else List.empty
        } else List.empty
      }
    }
  }

  /** A group is fresh only when its marker exists and the regular files on
    * disk are exactly the expected set, each still matching the archive's
    * size and CRC32 — content, not timestamps, decides staleness. */
  private def webjarGroupIsFresh(directory: File, expected: Vector[WebjarEntry]): Boolean =
    directory.isDirectory &&
      new File(directory, ".extracted").isFile &&
      regularFiles(directory) == expected.map(_.file).toSet &&
      expected.forall(entry => fileMatchesArchive(entry.file.toPath, entry.size, entry.crc))

  /** Regular files under a directory, canonicalized; the extraction marker
    * at the group root is not part of the content set. */
  private def regularFiles(directory: File): Set[File] = {
    val stream = Files.walk(directory.toPath)
    try {
      stream.iterator().asScala
        .filter(Files.isRegularFile(_))
        .map(_.toFile.getCanonicalFile)
        .toSet - new File(directory, ".extracted").getCanonicalFile
    } finally stream.close()
  }

  private def fileMatchesArchive(path: java.nio.file.Path, size: Long, crc: Long): Boolean =
    Files.isRegularFile(path) && Files.size(path) == size && crcOf(path) == crc

  private def crcOf(path: java.nio.file.Path): Long = {
    val checksum = new CRC32
    scala.util.Using.resource(Files.newInputStream(path)) { in =>
      val buffer = new Array[Byte](64 * 1024)
      var read = in.read(buffer)
      while (read >= 0) {
        checksum.update(buffer, 0, read)
        read = in.read(buffer)
      }
    }
    checksum.getValue
  }

  /** Replace a stale group: the validated directory is deleted once, every
    * descriptor's jar is reopened and the entry streamed to its validated
    * target, and the marker is touched only after all writes succeed — a
    * group that fails mid-extraction is never marked complete. */
  private def extractWebjarGroup(directory: File, expected: Vector[WebjarEntry], log: Logger): Unit = {
    IO.delete(directory)
    expected.map(_.jar).distinct.foreach { jar =>
      Using.zipFile(jar) { zipFile =>
        expected.filter(_.jar == jar).foreach { entry =>
          val zipEntry = zipFile.getEntry(entry.name)
          require(zipEntry != null, s"WebJar entry disappeared from ${jar.getName}: ${entry.name}")
          IO.createDirectory(entry.file.getParentFile)
          Using.zipEntry(zipFile)(zipEntry) { in =>
            IO.transfer(in, entry.file)
          }
          // The descriptor was recorded at scan time; if the jar changed
          // under us the extracted bytes no longer match its checksum.
          require(
            fileMatchesArchive(entry.file.toPath, entry.size, entry.crc),
            s"WebJar entry changed while extracting: ${entry.name}"
          )
          log.debug(s"Extracted webjar: ${entry.name} -> ${entry.file}")
        }
      }
    }
    IO.touch(new File(directory, ".extracted"))
  }

  /**
   * Convenience helpers for common paste setup.
   */
  def withPaste(version: String = "2.1.0"): Seq[Setting[_]] = Seq(
    libraryDependencies += "com.pastestack" % "paste-webjar" % version
  )

  def withPasteElements(version: String = "0.4.0"): Seq[Setting[_]] = Seq(
    libraryDependencies += "com.pastestack" % "paste-elements-webjar" % version
  )

  def withPasteStack(pasteVersion: String = "2.1.0", elementsVersion: String = "0.4.0"): Seq[Setting[_]] =
    withPaste(pasteVersion) ++ withPasteElements(elementsVersion)
}
