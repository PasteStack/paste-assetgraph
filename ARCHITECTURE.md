# paste-assetgraph Architecture

## Overview

`paste-assetgraph` is the primary PasteStack toolchain. It is a core domain —
the manifest aggregate plus the compile/scan pipeline — with two adapters,
and it runs no server of its own:

- **Preflight adapter:** the `build`/`watch` CLI commands, invoked at build
  time by the sbt plugin or anything else. They write the manifest and the
  checksum-named compiled files — individual versioned assets plus metadata
  only.
- **Runtime adapter:** the `render`/`combo` commands, exposed both as CLI
  invocations and in-process through the `ffi` crate's C ABI
  (`paste_render_page`/`paste_render`/`paste_combo`/`paste_combo_conditional`/
  `paste_free`). `combo`
  returns a response
  description — status, cache headers, optional redirect `location`,
  group `lastModified`, body —
  and the host framework (Pekko, Express, FastAPI) writes the HTTP.

The sbt plugin is a thin JVM build wrapper around the CLI. The Scala, Node
and Python runtimes do not shell out to the CLI: they load the same Rust code
in-process through the FFI library, resolved from `PASTE_ASSETGRAPH_LIB` or a
bundled `native-lib/{platform}` copy. The CLI binary is not a runtime
dependency.

Capabilities:

- SCSS compilation with include paths
- JavaScript compilation, dependency resolution, and safe AST printing
- Asset versioning with content hashing of the emitted bytes
- Manifest generation for runtime asset resolution
- JAM (JavaScript Asset Manager) combo URL support

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

### Local Backend

**Use case:** throwaway output where no saved page outlives a rebuild — an
explicit opt-in; `dist` is the default

```hocon
storage.backend = "local"
output.base = "target/public"
```

- Writes assets to `target/` directory (gitignored)
- No version retention — local builds publish the current individual
  content-hashed versions; prior unreferenced versions are removed rather
  than retained for URL-level rollback. Dist keeps version history by
  default.
- Relies on CI/CD for rollback (rebuild from git history)
- **Rollback support:** No (requires rebuild)

### Dist Backend (Default)

**Use case:** every project that serves pages across deploys — static HTML
sites, framework apps, rolling deploys. Retention lasts as long as the output
directory: an output under a cleaned build directory (`target/`) starts over
after a clean, so keep it on persistent storage or publish each build's
versioned files append-only to the serving origin

```hocon
storage.backend = "dist"
storage.keep-versions = all
output.base = "dist"
```

- Writes versioned per-module assets to `dist/js/` and `dist/css/`
- Preserves old per-module versions for rolling deploys: every version ever
  built is retained by default; `storage.keep-versions` bounds that to the
  most recent `N` (`0` keeps current only) when a deployment needs a cap.
  The bound applies to compiled per-asset history — active and retired
  entries alike; persisted module content exists only in those individual
  versioned files — no rollups, materialized combos, or loader copies
- Marks an asset whose source was deleted `retired`: it leaves the active
  set — no unversioned URL or render resolves it — while every
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

Pruning removes only what the loaded manifest recorded and retention has
since retired. A versioned file no loaded manifest names — the manifest was
deleted or replaced by an older copy, or the file came from elsewhere — may
still back a saved page, so it is left in place under any bound; a build
that starts without a manifest prunes nothing.

What a build creates or fails to remove is tracked in a pending-output
journal at the project root, beside `manifest.lock` and outside every
output tree: `.paste-pending.<digest>`, the digest naming the manifest's
physical path relative to the physical project root (its absolute physical
path when it sits outside the project), so two configurations sharing a
project root keep separate journals, every spelling of one project (a
symlinked root, an absolute or relative manifest path) finds the same one,
and a project moved or renamed as a whole keeps it. A build
records each new versioned file there before writing it, and each retired
version whose unlink failed. Just before it publishes, every entry the new
manifest names leaves the journal, so the journal only ever lists files no
published manifest named — a build killed after publishing leaves nothing
of what it published there. The build keeps its own list in memory across
that step, so a publication that fails (a full or read-only disk) is
settled against the manifest still on disk and its new files removed; only
a build killed between that step and the publish leaves its new files
behind, unlisted. Once the publish succeeded, the files it named are never
taken back by that build's closing settle — not when the manifest cannot be
read at that moment, not when it was deleted meanwhile. Every build starts and ends by settling the
journal under the build lock: each listed file the manifest on disk does
not name is removed, a failed unlink stays listed for the next build, and
the journal is deleted once empty. So a failed build, a killed build, and
a prune that failed (a file held open, an immutable flag) all converge:
the next build that can unlink the file does.

