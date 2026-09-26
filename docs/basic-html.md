# Using paste-assetgraph with Basic HTML Sites

No server required — `build` renders the JAM markup into your HTML at build
time and materializes every file the pages reference. Deploy the output
directory to any static host.

## Quick Start

```bash
# 1. Install the binary — see the README's release-asset steps
# 2. Init (writes paste-surface.conf with templates.* keys)
paste-assetgraph init -t basic

# 3. Build
paste-assetgraph build
```

## Project Structure

```
my-site/
├── paste-surface.conf      # Config
├── assets/                 # Source files
│   ├── js/
│   │   └── main.js
│   └── scss/
│       └── main.scss
├── templates/              # HTML templates with paste directives
│   └── index.html
└── public/                 # Generated (deploy this)
    ├── manifest.json
    ├── index.html          # rendered template
    ├── js/
    │   └── main.abc123.v1.min.js
    ├── css/
    │   └── main.def456.v1.min.css
    └── paste/              # combo files + loader files the pages reference
```

## Configuration

`paste-surface.conf` (as `init -t basic` writes it, with `source.dirs` in
place of the legacy `source.js`/`source.scss` keys):
```hocon
debug-mode = false

source.dirs = ["assets/js", "assets/scss"]

output.base = "public"
output.js = "public/js"
output.css = "public/css"
output.images = "public/images"

templates.source = "templates"   # html files rendered at build time
templates.output = "public"      # rendered pages + combo/loader files

storage.backend = "dist"
storage.keep-versions = 10
manifest = "public/manifest.json"

minify = true
source-maps = true

jam.enabled = true
jam.base-path = "/paste"
jam.inline-threshold = 1024      # automatic inline limit in bytes; 0 disables
```

`templates.output` defaults to `output.base` when only `templates.source` is
set. The build fails if `templates.output` equals `templates.source` or nests
inside it — output can never overwrite its source templates.

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
the checkout's file mtimes (clamped by `SOURCE_DATE_EPOCH` when set), so URLs
are correct but change per checkout and old URLs redirect to the current
build's.

## HTML Template

Directives are HTML comments. `paste:jam` records a group (it renders to
nothing); `paste:dump css|js` emits that type's markup for everything
jammed so far, and a bare `paste:dump` emits both — css then js — in one
dump.

`templates/index.html`:
```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>My Site</title>

  <!-- paste:jam css="site.theme" js="main" -->
  <!-- paste:dump css -->
</head>
<body>
  <h1>Hello World</h1>

  <!-- paste:dump js -->
</body>
</html>
```

Jam attributes: `css="…"` and `js="…"` take quoted module lists (split on
commas and whitespace); `inline` and `sync` are valueless flags; `media="…"`
takes a quoted value. End of file finishes the page: a `paste:jam` after the
last `paste:dump` in a file is a build error, and so is a jam queued after
its own type's last dump — a css jam below `<!-- paste:dump css -->` would
never be emitted, and the completion check fails the build naming the module
instead of silently dropping it. So css bound for `<head>` must be jammed
before the head's css dump; completion detects the omission, it cannot move
the tag back. Any malformed directive is a build error too; the error names
the template file and line.

After `build`, `public/index.html` contains the same markup a runtime `render`
call produces: an inlined runtime with `paste.config({packagesUrl:…})`,
`<link>` or inline `<style>` for CSS, and `isLoading` loader tags or
`<script src>` for JS.

## What build writes

- **Rendered pages** — every `*.html` under `templates.source`, at the same
  relative path under `templates.output`; non-HTML files and directive-free
  HTML copy through byte-identical.
- **Combo files** — every external URL the rendered markup references is
  materialized under `output.base` (e.g.
  `public/paste/1770462000/paste.dom+v2.0-h<content hash>.min.js`).
  A URL whose combo response is not 200 fails the build. When a
  hash-qualified list would exceed the filesystem's 255-byte component
  limit, the URL continues over additional `/`-separated path components —
  so the materialized file may sit in nested directories under the stamp
  segment; the combo endpoint rejoins them into the same ordered group.
- **Loader files** — every manifest module with a JS asset gets a file at
  `{output.base}/{base_path}/{packages-segment}/{module}.js` for the
  `paste.require` dynamic loader.
- **Compiled assets and manifest** — under `output.js`, `output.css`, and the
  `manifest` path as usual.

`templates.output` receives rendered HTML only — the combo and loader files go
under `output.base`. Pair the two (as `init -t basic` does) or serve both when
deploying.

## Development Workflow

### Watch mode

Terminal 1:
```bash
paste-assetgraph watch
```

Terminal 2 (simple HTTP server):
```bash
cd public && python -m http.server 8000
```

`watch` rebuilds on change, which re-renders the templates too.

### Optional: runtime combo resolution

`?require`/`?filter` requests are dynamic — they cannot be materialized as
static files, so they go through a runtime adapter: the `combo` CLI command
or the in-process FFI call behind the Scala/Node/Python wrappers returns the
response description for a host to write. The static site itself needs none
of this — the combo responses its pages reference are already files under
`public/paste/`. Each `+vN` in a versioned URL — carrying its `-h{hash}`
qualifier under the default `jam.hash-urls` policy, where a wrong hash is
itself a `no-cache` 404 — is served exactly: the current version or a
retained history version whose file is still on disk; a
version the manifest cannot serve is a `no-cache` 404 rather than another
version's content, and a stale stamp redirects to the canonical URL, so a CDN
never pins the wrong build under a year-cached URL.

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

Deploy the `public/` folder to any static host:
- GitHub Pages
- Netlify
- Vercel
- S3 + CloudFront
- Any web server

The versioned filenames and timestamps in the combo URLs ensure cache busting
automatically — no request-time server is needed.
