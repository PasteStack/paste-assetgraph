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