Settling never acts without its authority. With the manifest missing, no
journal entry is removed and they are forgotten — the manifest that build
then publishes starts a new lineage that never judged them; the build's own
new files, which no manifest ever named, still leave with a failed build. An
unreadable manifest settles nothing and keeps the entries. The journal is
rewritten in place (staged and renamed), never appended to across builds,
so it cannot grow while nothing settles. An entry no build may remove —
outside the project and every output root, not a versioned name, or
behind a symlinked component — is dropped with a warning and its file
left in place. A journal lost mid-rewrite only leaves files behind.

Every build verifies each retained version it materializes: one bounded
read and an MD5 per recorded version, current or history. The cost grows
with retained history — a no-op rebuild after 100 builds of four 500 KB
assets measured about 0.4 s — and is the price of never
serving a tampered or truncated retained file. File size and mtime are
not trusted as a shortcut: a checkout resets mtimes, and a same-size
rewrite can keep them. A deployment where this matters bounds history with
`storage.keep-versions`.

#### Serving the output

A build produces individual versioned files and the manifest only — page
markup and combo responses are computed per request by the runtime adapter
(`render`/`combo`, in-process through the FFI or as CLI commands). A plain
file host can serve each versioned asset directly by its manifest output
path; the `/paste`-style combo, loader, and canonicalization URLs require
the combo handler. Without one, a page loads the files itself through the
paste runtime — `paste.require` with the full URL of each versioned file,
each request issued in dependency order after the previous file loads, or
bare module names against an origin that publishes `<module>.js`
filenames. The retired `templates.source`/`templates.output` publication
keys are a configuration error. See the delivery model in `JAM_SPEC.md`
and `docs/basic-html.md`.

#### Reproducible output

A `dist` build is meant to be committed and diffed, so identical sources must
produce identical bytes on every machine:

- Module and file ordering is sorted, never dependent on map iteration or
  directory order
- Manifest JSON is written with recursively sorted object keys
- Asset and history timestamps stay in the manifest — they are last-modified
  stamps from the source files, clamped to `SOURCE_DATE_EPOCH` when the build
  environment sets it, so a determinism job pins that variable and both
  platforms stamp identically. Only the build's own `generated` timestamp
  (emptied) and the manifest `version` (set to `static`) are normalized;
  content hashes remain the asset identity.
- Each asset's `source` is recorded relative to the project root, with forward
  slashes, so it names the file rather than the machine that built it
- The configured CDN is recorded in the manifest as `cdn_base` — a build-time
  default origin for emitted URLs, not part of the asset bytes. Different
  environments render the same committed build and override the effective
  prefix with a runtime `cdnBase` (`--cdn-base`, settings `cdnBase`), so the
  artifact stays byte-identical while each host points the tags where it
  needs.

### S3 Backend (Future)

Not implemented: `storage.backend` accepts only `local` and `dist`, and
any other value — `s3` included — is a configuration error rather than a
silent fallback.

**Use case:** Production deployments with CDN, large-scale applications

```hocon
storage.backend = "s3"
storage.bucket = "my-assets-bucket"
storage.prefix = "paste/"
storage.keep-versions = all
```

- Writes assets to S3 bucket
- Integrates with CloudFront or other CDNs
- Version retention managed in S3
- **Rollback support:** Yes

## Project Type Conventions

