# paste-assetgraph

Primary PasteStack asset toolchain. The Rust library performs init, build, minification, dependency
resolution, manifest generation, JAM tag rendering, and combo serving. A
build emits individual versioned asset files plus manifest metadata — no
rollups, materialized combo responses, or per-module loader copies. All tag,
URL and combo logic lives in the library — the Scala,
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
# Production build (compiles, minifies, writes the manifest)
paste-assetgraph build

# Debug build (no minification)
paste-assetgraph build --debug
```

`build --react` is deprecated and has no effect — the build is
layout-agnostic; the flag is accepted so existing scripts keep parsing.

### Watch for changes

```bash
# Rebuild on change, in the same mode as `build`
paste-assetgraph watch

# Rebuild on change in debug mode (no minification)
paste-assetgraph watch --debug
```

`watch` builds exactly as `build` does: `minify` and `debug-mode` come from
the configuration, and `--debug` forces unminified output as it does for
`build`. Both commands write into the same output and version history, so
switching between them over unchanged sources adds no version. A
`watch --debug` session publishes unminified versions, kept under the same
`storage.keep-versions` retention as any other build, and the next `build`
without `--debug` publishes a minified one.

The watcher follows configured source/include roots and Sass imports actually
consumed outside those roots. An external import is tracked by its exact
file identity, not by treating the entire parent directory as a source root.
Existing imported files are also watched directly, since some backends do
not report in-place writes through a nonrecursive parent watch. For a Sass
import through a file or directory symlink, the traversed link entries
and the resolved imported file are tracked. Verification compares not just read bytes
but the symlink's resolved target, so even a retarget to equal-content bytes
refreshes the watcher registrations before another edit can be missed.
Missing partials are followed from the nearest existing physical ancestor,
including through symlinked directories and parent (`..`) components.
A dangling Sass file symlink also tracks its missing referent's ancestor,
so creating that referent can repair the import without touching the link. Directory-entry events with
ambiguous reported paths are checked against exact imported inputs before
triggering a rebuild. Source errors keep the
last published manifest in place while a watched input can be repaired;
manifest, output, and watcher failures terminate with an error instead of
reporting a healthy idle watcher. A just-discovered external dependency is
rechecked after its watch registration to close the build/registration gap.

A manifest kept inside a watched root does not trigger rebuilds of its
own: the manifest, its `.bin` cache, `manifest.lock`, and
`.paste-staged.*` and `.paste-pending.*` files are never source changes.
Backends that report publication only as a change to the manifest's
directory (kqueue on macOS) are handled by comparing that directory's
other entries — identity, size and modification time — with their state
when the last build started; an unchanged directory does not rebuild.

On macOS the kqueue backend's event thread can stop on an event for a file
it has just stopped watching — an editor's atomic save (write a temporary
file, rename it over the source) is enough, and so is publishing a
manifest kept inside a watched root. `paste-assetgraph watch` reports it
(a line naming where the thread failed, then `The file watcher's event
thread stopped — restarting it and rescanning every root`) and starts the
same command over in its own process: same arguments, same pid, same
terminal. Only a fresh process releases the dead thread's descriptors —
one per watched file — and its initial build picks up every edit made
meanwhile. Consequences of the restart:

- the configuration is read again, so an edit to it takes effect;
- the restart is skipped, and the dead watcher replaced in the running
  process instead, when the configuration no longer loads (watch keeps
  running on the configuration it started with and says why) or when the
  executable on disk is no longer the one running — a binary replaced or
  removed mid-session is never run silently. Each in-process replacement
  leaves the dead thread's descriptors open, so a long session in that
  state should be restarted by hand.

A program embedding the library's `graph::watch` gets no restart of its own
process: it calls `graph::install_watch_backend_monitor()` to have a dead
backend noticed and the watcher replaced in place, and can pass a process
restart of its own through `graph::watch_with`. Several watches may run in
one process; because a dying backend thread cannot say which watcher it
served, every running watch replaces its watcher and rescans. Each
in-place replacement leaves the dead thread's descriptors open — one per
watched file — so a long-running embedding should expect descriptor
growth with every replacement.

### Render page markup

`render` reads a JSON event list on stdin and prints the last dump's markup.
A list ending in the bare string `"finish"` is a completion check — every
jammed module must have reached a dump of its type or the command fails
naming it — and prints nothing:

```bash
echo '{"events":[{"jam":{"css":["site.theme"],"js":["paste.dom"]}},{"dump":"all"}]}' \
  | paste-assetgraph render \
      --manifest public/manifest.json \
      --output-base public
