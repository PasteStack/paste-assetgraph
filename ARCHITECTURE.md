# paste-assetgraph Architecture

## Overview

`paste-assetgraph` is the primary PasteStack toolchain: one Rust static binary that handles:
- SCSS compilation with include paths
- JavaScript bundling, dependency resolution, and safe AST printing
- Asset versioning with content hashing
- Manifest generation for runtime asset resolution
- JAM (JavaScript Asset Manager) combo URL support
- Static-site `dist` rollups for hosts with no combo handler

The sbt plugin is a thin JVM build wrapper around this CLI. Other consumers invoke the CLI directly;
the binary is not a server runtime dependency.

## Storage Backends

The asset pipeline supports multiple storage backends to accommodate different deployment strategies:

### Local Backend (Default)

**Use case:** Scala/sbt projects, standard web frameworks

```hocon
storage.backend = "local"
output.base = "target/public"
```

- Writes assets to `target/` directory (gitignored)
- No version retention - files are overwritten on rebuild
- Relies on CI/CD for rollback (rebuild from git history)
- **Rollback support:** No (requires rebuild)

### Dist Backend

**Use case:** Static HTML sites, projects that need rolling deploy support

```hocon
storage.backend = "dist"
storage.keep-versions = 10
output.base = "dist"
```

- Writes versioned per-module assets to `dist/js/` and `dist/css/`
- Emits `dist/js/assetgraph.js` and `dist/css/assetgraph.css` rollups
- Static HTML references those rollups, so no combo handler or server-side manifest lookup is needed
- Preserves old per-module versions for rolling deploys
- Prunes old versions beyond `keep-versions` limit
- **Rollback support:** Yes (old per-module URLs remain accessible)

#### Rollup composition

Each rollup concatenates every asset of its type in dependency order: a module's
dependencies are emitted before the module itself, and assets with no declared
module name (a stylesheet without `@module`) follow in key order. JavaScript
parts are joined by a newline, an empty statement, and a newline, so a file
ending in an expression cannot continue into the next file's leading
parenthesis. Stylesheets are joined by a newline.

#### Reproducible output

A `dist` build is meant to be committed and diffed, so identical sources must
produce identical bytes on every machine:

- Module and file ordering is sorted, never dependent on map iteration or
  directory order
- Manifest JSON is written with recursively sorted object keys
- Build timestamps are dropped and the manifest version becomes `static`;
  content hashes remain the asset identity
- Each asset's `source` is recorded relative to the project root, with forward
  slashes, so it names the file rather than the machine that built it

### S3 Backend (Future)

**Use case:** Production deployments with CDN, large-scale applications

```hocon
storage.backend = "s3"
storage.bucket = "my-assets-bucket"
storage.prefix = "paste/"
storage.keep-versions = 10
```

- Writes assets to S3 bucket
- Integrates with CloudFront or other CDNs
- Version retention managed in S3
- **Rollback support:** Yes

## Project Type Conventions

| Project Type | Source | Output | Backend | Rollback |
|--------------|--------|--------|---------|----------|
| Scala/sbt | `src/main/assets/` | `target/public/` | local | No |
| HTML/Static | `src/` | `dist/` | dist | Yes |
| Python/FastAPI | `assets/` | `static/` | dist | Yes |

## Asset Versioning

Assets are versioned using content hashing:

1. **Hash calculation:** Content is stripped of comments, then MD5 hashed
   together with the identity of the pipeline that prints the output
2. **Version increment:** Version number increases only when hash changes
3. **Output format:** `{name}.{hash}.v{version}.min.{ext}`

Example: `app.abc123def456.v3.min.js`

An output file whose name already exists is never rewritten, so the hash has to
cover the transformation as well as the source. It therefore includes a
transform signature (the JS and CSS printers and their options) and whether the
build emits printed or verbatim output. Switching `minify` off, or changing the
printer, produces new filenames instead of silently reusing the previous bytes.

### Module Names and Asset Keys

An asset's key in the manifest is `{type}/{path}.{type}` — for JavaScript,
derived from the declared module name. The `modules` map points a declared name
at that key, never at a source path, because every consumer resolves a module by
looking the value up in `assets`. A name declared for both a script and a
stylesheet keeps the entry that the key convention cannot reconstruct; the other
resolves through the convention key.

### Comment Stripping

Comments are stripped before hashing to prevent comment-only changes from triggering new versions:
- JS: `//` line comments and `/* */` block comments
- CSS: `/* */` block comments

## Rolling Deploys

During a rolling deploy, users may still have old HTML pointing to old asset URLs.

**Problem:**
1. User loads page → gets `<script src="/js/app.v5.js">`
2. Deploy happens → `v6` deployed, `v5` deleted
3. Browser requests `v5` → 404 → broken page

**Solution (dist/s3 backends):**
- Keep old versions available
- Old URLs continue to work
- Prune old versions periodically (keep last N)

## JAM Combo URLs

JAM (JavaScript Asset Manager) serves multiple modules concatenated:

**URL format:** `/paste/{timestamp}/{module1.v1,module2.v1}.js`

- Timestamp changes when any module in the combo changes
- Enables aggressive caching while ensuring freshness
- Server concatenates requested modules on-the-fly, separating JavaScript
  modules with a terminating empty statement
- A `.min.` request serves compiled output; a request without it serves the
  module sources, so an edited file is visible without a rebuild

## Configuration

### Scala/sbt (via plugin)

```scala
// build.sbt
assetGraphSourceDirs := Seq("src/main/assets/scss", "src/main/assets/js")
assetGraphOutputDir := "target/public"
assetGraphIncludePaths := Seq("..")
```

### HTML/Python (via config file)

```hocon
# paste-assetgraph.conf
source.scss = "src/scss"
source.js = "src/js"
output.base = "dist"
storage.backend = "dist"
storage.keep-versions = 10
manifest = "dist/manifest.json"
```

## Manifest Format

```json
{
  "version": "2026.02.03.0841",
  "generated": "2026-02-03T08:41:00Z",
  "assets": {
    "js/app.js": {
      "source": "src/main/assets/js/app.js",
      "output": "/js/app.abc123.v3.min.js",
      "hash": "abc123def456",
      "version": 3,
      "timestamp": "2026-02-03T08:41:00Z",
      "dependencies": ["paste", "paste.dom"],
      "history": [
        {"version": 2, "hash": "xyz789", "output": "/js/app.xyz789.v2.min.js"},
        {"version": 1, "hash": "first00", "output": "/js/app.first00.v1.min.js"}
      ]
    }
  }
}
```

## Future Enhancements

- [ ] S3 storage backend implementation
- [ ] CloudFront invalidation integration
- [ ] Source map generation
- [ ] Image optimization pipeline