| Project Type | Source | Output | Backend | Rollback |
|--------------|--------|--------|---------|----------|
| Scala/sbt | `src/main/assets/` | `target/public/` | dist | Yes (while the output persists) |
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
3. **Version increment:** a new version is created when emitted content
   changes or the recorded dependency contract changes. The content hash
   identifies emitted bytes; the numeric version identifies the complete
   retained asset version, including its dependency relationship — a
   `@requires` edit rolls the version even when the emitted bytes hash
   identically. An unchanged identity carries its entry — including its
   timestamp — forward unchanged. A version's recorded public output path
   is likewise immutable: a rebuild that would place it elsewhere issues
   the next version instead and retains the old record.
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

`.min` is an output and URL suffix, never part of a module name or key: a
source's `.min` is stripped with its extension (`vendor.min.js` → module
`vendor`, key `js/vendor.js`), a module name ending in `.min` is a build
error, and a source beside its own `.min` copy fails the build naming both.
The combo endpoint can therefore read a URL's suffix — one trailing
`.min.{ext}`, else `.{ext}` — without consulting the manifest.

## Rolling Deploys

During a rolling deploy, users may still have old HTML pointing to old asset URLs.

**Problem:**
1. User loads page → gets `<script src="/js/app.v5.js">`
2. Deploy happens → `v6` deployed, `v5` deleted
3. Browser requests `v5` → 404 → broken page

**Solution (dist/s3 backends):**
- Keep old versions available — the default retains every version;
  `storage.keep-versions` bounds it only when a deployment must cap history
- Old URLs continue to work — a `+vN` pin resolves to that version's retained file
- A version the server cannot serve (newer than the manifest, or pruned) is a
  `no-cache` 404, never another version's content
- A deployment that must cap history sets `storage.keep-versions` explicitly —
  the default keeps everything, so no periodic pruning is needed otherwise

## Build Serialization

**Writer contract: one build at a time per project — fail fast, never
interleave.**

- `AssetGraph::build` takes an exclusive advisory lock on
  `<base_dir>/manifest.lock` before touching anything and holds it for
  the whole run: discovery, compilation, cleanup and manifest
  publication all happen inside the lock, so two writers can never
  produce an output tree that mixes both builds' files.
- A second writer gets an immediate error naming the held lock rather
  than queuing. In watch mode this is a structural failure: the watcher
  exits non-successfully, rather than staying idle until some unrelated
  file change happens to offer a second chance.
- The lock is advisory — it binds AssetGraph writers, not other tools —
  and the OS releases it when the holder exits, so a crashed build never
  strands the project.
- Publication is atomic underneath the lock: the manifest and its bincode
  cache write to per-process uniquely named tmp files and rename into
  place, so a reader never sees a partial manifest and a writer that
  somehow bypasses the lock cannot rename another writer's half-written
  tmp into place. Tmp residue a crashed save leaves behind is inert: a
  save never deletes a sibling it did not stage. A later build removes it
  only when every one of these holds — the name has the exact
  `.paste-staged.{kind}.{pid}.{n}.{digest}` shape, it is a regular file
  in a directory this project's builds write into (the manifest's
  directory, the output roots, a recorded generated directory, the
  project root itself) whose resolved path lies beneath the project root
  — a symlinked output root pointing elsewhere does not — it was last
  modified before the build's process started and at least an hour ago, and, on Unix, the pid in its name is
  not a live process on this host. Elsewhere, and in any directory
  outside the project, residue stays until removed by hand. The rule
  holds against another project's build sharing an output directory on
  the same host; the remaining limit is a writer in another pid namespace
  (a container on a bind mount, another host on a share) whose clock runs
  more than an hour behind this one, whose in-flight file can look stale.
- Publication is one boundary: everything fallible — source
  re-verification, destructive-cleanup preflight, the bincode cache's
  staged write and read-back — completes while the previous manifest is
  still the live one, and the atomic JSON rename is the single swap
  after which the new generation exists. Work remaining after the swap
  (cache installation, version pruning, retired-location unlinks) is
  maintenance: a failure there warns and the next build retries, never
  an error reported over a manifest that already went live.