```

A dump's scope is `"css"`, `"js"`, or `"all"` — the combined scope emits
the css tags then the js tags in one dump, so a page needs only one.

Flags: `--output-js`, `--output-css` (compiled dirs, default `<output-base>/js|css`),
`--cdn-base` (URL prefix for emitted tags; empty inherits the manifest's
`cdn_base` when the build recorded one), `--base-path` (default `/paste`),
`--inline-threshold` (default `1024`, `0` disables automatic inlining),
`--per-module` (one versioned-file request per resolved module in
dependency order instead of one combo URL per group — a jam event's
`perModule` overrides it per group).

### Resolve a combo URL

`combo` resolves a request path (with optional `?query`) to a JSON response:

```bash
paste-assetgraph combo \
  --manifest public/manifest.json \
  --output-base public \
  /paste/1770462000/paste.dom+v2.0-h0bdbb2c325525e986d55ef50ca66d056~paste.event+v1.0-h7e9e92b0857c477a6e8aebb0ab2bd924.min.js
# => {"status":200,"contentType":"application/javascript; charset=utf-8","cacheControl":"public, max-age=31536000","cacheable":true,"location":null,"body":"..."}
```

The list is served in the order written whenever every entry is pinned —
`a+v1~b+v2` and `b+v2~a+v1` are different byte sequences, each its own
canonical address — while a list containing an unpinned name canonicalizes
to dependency order. A non-canonical versioned URL — a stale stamp, a
non-numeric segment, an unpinned name, or a legacy `+vN` token under a
hash-qualified manifest — answers `302` with `location` set to the
canonical URL instead of serving under a wrong cacheable address:

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
- [Basic HTML Sites](docs/basic-html.md) — Asset builds for sites without a framework
- [Node/Express](docs/node-express.md) — Express middleware + the Node wrapper
- [Python/FastAPI](docs/python-fastapi.md) — FastAPI with Jinja2 templates
- [Scala/Pekko HTTP](docs/scala-pekko.md) — Pekko HTTP routes, Twirl templates

## Configuration

See `paste-assetgraph.conf` — the default name `build` and `watch` read;
a project still carrying the former `paste-surface.conf` name keeps
building with a notice until it is renamed:

```hocon
source.dirs = ["src/main/assets/js", "src/main/assets/scss"]
output.base = "src/main/public"
output.js = "src/main/public/js"
output.css = "src/main/public/css"
manifest = "src/main/public/manifest.json"
```

Quote any value containing `#` or `//`: unquoted, those characters start a
comment under HOCON's own rule and the rest of the line is dropped.

### Source discovery

Every directory in `source.dirs` is walked recursively and each file is
classified by extension — `.js` becomes a JS module, `.scss` and plain
`.css` become style modules. The folder layout means nothing: one
directory may hold JS and styles together. `_`-prefixed `.scss` files are
partials and never assets; a `.css` file always is. A `.min` before the
extension belongs to the file, not the module: `vendor.min.js` is module
`vendor` (asset key `js/vendor.js`), `theme.min.scss` is `theme`. `.min` is
the output and URL suffix only, so a module name ending in `.min` — declared
with `@module`, or derived from a path like `vendor/min.js` — fails the
build, and so do a source and its `.min` copy side by side (`vendor.js` and
`vendor.min.js`), naming both files, and a `@requires vendor.min` (the error
says to require `vendor`). A manifest written before this rule that
recorded a `vendor.min` module rebuilds once its sources follow the rule:
the module becomes `vendor`, the old entry is retired and its pinned URLs
keep serving. A loader URL of a packages segment recorded before the rule
reads `{segment}/vendor.min.js` as that segment's own `vendor.min` entry and
serves the bytes it pinned (retention keeps them), never `vendor`'s;
`{segment}/vendor.js` still serves `vendor`. Everywhere else
`vendor.min.js` names `vendor`. The first root to
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

