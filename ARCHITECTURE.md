# paste-assetgraph Architecture

## Overview

`paste-assetgraph` is the primary PasteStack toolchain. It is a core domain —
the manifest aggregate plus the compile/scan pipeline — with two adapters,
and it runs no server of its own:

- **Preflight adapter:** the `build`/`watch` CLI commands, invoked at build
  time by the sbt plugin or anything else. They write the manifest, the
  checksum-named compiled files, and — for templated sites — the rendered
  pages a static host serves directly.
- **Runtime adapter:** the `render`/`combo` commands, exposed both as CLI
  invocations and in-process through the `ffi` crate's C ABI
  (`paste_render`/`paste_combo`/`paste_free`). `combo` returns a response
  description — status, cache headers, optional redirect `location`, body —
  and the host framework (Pekko, Express, FastAPI) writes the HTTP.

The sbt plugin is a thin JVM build wrapper around the CLI. The Scala, Node
and Python runtimes do not shell out to the CLI: they load the same Rust code
in-process through the FFI library, resolved from `PASTE_ASSETGRAPH_LIB` or a
bundled `native-lib/{platform}` copy. The CLI binary is not a runtime
dependency.

Capabilities:

- SCSS compilation with include paths
- JavaScript bundling, dependency resolution, and safe AST printing
- Asset versioning with content hashing of the emitted bytes
- Manifest generation for runtime asset resolution
- JAM (JavaScript Asset Manager) combo URL support
- Static-site `dist` rollups for hosts with no combo handler

## Source Discovery

Every configured source root — `source.dirs`, then the legacy `source.js` and
`source.scss` roots, then every `include-paths` entry — is walked recursively
in a deterministic name-sorted order, and each file is classified by extension:
`.js` is a JavaScript module, `.scss` and plain `.css` are style modules that
both compile through the grass → lightningcss path. The folder layout carries
no meaning: a directory may hold scripts and styles together, and a module's
name comes from its `@module` comment or its path relative to the walked root.
Only `_`-prefixed `.scss` files are partials; a `.css` file is always an asset.

The first root to claim a module name or a style asset key wins — later files
with the same name warn and are skipped. A walk never enters a configured
output directory, a vendored/build directory (`target`, `dist`,
`node_modules`, `.git`), or a directory named by `output.build-dir-name`.

## Output Layouts

`output.layout` selects where a compiled file lands:

- **`flat` (default):** every output under its type directory —
  `output.js/app.<hash>.v1.min.js`, manifest output `/js/app.<hash>.v1.min.js`.
  The runtime's output root is `output.base`.
- **`beside-source`:** a file discovered under a source root inside the
  project compiles into `<file dir>/<build-dir-name>/` beside its source —
  `src/main/assets/js/_paste_build/app.<hash>.v1.min.js` — and its manifest
  output is the project-relative path with forward slashes. An include-path
  asset is never written into another tree: it keeps flat placement under
  `output.base`, still recorded relative to the project root. Every output in
  this layout is relative to the project root, so the runtime's output root is
  the project root itself and `output.base` must sit inside it. A packaged
  deployment ships the `<build-dir-name>` directories at their
  project-relative paths. `output.build-dir-name` (default `_paste_build`)
  must be a single path segment and is excluded from source walks in either
  layout.

One resolver — `OutputDirs`/`compiled_file` — turns every manifest `output`
into a file for both build-time writes and runtime reads. Pruning derives the
directory and stem from that resolved path, so the same code prunes flat type
directories and beside-source build directories, and it matches the exact
versioned filename pattern rather than a stem prefix.

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
- Marks an asset whose source was deleted `retired`: it leaves the active
  set — no unversioned URL, rollup, or render resolves it — while every
  retained version still answers its exact pinned URL
- Versions roll on content *or* dependency-list changes; a requires-edit
  that compiles to identical bytes still produces a new version
- **Rollback support:** Yes (old per-module URLs remain accessible)

#### Relocation and retention integrity

`output.layout` and `output.build-dir-name` changes move where assets
are written. The manifest records `generated_dirs` — the project-relative
directories builds have generated into — so a relocation finds every
retained file in that finite, manifest-owned set of locations (there is
no recursive filesystem search). Each retained version is checked against
its recorded hash and rewritten at its new logical path, preserving its
version number and stamp; a retained file that is missing or fails the
hash check fails the build rather than falling back to current bytes.
Recorded generated directories are also excluded from source discovery,
so generated files can never re-enter the build as sources.

