# Using paste-assetgraph with Basic HTML Sites

`paste-assetgraph build` is a precompiler: it compiles each source module
into its own versioned file and writes a JSON manifest pointing at them.
Persisted module content exists only in those individual versioned files —
no rollups, no materialized combo responses, no per-module loader copies.

Page markup and `/paste`-style combo URLs are computed per request through
the `render`/`combo` commands (or the in-process FFI behind the Node,
Python, and Scala runtimes). A plain static file host can serve each
compiled file directly by its manifest output path; combo, loader, and
canonicalization URLs need a host that runs the combo handler.

## Serving from a static origin

The compiled files are immutable and individually addressable, so they can
live on a CDN or object storage with nothing in front of them. A page gets
them one of two ways without a combo handler — both are the paste runtime's
own behaviour, not a build feature:

**Full URLs.** Include the runtime, mark every module the files will
declare as loading, then `paste.require` every versioned file the page
needs, dependencies included, at the bottom of the page. The runtime
treats a name that ends in `.js`, contains `:`, or starts with `/` as a
URL and loads it verbatim. An unmarked, undefined dependency can trigger
a bare-name `packagesUrl` request; a static host with only versioned
files cannot answer that URL — copy the list from the manifest's
`sorted_deps`.

The marks are not optional: a file executes `paste.define('main',
['util'], …)`, and if `main`'s file happens to arrive before `util`'s,
the runtime sees an undefined `util` and fetches `packagesUrl +
'util.js'` on its own — a URL this build's versioned output does not
answer. `paste.define('<name>').isLoading = true` declares the name up
front so arrival order cannot trigger that fetch.

The example's files are `paste.define` modules, so the runtime defers each
factory until its dependencies arrive. A file that assigns top-level
globals without a `paste.define` wrapper executes the moment it loads —
marking its name does not defer it — so raw globals need ordered loading:
`render --per-module`'s chained `paste.require` requests, or synchronous
`src` tags in dependency order.

```html
<link rel="stylesheet" href="https://cdn.example/css/main.def456.v1.min.css">
...
<script src="https://cdn.example/js/paste.abc123.v2.min.js"></script>
<script>
  paste.define('util').isLoading = true;
  paste.define('main').isLoading = true;
  paste.require('https://cdn.example/js/util.9f1c2d3e.v4.min.js');
  paste.require('https://cdn.example/js/main.abc123.v1.min.js');
</script>
```

Maintain the loading marks and file URLs from the manifest's dependency
metadata when writing this by hand. `render --per-module` (below) reads
that metadata and generates the loader block; it does not generate or
modify `sorted_deps`.

**Bare module names.** `paste.config({packagesUrl: '<origin>/<version>/'})`
then `paste.require('<module>')` requests `packagesUrl + <module> + '.js'`
and auto-fetches undefined dependencies the same way. This needs an origin
whose filenames are exactly `<module>.js` — a version in the path works, a
version or hash in the filename does not. The versioned `dist` output this
build writes does not follow that convention, so it is served by full URL
or through the combo handler.

There is no `paste.load` — the runtime's loader is `paste.require`, and a
URL-shaped name (ending in `.js`, containing `:`, or starting with `/`)
is what performs a file load.

### Updating and rolling back

A static site carries its asset references in its pages, so an update is a
build plus a page-tag refresh — and a rollback is a page-tag edit, not a
rebuild:

1. **Build.** Edit `assets/` and run `paste-assetgraph build`. A changed
   module gets a new versioned file beside the old one —
   `main.<newhash>.v2.min.js` next to `main.<oldhash>.v1.min.js` — and the
   manifest's current pointer moves to `v2` while `v1` stays in the
   asset's `history` (as long as `storage.keep-versions` retains it).
   Nothing is overwritten, so a page still naming `v1` keeps serving.
2. **Refresh the page's tag block.** Regenerate it with the per-module
   render and paste the output over the old block:

   ```bash
   echo '{"events":[{"jam":{"css":["site.theme"],"js":["main"]}},{"dump":"all"}]}' \
     | paste-assetgraph render --manifest public/manifest.json \
         --output-base public --inline-threshold 0 --per-module
   ```

   Per-module output requests each versioned file in dependency order —
   a `<link>` per stylesheet and one loader block whose
   `paste.require(url, callback)` chain fetches each script only after
   the previous file loaded — so the block is exactly what the page
   needs and nothing it does not. Editing the changed filename in place
   by hand works too; the manifest's `sorted_deps` lists what the block
   should name and in which order.
