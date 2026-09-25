# paste-assetgraph Scala Runtime

Scala wrapper around the `paste-assetgraph` native library. All tag, URL and
combo logic lives in the Rust library — this package locates and loads it
through JNA and calls `render`/`combo` in-process. An optional Pekko HTTP
route serves combo responses, and an sbt plugin runs the build.

## Installation

```scala
// Runtime library (Paste wrapper) — select a published build that contains
// the Paste/PastePage API
libraryDependencies += "com.pastestack" %% "paste-assetgraph-runtime" % "<native-runtime-version>"

// Pekko HTTP route (optional)
libraryDependencies += "com.pastestack" %% "paste-assetgraph-pekko" % "<native-runtime-version>"

// sbt plugin (add to project/plugins.sbt)
addSbtPlugin("com.pastestack" % "paste-assetgraph-sbt" % "0.1.0")
```

The runtime jar bundles the shared libraries; `PasteLibrary.resolve()` picks, in
order:

1. `PASTE_ASSETGRAPH_LIB` — must point at a regular file; a value that is
   set but does not name a file fails loudly rather than falling through;
2. the bundled platform library
   (`/native-lib/{platform}/libpaste_assetgraph_ffi.{so,dylib}` or
   `paste_assetgraph_ffi.dll` on Windows), extracted once per JVM to a temp
   directory and verified against `/native-lib/manifest.json` — a checksum
   mismatch fails loudly.

## Usage

```scala
import java.nio.file.Paths
import com.pastestack.assetgraph.Paste
import com.pastestack.assetgraph.pekko.PasteComboRoute

// Construct the wrapper with explicit paths
val paste = Paste(Paste.Settings(
  manifest   = Paths.get("src/main/public/manifest.json"),
  outputBase = Paths.get("src/main/public"),
  // optional overrides:
  // outputJs, outputCss, cdnBase = "", basePath = "/paste", inlineThreshold = 1024
))

// …or load the manifest from the classpath (e.g. produced by the sbt plugin's
// resource generator)
val paste2 = Paste.fromClasspath()   // /paste-assetgraph-manifest.json by default

// Pekko HTTP combo route — answers GETs under paste.basePath
val routes = concat(new PasteComboRoute(paste).route, yourRoutes)
```

## In Twirl Templates

A `PastePage` keeps the ordered jam/dump event list for one page render; each
`dumpCss`/`dumpJs` re-renders the whole list through the library, so modules
already delivered stay out of later URLs. Create one per render — it is not
thread-safe. A dump renders synchronously, so render templates on a blocking
dispatcher, never the server's default one.

```scala
@defining(paste.page()) { page =>
  @page.jam(css = Seq("site.theme"), js = Seq("paste.dom", "paste.event"))
  @Html(page.dumpCss())
  @Html(page.dumpJs())
}
```

`page.jam(css = …, js = …, inline = false, sync = false, media = None)` records a
jam and returns `""` so it can sit in the template as a statement. The dumps emit
the real markup: an inlined runtime once, exact-group loader URLs, inline
`<script>`/`<style>` groups under `inlineThreshold` (1024 bytes; `0` disables
automatic inlining, `inline = true` forces it), `isLoading` marks for pending
modules.

## Combo route

