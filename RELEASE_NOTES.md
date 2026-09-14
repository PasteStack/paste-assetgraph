# Unreleased

## Fixed

- Scanning an include path no longer walks another project's build output.
  `target/`, `dist/`, `node_modules/` and `.git/` are skipped below the walk root,
  so compiled copies of a module stop entering the graph as extra modules named
  after their output paths. A walk root the caller names explicitly is still
  scanned, so an include path may point at a built distribution.

## Added

- Cross-compiled binaries for the five supported platforms, published with a
  `SHA256SUMS` file whose names match the downloaded assets.
- A cross-platform determinism check: the Linux build publishes reference bytes
  and the Windows build rebuilds and compares them, before anything publishes.
- Webjar module resolution in the sbt plugin, combo handler and Pekko route.
- The sbt plugin extracts its bundled binary into a directory keyed by the plugin
  version and verifies the packaged checksum before running it.
- The JVM libraries and the sbt plugin publish the rolling
  `<VERSION>-develop-latest` coordinate alongside the immutable
  `<VERSION>-develop-<sha>`, so a consumer can track develop by a version that
  keeps existing. The house recipe publishes only the immutable one.

## Changed

- The Rust sources are rustfmt-formatted, `rustfmt.toml` pins the style edition so
  every toolchain agrees, and the pipeline checks formatting before it builds.
- The macOS SDK the Darwin cross-builds link against is pinned by SHA-256, not by
  release tag alone.
- Branch and tag pipelines carry the build; merge request pipelines no longer
  duplicate a five-target cross-compile.
- Manifest source labels are recorded relative to the project, so a manifest
  describes the project rather than the machine that built it.
- The default site module prefix is `site`; it was a specific consumer's prefix.

## Licensing

- Licensed under the Apache License, Version 2.0 (`LICENSE`, `NOTICE.md`). The
  Cargo and sbt package metadata previously named a different license.

# paste-assetgraph v0.1.0

**Date:** 2026-02-07

## Initial Release

### Rust Pipeline
- SCSS compilation with include paths
- JavaScript bundling and dependency resolution
- Asset versioning with content hashing
- Manifest generation for runtime asset resolution
- JAM combo URL support
- Pluggable storage backends: local, dist (future: S3)
- HOCON configuration support

### Scala Runtime
- Migrated Akka to Apache Pekko
- Restructured into `pekko/`, `sbt-plugin/`, `runtime/`
- `Manifest.scala` with JAM URL generation and bootstrap
- `PasteComboHandler.scala` for combo URL serving
- `AssetGraphPlugin.scala` sbt plugin for build integration
- `ManifestSpec.scala` test suite

### Documentation
- `ARCHITECTURE.md` — storage backends, versioning, rolling deploys
- `JAM_SPEC.md` — combo URL format, bootstrap, module system (v1.1)
- `ROADMAP.md` — deferred items from code review
- BDD feature files for JAM URLs, bootstrap, serving, module extraction

### Known Issues
- Server manifest caching not yet implemented (see ROADMAP.md)