- `manifest.lock` is build-tooling state, not an asset: it sits at the
  project root, empty, and is never recorded in the manifest. Layouts
  that serve the project root itself (beside-source) carry it as a
  zero-byte marker; every other layout keeps it outside the served
  tree.

## Watch Observation Boundary

The single file watcher owns configured roots and imports consumed by Sass
outside those roots. The build captures exact read bytes and verifies them
before manifest publication; after the new external watch registrations
are installed it rechecks the captured observations and immediately rebuilds
when a previously unobserved change is detected. Imports through file or
directory symlinks retain their traversed entries and resolved targets, and
verification compares target identity even when the target bytes are equal.
Missing imports resolve the longest existing ancestor before interpreting
remaining path components, preserving symlink and `..` filesystem semantics.
A dangling file symlink's missing referent is separately observed so creating
its target can recover a failed compilation without editing the alias.
Imported files share nonrecursive parent registrations for creation and
replacement and receive exact-file registrations for in-place writes.
Filesystem notifications are filtered to exact dependencies and ancestor
lifecycle; ambiguous sibling-labelled directory-entry notifications compare
only recorded external inputs and resolved symlink identities before rebuilding. A rescan replaces existing
physical registrations rather than registering duplicates, tolerating an
already-lost backend registration during removal.
On rebuild failure, last-successful dependencies remain registered alongside
only the current attempt's missing/consumed candidates; a successful rebuild
replaces both sets. Only watched source errors are retryable. Invalid stored
manifest state, unsafe or unwritable output and notification-channel failures
return errors rather than leaving a nonfunctional watcher running.

## Filesystem Trust Model

The build treats its configured locations — source and include roots,
output roots, `base_dir`, the manifest path — as **cooperative, trusted
directories**: locations the deployment owns and no hostile actor
writes into. The ownership checks throughout the build defend the
project against configuration drift and operator accidents, not
against an adversary racing the build.

What the checks do cover:

- **A swapped-out location.** A directory the build once wrote —
  replaced by a symlink or a regular file — is refused at write,
  delete, and discovery time rather than followed into a tree the
  build never owned.
- **A torn or stale manifest.** Atomic tmp-then-rename publication,
  plus a bincode cache bound to the JSON's digest, means a reader
  never sees a partial or mismatched manifest.
- **Mid-build source edits.** Every byte the compilers consumed is
  captured at first read and re-verified before publication, keyed by
  canonical path: one source file contributes exactly one byte
  sequence to a generation, so two entry points importing a shared
  file can never compile from different states of it. An edit that
  lands during the build aborts it rather than publishing a manifest
  that describes an input state that never existed.
- **A contributing root that vanishes.** If a source or include root
  the previous manifest still serves files from disappears — even
  between the preflight check and the walk itself — the build fails
  naming the root rather than silently retiring its modules into an
  emptied manifest. A root that never contributed may be absent, and
  deleting a file inside a present root remains ordinary retirement.
- **Concurrent writers.** The advisory lock serializes AssetGraph
  builds; tmp names are unique per writer so even a writer that
  bypassed the lock cannot complete another's half-written file.

How the checks are enforced — descriptor-anchored operations:

- **On Unix, directories are pinned by descriptor, not by pathname.**
  `src/dirfd.rs` opens each operation's parent with
  `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` and runs `openat`, `renameat`,
  and `unlinkat` against that handle. A directory swapped between the
  check and the mutation redirects nothing — the descriptor still names
  the directory that was validated. Staged outputs are `create_new`'d
  under the pinned parent and `renameat`'d into place, so the file the
  build created is the only one it can publish or clean up.
- **Read paths allow aliases, write paths never follow them.** Reads
  (manifest, cache, verified combo files) open nonblocking, verify the
  handle is a regular file, and read a bounded number of bytes — a FIFO
  or device can neither stall the open nor grow an allocation. The
  containing directory may resolve through links on reads — a deployment
  that assembles its tree through aliases still serves the recorded,
  hash-verified bytes — while writes and deletes refuse any symlinked
  component between the trusted boundary and the target.
