# JAM (JavaScript Asset Management) Specification

## Overview

JAM is a system for managing JavaScript and CSS module dependencies in web applications. It generates optimized, cacheable URLs for module groups, serves their compiled output through a combo endpoint, and renders the markup that loads them.

**The Rust library owns all of it.** Tag generation, URL construction, dependency filtering and expansion, rendering, and combo responses are implemented once in `paste-assetgraph` and exposed through a small C ABI:

- `paste_render_page(settings_json, events_json, generation)` — renders one page step and answers `{"html": ..., "generation": ...}`: the step's markup plus the manifest-generation token the caller posts back on the next call. A page's first call sends an empty `generation` ("whatever the manifest currently is"); every later call forwards the token it last received, so a build landing mid-page can never make one page mix two manifests' module maps. A token that no longer matches the manifest is a terminal failure for that page — the envelope carries `"error"` with `"kind": "manifest-generation"`, and the runtime must retire the page permanently rather than render the rest of it against a different manifest;
- `paste_render(settings_json, events_json)` — the retained one-shot interface: renders a JSON event list and returns the dump HTML; an event list ending in the bare string `"finish"` is a completion check instead — it validates that every jammed module was dumped and returns an empty string;
- `paste_combo(settings_json, path_and_query)` — resolves a request path (optionally followed by `?query`) to a response (`status`, `contentType`, `cacheControl`, `cacheable`, `location`, `lastModified`, `body`).
- `paste_combo_conditional(settings_json, path_and_query, if_modified_since)` — the same resolution with the request's `If-Modified-Since` value (NULL or empty behaves exactly like `paste_combo`), allowing a `304` for an unchanged immutable group (see §6).

Each returns a JSON envelope — `{"ok": ...}` or `{"error": "<message>"}`, optionally with a machine-readable `"kind"` — as a heap string the caller releases with `paste_free`. The same logic also backs the `render`/`combo` commands of the `paste-assetgraph` CLI. There is no server in the library: `combo` returns a response description — including a `location` for the canonical-URL redirects below — and the host framework writes the HTTP answer.

Language runtimes (Scala, Node, Python) are thin loaders: they locate the shared library (`PASTE_ASSETGRAPH_LIB`, else the bundled copy), call it with a settings JSON, and surface the envelope. They implement no tag, URL, dependency, combo, or caching logic of their own. Builds emit individual versioned assets and manifest metadata only — markup and combo responses are produced per request through `render`/`combo`, never persisted by a build.

## Delivery model

Compiled modules are individual immutable versioned files. They can be served from any static origin — a CDN, object storage, a plain file host — with no server logic in front of them. A page reaches them in exactly one of three ways; the build writes no page, no combo response, no loader copy, and no aggregate for any of them.

1. **Through the library.** The host (Scala, Node, Python, or the CLI) calls `render` per request to produce the tags and answers `combo` URLs with the combo handler, which reads the individual files and concatenates the exact requested group. The handler also resolves the runtime's `packagesUrl + <module>.js` loader fetches to the versioned file. A CDN may sit in front of the handler; the URL contract below makes every response safe to cache for a year.
2. **Bare module names against a conventional origin.** The page includes the paste runtime, then `paste.config({packagesUrl: '<origin>/<version>/'})` and `paste.require('<module>')`. The runtime requests `packagesUrl + <module> + '.js'` and auto-fetches undefined dependencies the same way. This works only when the origin publishes files named exactly `<module>.js` — a version in the path is fine, a version or hash in the filename is not. The library's own `dist` output does not satisfy that convention; it is served through the library's combo handler or by full URL.
3. **Full URLs.** The page includes the paste runtime, then `paste.define('<name>').isLoading = true` for every module name the files will declare, before any request — `paste.define` modules coordinate activation, so the marks stop a dependent that arrives first from fetching its dependencies — then `paste.require('<absolute URL of each versioned file>')` for every module it needs, dependencies included, in dependency order; `<link>` per stylesheet. The runtime treats a name ending in `.js`, containing `:`, or starting with `/` as a source URL and loads it verbatim. Modules written as raw globals rather than `paste.define` calls have no activation coordination and need ordered loading — synchronous tags, or requests issued one after the previous file loads; the manifest's `sorted_deps` is the ordered list to copy from. An unmarked, undefined dependency can trigger a bare-name `packagesUrl` request; a static host with only versioned files cannot answer that URL.

