# paste-assetgraph Architecture

## Overview

`paste-assetgraph` is a Rust-based asset pipeline for PasteStack that handles:
- SCSS compilation with include paths
- JavaScript bundling and dependency resolution
- Asset versioning with content hashing
- Manifest generation for runtime asset resolution
- JAM (JavaScript Asset Manager) combo URL support

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

- Writes versioned assets to `dist/` directory
- Preserves old versions for rolling deploys
- Assets should be committed to source control
- Prunes old versions beyond `keep-versions` limit
- **Rollback support:** Yes (old versions remain accessible)

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
2. **Version increment:** Version number increases only when hash changes
3. **Output format:** `{name}.{hash}.v{version}.min.{ext}`

Example: `app.abc123def456.v3.min.js`

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
- Server concatenates requested modules on-the-fly

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
- [ ] CSS/JS minification (lightningcss, swc)
- [ ] Image optimization pipeline
