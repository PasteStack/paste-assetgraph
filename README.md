# paste-assetgraph

Primary PasteStack asset toolchain. The Rust library performs init, build, minification, dependency
resolution, manifest generation, JAM tag rendering, combo serving, build-time HTML template
rendering, and static dist rollups. All tag, URL and combo logic lives in the library — the Scala,
Node and Python runtimes are thin loaders that call it in-process through its C ABI.

## Structure

```
paste-assetgraph/
├── src/                    # Rust library + CLI (build, render, combo, watch)
├── ffi/                    # C ABI shared library the runtimes load
├── runtime/
│   ├── scala/              # Scala wrapper + Pekko HTTP route + sbt plugin
│   ├── node/               # Node ESM wrapper (used from source)
│   └── python/             # Python wrapper (used from source)
└── Cargo.toml
```

## Features

- **Auto-versioning** — Content hash-based versioning for cache busting
- **Dependency graph** — Topological sort of JS modules based on `@requires`/`@module` comments
- **SCSS compilation** — Compiles SCSS to CSS (via grass)
- **Minification** — Minifies JS and CSS for production
- **Manifest generation** — JSON manifest mapping source paths to versioned output
- **JAM rendering** — `render` turns a page's jam/dump events into bootstrap markup: inlined runtime, exact-group loader URLs, threshold-based inlining
- **JAM endpoint** — `combo` answers exact-group, `require`, `filter` and unversioned URLs as a response description (status, cache headers, optional redirect `location`, body) the host framework writes as HTTP
- **Build-time templates** — `<!-- paste:jam -->`/`<!-- paste:dump -->` directives render the same markup into static HTML, with combo and loader files materialized under the output directory
- **Static dist rollups** — Emits `dist/js/assetgraph.js` and `dist/css/assetgraph.css` for plain static hosts
- **Version history** — Keeps old fingerprinted module files for rolling deploys
- **Thin runtimes** — Scala (`Paste`, `PastePage`, `PasteComboRoute`), Node (`createPaste`), Python (`Paste`) call the shared library in-process

## Installation

### Pre-built binaries