3. **Deploy** `public/` and the updated page together.
4. **Roll back** by pointing the page at the retained `v1` filename —
   it is still on disk, still immutable, still correct. No build step
   recreates it; rolling forward again is the same refresh.

When the origin is a CDN, add the prefix with `--cdn-base` or a build-time
`cdn.enabled`/`cdn.url`, which records it in the manifest for renders to
inherit.

## Quick Start

```bash
# 1. Install the binary — see the README's release-asset steps
# 2. Init (writes paste-assetgraph.conf)
paste-assetgraph init -t basic

# 3. Build
paste-assetgraph build
```

## Project Structure

```
my-site/
├── paste-assetgraph.conf   # Config
├── assets/                 # Source files
│   ├── js/
│   │   └── main.js
│   └── scss/
│       └── main.scss
└── public/                 # Generated (deploy this)
    ├── manifest.json
    ├── js/
    │   └── main.abc123.v1.min.js
    └── css/
        └── main.def456.v1.min.css
```

## Configuration

`paste-assetgraph.conf` (as `init -t basic` writes it, with `source.dirs` in
place of the legacy `source.js`/`source.scss` keys):
```hocon
debug-mode = false

source.dirs = ["assets/js", "assets/scss"]

output.base = "public"
output.js = "public/js"
output.css = "public/css"
output.images = "public/images"

storage.backend = "dist"
storage.keep-versions = all   # every built version stays servable; a number bounds it
manifest = "public/manifest.json"

minify = true
```

The combo base path and inline threshold are render settings
(`--base-path`, `--inline-threshold`), not build config —
`jam.base-path`/`jam.inline-threshold` in the config still parse for
compatibility but the build ignores them and reports a note.

The retired `templates.source`/`templates.output` keys fail validation:
earlier builds published rendered pages plus the combo and loader files
they referenced, duplicating module content into a second tree. Configs
still carrying either key get a named migration error — remove them and
serve pages through `render`/`combo` or a runtime adapter. Files a previous
release already generated are not recreated and are not automatically
deleted; their lifetime is an operator decision.

Every `source.dirs` root is walked recursively and each file is classified by
extension, so scripts and styles may share a directory at any depth —
`source.dirs = ["assets"]` discovers the same files the two separate roots
above do. The default `output.layout = "flat"` puts all compiled files under
the output directory, which is what a static deploy ships. `output.layout =
"beside-source"` instead writes each file into an `output.build-dir-name`
directory (default `_paste_build`) beside its source and records every
manifest output relative to the project root — useful when the sources live
in versioned layout and the deploy maps the tree rather than one output
directory.

`storage.backend = "dist"` is what makes the output committable: old
fingerprinted files stay on disk so a URL minted by an earlier build keeps
serving after a redeploy. Commit `public/` and the manifest together — the
manifest carries each version's stamp forward. On a fresh clone of a project
that does not commit output, versions restart at v1 and the stamps come from
the checkout's file mtimes (clamped by `SOURCE_DATE_EPOCH` when set) — the
new build's URLs are correct, but pins minted under a different manifest
resolve only what this manifest still records.

Serving never moves a pin to other bytes: an exact retained pin resolves to
its retained bytes; a redirect only normalizes spelling or stamp onto that
same recorded identity; a version the manifest no longer retains — or whose
recorded hash no longer verifies — is a 404, never a redirect onto another
version's content.

## Page Markup

Pages get their tags from `render`: it replays the page's jam/dump events and
prints the last dump's markup — an inlined runtime with
`paste.config({packagesUrl:…})`, `<link>` or inline `<style>` for CSS, and
`isLoading` loader tags or `<script src>` for JS. Jinja, EJS, Twirl, or plain
string interpolation all consume the same output; see the Node, Python, and
Scala guides for adapter examples, or invoke `render` directly:

```bash
echo '{"events":[{"jam":{"css":["site.theme"],"js":["main"]}},{"dump":"all"}]}' \
  | paste-assetgraph render --manifest public/manifest.json --output-base public
```