- **Non-Unix fallback.** Where descriptor traversal is unavailable the
  same `OwnedDir` surface keeps a validated root path plus a
  revalidation pass before every mutation — the window narrows but does
  not close, which is the honest limit of pathname checks.

What the checks do not cover — the honest boundary:

- **Other tools writing the tree.** The advisory lock binds
  AssetGraph writers only; editors, rsync jobs, and package managers
  writing underneath a build are detected by the input re-verification
  (and abort the build), not prevented.
- **A compromised trusted root itself.** If the deployment's own
  anchor is hostile, no in-process check can tell it apart — physical
  ownership begins at the configured root.

Deployments that need adversarial isolation must place the project
tree behind filesystem permissions the builder does not share with
untrusted writers; that is the mechanism the threat model expects, not
a sandbox inside the build.

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
  each component fits a filesystem limit wherever a URL was written to
  disk as a file; the combo endpoint rejoins the components into one
  ordered list.
- The canonical URL keeps the listed order whenever every listed entry
  carries an explicit version pin — a fully pinned URL names an exact byte
  sequence, so two orders of the same members are two different addresses
  and neither is rewritten to the other's. Dependency layering orders the
  list only when at least one entry is unpinned — the request's order
  means nothing then, so the canonical form is the manifest's layering.
- A request whose list or stamp is not canonical — a stale stamp, a
  non-numeric segment, an unpinned name — answers 302 with a `Location`
  pointing at the canonical URL, never a year-cached wrong URL. A stamp
  newer than anything the manifest knows is a `no-cache` 404 (a URL from
  a newer build hitting an older server).
- The runtime loader's `{packages-segment}/{module}.js` is the one unversioned
  cacheable form; the segment is the build's JS content fingerprint, and the
  manifest records each emitted fingerprint's `key:version` map so a saved
  page's loader URL keeps resolving the bytes it was rendered against.
- Served bytes are hashed per request: a file that no longer matches its
  recorded hash is a `no-cache` 404, never a year-cached wrong answer.
- The `combo` runtime adapter concatenates the requested modules on-the-fly,
  separating JavaScript modules with a terminating empty statement, and
  returns the response description for the host framework to write.
- A request with or without `.min.` serves the same compiled output — the
  suffix only names the URL form, and never a module (see Module Names and
  Asset Keys).

## Configuration

### Scala/sbt (via plugin)

```scala
// build.sbt
pasteAssetGraphSourceDirs := Seq("src/main/assets/scss", "src/main/assets/js")
pasteAssetGraphOutputDir := "target/public"
pasteAssetGraphIncludePaths := Seq("..")
```

### HTML/Python (via config file)

```hocon
# paste-assetgraph.conf
source.dirs = ["src/js", "src/scss"]
output.base = "dist"
storage.backend = "dist"
storage.keep-versions = all
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
  "hash_urls": true,
  "cdn_base": "https://cdn.example",
  "output_roots": {"js": ["dist/js"], "css": ["dist/css"]}
}
```

- `assets[key].retired` is `true` only on an entry whose source was
  deleted — the field is always written, and absent in an older manifest
  means active: it resolves exact `+vN` pins but nothing unpinned.
- `generated_dirs` lists project-relative generated directories carried
  forward across output-layout and build-directory changes.
- `output_roots` records the directories builds used as type output
  roots (`js`/`css`), accumulated across moves — the only provenance a
  build accepts for removing a legacy `assetgraph.{type}` at a
  historical location. The same name in any other recorded directory is
  reported on stderr, never deleted; a manifest without the field
  establishes no historical roots.
- `hash_urls` records whether the build emits hash-qualified version
  tokens — always written, absent in a pre-field manifest means the
  legacy `+vN.0` form.
- `cdn_base` is present only when the build ran under `cdn.enabled` with
  a `cdn.url` — the render-time default origin prefix for emitted tags,
  which a settings `cdnBase` overrides; `combo` never consults it.

## Future Enhancements

- [ ] S3 storage backend implementation
- [ ] CloudFront invalidation integration
- [ ] Source map generation
- [ ] Image optimization pipeline
