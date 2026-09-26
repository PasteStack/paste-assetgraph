# JAM (JavaScript Asset Management) Specification

## Overview

JAM is a system for managing JavaScript and CSS module dependencies in web applications. It generates optimized, cacheable URLs for module groups, serves their compiled output through a combo endpoint, and renders the markup that loads them.

**The Rust library owns all of it.** Tag generation, URL construction, dependency filtering and expansion, rendering, and combo responses are implemented once in `paste-assetgraph` and exposed through a small C ABI:

- `paste_render(settings_json, events_json)` — renders a JSON event list and returns the dump HTML; an event list ending in the bare string `"finish"` is a completion check instead — it validates that every jammed module was dumped and returns an empty string;
- `paste_combo(settings_json, path_and_query)` — resolves a request path (optionally followed by `?query`) to a response (`status`, `contentType`, `cacheControl`, `cacheable`, `location`, `body`).

Both return a JSON envelope — `{"ok": ...}` or `{"error": "<message>"}` — as a heap string the caller releases with `paste_free`. The same logic also backs the `render`/`combo` commands of the `paste-assetgraph` CLI. There is no server in the library: `combo` returns a response description — including a `location` for the canonical-URL redirects below — and the host framework writes the HTTP answer.

Language runtimes (Scala, Node, Python) are thin loaders: they locate the shared library (`PASTE_ASSETGRAPH_LIB`, else the bundled copy), call it with a settings JSON, and surface the envelope. They implement no tag, URL, dependency, combo, or caching logic of their own. Static sites do not need a runtime at all — `build` renders the same markup into HTML templates at build time.

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
- Module order is dependency order (dependencies first), not request order — for groups of current versions. A group containing a historical pin keeps the order the URL lists.
- `~` separates modules in emitted URLs; `,` is accepted as a separator when a combo request is parsed, and both are accepted in `filter` lists.
- `.min.js` and `.js` (likewise `.min.css`/`.css`) are both accepted. They serve the same compiled output — the suffix is part of the URL contract, not a serving-mode switch; whether output is minified is decided by the build (`--debug` build flag).

**Long lists.** A hash-qualified list can exceed a filesystem's component limit (255 bytes) when a static build materializes each URL as a file. A rendered URL therefore may continue the list over `/`-separated path components — each component carries complete `~`-joined tokens, the `.min.{ext}` suffix lands only on the last component, and the combo endpoint joins the components back into one ordered list. The component boundary is a separator, never part of a token: the canonical form is still the single-component URL, and a segmented canonical request serves 200 while a stale one redirects to the joined canonical form.

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

The canonical list for an all-current group is the listed modules ordered by the manifest's dependency layering; once any pinned version is historical, the canonical list is the listed order itself. The canonical stamp is the maximum epoch seconds over the resolved versions' own timestamps — history entries carry theirs. A segment that is not all digits, a listed order that differs from canonical, or a stamp below canonical is a redirect; a stamp above is the not-found form.

