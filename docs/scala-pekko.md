# Using paste-assetgraph with Scala/Pekko HTTP

The Scala runtime is a thin loader: all tag, URL and combo logic lives in the
`paste-assetgraph` Rust library. `Paste` loads it through JNA and calls
`paste_render_page` and `paste_combo` in-process, and `PasteComboRoute` serves the
combo responses from Pekko HTTP.

## Installation

```scala
// build.sbt — select a published build that contains the Paste/PastePage API
libraryDependencies ++= Seq(
  "com.pastestack" %% "paste-assetgraph-runtime" % "<native-runtime-version>",
  "com.pastestack" %% "paste-assetgraph-pekko" % "<native-runtime-version>"
)
```

The runtime jar bundles the shared libraries; resolution order is
`PASTE_ASSETGRAPH_LIB` → bundled `native-lib/{platform}` library
(checksum-verified on extraction).

## Quick Start

```scala
import java.nio.file.Paths
import scala.concurrent.Future
import org.apache.pekko.actor.ActorSystem
import org.apache.pekko.http.scaladsl.Http
import org.apache.pekko.http.scaladsl.server.Directives.*
import com.pastestack.assetgraph.Paste
import com.pastestack.assetgraph.pekko.PasteComboRoute

@main def run(): Unit = {
  given system: ActorSystem = ActorSystem()
  // Page dumps call the native library synchronously: render off the
  // default dispatcher, as PasteComboRoute does for combo.
  val blockingIo = system.dispatchers.lookup("pekko.actor.default-blocking-io-dispatcher")

  // Manifest + compiled output built by paste-assetgraph
  val paste = Paste(Paste.Settings(
    manifest   = Paths.get("src/main/public/manifest.json"),
    outputBase = Paths.get("src/main/public")
  ))

  val routes = concat(
    new PasteComboRoute(paste).route,  // JAM endpoint under paste.basePath
    path("") {
      get {
        onSuccess(Future(views.html.home(paste))(blockingIo)) { page =>
          complete(page)
        }
      }
    }
  )

  Http().newServerAt("0.0.0.0", 8080).bind(routes)
}
```

If the manifest is on the classpath — e.g. produced by the sbt plugin's
resource generator — use `Paste.fromClasspath()` instead (it copies the
resource to a temp file once, defaulting `outputBase` to
`Paste.defaultOutputBasePath`).

`PasteComboRoute` serves combo URLs only. When `perModule` is on, dumps
request each versioned file in dependency order naming its output path —
`/js/…` and `/css/…` under the default flat layout — and the host serves
them through its ordinary static handling. Composing the combo route with
`getFromDirectory` keeps both on one server, using the same imports plus
`java.nio.file.Paths` (already imported above):

```scala
  val compiled = Paths.get("src/main/public")   // the outputBase above
  val routes = concat(
    new PasteComboRoute(paste).route,
    pathPrefix("js") { getFromDirectory(compiled.resolve("js").toString) },
    pathPrefix("css") { getFromDirectory(compiled.resolve("css").toString) },
    path("") {
      get {
        onSuccess(Future(views.html.home(paste))(blockingIo)) { page =>
          complete(page)
        }
      }
    }
  )
```

The prefixes must match the emitted output paths: the flat layout emits
`/js/…` and `/css/…` relative to `outputBase`. Custom `outputJs`/`outputCss`
settings change the filesystem roots behind `/js` and `/css`; they do not
rewrite manifest output paths. A CDN prefix or beside-source layout
requires matching URL routing at the static origin; the emitted URL is the
manifest's `output` pointer with one leading slash, percent-encoded per
segment. There is no built-in static handler.

## Project Structure

```
my-app/
├── build.sbt
├── src/main/
│   ├── resources/
│   │   └── paste-assetgraph.conf
│   ├── assets/                 # Source files
│   │   ├── js/
│   │   └── scss/
│   ├── public/                 # Generated
│   │   ├── manifest.json
│   │   ├── js/
│   │   └── css/
│   ├── scala/
│   │   └── Main.scala
│   └── twirl/
│       └── views/
│           └── home.scala.html
```

## Configuration

`src/main/resources/paste-assetgraph.conf`:
```hocon
debug-mode = false

# Each root is walked recursively and classified by extension; one dir may
# hold .js, .scss and .css together.
source.dirs = ["src/main/assets/js", "src/main/assets/scss"]

output.base = "src/main/public"
output.js = "src/main/public/js"
output.css = "src/main/public/css"
output.images = "src/main/public/images"

manifest = "src/main/public/manifest.json"

minify = true

jam.hash-urls = true    # qualify versioned URL tokens with content hashes (default)
```

The combo base path and inline threshold live on `Paste.Settings`
(`basePath`, `inlineThreshold`) — `jam.base-path`/`jam.inline-threshold`
in the config still parse for compatibility but the build ignores them
and reports a note.