`new PasteComboRoute(paste).route` matches `GET` requests whose full path starts
with `paste.basePath + "/"` and forwards the path plus raw query to
`paste.combo(pathAndQuery)` — a `ComboResponse` computed on the
`default-blocking-io-dispatcher`. Because the match uses the full request path,
the route composes under an enclosing `pathPrefix`: nest it with `basePath` set
to the complete prefix (e.g. `"/app/paste"` under `pathPrefix("app")`). The
library's response passes straight through: its status, `Cache-Control` header,
content type, body — and `location`, which the route writes as the `Location`
header on a 302. A canonical versioned URL (pinned entries, canonical order,
the group's own stamp) serves each `+vN` pin exactly — the current version or
a retained history version — and gets `public, max-age=31536000`, with or
without `?filter=`. A non-canonical versioned URL — a stale stamp, a
non-numeric segment, an unsorted list, an unpinned name — is a `no-cache` 302
to the canonical URL; a stamp newer than the manifest knows, a version it
cannot serve, and unversioned, `require` and 404 responses get `no-cache`.
The route serves compiled output only — no on-the-fly compilation.

## Migrating from the per-call tag API

The previous releases implemented tag/URL logic in Scala. Those APIs are
removed; their behavior now lives in the Rust library and is reached through
`Paste`, `PastePage`, or `paste.combo(pathAndQuery)`:

| Removed API | Replacement |
|---|---|
| `manifest.jamUrl` / `jamJsUrl` / `jamCssUrl` | `page.jam(...)` + `dumpJs()`/`dumpCss()` emit the tags; the URL lives inside them |
| `manifest.jamBootstrap*` | `page.jam(js = …)` + `page.dumpJs()` — the runtime inlines itself once per page |
| `manifest.jamInlineBootstrap*` | same; small groups inline automatically, or pass `inline = true` |
| `manifest.jamScriptTag*` | `page.jam(js = …)` + `page.dumpJs()` (or `sync = true` for a `src` tag) |
| `manifest.jamCssBootstrap*` | `page.jam(css = …)` + `page.dumpCss()` |
| `manifest.page()` / `JamPage` | `paste.page()` returns a `PastePage` (per render, not thread-safe) |
| `PasteComboHandler` | `paste.combo(pathAndQuery)` |
| old `PasteComboRoute` constructors/factories (`withManifest`, path pairs) | `new PasteComboRoute(paste)` |
| `manifest.expandDependencies` / `getDependencies` | the library expands on `?require`/`/require/`; render orders by dependency |
| `manifest.readFile` / `readResource` / `webjarResource` | removed — the library reads compiled output itself |
| `Manifest` itself | removed — the library parses the manifest |

## sbt Integration

The sbt plugin automatically builds assets during compilation:

```scala
// build.sbt
enablePlugins(AssetGraphPlugin)

// Configure source directories. Every entry is walked recursively and
// classified by extension — one directory may hold .js, .scss and .css
// together; the folder layout means nothing to discovery.
pasteAssetGraphSourceDirs := Seq("src/main/assets/js", "src/main/assets/scss")
pasteAssetGraphOutputDir := "target/public"
pasteAssetGraphMinify := true
pasteAssetGraphSourceMaps := true

// Optional: the manifest location (defaults to the managed-resources dir),
// and the storage backend. `local` keeps only the current version of each
// asset; `dist` retains `pasteAssetGraphKeepVersions` versions per asset.
pasteAssetGraphManifestPath := "src/main/resources/paste-assetgraph-manifest.json"
pasteAssetGraphStorageBackend := "dist"

// Optional: URL policy. `pasteAssetGraphHashUrls` (default true) qualifies
// each versioned JAM URL token with the version's full content hash
// (`module+vN.0-h{hash}`); `false` emits the legacy `+vN.0` form. The build
// records the choice in the manifest, so runtime adapters need no setting.
pasteAssetGraphHashUrls := true
```

`pasteAssetGraphBuild` runs as a `Compile / resourceGenerators` task, so the
manifest lands on the classpath where `Paste.fromClasspath()` finds it.

### Output layout

`pasteAssetGraphOutputLayout` selects where compiled files land:

- `"flat"` (default): everything under `pasteAssetGraphOutputDir`; the
  runtime's `outputBase` is that directory.
- `"beside-source"`: each source file compiles into a
  `pasteAssetGraphBuildDirName` directory (default `_paste_build`) beside it —
  `src/main/assets/js/_paste_build/app.<hash>.v1.min.js` — and its manifest
  output is the project-relative path. Files discovered through include paths
  (webjars, sibling projects) are never written into another tree: they keep
  flat placement under `pasteAssetGraphOutputDir`, which must sit inside the
  project. Because every output is project-relative in this layout, the
  runtime's `outputBase` is the project base directory, and a packaged
  deployment ships the `_paste_build` directories at their project-relative
  paths.

```scala
pasteAssetGraphOutputLayout := "beside-source"
pasteAssetGraphBuildDirName := "_paste_build"
// and at runtime:
Paste(Paste.Settings(
  manifest   = Paths.get("src/main/public/manifest.json"),
  outputBase = Paths.get("."),   // the project root, not the output dir
))
```

### Stamps and committed output

Asset stamps are filesystem mtimes — the source file's last-modified time,
clamped by `SOURCE_DATE_EPOCH` when the environment sets it. Nothing in the
pipeline reads version control. Stable combo URLs — and serving old pinned
versions while a rollout mixes builds — therefore depend on the manifest
carrying each version's stamp forward and on the compiled files staying on
disk. Applications that want either guarantee should point
`pasteAssetGraphManifestPath` and `pasteAssetGraphOutputDir` at committed
locations (with the `dist` backend so retained versions survive a rebuild)
and keep both in version control. With uncommitted output a fresh checkout
has no previous manifest: versions restart at v1 and stamps come from the
checkout's file mtimes — URLs are still correct but change per checkout, and
an older URL redirects to the current one because there is no history.