Query parsing is form decoding (`+` means space in query values; the `+` in the path's `name+vN.0` tags stays literal — the path segment is percent-decoded, not form-decoded). `filter` names that are not valid module names are ignored. `require` may appear with any value (`?require`, `?require=1`).

Each `+vN` in a versioned list entry is served exactly: the module's current version when `N` matches it, or a version retained in the entry's `history` whose compiled file is still on disk (the `dist` storage backend keeps the current output plus the `storage.keep-versions` most recent historical versions; `local` keeps only the current one). A version the server cannot serve — newer than the manifest knows, or pruned from history — is a 404 with `no-cache`, never another version's content. A malformed version token — a bad major/minor, an overflowing version, or a malformed `-h` qualifier (empty, non-hex, or not exactly 32 hex characters) — or the same module listed at two different pins (different versions, or a hash on one spelling but not the other, or two different hashes) is a 404 too.

A supplied `-h` hash is checked against the resolved version's recorded hash **before any redirect decision** — for every listed module, including one `?filter=` would later drop — so a wrong hash is always a `no-cache` 404. Under a hash-enabled manifest a legacy `+vN` request canonicalizes onto the qualified URL (302); under an opted-out manifest legacy URLs serve as before while qualified requests still resolve and validate.

An entry whose source was deleted is marked `retired` in the manifest. It resolves through a `+vN` pin only — the current version included — and each resolution counts as historical for canonical ordering. Every unversioned form ignores retired entries entirely: the bare list, `require`, `filter`, the loader's packages-segment fetch, and render-time module expansion all see only the active set.

Under the cacheable form every listed entry must pin a version — with one exception: the runtime loader fetches `{packagesUrl}{module}.js`, i.e. `{base}/{packages-segment}/{module}.js`. That unversioned URL is served and year-cached only for a js list under this manifest's own packages segment; any other unpinned entry under a versioned segment redirects to its canonical pinned URL like any non-canonical spelling. Under `require` and the unversioned single-segment form, unversioned entries resolve to the current version — those responses are `no-cache`.

**Rolling deploys.** A page rendered by build N can reach a server already running build N+1. The page's `+vN` pins ask for exactly what was rendered, and the packages segment pins the loader fetch to the build that produced the page, so old-version requests keep working while the files are retained — and anything the server cannot serve exactly is a `no-cache` 404 rather than a year-cached wrong version. Serving whatever the current manifest holds under a versioned URL would let a CDN pin v3 content under a v2 URL.

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
asset of the dump's type fails the `render` command (and therefore the
build) rather than emitting a URL that cannot be served.

## Bootstrap HTML

`render` replays a JSON event list and prints the markup the last `dump` event produces. A list ending in the bare string `"finish"` is a completion validation, not a render: the page is checked (every jammed module dumped — see Page Tracking) and `render` prints nothing. Input:

```json
{"events": [
  {"jam": {"css": ["site.theme"], "js": ["paste.dom", "paste.event"]}},
  {"dump": "css"},
  {"dump": "js"}
]}
```

Output for the first `js` dump (the `paste` runtime module is inlined ahead of it, once):

```html
<script type="text/javascript">…paste runtime source…
;
paste.config({packagesUrl:'/paste/71f8e72a3d6b/'});</script>
<script type="text/javascript">(function(d,r){d('paste.dom').isLoading=d('paste.event').isLoading=!0;r('/paste/1770462000/paste.dom+v2.0~paste.event+v1.0.min.js');})(paste.define,paste.require);</script>
```

- The runtime script is emitted once, before the first JS tag, and joins `paste.config({packagesUrl:'{cdn}{base}/{segment}/'})` onto the runtime source with the `\n;\n` separator so a runtime ending in a `//` comment cannot swallow the config. `segment` is the first 12 hex chars of MD5 over the manifest's sorted JS `key:hash` pairs — it changes whenever any JS content changes, so a deterministic build still gets fresh loader URLs. When the runtime source cannot be inlined (missing or unsafe content) it is emitted as an external `<script src>` plus a separate config script instead.
- The async loader marks each pending module `isLoading` and `paste.require`s the exact-group URL.
- `jam.sync = true` emits `<script type="text/javascript" src="{cdn}{base}/{ts}/{list}.min.js"></script>` instead.
- CSS groups emit `<link rel="stylesheet" href="{cdn}{base}/{ts}/{list}.min.css"/>` — with `media="print"` when the jam event carries `media` — or an inline `<style type="text/css">…</style>` when the group is below the inline threshold.
- Groups at or under `inline_threshold` UTF-8 bytes of compiled output (default `1024`, `0` disables automatic inlining) are inlined as `<script type="text/javascript">…</script>` / `<style>…</style>`; `jam.inline = true` forces inlining regardless of size. JS content containing `</script` or `<!--`, and CSS containing `</style`, is never inlined — the group falls back to the external tag.
- A page jams `paste` itself as an ordinary module: the runtime emits `paste` inline first, so a later `paste.dom` group's URL names only `paste.dom` — `paste` is already delivered and stays out of the URL entirely (no exclusion query is needed).

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