## Twirl Templates

`PastePage` keeps the page's ordered jam/dump events; each dump re-renders the
full list through the library so delivered modules stay out of later URLs. One
page per render — not thread-safe. Each dump calls the library synchronously,
so render templates on a blocking dispatcher, never the default one.

`views/home.scala.html`:
```scala
@(paste: com.pastestack.assetgraph.Paste)
@import com.pastestack.assetgraph.AssetType

@defining(paste.page()) { page =>
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>My App</title>

  @page.jam(css = Seq("site.theme"), js = Seq("paste.dom", "paste.ui.heroscroll"))
  @Html(page.dump(AssetType.Css))
</head>
<body>
  <h1>Hello World</h1>

  @Html(page.dump(AssetType.Js))
  @page.finish()
</body>
</html>
}
```

`page.jam` records intent and returns `""`; the dumps emit markup: the inlined
runtime once, `<link>`/`<style>` for CSS, and `isLoading` loader tags or
`<script src>` for JS. Groups under `inlineThreshold` (1024 bytes; `0` disables
automatic inlining) inline automatically; `inline = true` forces it.

`page.finish()` ends the page: it returns `""` and seals it after the library
verifies every jammed module reached a dump of its type (css delivery is
tracked per `media`). A jam queued after its type's last dump — a css jam
below the head's css dump — throws `PasteException` naming the module rather
than dropping the tag silently, and a failed finish leaves the page open so
the missing dump can be added and finish retried. Include templates never
finish — they are partial renders — so finish lives on the root layout (or in
the host's `renderPage`-style helper, where the layout's own finish makes the
call idempotent).

## Build Integration

### Option 1: sbt plugin

```scala
// project/plugins.sbt
addSbtPlugin("com.pastestack" % "paste-assetgraph-sbt" % "0.1.0")

// build.sbt
enablePlugins(AssetGraphPlugin)

// Settings (defaults shown):
//   pasteAssetGraphSourceDirs   := Seq("src/main/assets/scss", "src/main/assets/js")
//   pasteAssetGraphOutputDir    := "target/public"
//   pasteAssetGraphManifestPath := ""  // defaults to Compile/resourceManaged/paste-assetgraph-manifest.json
//   pasteAssetGraphMinify       := true
//   pasteAssetGraphDebug        := false
//   pasteAssetGraphHashUrls     := true  // -h{hash} pins in versioned URLs
//   pasteAssetGraphCdnUrl       := ""    // origin prefix written into the
//                                        // manifest; empty leaves it unset
//   pasteAssetGraphOutputLayout := "flat"           // or "beside-source"
//   pasteAssetGraphBuildDirName := "_paste_build"   // beside-source output dir name

// With "beside-source", compiled files land in a <build-dir-name> directory
// beside each source file and every manifest output is project-relative —
// the runtime's outputBase is then the project base directory, and
// pasteAssetGraphOutputDir must sit inside it.
```

The plugin's `pasteAssetGraphBuild` runs as a `Compile / resourceGenerators`
task: assets build whenever resources are needed — `run`, `test`, `package`,
`fullClasspath`, not a bare `compile` — and the manifest lands on the
classpath (`Paste.fromClasspath()` finds it). A pipeline that packages
`target/public` itself should run `pasteAssetGraphBuild` (or `package`)
rather than only `compile`.

### Option 2: Manual

```bash
# Build assets before running
paste-assetgraph build --config src/main/resources/paste-assetgraph.conf

# Run server
sbt run
```

### Option 3: Watch mode (development)

Terminal 1:
```bash
paste-assetgraph watch --config src/main/resources/paste-assetgraph.conf
```

Terminal 2:
```bash
sbt run
```

## API Reference

### `Paste`

```scala
final case class Settings(
  manifest: Path,
  outputBase: Path,
  outputJs: Option[Path] = None,       // default: <outputBase>/js
  outputCss: Option[Path] = None,      // default: <outputBase>/css
  cdnBase: String = "",                // URL prefix for emitted tags;
                                       // empty inherits the manifest's cdn_base
  basePath: String = "/paste",         // combo URL prefix + route mount
  inlineThreshold: Int = 1024,         // automatic inline limit; 0 disables
  perModule: Boolean = false           // one versioned-file request per
                                       // resolved module, in dependency
                                       // order
)

Paste(settings: Settings): Paste
Paste.fromClasspath(
  resource: String = "/paste-assetgraph-manifest.json",
  outputBase: Path = Paths.get(Paste.defaultOutputBasePath),
  cdnBase: String = "",
  basePath: String = "/paste",
  inlineThreshold: Int = 1024,
  perModule: Boolean = false
): Paste
```