Bare-name and full-URL loading are the original paste runtime's behaviour, unchanged; delivery through the library re-implements the original combo-server contract.

## URL Format

The canonical URL identifies the **exact filtered group** it serves:

```
{cdn}{base_path}/{timestamp}/{module}+v{version}.0[-h{hash}][~{module}+v{version}.0[-h{hash}]...].min.{js|css}
```

Examples (a real hash is the full 32-character lowercase MD5 hex):

```
/paste/1770462000/paste.dom+v2.0-h0bdbb2c325525e986d55ef50ca66d056~paste.event+v1.0-h7e9e92b0857c477a6e8aebb0ab2bd924.min.js
/paste/1770465409/site.theme+v1.0-h2c56c360580420d293172f42d85dfbed.min.css
```

- `timestamp` — the maximum last-modified stamp (Unix seconds) across the modules actually delivered by the URL, at the versions being rendered — after dependency expansion where requested and after render-time filtering. A build stamps a new version from the source file's mtime (clamped to `SOURCE_DATE_EPOCH` when set) and carries an unchanged asset's stamp forward, so the segment moves only when a member of the group actually changed.
- `{module}+v{N}.0` — the module name plus its major version from the manifest. Every module a render names — jammed directly or reached as a dependency — must resolve to a manifest asset of the dump's type; an unknown module fails the render.
- `-h{hash}` — the hash qualifier: the full 32-character content hash (MD5, lowercase hex) recorded for that exact version. **On by default** for new builds (`jam.hash-urls`); a build may opt out with `jam.hash-urls = false`, which keeps emitting and serving the legacy `+vN.0` shape. The build writes the policy into the manifest (`hash_urls`), so runtimes and adapters need no setting of their own — the URL a page carries is interpreted by the library alone. A manifest written before the field existed has no `hash_urls` key and stays on the legacy form until a build rewrites it. Any `-h` pin in a request is checked against the resolved version's recorded hash — case-insensitively, canonical URLs emit lowercase — and a mismatch is a 404, never a redirect onto different bytes. A correctly qualified URL resolves under an opted-out manifest too: one qualified pin in the request makes the canonical form qualified for every member, so a mixed list redirects to all-qualified tokens.
- Module order is the listed order whenever every entry carries an explicit `+vN` pin — a fully pinned URL names an exact byte sequence, so two orders of the same members are two different addresses. Dependency order (dependencies first) applies only when at least one entry is unpinned — the request's order means nothing then, so the canonical form is the manifest's layering.
- `~` separates modules in emitted URLs; `,` is accepted as a separator when a combo request is parsed, and both are accepted in `filter` lists.
- `.min.js` and `.js` (likewise `.min.css`/`.css`) are both accepted. They serve the same compiled output — the suffix is part of the URL contract, not a serving-mode switch; whether output is minified is decided by the build (`--debug` build flag). The suffix is read off the list without consulting the manifest: exactly one trailing `.min.{ext}`, else `.{ext}`. `.min` is never part of a module name — the build derives `vendor` from `vendor.min.js` and rejects a module name ending in `.min` — so `vendor.min.js` always names `vendor`. One exception keeps saved pages honest: a packages segment recorded before this rule whose snapshot holds `vendor.min` reads its loader URL `{segment}/vendor.min.js` as that snapshot's `vendor.min` entry and serves the bytes it pinned — what the saved page meant — never `vendor`'s.
- `{cdn}` is the origin prefix on *emitted* URLs only. A build under `cdn.enabled = true` with `cdn.url` writes the origin into the manifest as `cdn_base`; a render settings `cdnBase` overrides it, an empty `cdnBase` inherits it, and a manifest without the key renders origin-relative URLs. `combo` never consults it: the endpoint resolves request paths to response bytes, so the same built output serves any origin.

**Long lists.** A hash-qualified list can exceed a filesystem's component limit (255 bytes) wherever a URL was written to disk as a file. A URL may therefore continue the list over `/`-separated path components — each component carries complete `~`-joined tokens, the `.min.{ext}` suffix lands only on the last component, and the combo endpoint joins the components back into one ordered list. The component boundary is a separator, never part of a token: the canonical form is still the single-component URL, and a segmented canonical request serves 200 while a stale one redirects to the joined canonical form.

### Request forms