- `dist` (default): retains every version ever built per asset, so a versioned URL
  minted by any earlier release keeps serving. `storage.keep-versions`
  bounds that retention: `all` (the default) keeps everything, `0` keeps
  the current output only, `N` keeps the current output plus the `N` most
  recent versions. Intended for committed output: a rolling deploy can
  still serve a versioned URL minted before the release. Retention lasts
  only as long as the output directory does: an output under a directory
  the build tool cleans (such as `target/`) starts over after a clean, so a
  rolling deploy needs the output on persistent storage or published
  append-only to the origin pages load from. Pruning only removes versions
  the loaded manifest recorded and the bound has retired: a build without
  its manifest prunes nothing. Files a failed or killed build wrote, and
  versions whose unlink failed, are listed in a `.paste-pending.<digest>`
  journal at the project root beside `manifest.lock`, and the next build
  removes each one the manifest on disk does not name (with the manifest
  missing, none of the listed entries — only a failed build's own new
  files go). Each build re-verifies every retained version by hash, so build
  time grows with retained history.
- `local`: writes the current output only — a superseded file is removed.
  An explicit opt-in for throwaway output.

`storage.keep-versions` bounds compiled per-asset history — the files a
runtime pin resolves. Persisted module content exists only in those
individual versioned files: the build writes no rollups, no materialized
combo responses, and no per-module loader copies. Older releases could
publish such files; removing them from an existing output tree is an
operator decision, not a build step.

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
render and adapter reads — adapters forward paths without
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
entry `retired`: fresh renders and unversioned requests no
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

The retired `templates.source`/`templates.output` publication keys are a
configuration error: page markup comes from the `render` command or a
runtime adapter at request time, and the build emits only individual
versioned assets plus the manifest. Those files can be served from any
static origin; a page without a combo handler loads them through the paste
runtime itself — `paste.require` with each file's full URL, or bare module
names against an origin that publishes `<module>.js` filenames. See the
delivery model in `JAM_SPEC.md`.

```hocon
jam.hash-urls = true             # qualify versioned URL tokens with content hashes

cdn.enabled = true               # record an origin prefix for emitted URLs
cdn.url = "https://cdn.example"  # required when enabled
```

The combo base path and inline threshold are render settings
(`--base-path`, `--inline-threshold`, or the adapter equivalents), not
build config: `jam.base-path`/`jam.inline-threshold`/`jam.enabled` still
parse for compatibility but the build ignores them and reports a note.
`source-maps` parses the same way — no source maps are emitted.

`cdn.enabled` with `cdn.url` writes the origin into the manifest as
`cdn_base` — the render-time default prefix for emitted tags, so pages
can point at a CDN without per-host render settings. A runtime `cdnBase`
(`--cdn-base`, or the adapter setting) overrides it; an empty one
inherits it; a manifest without the key renders origin-relative URLs.
`combo` ignores it — request paths resolve to bytes either way. The
former `cdn.fallback-to-local` key is retired and rejected by name:
there is no fallback to switch. CDN authority validation rejects encoded
ASCII hostname spellings and malformed UTF-8 instead of allowing browser
host reinterpretation; valid UTF-8-encoded internationalized names remain
supported. URL-path percent encodings are independent of this host rule.

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
  "hash_urls": true,
  "cdn_base": "https://cdn.example",
  "output_roots": {"js": ["dist/js"], "css": ["dist/css"]},
  "packages_segments": {
    "0123456789ab": {
      "paste.dom": {"key": "js/paste/dom.js", "version": 1, "hash": "2c56c360580420d293172f42d85dfbed"}
    }
  }
}
```

`history` holds the retained older versions — each with its own `hash`,
`version`, `output`, and `timestamp` — and an asset whose source was
deleted carries `"retired": true` (the field is always written; absent
in older manifests means active). `cdn_base` is present only when the
build ran under `cdn.enabled` with a `cdn.url` — it is the render-time
default origin prefix, which a runtime `cdnBase` overrides.
`hash_urls` records whether the build
emits hash-qualified URL tokens (always written; absent in older
manifests means the legacy `+vN.0` form). `generated_dirs`
records the project-relative directories builds have generated into, so
a layout or build-directory change can relocate retained outputs without
scanning the filesystem and the scanner never treats generated files as
sources. `output_roots` records which directories a build used as its
type output roots — accumulated across moves — and it is the only
provenance a later build accepts for removing a legacy unversioned
`assetgraph.js`/`assetgraph.css` at a historical location: cleanup
deletes those names at the configured root and at roots a manifest
recorded, while the same name anywhere else is reported on stderr and
kept. `packages_segments` is metadata only — it holds no content: each
packages fingerprint a build emitted mapped to the `key`, `version`, and
`hash` every JS module name resolved at then, so a page rendered by an
earlier build keeps resolving its saved loader URL to the bytes it was
built against while the files are retained.

Upgrading a deployment whose manifest predates `output_roots`: a first
rebuild with unchanged output roots removes the legacy pair at the
configured roots and records provenance for later builds. Moving the
output roots in that same first upgrade is the one sequence that can
leave a historical `assetgraph.*` behind — its location is named in a
build warning and left for explicit review. The warning names a
candidate, not a verdict; it is never automatic permission to delete the
file.

## Runtime wrappers

Each wrapper locates the shared library (`PASTE_ASSETGRAPH_LIB`, else the
bundled `native-lib/{platform}` copy its package ships), calls
`paste_render_page`/`paste_render`/`paste_combo` with a settings JSON, and
releases every result with `paste_free`. No wrapper implements tag, URL,
dependency, combo, or caching logic itself.

```scala
// Scala — see runtime/scala/README.md
val paste = Paste(Paste.Settings(manifest = Paths.get("public/manifest.json"),
  outputBase = Paths.get("public")))