`render` writes nothing to disk — the markup is stdout for the host to use.

## What build writes

- **Compiled assets** — one versioned file per module/version under
  `output.js` and `output.css`, minified or preserved per `minify` and
  `debug-mode`.
- **Manifest** — module names, source/output pointers, content hashes,
  versions, dependencies, history, and precomputed ordering. Metadata only;
  it contains no module bodies.

Nothing else into the output. (The build also holds an empty `manifest.lock`
at the project root while it runs — tooling state that serializes builds,
never an asset; a beside-source layout, which serves the project root, carries
it as a zero-byte file. Beside it, a `.paste-pending.<digest>` journal exists
only while a failed or killed build's files, or a version whose removal
failed, await the next build, which removes them and then the journal.)
In particular the build does **not** write:

- rendered HTML pages;
- materialized combo responses under the combo `basePath` (e.g.
  `public/paste/<stamp>/<group>.min.js`);
- per-module loader files under `{basePath}/{packages-segment}/`;
- whole-application `assetgraph.js`/`assetgraph.css` rollups.

Combo responses and loader fetches are produced per request by `combo` —
the CLI command or the in-process FFI call behind the Scala/Node/Python
wrappers returns the response description for a host to write. `?require`
and `?filter` requests are dynamic for the same reason. Each `+vN` in a
versioned URL — carrying its `-h{hash}` qualifier under the default
`jam.hash-urls` policy, where a wrong hash is itself a `no-cache` 404 — is
served exactly: the current version or a retained history version whose
file is still on disk; a version the manifest cannot serve is a `no-cache`
404 rather than another version's content, and a stale stamp redirects to
the canonical URL, so a CDN never pins the wrong build under a year-cached
URL.

One automatic cleanup exception exists for the legacy aggregate files
`assetgraph.js`/`assetgraph.css` that older rollup-generating builds wrote
at the type output roots: a `dist` build removes them at the configured
roots and at roots an earlier manifest recorded as that type's root (the
manifest's `output_roots` field). The same name in any other directory —
the output base, an images root, a module subdirectory — is reported on
stderr and kept. A manifest that predates the field establishes no
historical roots, so an output-root move made during that first upgrade
can leave an old aggregate behind; treat the warning as a review prompt,
not a deletion instruction. The same review stance applies to combo,
loader, and page files published by the retired `templates.*` feature —
the build does not guess which old paths a site still serves.

## Development Workflow

`watch` rebuilds the assets and manifest on every source change, in the
same mode as `build` (`watch --debug` for unminified output):

```bash
paste-assetgraph watch
```

Page markup and combo URLs resolve at request time — run a host that wires
`combo` into its routing (see the runtime guides) when iterating on pages.

## Manifest Format

`public/manifest.json`:
```json
{
  "version": "2025.02.02.2156",
  "generated": "2025-02-02T21:56:00Z",
  "modules": {
    "main": "js/main.js",
    "site.theme": "css/main.css"
  },
  "assets": {
    "css/main.css": {
      "source": "assets/scss/main.scss",
      "output": "/css/main.def456.v1.min.css",
      "hash": "def456",
      "version": 1,
      "timestamp": "2025-02-02T21:56:00Z",
      "dependencies": [],
      "history": []
    },
    "js/main.js": {
      "source": "assets/js/main.js",
      "output": "/js/main.abc123.v1.min.js",
      "hash": "abc123",
      "version": 1,
      "timestamp": "2025-02-02T21:56:00Z",
      "dependencies": [],
      "history": []
    }
  },
  "sorted_deps": {
    "js": [["main", "/js/main.abc123.v1.min.js", 1]],
    "css": [["site.theme", "/css/main.def456.v1.min.css", 1]]
  }
}
```

## Deployment

Deploy `public/` to any file host — GitHub Pages, Netlify, S3 + CloudFront,
or a plain web server — and each versioned asset is servable directly by
its output path. The `/paste/...` combo, loader, `require`, `filter`, and
canonicalization URLs resolve through the combo handler: run it behind a
host using one of the runtime guides, or route those paths to a service
that invokes `combo`. The versioned filenames and stamps keep cache
busting automatic either way.