| Form | Behavior | Cache |
|---|---|---|
| `{base}/{ts}/{list}.{ext}` | Versioned, canonical: every entry pins `+vN` (plus `-h{hash}` under the qualified policy), the list is in canonical order, and `ts` equals the group's stamp — serves the listed group exactly as named | `public, max-age=31536000` |
| `{base}/{ts}/{list}.{ext}` | Versioned, non-canonical: a stale stamp, a non-numeric segment, a list out of canonical order, or an unpinned name — the request resolves, but the URL is not the canonical one | `302` with `Location` = the canonical URL (query preserved), `no-cache`, empty body |
| `{base}/{ts}/{list}.{ext}` | Versioned, from the future: `ts` is newer than the group's stamp — a URL from a newer build hitting an older deployment | `404`, `no-cache` |
| `{base}/require/{list}.{ext}` | Expands the listed modules' transitive dependencies and serves the whole set, ordered | `no-cache` |
| `{base}/{list}.{ext}?require` | Same expansion via the query flag | `no-cache` |
| `{base}/{list}.{ext}?filter=a~b` | Drops the named modules from the ordered group before serving (they were already delivered on the page) | follows the versioned/unversioned rule — a versioned request canonicalizes before `filter` is consulted |
| `{base}/{list}.{ext}` | Unversioned: serves the named group under a path that never changes | `no-cache` |

The canonical list is the listed order itself whenever every listed entry carries an explicit `+vN` pin; once any entry is unpinned, the canonical list is the listed modules ordered by the manifest's dependency layering. The canonical stamp is the maximum epoch seconds over the resolved versions' own timestamps — history entries carry theirs. A segment that is not all digits, a list that differs from its canonical form, or a stamp below canonical is a redirect; a stamp above is the not-found form.