The release assets are available on the
[GitLab Releases page](https://gitlab.com/tomshley/brands/global/tware/tech/products/paste/paste-assetgraph/-/releases).
Assets are named `paste-assetgraph-darwin-aarch64`, `paste-assetgraph-darwin-amd64`,
`paste-assetgraph-linux-amd64-musl`, `paste-assetgraph-linux-aarch64-musl`, and
`paste-assetgraph-windows-amd64.exe`. Download the matching binary and `SHA256SUMS`, then verify:

```bash
sha256sum -c SHA256SUMS --ignore-missing
chmod +x paste-assetgraph-<platform>
./paste-assetgraph-<platform> --version
```

On Windows PowerShell:

```powershell
(Get-FileHash .\paste-assetgraph-windows-amd64.exe -Algorithm SHA256).Hash
.\paste-assetgraph-windows-amd64.exe --version
```

Linux release binaries are statically linked for musl and run on both Alpine and glibc-based Linux distributions. The sbt plugin selects these binaries on Linux without requiring a glibc compatibility layer.

### Build from source

```bash
cargo install --path .
```

## Usage

### Initialize a project

```bash
# Basic HTML site
paste-assetgraph init -t basic

# React/Vite project
paste-assetgraph init -t react

# Scala/Twirl project
paste-assetgraph init -t scala
```

### Build assets

```bash
# Production build (compiles, minifies, writes the manifest,
# renders templates.* when configured)
paste-assetgraph build --config paste-surface.conf

# Debug build (no minification)
paste-assetgraph build --config paste-surface.conf --debug

# React project
paste-assetgraph build --config paste-surface.conf --react
```

### Watch for changes

```bash
paste-assetgraph watch --config paste-surface.conf
```

### Render page markup

`render` reads a JSON event list on stdin and prints the last dump's markup.
A list ending in the bare string `"finish"` is a completion check — every
jammed module must have reached a dump of its type or the command fails
naming it — and prints nothing:

```bash
echo '{"events":[{"jam":{"css":["site.theme"],"js":["paste.dom"]}},{"dump":"css"},{"dump":"js"}]}' \
  | paste-assetgraph render \
      --manifest public/manifest.json \
      --output-base public
```

Flags: `--output-js`, `--output-css` (compiled dirs, default `<output-base>/js|css`),
`--cdn-base` (URL prefix for emitted tags), `--base-path` (default `/paste`),
`--inline-threshold` (default `1024`, `0` disables automatic inlining).

### Resolve a combo URL

`combo` resolves a request path (with optional `?query`) to a JSON response:

```bash
paste-assetgraph combo \
  --manifest public/manifest.json \
  --output-base public \
  /paste/1770462000/paste.dom+v2.0-h0bdbb2c325525e986d55ef50ca66d056~paste.event+v1.0-h7e9e92b0857c477a6e8aebb0ab2bd924.min.js
# => {"status":200,"contentType":"application/javascript; charset=utf-8","cacheControl":"public, max-age=31536000","cacheable":true,"location":null,"body":"..."}
```

A non-canonical versioned URL — a stale stamp, a list out of dependency
order, an unpinned name, or a legacy `+vN` token under a hash-qualified
manifest — answers `302` with `location` set to the canonical URL instead
of serving under a wrong cacheable address:

```bash
paste-assetgraph combo \
  --manifest public/manifest.json \
  --output-base public \
  /paste/1770462000/paste.dom+v2.0.min.js
# => {"status":302,"cacheControl":"no-cache","location":"/paste/1770462000/paste.dom+v2.0-h0bdbb2c325525e986d55ef50ca66d056.min.js","body":""}
```

A supplied `-h` hash must equal the resolved version's recorded content
hash — a mismatch is a `404`, never a redirect onto different bytes.

The host framework turns that response into the HTTP answer — see the
runtime wrappers below.

## Documentation

- [JAM specification](JAM_SPEC.md) — URL contract, rendering, combo serving
- [Basic HTML Sites](docs/basic-html.md) — Build-time templates, static deploys
- [Node/Express](docs/node-express.md) — Express middleware + the Node wrapper
- [Python/FastAPI](docs/python-fastapi.md) — FastAPI with Jinja2 templates
- [Scala/Pekko HTTP](docs/scala-pekko.md) — Pekko HTTP routes, Twirl templates

## Configuration

See `paste-surface.conf`:

```hocon
source.dirs = ["src/main/assets/js", "src/main/assets/scss"]
output.base = "src/main/public"
output.js = "src/main/public/js"
output.css = "src/main/public/css"
manifest = "src/main/public/manifest.json"
```

### Source discovery

Every directory in `source.dirs` is walked recursively and each file is
classified by extension — `.js` becomes a JS module, `.scss` and plain
`.css` become style modules. The folder layout means nothing: one
directory may hold JS and styles together. `_`-prefixed `.scss` files are
partials and never assets; a `.css` file always is. The first root to
claim a module name or style key wins, and the legacy keys
`source.js`/`source.scss` still work — when set they are appended after
`source.dirs`. With nothing configured, the defaults are
`src/main/assets/js` then `src/main/assets/scss`. Include paths
(`include-paths`) are walked the same way, after the source dirs, for
assets another project provides.

### Output layout

`output.layout` selects where compiled files land:

- `flat` (default): every output under its type directory —
  `output.js/app.<hash>.v1.min.js`, manifest output `/js/app.<hash>.v1.min.js`.
  The runtime's output root is `output.base`.
- `beside-source`: a source file compiles into a `<build-dir-name>`
  directory beside it — `src/main/assets/js/_paste_build/app.<hash>.v1.min.js`
  — and its manifest output is the project-relative path
  `/src/main/assets/js/_paste_build/app.<hash>.v1.min.js`. Include-path
  assets are never written into another tree: they keep flat placement
  under `output.base`, still recorded relative to the project root. Every
  beside-source output is relative to the project root, so **the
  runtime's output root is the project root**, and `output.base` must
  live inside it. `output.build-dir-name` (default `_paste_build`) names
  the directory; directories with that name are never scanned as sources
  in either layout. When packaging a beside-source site into an image,
  ship the build directories at their project-relative paths — the
  manifest's outputs point there.

### Storage and stamps

`storage.backend` decides what a rebuild keeps:

- `local` (default): writes the current output only — a superseded file
  is removed. Intended for uncommitted output (e.g. under `target/`).
- `dist`: retains the current output plus the `storage.keep-versions` most
  recent historical versions per asset. Intended
  for committed output: a rolling deploy can still serve a versioned URL
  minted before the release.

Asset stamps come from filesystem mtimes only — the source file's
last-modified time, clamped by `SOURCE_DATE_EPOCH` when the environment
sets it. There is no version-control involvement: committing the `dist`
output and manifest carries version history and stamps forward, while a
clean build without that manifest restarts versions at v1 and takes
stamps from the checkout's file mtimes.

Carried history is what version identity depends on, and the URL binds
content directly: versioned tokens carry the version's full 32-character
content hash as `+vN.0-h{hash}` (`jam.hash-urls`, default on — see
`jam.hash-urls` below), so two clean builds that preserve identical source
timestamps still mint different URLs for different bytes. The build writes
the policy into the manifest's `hash_urls` field, which is what every
runtime and template render reads — adapters forward paths without
reimplementing any policy. A manifest written before the field existed
has no `hash_urls` key and keeps serving and emitting the legacy `+vN.0`
form until a build rewrites it; `jam.hash-urls = false` opts a build back
to that form without changing asset versions. A correctly qualified URL
resolves under either policy — one qualified pin in a request makes the
canonical redirect qualified for every member — while legacy URLs under a
qualified manifest redirect to their hashed canonical spellings.

An asset's version rolls when its compiled bytes *or* its dependency
list changes — a `@requires` edit produces a new version even when the
minified output is byte-identical, because the dependency metadata is
part of what a URL pins down. Deleting a source under `dist` marks its
entry `retired`: fresh renders, rollups, and unversioned requests no
longer see it, but every retained version still serves its exact pinned
URL. Under `local` a deleted source is simply gone.

Changing `output.layout` or `output.build-dir-name` relocates outputs:
retained versions are found through the manifest's recorded
`generated_dirs` (plus the manifest's own output records — never a
recursive filesystem search), verified against their recorded hashes,
and rewritten at the new paths with their versions and stamps intact. A
retained file that is missing or fails its hash check fails the build
rather than being silently replaced by current bytes, and the same
check repairs a missing or tampered current output on every rebuild.

Build-time template rendering adds:

```hocon
templates.source = "templates"   # HTML files with paste directives
templates.output = "public"      # rendered pages + combo/loader files (defaults to output.base)
jam.inline-threshold = 1024      # automatic inline limit in bytes (0 disables)
jam.base-path = "/paste"         # combo URL prefix
jam.hash-urls = true             # qualify versioned URL tokens with content hashes
```

`jam.hash-urls` defaults to `true` for new builds; setting it `false`
emits and serves the legacy `+vN.0` URL shape instead. The rendered URL
is interpreted by the library alone — the policy governs the tokens
emitted for versioned JAM URLs, while a hash supplied on a `require` or
unversioned request is still validated and those responses stay
`no-cache` either way. Opting out is safe only where version history is
preserved — it changes URL spelling, not the build: content hashing,
version increments, and stamp carry-forward all still apply.

## Manifest Format

This excerpt shows the entry fields; the generated manifest also contains
every referenced module and dependency.

```json
{
  "version": "2025.02.02.1",
  "generated": "2025-02-02T21:30:00Z",
  "modules": {
    "paste.dom": "js/paste/dom.js",
    "site.theme": "css/site/theme.css"
  },
  "assets": {
    "js/paste/dom.js": {
      "source": "src/main/assets/js/dom.js",
      "output": "/js/paste.dom.2c56c360580420d293172f42d85dfbed.v1.min.js",
      "hash": "2c56c360580420d293172f42d85dfbed",
      "version": 1,
      "timestamp": "2025-02-02T21:30:00Z",
      "dependencies": ["paste"],
      "history": []
    }
  },
  "sorted_deps": {
    "js": [["paste.dom", "/js/paste.dom.2c56c360580420d293172f42d85dfbed.v1.min.js", 1]]
  },
  "generated_dirs": ["dist/js", "dist/css"],
  "hash_urls": true
}
```

`history` holds the retained older versions — each with its own `hash`,
`version`, `output`, and `timestamp` — and an asset whose source was
deleted carries `"retired": true` (the field is always written; absent
in older manifests means active). `hash_urls` records whether the build
emits hash-qualified URL tokens (always written; absent in older
manifests means the legacy `+vN.0` form). `generated_dirs`
records the project-relative directories builds have generated into, so
a layout or build-directory change can relocate retained outputs without
scanning the filesystem and the scanner never treats generated files as
sources.

## Runtime wrappers

Each wrapper locates the shared library (`PASTE_ASSETGRAPH_LIB`, else the
bundled `native-lib/{platform}` copy its package ships), calls
`paste_render`/`paste_combo` with a settings JSON, and releases every result
with `paste_free`. No wrapper implements tag, URL, dependency, combo, or
caching logic itself.

```scala
// Scala — see runtime/scala/README.md
val paste = Paste(Paste.Settings(manifest = Paths.get("public/manifest.json"),
  outputBase = Paths.get("public")))
val page = paste.page()
page.jam(css = Seq("site.theme"), js = Seq("paste.dom"))
page.dumpCss(); page.dumpJs()
page.finish()   // validates every jam reached a dump, returns ""
```

```js
// Node — see runtime/node/README.md (source-only for now)
import { createPaste } from './runtime/node/index.js';
const paste = createPaste({ manifest: 'public/manifest.json', outputBase: 'public' });
const page = paste.page();                    // synchronous
const response = await paste.combo(url);      // asynchronous
```

```python
# Python — see runtime/python/README.md (source-only for now)
from paste_assetgraph import Paste
paste = Paste("public/manifest.json", "public")
page = paste.page()
page.jam(css=["site.theme"], js=["paste.dom"])
page.dump_css(); page.dump_js()
page.finish()   # validates every jam reached a dump, returns ""
```

## JVM artifacts

`paste-assetgraph-runtime`, `paste-assetgraph-pekko` and the `paste-assetgraph-sbt`
plugin publish to the PasteStack Maven registry
(`https://gitlab.com/api/v4/projects/70289607/packages/maven`) at three coordinates,
derived from `VERSION` plus the pipeline's revision:

| Coordinate | Published by | Use for |
|---|---|---|
| `<VERSION>` | a release tag pipeline | anything that ships |
| `<VERSION>-develop-latest` | every develop pipeline, moving | tracking unreleased work |
| `<VERSION>-develop-<sha>` | every develop pipeline, immutable | reproducing one develop build |

There is no `-SNAPSHOT` coordinate, and a version not derived from `VERSION` cannot be
produced by the pipeline at all. A pin on either is satisfied only by a locally
published artifact, which no other machine and no CI job has.

The Node and Python packages are not published yet — use them from source.

## License

Apache License, Version 2.0. See `LICENSE` and `NOTICE.md`.