#### Rollup composition

Each rollup concatenates every asset of its type in dependency order: a module's
dependencies are emitted before the module itself, and assets left unnamed — a
stylesheet whose path-derived name was invalid or already claimed — follow in key
order. JavaScript
parts are joined by a newline, an empty statement, and a newline, so a file
ending in an expression cannot continue into the next file's leading
parenthesis. Stylesheets are joined by a newline.

#### Reproducible output

A `dist` build is meant to be committed and diffed, so identical sources must
produce identical bytes on every machine:

- Module and file ordering is sorted, never dependent on map iteration or
  directory order
- Manifest JSON is written with recursively sorted object keys
- Asset and history timestamps stay in the manifest — they are last-modified
  stamps from the source files, clamped to `SOURCE_DATE_EPOCH` when the build
  environment sets it, so a determinism job pins that variable and both
  platforms stamp identically. Only the build's own `generated` timestamp and
  the manifest `version` are normalized (to `static`); content hashes remain
  the asset identity.
- Each asset's `source` is recorded relative to the project root, with forward
  slashes, so it names the file rather than the machine that built it

### S3 Backend (Future)

Not implemented: `storage.backend` accepts only `local` and `dist`, and
any other value — `s3` included — is a configuration error rather than a
silent fallback.

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

Assets are versioned by hashing the emitted bytes:

1. **Compile:** the source is transformed — SCSS through grass, JS through
   oxc's parser, ES5-target minifier, and codegen printer.
2. **Hash calculation:** MD5 over the exact bytes that will be written. A
   comment-only change the printer drops produces identical bytes and keeps
   the version; a whitespace change that reaches the output rolls it. Because
   the hash is of emitted bytes, switching `minify` or changing the printer
   produces new filenames instead of silently reusing the previous bytes.
3. **Version increment:** the version number increases only when the hash
   changes; an unchanged hash carries its entry — including its timestamp —
   forward unchanged.
4. **Output format:** `{name}.{hash}.v{version}.min.{ext}`

Example: `app.0bdbb2c325525e986d55ef50ca66d056.v3.min.js`

### ES5 restoration passes

oxc's codegen in minify mode emits a few spellings ES5 cannot take, so two
restoration passes run over the printed output:

- Substitution-free, untagged template literals are rewritten as double-quoted
  strings (`` `x` `` is one byte shorter to print but a parse error on ES5);
  a whole-statement literal is parenthesized so it cannot become a directive.
- Object shorthand `{key}` is rewritten to `{key: key}` — with one exception:
  `{__proto__}` shorthand is an own data property while `__proto__: value`
  longhand is a prototype setter, so there is no safe longhand spelling. When
  the printed output contains it, minification fails and the build emits the
  original unminified source instead of silently changing semantics.

Any other parse or print failure takes the same fallback to the unminified
source rather than emitting broken output.

### Module Names and Asset Keys

An asset's key in the manifest is `{type}/{path}.{type}` — for JavaScript,
derived from the declared module name. The `modules` map points a declared name
at that key, never at a source path, because every consumer resolves a module by
looking the value up in `assets`. A name declared for both a script and a
stylesheet keeps the entry that the key convention cannot reconstruct; the other
resolves through the convention key.

## Rolling Deploys

During a rolling deploy, users may still have old HTML pointing to old asset URLs.

**Problem:**
1. User loads page → gets `<script src="/js/app.v5.js">`
2. Deploy happens → `v6` deployed, `v5` deleted
3. Browser requests `v5` → 404 → broken page

**Solution (dist/s3 backends):**
- Keep old versions available (`storage.keep-versions`)
- Old URLs continue to work — a `+vN` pin resolves to that version's retained file
- A version the server cannot serve (newer than the manifest, or pruned) is a
  `no-cache` 404, never another version's content
- Prune old versions periodically (keep last N)

## JAM Combo URLs

JAM (JavaScript Asset Manager) serves multiple modules concatenated:

**URL format:** `/paste/{stamp}/{module1}+v{N}.0[-h{hash}]~{module2}+v{M}.0[-h{hash}].min.{js|css}`

- The `{stamp}` segment is the latest last-modified stamp — in epoch seconds —
  among the listed members, at the versions being rendered. A new version is
  stamped from its source file's mtime (clamped to `SOURCE_DATE_EPOCH` when
  set); an unchanged asset carries its stamp forward, so the segment moves
  only when a member of the group actually changed.
