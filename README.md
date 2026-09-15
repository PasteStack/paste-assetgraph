# paste-assetgraph

Primary PasteStack CLI toolchain. The Rust binary performs init, build, minification, dependency
resolution, manifest generation, JAM serving, and static dist rollups; the Scala runtime and sbt
plugin are optional wrappers for JVM consumers.

## Structure

```
paste-assetgraph/
├── src/                    # Rust binary (builds manifest)
├── runtime/
│   └── scala/              # Scala lib (Pekko HTTP routes, manifest reader, sbt plugin)
└── Cargo.toml
```

## Features

- **Auto-versioning** — Content hash-based versioning for cache busting
- **Dependency graph** — Topological sort of JS modules based on `@requires` comments
- **SCSS compilation** — Compiles SCSS to CSS (via grass)
- **Minification** — Minifies JS and CSS for production
- **Manifest generation** — JSON manifest mapping source paths to versioned output
- **JAM endpoint** — Serves bundled modules via cacheable URLs
- **Static dist rollups** — Emits `dist/js/assetgraph.js` and `dist/css/assetgraph.css` for plain static hosts
- **Version history** — Keeps old fingerprinted module files for rolling deploys
- **Scala runtime** — Pekko HTTP routes for serving assets when a server-side combo handler is desired

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
# Production build
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

### Serve JAM endpoint

```bash
paste-assetgraph serve --port 8080
```

## Documentation

- [Basic HTML Sites](docs/basic-html.md) — Static sites with shell scripts
- [Python/FastAPI](docs/python-fastapi.md) — FastAPI with Jinja2 templates
- [Scala/Pekko HTTP](docs/scala-pekko.md) — Pekko HTTP routes, Twirl templates

## Configuration

See `paste-surface.conf`:

```hocon
paste.surface.assets {
  source {
    js = "src/main/assets/js"
    scss = "src/main/assets/scss"
  }
  output {
    base = "src/main/public"
    js = ${paste.surface.assets.output.base}"/js"
    css = ${paste.surface.assets.output.base}"/css"
  }
  manifest = ${paste.surface.assets.output.base}"/manifest.json"
}
```

## Manifest Format

```json
{
  "version": "2025.02.02.1",
  "generated": "2025-02-02T21:30:00Z",
  "assets": {
    "js/paste.js": {
      "source": "src/main/assets/js/paste.js",
      "output": "/js/paste.abc123.min.js",
      "hash": "abc123",
      "dependencies": []
    },
    "js/paste.dom.js": {
      "source": "src/main/assets/js/dom.js",
      "output": "/js/paste.dom.def456.min.js",
      "hash": "def456",
      "dependencies": ["paste"]
    }
  }
}
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

## License

Apache License, Version 2.0. See `LICENSE` and `NOTICE.md`.
