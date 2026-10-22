# paste-assetgraph (Python)

Thin Python loader for the `paste-assetgraph` native library. All tag, URL
and combo logic lives in the Rust library — this package locates it and
calls `paste_render_page`, `paste_combo` and `paste_combo_conditional`
in-process through `ctypes` (no third-party
dependency). It imports no web framework: usable from FastAPI, Flask, Jinja,
or anything else.

Requires Python >= 3.9, no runtime dependencies.

```bash
pip install -e runtime/python          # or add runtime/python/src to PYTHONPATH
```

## API

```python
from paste_assetgraph import Paste, PastePage, ComboResponse, PasteError, resolve_library

paste = Paste(
    "public/manifest.json",    # built by paste-assetgraph
    "public",                  # compiled output root
    output_js=None, output_css=None,   # default <output_base>/js|css
    cdn_base="",               # URL prefix for emitted tags; empty
                               # inherits the manifest's cdn_base
    base_path="/paste",        # combo URL prefix
    inline_threshold=1024,     # automatic inline limit; 0 disables
    per_module=False,          # request each versioned file in
                               # dependency order, not combo URLs
    library=None,              # optional explicit library path
)
```

- `paste.base_path` — the configured combo URL prefix.
- `paste.page() -> PastePage` — a per-render page object.
- `paste.combo(path_and_query, if_modified_since=None) -> ComboResponse` —
  resolves a request path (with optional `?query`) to `status`,
  `content_type`, `cache_control`, `cacheable`, `body`, `location`,
  `last_modified`. `location` is `None` unless the response is a redirect
  to the canonical versioned URL. Pass the request's `If-Modified-Since`
  header value to evaluate the library's manifest-derived conditional — a
  matching validator answers `status` 304 with an empty `body`.

### PastePage

```python
page = paste.page()
page.jam(css=["site.theme"], js=["paste.dom"])   # records, returns ""
page.dump("css")  # markup for css jams so far
page.dump("js")   # inlined runtime (once) + loader/inline tags
page.dump()       # both in one dump — css markup first, then js
page.finish()     # validates and seals the page, returns ""
```

`jam(css=(), js=(), inline=False, sync=False, media=None, condition=None,
per_module=None)`. `condition` wraps the group's tags in
`<!--[if {cond}]>`…`<![endif]-->` and keeps a conditional js group out
of the loaded set — a later unconditional jam serves those modules again
(a conditional css group still marks loaded, as the original did).
`per_module` is a nullable per-jam override: `True` or `False` pins that
jam's emission mode, while `None` inherits the settings-level
`per_module`. With per-module on, an asynchronous JavaScript group
requests its versioned files in dependency order through one loader
block — each request starts after the previous file loads — and every
external tag names the module's own URL-encoded versioned output path
instead of a combo URL; the file must be readable and hash-verified at
render time. Files before the first external request may still inline;
from the first external request onward the group stays external even
with `inline=True`, so no script can overtake its dependency or lose
global scope. With `sync=True` the group's files run in order through
synchronous `src` and inline tags, each file deciding to inline on its
own; CSS emits one link or style tag per module with its own inlining
decision. Separate asynchronous groups remain independent, as in combo
mode. The runtime follows the same contract: verified content that
cannot safely inline falls back to synchronous tags naming the versioned
runtime files rather than a combo URL, and a missing or tampered runtime
file is a render error. A `PastePage` is not thread-safe —
create one per render.

`finish()` belongs at the end of a full-page render — usable as the last
line of a Jinja template since it returns `""`. The library verifies every
jammed module reached a dump of its type (css delivery is tracked per
`media`), then seals the page: later jams and dumps raise `PasteError`. A
jam queued after its type's last dump — a css jam below the head's css dump
— fails finish naming the module instead of dropping it silently; a failed
finish leaves the page open so the missing dump can be added and finish
retried. Repeated successful calls return `""`.

### resolve_library(env=os.environ, package_dir=<package dir>) -> str

Lookup order: `PASTE_ASSETGRAPH_LIB` → the bundled
`native-lib/{platform}/libpaste_assetgraph_ffi.{so,dylib}` (or
`paste_assetgraph_ffi.dll` on Windows) under `package_dir` → raises
`PasteError` naming both places searched and the platform. The `{platform}`
segment is `{os}-{arch}`; on Linux it also carries the libc —
`linux-amd64` on glibc hosts, `linux-amd64-musl` on musl hosts such as
Alpine — since a musl process cannot load a glibc build. An override that
is set — including an empty value — but not a regular file raises
`PasteError` naming the value rather than falling through.

### PasteError / ComboResponse

`PasteError` (a `RuntimeError`) is raised when the library reports an error —
an invalid settings JSON, an unreadable manifest, or a render naming an
unknown module; the message is the library's. A page pins the manifest
generation its first dump saw: a build landing between two dumps raises
`PasteError` with `kind == "manifest-generation"` and retires the page for
good — discard it and render the request again on a fresh `paste.page()`.
`ComboResponse` is a frozen dataclass mirroring the library's JSON.

## Tests

```bash
PYTHONPATH=runtime/python/src python3 -m unittest discover -s runtime/python/tests -v
```

The tests resolve the library exactly like the package does at run time:
`PASTE_ASSETGRAPH_LIB` when set, else the bundled `native-lib/` copy the
package ships — so a plain run exercises the bundled package. They also
verify a shared CLI-built distribution: generate it once and export
`PASTE_ASSETGRAPH_TEST_FIXTURE`, or the suite fails on its absence:

```bash
cargo build --locked --workspace
python3 tests/build_shared_fixture.py \
  --build debug \
  --out /path/to/empty/shared-fixture
export PASTE_ASSETGRAPH_TEST_FIXTURE="/path/to/empty/shared-fixture"  # absolute
```

To run them against a source-built library, build the ffi crate and
point the variable at it:

```bash
cargo build -p paste-assetgraph-ffi
PYTHONPATH=runtime/python/src \
  PASTE_ASSETGRAPH_LIB=$PWD/target/debug/libpaste_assetgraph_ffi.dylib \
  python3 -m unittest discover -s runtime/python/tests -v
```