val page = paste.page()
page.jam(css = Seq("site.theme"), js = Seq("paste.dom"))
page.dump()     // both types; dump(AssetType.Css)/dump(AssetType.Js) pick one
page.finish()   // validates every jam reached a dump, returns ""
```

```js
// Node — see runtime/node/README.md (source-tree package; CI builds and
// publishes the npm artifact — registry availability is a deployment fact)
import { createPaste } from './runtime/node/index.js';
const paste = createPaste({ manifest: 'public/manifest.json', outputBase: 'public' });
const page = paste.page();                    // synchronous
const response = await paste.combo(url);      // asynchronous
```

```python
# Python — see runtime/python/README.md (source-tree package; CI builds and
# publishes the PyPI artifact — registry availability is a deployment fact)
from paste_assetgraph import Paste
paste = Paste("public/manifest.json", "public")
page = paste.page()
page.jam(css=["site.theme"], js=["paste.dom"])
page.dump()     # both types; dump("css")/dump("js") pick one
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

The Node package (`@pastestack/assetgraph`) and the Python package
(`paste-assetgraph`) publish from the same release pipeline to this
project's GitLab package registries — each bundles the per-platform
native libraries and follows the `VERSION`-derived version scheme.

## Testing

Build the workspace once so every runner uses the same native library and
CLI binary, then generate the shared runner fixture into an empty directory
and point the suites at it:

```sh
cargo build --locked --workspace
python3 tests/build_shared_fixture.py \
  --build debug \
  --out /path/to/empty/shared-fixture

# The native library name follows the platform: .dylib on macOS, .so on
# Linux, .dll on Windows (and paste-assetgraph.exe for the CLI).
export PASTE_ASSETGRAPH_LIB="$PWD/target/debug/libpaste_assetgraph_ffi.dylib"
export PASTE_ASSETGRAPH_TEST_BIN="$PWD/target/debug/paste-assetgraph"
export PASTE_ASSETGRAPH_TEST_FIXTURE="/path/to/empty/shared-fixture"  # absolute

npm test --prefix runtime/node
PYTHONPATH=runtime/python/src python3 -m unittest discover -s runtime/python/tests -v
cd runtime/scala && sbt 'runtime/test' 'pekko/test'
cd sbt-plugin && sbt 'testOnly com.pastestack.assetgraph.sbt.AssetGraphPluginSpec'
```

The runner suites fail without `PASTE_ASSETGRAPH_TEST_FIXTURE` — the fixture
is generated once per build and its `reference.json` is the frozen contract
each adapter verifies, including every file's SHA-256 and each page
sequence's output digest. CI builds the fixture once and passes the exact
artifact to every runtime test job.

The shared fixture proves the adapters agree with one CLI-built
distribution — it is a consistency reference, not evidence of legacy
behavioral parity. The Node loader test runs the vendored runtime in a VM
with a mock document to exercise real module scheduling; it is not a real
browser or HTTP deployment.

## License

Apache License, Version 2.0. See `LICENSE` and `NOTICE.md`.