Query parsing is form decoding (`+` means space in query values; the `+` in the path's `name+vN.0` tags stays literal — the path segment is percent-decoded, not form-decoded). `filter` names that are not valid module names are ignored. `require` may appear with any value (`?require`, `?require=1`).

Each `+vN` in a versioned list entry is served exactly: the module's current version when `N` matches it, or a version retained in the entry's `history` whose compiled file is still on disk (the `dist` storage backend keeps every historical version unless `storage.keep-versions` bounds it — `all` is the default, `0` keeps current only; `local` keeps only the current one). A version the server cannot serve — newer than the manifest knows, or pruned from history — is a 404 with `no-cache`, never another version's content. A malformed version token — a bad major/minor, an overflowing version, or a malformed `-h` qualifier (empty, non-hex, or not exactly 32 hex characters) — or the same module listed at two different pins (different versions, or a hash on one spelling but not the other, or two different hashes) is a 404 too.

A supplied `-h` hash is checked against the resolved version's recorded hash **before any redirect decision** — for every listed module, including one `?filter=` would later drop — so a wrong hash is always a `no-cache` 404. Under a hash-enabled manifest a legacy `+vN` request canonicalizes onto the qualified URL (302); under an opted-out manifest legacy URLs serve as before while qualified requests still resolve and validate.

An entry whose source was deleted is marked `retired` in the manifest. It remains resolvable through explicit `+vN` pins and recorded historical loader pins. Fresh unversioned resolution and render-time expansion exclude it, and a recorded historical segment still names a retired entry's retained version: a saved page's loader URL resolves through its snapshot pins, which is exactly what the record exists for.

Under the cacheable form every listed entry must pin a version — with one exception: the runtime loader fetches `{packagesUrl}{module}.js`, i.e. `{base}/{packages-segment}/{module}.js`. That unversioned URL is served and year-cached under the manifest's own packages segment (resolving to current), or under a packages segment the manifest recorded for an earlier build, where every listed module resolves at the version that build pinned — the manifest's `packages_segments` history maps each fingerprint to its `key:version:hash` records, and the hash pins the bytes: a same-key, same-version slot under different content never satisfies an old pin. An exactly-12-hex segment is reserved for packages fingerprints — one the manifest never recorded, all-digit spellings included, is a `no-cache` 404 rather than a substitution for current content — while any other segment (a numeric stamp or a free-form token) canonicalizes like any non-canonical spelling. Under `require` and the unversioned single-segment form, unversioned entries resolve to the current version — those responses are `no-cache`.

**Served bytes are verified.** A resolved version's file is read and hashed per request; bytes that no longer match the recorded content hash answer `no-cache` 404 — a corrupted or replaced file under a year-cached URL must never become a permanent wrong answer. The check covers current outputs, retained history, and loader pins alike. Reads are bounded: no compiled asset may exceed **64 MiB**. The same bound applies at write time — a build that would emit a larger file fails naming the limit rather than publishing bytes the runtime cannot serve.

**Rolling deploys.** A page rendered by build N can reach a server already running build N+1. The page's `+vN` pins ask for exactly what was rendered, and the packages segment pins the loader fetch to the build that produced the page — the manifest records each emitted fingerprint's `key:version:hash` map so the old page's loader URL resolves its original bytes while the files are retained. Anything the server cannot serve exactly is a `no-cache` 404 rather than a year-cached wrong version. Serving whatever the current manifest holds under a versioned URL would let a CDN pin v3 content under a v2 URL.

### Errors

When any module in the resolved group has no manifest asset of the request's
type or no readable compiled output at the version its entry pinned — an
unknown name, an unresolvable dependency, a missing or pruned file, a
version the manifest does not know, or an empty or invalid module name or
version token in the list — the whole response is a not-found, never a
partial group:

```
status: 404
body: /* modules not found */
Cache-Control: no-cache
```

The same strictness applies at render time: a jam naming a module with no
asset of the dump's type fails the `render` command rather than emitting a
URL that cannot be served.

## Bootstrap HTML

`render` replays a JSON event list and prints the markup the last `dump` event produces. A list ending in the bare string `"finish"` is a completion validation, not a render: the page is checked (every jammed module dumped — see Page Tracking) and `render` prints nothing. Input:

```json
{"events": [
  {"jam": {"css": ["site.theme"], "js": ["paste.dom", "paste.event"]}},
  {"dump": "all"}
]}
```

A dump's scope is `"css"`, `"js"`, or `"all"`. The `all` scope drains both
queues in one dump — the css tags, a newline, then the js tags — producing
exactly what a `"css"` dump followed by a `"js"` dump would render, as one
`dump` event.

Output for the `js` part (the `paste` runtime module is inlined ahead of it, once):

```html
<script type="text/javascript">…paste runtime source…
;
paste.config({packagesUrl:'/paste/71f8e72a3d6b/'});</script>
<script type="text/javascript">(function(d,r){d('paste.dom').isLoading=d('paste.event').isLoading=!0;r('/paste/1770462000/paste.dom+v2.0~paste.event+v1.0.min.js');})(paste.define,paste.require);</script>
```

- The runtime script is emitted once, before the first JS tag, and joins `paste.config({packagesUrl:'{cdn}{base}/{segment}/'})` onto the runtime source with the `\n;\n` separator so a runtime ending in a `//` comment cannot swallow the config. `segment` is the first 12 hex chars of MD5 over the manifest's JS loader pins — `name:key:version:hash` lines for every declared alias and conventional module name — followed by the ordered `sorted_deps` module names: it changes whenever any JS content, version, alias mapping, or layering changes, so a deterministic build still gets fresh loader URLs. When the runtime source cannot be inlined (missing or unsafe content) it is emitted as an external `<script src>` plus a separate config script instead.
- The async loader marks each pending module `isLoading` and `paste.require`s the exact-group URL.
- `jam.sync = true` emits `<script type="text/javascript" src="{cdn}{base}/{ts}/{list}.min.js"></script>` instead.
- CSS groups emit `<link rel="stylesheet" href="{cdn}{base}/{ts}/{list}.min.css"/>` — with `media="print"` when the jam event carries `media` — or an inline `<style type="text/css">…</style>` when the group is below the inline threshold.
- Groups at or under `inline_threshold` UTF-8 bytes of compiled output (default `1024`, `0` disables automatic inlining) are inlined as `<script type="text/javascript">…</script>` / `<style>…</style>`; `jam.inline = true` forces inlining regardless of size. JS content containing `</script` or `<!--`, and CSS containing `</style`, is never inlined — the group falls back to the external tag.
- `perModule` on the render settings (CLI `--per-module`, settings JSON `perModule`) switches each group from one combo URL to per-module delivery naming the versioned files directly, in the same dependency order and after the same loaded-state filtering — so a page's tag block names every file a static host must serve. Each URL is `{cdn}{encoded output}` where the output path is the versioned file the build wrote, normalized to one leading `/` and percent-encoded segment by segment — a legal beside-source filename containing `'`, `#`, `%`, ` `, `&`, or non-ASCII bytes cannot break the emitted markup or point at the wrong file, while the manifest's `output` pointers and the physical files stay raw. Per-module JS delivery requests each versioned file in dependency order through one loader block per group: the block marks every external member `d('{module}').isLoading=!0` before the first request, then issues each `r('{cdn}{encoded output}')` through `paste.require(url, callback)` so each request starts after the previous file loads. Eligible files before the first external request may inline; from the first external request onward, the group stays external, including with `inline = true`, so later scripts retain global scope and cannot overtake an earlier file. With `sync = true`, each file is evaluated in order through inline code or a `<script type="text/javascript" src="{cdn}{encoded output}"></script>` tag. Separate asynchronous groups are independent, as in combo mode. A CSS dump emits one `<link>` per module carrying the jam's `media`; CSS inlining still decides per module, so a small file may inline beside a large file's link. Every emitted tag's file is verified before it is named: the module's compiled output must be readable and hash-verified (the manifest's recorded MD5 over the bytes actually on disk) — missing or corrupt output fails the render rather than emit a tag for missing content, and never falls back to a combo URL. `jam.perModule` overrides the setting for that event's groups in either direction; `combo` request handling is unaffected.
- Under `perModule` the runtime bootstrap fallback follows the same contract: when the `paste` runtime cannot be inlined, the page emits one synchronous `<script src>` per verified versioned runtime/dependency file in order — each naming its encoded output path — followed by the usual `paste.config` inline tag, instead of a combo URL. Runtime content that cannot safely inline takes this fallback; the bootstrap is otherwise forced inline regardless of the automatic threshold. A runtime file that is missing or fails verification is a render error, never a tag for unreadable content. The per-module bootstrap applies when `perModule` is set on the render settings — the page-level declaration that no combo handler stands behind the page. A jam event's own `perModule` scopes to that jam's groups only and never moves the bootstrap: the page API is incremental, so a later event cannot rewrite a bootstrap the first dump already emitted. No file copies or extra files are published — the tags name the same versioned outputs the build already wrote.
- `perModule` is not the original renderer's pretty mode, though it fills the same role — one tag per module. The recorded differences, each intentional: the original named a combo-shaped URL per module (`{root}/{group_lastmod}/{name}+v{ver}{ext}` — the *group's* maximum stamp on every file's URL) where `perModule` names the module's own versioned output file, verified before it is referenced; the original emitted one complete `paste.require` tag per file so every request fired independently and unordered — the dependency race where raw-globals could execute before their dependencies — where `perModule`'s single loader block serializes external fetches in dependency order (the fix that motivated the mode); and the original inlined a module whenever it fit the threshold where `perModule` keeps every file after the first external request external, because inlining it would execute it before the file it depends on. Identical behavior carries forward: one tag per file, per-file inline decisions, `sync` `src` tags in order, a `<link>` per stylesheet, conditional wrappers per tag, and CSS media.
- A page jams `paste` itself as an ordinary module: the runtime emits `paste` inline first, so a later `paste.dom` group's URL names only `paste.dom` — `paste` is already delivered and stays out of the URL entirely (no exclusion query is needed).
- `jam.condition` wraps the group's emitted tags in the downlevel conditional comment — `<!--[if {condition}]>` newline, tag, newline, `<![endif]-->` — in every emission mode (combo tag, sync tag, inline tag, and each per-module tag). An empty condition is no condition at all. The condition lands inside an HTML comment: any value containing `<`, `>`, `--`, or a control byte fails the render rather than emit breakable markup. Loaded-set semantics follow the original exactly: a conditional *js* group does not mark its modules loaded — the page's loaded state is unchanged, so a later unconditional jam serves them again — and cannot carry the runtime bootstrap (the bootstrap leads the first dump that has an unconditional js group); a conditional *css* group does mark loaded. A jammed module that only ever emitted under a condition was still dumped, so `finish` accepts it.

## Behavior Requirements

### 1. Manifest Loading

- `render` and `combo` take `--manifest` and `--output-base` (plus optional `--output-js`, `--output-css`) and only read the files they need — the manifest JSON and the compiled assets under the output directories.
- The manifest must validate: `version` and `generated` strings, `assets` map present, every asset `output` path resolving under the output directories. Invalid manifests fail the command; null bytes in strings are rejected.
- Asset paths are safe: no `..`, no absolute paths, no escaping the output directories.

### 2. Stamp Computation

The URL's stamp segment is the maximum, in epoch seconds, of the members' `timestamp` values at the versions being rendered — over the **exact delivered group** after expansion and filtering, not just the requested modules. Every member must carry a parseable timestamp: a member without one is a render error naming the module and telling the operator to rebuild the manifest — there is no constant fallback, because a wrong stamp would mint an unchanging URL for changing content. The build stamps new versions from source mtime (clamped by `SOURCE_DATE_EPOCH`), carries unchanged entries forward, and backfills an empty or unparseable stamp on a carried-forward entry the same way.

### 3. Module Ordering

Modules are served and named in dependency order: the render path expands each jam's module list to its transitive closure, removes modules already delivered on the page, orders the remainder by dependency, and names exactly those modules in the URL.

### 4. Dependencies only when requested

A versioned URL serves precisely the modules it names. Dependencies are added to a served group only when the request asks — `/require/{list}.{ext}` or `?require`.

### 5. isLoading

For each async JS group the loader emits `d('{module}').isLoading` for every pending module so `paste.define` callers can detect in-flight loads.

### 6. Combo Serving

`combo` resolves any of the request forms above to a single response. Each `+vN` pin resolves through the manifest: the entry's current output when `N` is the current version, else a retained `history` version's output, else the 404 form. For the versioned form the resolved group is checked against its canonical URL — canonical order (the listed order when every entry is pinned, the manifest's layering when any entry is unpinned), the `+vN.0` spellings, and the canonical stamp — and a mismatch answers the 302 form with `location` set to the canonical URL plus the request's original query. A stamp above canonical is the 404 form. The group is read from compiled output under the output directories, joined with the `\n;\n` separator for JS and newline for CSS. Any member that cannot be resolved and read, and any empty or invalid module name or version token in the list, turns the whole response into the 404 form; a partial group is never served. Cache-Control is `public, max-age=31536000` for a successful versioned non-require response — `filter` does not change that — and `no-cache` for unversioned, require, redirect, and 404 responses.

A resolvable group carries a *group stamp*: the maximum of the `timestamp` values recorded in the manifest for the resolved versions — a history pin contributes its own recorded stamp, never the current version's. Only the immutable forms — the fully pinned canonical URL and a manifest-recorded loader segment — can answer 304; unversioned and `require` responses always answer 200 and carry `lastModified`. For those immutable forms `combo_conditional` compares the request's `If-Modified-Since` (parsed as an HTTP-date) against that stamp: a validator at or after the group stamp answers the 304 form — the RFC comparison, one deliberate improvement over the original, which answered 304 only when the validator formatted to the identical string (a *later* validator was served the whole 200 body) — `status` 304 with an empty `body`, the resolved response's `cacheControl`, and `lastModified` set to the group stamp in IMF-fixdate form. Every member file is still read and hash-verified before the 304 is answered, so a missing or corrupt member stays the 404 form. The stamp is computed from recorded manifest metadata only; nothing inspects source mtimes or rebuilds dependencies at request time. Canonicalization precedes the validator, so a non-canonical request still answers the 302/404 forms before any 304 — a conditional request never suppresses a redirect a saved page still needs. `lastModified` is written on every 200/304 group response that has a stamp (and is `null` on redirects, 404s, and groups whose manifest records no timestamps); a stale, absent, or unparsable validator behaves exactly like `paste_combo`. Adapters forward the header unchanged — the comparison is the library's decision, not the adapter's — with one RFC 9110 §13.2.2 rule of their own: while the request also carries `If-None-Match`, they forward no `If-Modified-Since`, because the library evaluates no entity tags and a date must not decide while a tag condition stands unevaluated. A host serving direct files relies on its static origin for conditional handling instead.

### 7. Inline Threshold

Automatic inlining applies to groups whose joined compiled output is at most `inline_threshold` bytes (default `1024`; `0` disables it). `jam.inline` forces inlining regardless of threshold. Unsafe content never inlines (see Bootstrap HTML).

### 8. Page Tracking via Event Replay

A page render is an ordered list of events: `{"jam":{...}}` records a group; `{"dump":"css"|"js"|"all"}` renders the markup for all events so far — `all` delivers both queues, css first, against the same loaded state as separate dumps. Modules already emitted by earlier groups are excluded from later ones — state lives in the event list, so any wrapper renders identically by replaying the same events. Each dump re-renders the full list; the last dump's output is what `render` prints.

A page ends with the bare string `"finish"`, which must be the last event and produces no markup. Finish is the completion check: the library replays the same plan the dumps used — pending queues per type, loaded sets with per-media css delivery — and refuses the page if any jammed module was never dumped, naming the asset type and module (and the media for css). A module already delivered under the same media is not outstanding; a failed finish leaves the page open so the missing dump can still be added and finish retried. Ordinary dumps stay partial checkpoints — only finish validates completeness. `render` of a finished list returns `""`.

### 9. Retired Build-Time Publication

Earlier builds could render `paste:jam`/`paste:dump` HTML comments under `templates.source` into pages and materialize every referenced combo and per-module loader file under `templates.output`/`output.base`. That publication feature was removed: the build's output contract is individual versioned assets plus manifest metadata, and persisted module content exists only in those files. Configurations still carrying `templates.source` or `templates.output` fail validation naming the retired keys; page markup comes from `render` at request time, and previously generated publication files are operator-managed, not recreated.

### 10. Runtime Configuration

The inlined runtime is configured per render: `paste.config({packagesUrl:'{cdn}{base}/{segment}/'})`, where `segment` is content-derived from the manifest's JS assets (see Bootstrap HTML). The runtime source is read from the compiled `paste` module; when it cannot be inlined the runtime loads externally and the config follows in its own script tag.

## Manifest Structure

```json
{
  "version": "2026.02.07.1156",
  "generated": "2026-02-07T11:56:49Z",
  "modules": {
    "paste": "js/paste.js",
    "paste.dom": "js/paste/dom.js"
  },
  "assets": {
    "js/paste.js": {
      "source": "src/paste.js",
      "output": "/js/paste.7e9e92b0857c477a6e8aebb0ab2bd924.v1.min.js",
      "hash": "7e9e92b0857c477a6e8aebb0ab2bd924",
      "version": 1,
      "timestamp": "2026-02-06T10:00:00+00:00",
      "dependencies": [],
      "history": []
    },
    "js/paste/dom.js": {
      "source": "src/dom.js",
      "output": "/js/dom.0bdbb2c325525e986d55ef50ca66d056.v2.min.js",
      "hash": "0bdbb2c325525e986d55ef50ca66d056",
      "version": 2,
      "timestamp": "2026-02-06T11:00:00+00:00",
      "dependencies": ["paste"],
      "history": []
    }
  },
  "sorted_deps": {
    "js": [
      ["paste", "/js/paste.7e9e92b0857c477a6e8aebb0ab2bd924.v1.min.js", 1],
      ["paste.dom", "/js/dom.0bdbb2c325525e986d55ef50ca66d056.v2.min.js", 2]
    ]
  },
  "hash_urls": true
}
```

- `modules` maps a module name to its asset key; a key whose entry is `retired` may remain here — resolution, not the map, filters it.
- `assets[key].output` is the path under the output directory; `dependencies` are module names; `version` and `timestamp` feed the URL tags and timestamp. A new version is minted when the compiled content hash changes, when the recorded output path would move, or when the dependency edges change — a dependency-only source edit still versions the asset, because the group a module expands into at request time is part of what its version means and an unbumped edge would alias a saved pin onto different bytes (the original implementation serialized dependencies per version, so it could keep the version on identical compiled output; this manifest keeps them on the entry). An unchanged rebuild bumps nothing and carries the entry's stamp forward.
- `assets[key].retired` is `true` only on an entry whose source was deleted — the field is always written, and absent in an older manifest means active: its retained pins keep serving while nothing unpinned resolves it.
- `sorted_deps` is a named-module index the build emits per type (`"js"`, `"css"`): topologically sorted `[module_name, output_path, version]` triples over active assets.
- `generated_dirs` lists project-relative directories the build has generated into — how a layout or build-directory change relocates retained outputs without a filesystem search, and what discovery excludes so generated files never re-enter as sources.
- `packages_segments` is loader metadata only — it carries no content: each packages fingerprint a build emitted mapped to the `key`, `version`, and `hash` every JS module name resolved at then. A saved page's `{segment}/{module}.js` URL resolves through this map; pins are pruned with the versions they can no longer verify, and a recorded segment is immutable — recording one under different pins is an error. A build upgrading a manifest with no records preserves the fingerprint its loaded state would have emitted — the legacy `key:hash`-form segment included — so pages that already exist keep resolving; fingerprints no build ever emitted are not synthesized, and exactly-12-hex segments are reserved for this form.
- `hash_urls` records the URL policy the build ran under (`jam.hash-urls`, default `true`): whether rendered versioned tokens carry the `-h{hash}` qualifier. Always written. Absent in a manifest built before the field existed means `false` — the manifest keeps serving and emitting legacy `+vN.0` URLs until a build rewrites it — while correctly qualified requests resolve and validate under either policy. The manifest carries the policy so adapters forward paths without reimplementing it.
- `cdn_base` is written only when the build ran under `cdn.enabled` with a `cdn.url` — absent from the JSON otherwise. It is the render-time default origin prefix for emitted URLs: a settings `cdnBase` overrides it and an empty one inherits it, so emitted tags need no per-host configuration beyond what the build recorded.

## Module Declaration

In JavaScript files:

```javascript
/**
 * My module
 * @requires paste/dom
 * @module site/mymodule
 */
```

In SCSS files:

```scss
/**
 * @requires site/theme
 * @module site/mypage
 */
```

## Runtime Implementation Checklist

A language runtime is a thin loader for the shared library. To conform:

- [ ] Locate the library: `PASTE_ASSETGRAPH_LIB`, else the bundled `native-lib/{platform}/{file}` copy the package ships.
- [ ] Drive page markup through `paste_render_page` with the settings JSON (`manifest`, `outputBase`, optional `outputJs`/`outputCss`, `cdnBase`, `basePath`, `inlineThreshold`, `perModule`) and the events JSON: send an empty `generation` on the page's first call, forward the answered token on each later call, and treat a `manifest-generation` failure as terminal for that page. `paste_render` remains the one-shot call for a complete event list.
- [ ] Call `paste_combo` with the settings JSON and the request path (plus `?` and raw query) for serving.
- [ ] Free every returned string — render, page, or combo — with `paste_free`; raise the envelope's `error` as the runtime's own error type, preserving `kind` when present.
- [ ] Do **not** implement tag generation, URL construction, dependency expansion, filtering, caching, or combo responses in the runtime.

Existing runtimes: `runtime/scala` (`Paste`, `PastePage`, `PasteComboRoute`), `runtime/node` (`createPaste`), `runtime/python` (`Paste`).

## Build System Primer

*(Historical — describes the Python build system JAM originated in.)*

The original system used YAML files for asset configuration:

```yaml
site.theme:
  source: static/scss/theme.scss
  output: static/css/theme.css
  requires: []

site.heroscroll:
  source: static/js/heroscroll.js
  output: static/js/heroscroll.js
  requires:
    - site.theme
```

Build script read YAML, compiled SCSS, minified, and generated a JSON manifest.

**Key concept:** Assets are declared modules with explicit dependencies. The build produces a manifest the runtime uses for URL generation and dependency resolution.

## Runtime: Jammer

*(Historical — describes the original Python runtime.)*

The original runtime was a Python class called "Jammer" (`py/source/jammer.py`):

```python
class Jammer(object):
    def __init__(self, cache_time=False, read_file=read_file, inline_threshold=1024):
        self.jams = {}
        self.cache = LRUCache(1000)
        # ...
```

Key methods:

```python
def jam(self, package, files, media=None, tag_type=None, inline=None):
    """Record a page's intent to load modules"""
    self.jams[package].update(dict.fromkeys(files, media))

def dump(self, package=None, media=None):
    """Generate markup for jams"""
    # Returns inline content or URL-based tags

def filter_loaded(self, package):
    """Remove already-loaded modules"""
    return dict((k, v) for k, v in self.jams[package].items()
                if k not in self.loads[package])
```

**Key concepts still operative:**

- `jam()` records intent; `dump()` emits markup. `render` replays this as the events JSON.
- `filter_loaded` maps to render-time filtering — a module emitted by an earlier group is dropped from later ones — and to the `filter` query, which drops already-delivered names from a served group after ordering.
- Bounded caching of rendered output has no runtime-side equivalent; caching of parsed manifests lives in the Rust library, keyed on the file's modification stamp.

**Removed:**

- The `exclude` query parameter. Render-time filtering now encodes the exact remaining group in the URL itself; ad-hoc removal on a request uses `filter`.

## Version History

- **v1.0** — Original Python implementation (YAML config, Jammer class)
- **v1.1** — Scala implementation (JamPage, Manifest-based URL generation)
