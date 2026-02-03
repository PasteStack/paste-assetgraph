# paste-assetgraph

Asset pipeline for PasteStack. Rust binary + Scala runtime library.

## Structure

```
paste-assetgraph/
├── src/                    # Rust binary (builds manifest)
├── runtime/
│   └── scala/              # Scala lib (Akka HTTP routes, manifest reader)
└── Cargo.toml
```

## Features

- **Auto-versioning** — Content hash-based versioning for cache busting
- **Dependency graph** — Topological sort of JS modules based on `@requires` comments
- **SCSS compilation** — Compiles SCSS to CSS (via grass)
- **Minification** — Minifies JS and CSS for production
- **Manifest generation** — JSON manifest mapping source paths to versioned output
- **JAM endpoint** — Serves bundled modules via cacheable URLs
- **Version history** — Keeps last 5 versions for rolling deploys
- **Scala runtime** — Akka HTTP routes for serving assets

## Installation

### Pre-built binaries

```bash
# macOS (Apple Silicon)
curl -L https://github.com/PasteStack/paste-assetgraph/releases/latest/download/paste-assetgraph-darwin-arm64 -o /usr/local/bin/paste-assetgraph
chmod +x /usr/local/bin/paste-assetgraph

# macOS (Intel)
curl -L https://github.com/PasteStack/paste-assetgraph/releases/latest/download/paste-assetgraph-darwin-amd64 -o /usr/local/bin/paste-assetgraph

# Linux
curl -L https://github.com/PasteStack/paste-assetgraph/releases/latest/download/paste-assetgraph-linux-amd64 -o /usr/local/bin/paste-assetgraph
```

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
- [Scala/Akka HTTP](docs/scala-akka.md) — Akka HTTP routes, Twirl templates

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

## License

MIT
