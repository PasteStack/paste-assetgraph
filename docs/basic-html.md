# Using paste-assetgraph with Basic HTML Sites

No server required. Just the CLI and a simple build script.

## Quick Start

```bash
# 1. Install
curl -L https://github.com/PasteStack/paste-assetgraph/releases/latest/download/paste-assetgraph-$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m) -o /usr/local/bin/paste-assetgraph
chmod +x /usr/local/bin/paste-assetgraph

# 2. Init
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
├── templates/              # HTML templates
│   └── index.html
├── public/                 # Generated (deploy this)
│   ├── manifest.json
│   ├── index.html
│   ├── js/
│   │   └── main.abc123.v1.min.js
│   └── css/
│       └── main.def456.v1.min.css
└── build.sh                # Build script
```

## Configuration

`paste-surface.conf`:
```hocon
paste.surface.assets {
  source {
    js = "assets/js"
    scss = "assets/scss"
  }
  output {
    base = "public"
    js = "public/js"
    css = "public/css"
  }
  manifest = "public/manifest.json"
}
```

## HTML Template

`templates/index.html`:
```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>My Site</title>
  <link rel="stylesheet" href="{{CSS_MAIN}}">
</head>
<body>
  <h1>Hello World</h1>
  
  <script src="{{JS_MAIN}}" defer></script>
</body>
</html>
```

## Build Script

`build.sh`:
```bash
#!/bin/bash
set -e

# Build assets
paste-assetgraph build

# Read manifest and inject paths into HTML
CSS_MAIN=$(jq -r '.assets["css/main.css"].output' public/manifest.json)
JS_MAIN=$(jq -r '.assets["js/main.js"].output' public/manifest.json)

# Generate HTML
sed -e "s|{{CSS_MAIN}}|$CSS_MAIN|g" \
    -e "s|{{JS_MAIN}}|$JS_MAIN|g" \
    templates/index.html > public/index.html

echo "Build complete. Deploy public/ folder."
```

Make it executable:
```bash
chmod +x build.sh
```

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

### JAM server (bundled modules)

If you have multiple JS modules with dependencies:
```bash
paste-assetgraph serve --port 8080
```

Then in your HTML:
```html
<script src="http://localhost:8080/paste/2025.02.02/abc123/paste.dom,paste.ui.heroscroll.min.js" defer></script>
```

## Manifest Format

`public/manifest.json`:
```json
{
  "version": "2025.02.02.2156",
  "generated": "2025-02-02T21:56:00Z",
  "assets": {
    "css/main.css": {
      "source": "assets/scss/main.scss",
      "output": "/css/main.def456.v1.min.css",
      "hash": "def456",
      "version": 1
    },
    "js/main.js": {
      "source": "assets/js/main.js",
      "output": "/js/main.abc123.v1.min.js",
      "hash": "abc123",
      "version": 1
    }
  }
}
```

## Deployment

Just deploy the `public/` folder to any static host:
- GitHub Pages
- Netlify
- Vercel
- S3 + CloudFront
- Any web server

The versioned filenames ensure cache busting automatically.

## Advanced: Multiple Pages

For multiple pages, extend the build script:

```bash
#!/bin/bash
set -e

paste-assetgraph build

CSS_MAIN=$(jq -r '.assets["css/main.css"].output' public/manifest.json)
JS_MAIN=$(jq -r '.assets["js/main.js"].output' public/manifest.json)

for template in templates/*.html; do
  filename=$(basename "$template")
  sed -e "s|{{CSS_MAIN}}|$CSS_MAIN|g" \
      -e "s|{{JS_MAIN}}|$JS_MAIN|g" \
      "$template" > "public/$filename"
done
```

## Advanced: Using Make

`Makefile`:
```makefile
.PHONY: build watch clean

MANIFEST = public/manifest.json
TEMPLATES = $(wildcard templates/*.html)
HTML = $(patsubst templates/%.html,public/%.html,$(TEMPLATES))

build: $(HTML)

$(MANIFEST): assets/js/* assets/scss/*
	paste-assetgraph build

public/%.html: templates/%.html $(MANIFEST)
	@mkdir -p public
	CSS=$$(jq -r '.assets["css/main.css"].output' $(MANIFEST)); \
	JS=$$(jq -r '.assets["js/main.js"].output' $(MANIFEST)); \
	sed -e "s|{{CSS_MAIN}}|$$CSS|g" -e "s|{{JS_MAIN}}|$$JS|g" $< > $@

watch:
	paste-assetgraph watch

clean:
	rm -rf public/*
```

Then:
```bash
make build
make watch
make clean
```