`combo` resolves any of the request forms above to a single response. Each `+vN` pin resolves through the manifest: the entry's current output when `N` is the current version, else a retained `history` version's output, else the 404 form. For the versioned form the resolved group is checked against its canonical URL — canonical order (the layering for all-current groups, the listed order once a historical pin is present), the `+vN.0` spellings, and the canonical stamp — and a mismatch answers the 302 form with `location` set to the canonical URL plus the request's original query. A stamp above canonical is the 404 form. The group is read from compiled output under the output directories, joined with the `\n;\n` separator for JS and newline for CSS. Any member that cannot be resolved and read, and any empty or invalid module name or version token in the list, turns the whole response into the 404 form; a partial group is never served. Cache-Control is `public, max-age=31536000` for a successful versioned non-require response — `filter` does not change that — and `no-cache` for unversioned, require, redirect, and 404 responses.

### 7. Inline Threshold

Automatic inlining applies to groups whose joined compiled output is at most `inline_threshold` bytes (default `1024`; `0` disables it). `jam.inline` forces inlining regardless of threshold. Unsafe content never inlines (see Bootstrap HTML).

### 8. Page Tracking via Event Replay

A page render is an ordered list of events: `{"jam":{...}}` records a group; `{"dump":"css"|"js"}` renders the markup for all events so far. Modules already emitted by earlier groups are excluded from later ones — state lives in the event list, so any wrapper renders identically by replaying the same events. Each dump re-renders the full list; the last dump's output is what `render` prints.

A page ends with the bare string `"finish"`, which must be the last event and produces no markup. Finish is the completion check: the library replays the same plan the dumps used — pending queues per type, loaded sets with per-media css delivery — and refuses the page if any jammed module was never dumped, naming the asset type and module (and the media for css). A module already delivered under the same media is not outstanding; a failed finish leaves the page open so the missing dump can still be added and finish retried. Ordinary dumps stay partial checkpoints — only finish validates completeness. `render` of a finished list returns `""`.

### 9. Build-Time Templates

`build` (and every `watch` rebuild) renders HTML templates when `templates.source` is configured: `<!-- paste:jam ... -->` and `<!-- paste:dump css|js -->` directives in `*.html` under the source directory produce the same markup as a runtime `render` call. Rendered pages land under `templates.output`; every referenced combo file and the per-module loader files are materialized under `output.base`. The build fails if `templates.output` equals or nests inside `templates.source`. End of file is the page's finish — a jam queued after its type's last dump fails the build rather than shipping a page that silently lost it, so css for `<head>` must be jammed before the head's css dump; completion detects the omission, it cannot move the tag back.

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
- `assets[key].output` is the path under the output directory; `dependencies` are module names; `version` and `timestamp` feed the URL tags and timestamp.
- `assets[key].retired` is `true` only on an entry whose source was deleted — the field is always written, and absent in an older manifest means active: its retained pins keep serving while nothing unpinned resolves it.
- `sorted_deps` is a named-module index the build emits per type (`"js"`, `"css"`): topologically sorted `[module_name, output_path, version]` triples over active assets.
- `generated_dirs` lists project-relative directories the build has generated into — how a layout or build-directory change relocates retained outputs without a filesystem search, and what discovery excludes so generated files never re-enter as sources.
- `hash_urls` records the URL policy the build ran under (`jam.hash-urls`, default `true`): whether rendered versioned tokens carry the `-h{hash}` qualifier. Always written. Absent in a manifest built before the field existed means `false` — the manifest keeps serving and emitting legacy `+vN.0` URLs until a build rewrites it — while correctly qualified requests resolve and validate under either policy. The manifest carries the policy so adapters forward paths without reimplementing it.

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
- [ ] Call `paste_render` with the settings JSON (`manifest`, `outputBase`, optional `outputJs`/`outputCss`, `cdnBase`, `basePath`, `inlineThreshold`) and the events JSON for page markup.
- [ ] Call `paste_combo` with the settings JSON and the request path (plus `?` and raw query) for serving.
- [ ] Free every returned string with `paste_free`; raise the envelope's `error` as the runtime's own error type.
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
