# paste-assetgraph v0.4.2

**Date:** 2026-10-28

## Changed

- Apache License 2.0 applied consistently: verbatim LICENSE text, NOTICE rename, Apache short header in every first-party source file, corrected license metadata.

---

# paste-assetgraph v0.4.1

**Date:** 2026-10-22

## Fixed

- Version metadata: `Cargo.toml`, `Cargo.lock` and `runtime/node`'s
  package files now carry the release version, satisfying the
  version-composition check that requires every manifest to agree with
  `VERSION`.

# paste-assetgraph v0.4.0

**Date:** 2026-10-22

## Added

- Versioned output contract: the build emits versioned, hash-addressed
  files plus a manifest that records current outputs and retained history,
  so pinned URLs stay resolvable across rebuilds and rollbacks.
- `dist` is the default storage backend with retained history recorded
  per asset; the `local` backend remains an explicit opt-in.
- Licence collection and verification tooling: npm package and wheel
  legal checks plus the extended third-party licence script audit
  packaged artifacts against collected notice texts.

## Changed

- Combo URLs resolve their suffix from the URL alone — exactly one
  trailing `.min.{ext}` or `.{ext}` is stripped without consulting the
  manifest, since `.min` can never be part of a module name.

## Fixed

- Conditional (`304`) responses preserve the representation's content
  type instead of reporting a generic one.
- Serving, watching and packaging paths hardened across the pipeline;
  the pekko combo route applies the same resolution rules.

# paste-assetgraph v0.3.1

**Date:** 2026-09-25

## Fixed

- The Python version test composes its expectation from the repository
  `VERSION` file instead of a literal, so a version bump no longer fails the
  source-tree composition check.

# paste-assetgraph v0.3.0

**Date:** 2026-09-25

## Changed — breaking

- Asset discovery, dependency ordering, versioned filenames, tag rendering
  and combo resolution now live in the Rust core. The Scala/Pekko, Node and
  Python packages are thin in-process adapters that return rendered markup
  and response descriptions; the embedded HTTP server and its `serve`
  command are removed. Host frameworks own HTTP.
- Page APIs: one scoped dump replaces the per-type methods —
  `dump(AssetType.Css)` in Scala, `dump('css')` in Node and `dump("css")` in
  Python, plus a no-argument `dump()` that emits the css markup then the js.
  Node also exposes `dumpAsync(type)`. `dumpCss`, `dumpJs`, `dumpCssAsync`,
  `dumpJsAsync`, `dump_css` and `dump_js` are removed.
- Versioned asset URLs carry the content hash by default —
  `/paste/<stamp>/module+v1.0-h<hash>.min.js`. Set `jam.hash-urls = false`
  or `pasteAssetGraphHashUrls := false` for the previous `module+v1.0`
  format. The manifest carries the policy so every adapter agrees, and
  toggling it preserves versions, hashes and history.
- Stylesheet sources without `@module` take a path-derived module name like
  JS sources and join the dependency ordering. A `@module` declaration still
  wins; a colliding or invalid derived name warns and the file compiles as
  before.
- The `source.images` key is removed. Configurations that still contain it
  load normally in both HOCON and JSON.
- `withPaste`, `withPasteElements` and `withPasteStack` default to the
  current released webjars (2.1.0 and 0.4.0).

## Added

- CLI: `render` replays an event list from stdin and prints the last dump's
  markup, `combo` resolves a combo URL to its response as JSON, `build`
  gained `--react` and `--cwd`, and `init` scaffolds a configuration.
- In-process adapters for Scala, Pekko, Node and Python over the bundled
  native library; the Scala adapter exposes the public `AssetType`.
- Static template rendering: `<!-- paste:jam ... -->` and
  `<!-- paste:dump [css|js] -->` comments under `templates.source` render
  into `templates.output`, and the combo responses and loader files the
  emitted tags reference are written beside them. A bare
  `<!-- paste:dump -->` emits both types.
- Page finish: `finish()` in Scala and Python, and `finish()` /
  `finishAsync()` in Node, fail a render naming any jammed module that never
  reached a dump of its type. A failed finish leaves the page open; success
  seals it and repeats are inert.
- Configurable discovery: multiple source roots, recursive scanning that
  skips generated output, and flat or beside-source output layouts.
- Retained-output integrity: stored files are byte-verified against the
  manifest and repaired or rebuilt when tampered; version history survives
  layout changes and source retirement.
- Manifest caches validate JSON content and binary schema before use; a
  corrupt manifest or unreadable source fails explicitly.
- WebJar extraction verifies each entry against its archive checksum. The
  bundled native binary is extracted per plugin version, and when
  replacement fails on Windows a checksum-verified existing binary is used.
- Saving a manifest refuses cross-type output prefixes (a js asset under
  `/css/` or the reverse).

## Fixed

- Include-path sources without `@module` warn when their name falls back to
  the path.
- An absolute manifest path in the generated sbt configuration warns.
- Array configuration values containing `=` are covered by test.
- Local-storage writes skip unchanged files, matching the committed-output
  backend.
# paste-assetgraph v0.2.0

**Date:** 2026-09-22

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

- Licensed under the Apache License, Version 2.0 (`LICENSE`, `NOTICE`). The
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