- Each `+vN` pin is served exactly — current version or retained history —
  so a year-cached URL never carries another build's content.
- Under the default hash-qualified policy each pin also carries the
  resolved version's full 32-character MD5 content hash as `-h{hash}`: two
  builds minting the same version number and stamp for different bytes
  still produce different URLs, and a request naming a hash that is not
  the recorded one is a `no-cache` 404 — checked before any redirect —
  never a redirect onto the recorded bytes. The build records the policy
  in the manifest's `hash_urls` field; `jam.hash-urls = false` emits and
  serves the legacy `+vN.0` form, manifests written before the field
  existed behave as legacy, and a correctly qualified request still
  resolves under an opted-out manifest. The policy governs emitted tokens
  for versioned JAM URLs — a hash supplied on a `require` or unversioned
  request is still validated, and those responses stay `no-cache` — and
  opting out changes URL spelling only, not hashing, version increments,
  or stamp carry-forward.
- A hash-qualified list may continue over `/`-separated path components so
  each component fits the filesystem limit when a static build
  materializes the URL as a file; the combo endpoint rejoins the
  components into one ordered list.
- The canonical URL orders all-current groups by the manifest's dependency
  layering; a group containing a historical pin keeps the listed order, the
  order the page that minted the URL depends on.
- A request whose list or stamp is not canonical — a stale stamp, a
  non-numeric segment, an unsorted list, an unpinned name — answers 302 with
  a `Location` pointing at the canonical URL, never a year-cached wrong URL.
  A stamp newer than anything the manifest knows is a `no-cache` 404 (a URL
  from a newer build hitting an older server).
- The runtime loader's `{packages-segment}/{module}.js` is the one unversioned
  cacheable form; the segment is the build's JS content fingerprint.
- The `combo` runtime adapter concatenates the requested modules on-the-fly,
  separating JavaScript modules with a terminating empty statement, and
  returns the response description for the host framework to write.
- A request with or without `.min.` serves the same compiled output — the
  suffix only names the URL form.

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
source.dirs = ["src/js", "src/scss"]
output.base = "dist"
storage.backend = "dist"
storage.keep-versions = 10
manifest = "dist/manifest.json"
```

A build that wants output beside its sources instead:

```hocon
source.dirs = ["src"]
output.layout = "beside-source"
output.build-dir-name = "_paste_build"
output.base = "dist"          # must sit inside the project root
storage.backend = "dist"
manifest = "dist/manifest.json"
```

## Manifest Format

This excerpt shows the entry fields; the generated manifest also contains
every referenced module and dependency.

```json
{
  "version": "2026.02.03.0841",
  "generated": "2026-02-03T08:41:00Z",
  "assets": {
    "js/app.js": {
      "source": "src/main/assets/js/app.js",
      "output": "/js/app.0bdbb2c325525e986d55ef50ca66d056.v3.min.js",
      "hash": "0bdbb2c325525e986d55ef50ca66d056",
      "version": 3,
      "timestamp": "2026-02-03T08:41:00Z",
      "dependencies": ["paste", "paste.dom"],
      "history": [
        {"version": 2, "hash": "90affbd9a1954ec9ff029b7ad7183a16", "output": "/js/app.90affbd9a1954ec9ff029b7ad7183a16.v2.min.js",
         "timestamp": "2026-01-20T09:15:00Z"},
        {"version": 1, "hash": "7e9e92b0857c477a6e8aebb0ab2bd924", "output": "/js/app.7e9e92b0857c477a6e8aebb0ab2bd924.v1.min.js",
         "timestamp": "2026-01-05T16:02:00Z"}
      ]
    }
  },
  "generated_dirs": ["dist/js", "dist/css"],
  "hash_urls": true
}
```

- `assets[key].retired` is `true` only on an entry whose source was
  deleted — the field is always written, and absent in an older manifest
  means active: it resolves exact `+vN` pins but nothing unpinned.
- `generated_dirs` lists project-relative generated directories carried
  forward across output-layout and build-directory changes.
- `hash_urls` records whether the build emits hash-qualified version
  tokens — always written, absent in a pre-field manifest means the
  legacy `+vN.0` form.

## Future Enhancements

- [ ] S3 storage backend implementation
- [ ] CloudFront invalidation integration
- [ ] Source map generation
- [ ] Image optimization pipeline