- `paste.page(): PastePage` — new per-render event list.
- `paste.combo(pathAndQuery: String): ComboResponse`
  — resolves a request path (`/paste/…` optionally plus `?query`) to `status`,
  `contentType`, `cacheControl`, `cacheable`, `body`. It calls the library
  synchronously — `PasteComboRoute` wraps it in a `Future` on pekko's
  `default-blocking-io-dispatcher`.
- `paste.basePath` — the mount prefix the route matches.

Both calls go straight to the shared library: the wrapper adds no cache of
its own (the library keeps a bounded manifest cache that reloads when the
manifest file changes) and implements no tag, URL, dependency, or combo
logic. A library error throws `PasteException` carrying the library's
message.

### `PastePage`

```scala
page.jam(css: Seq[String] = Nil, js: Seq[String] = Nil,
         inline: Boolean = false, sync: Boolean = false,
         media: Option[String] = None,
         condition: Option[String] = None,
         perModule: Option[Boolean] = None): String
                                             // records the jam, returns "";
                                             // condition wraps its tags in a
                                             // downlevel conditional comment;
                                             // perModule overrides the
                                             // render setting for this jam
page.dump(assetType: AssetType): String  // AssetType.Css or AssetType.Js
page.dump(): String                      // both in one dump — css first, then js
page.finish(): String                    // "" once every jammed module was dumped
```

A page pins the manifest generation its first dump saw. A build that lands
between two dumps of the same page makes the next call throw a
`PasteException` whose `isManifestGeneration` is true, and the page is retired
for good: discard it and render the request again on a fresh `paste.page()`.

## PasteComboRoute

```scala
new PasteComboRoute(paste).route: Route
```

Matches `GET` and `HEAD` requests whose full request path starts with
`paste.basePath + "/"` and completes `Future(paste.combo(path + optional "?"
+ rawQuery))(blockingIo)` — the library call runs on the
`default-blocking-io-dispatcher`. Because the match looks at the full URI, the
route composes under an enclosing `pathPrefix`: nest it inside
`pathPrefix("app")` with `basePath = "/app/paste"`. Paths outside the base
path are rejected.

Set `pekko.http.server.raw-request-uri-header = on` for the same answers the
Node and Python runtimes give: the route then forwards the request target as
the client spelled it (taken from the `Raw-Request-URI` header, and only when
it names the path the server parsed). Without it Pekko's parsed path renders
in canonical percent-encoding, so a non-canonical spelling such as
`app%2Bv1.0` reaches the library as `app+v1.0` and serves 200 instead of a
302 to the canonical URL.

The response passes through untouched:

| Request | Status | Cache-Control |
|---|---|---|
| `/paste/{ts}/{module+vN-h{hash}~...}.min.{js\|css}` — canonical: pinned entries carrying each version's content hash under the default `jam.hash-urls` policy, `ts` = the group's stamp; a fully pinned list is canonical in the order written (each order is a different byte sequence), while a list containing an unpinned name canonicalizes to dependency order | 200 at each pinned version — current or a retained history version | `public, max-age=31536000` |
| `/paste/{ts}/{module+vN~...}.min.{js\|css}` — non-canonical: a stale stamp, a non-numeric segment, an unpinned name, or a legacy `+vN` token under a qualified manifest | 302 with `Location` = the canonical URL (query preserved) | `no-cache`, empty body |
| Same form with `ts` newer than the group's stamp — a URL from a newer build | 404 | `no-cache`, body `/* modules not found */` |
| `/paste/{packages-segment}/{module}.js` | 200 — the runtime loader's URL, under the current segment or one the manifest recorded for an earlier build; an unrecorded segment is a `no-cache` 404 | `public, max-age=31536000` |
| `/paste/{modules}.{js\|css}` (unversioned) | 200 | `no-cache` |
| `/paste/require/{modules}.{ext}` or `?require` | 200 | `no-cache` |
| `?filter=a~b` on a canonical versioned form | 200 | `public, max-age=31536000` |
| Any named module unresolvable or unreadable, a pinned version the manifest cannot serve, a `-h` hash that is not the resolved version's recorded hash, a malformed version or hash token, or an empty or invalid module name in the list | 404 | `no-cache`, body `/* modules not found */` — never a partial group |

The `Location` value travels on the response object as `location`; the route
writes it as the `Location` header on a 302.

## Debug Mode

The combo route serves **compiled output only** — there is no on-the-fly SCSS
compilation and no source serving. The `.js` (non-min) URL form resolves to the
same compiled file as `.min.js`. To see edited files, rebuild:
`paste-assetgraph watch` in a separate terminal recompiles on change, and the
unversioned `/paste/{module}.{js|css}` path is served `no-cache` so a refresh
picks up new output immediately (versioned responses are cached for a year).
The `debug-mode` config key (or `pasteAssetGraphDebug` in sbt) controls the
build's debug flag — unminified output — which is separate from serving.
