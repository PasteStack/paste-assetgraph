# paste-assetgraph (Python)

Thin Python loader for the `paste-assetgraph` native library. All tag, URL
and combo logic lives in the Rust library — this package locates it and
calls `render`/`combo` in-process through `ctypes` (no third-party
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
    cdn_base="",               # URL prefix for emitted tags
    base_path="/paste",        # combo URL prefix
    inline_threshold=1024,     # automatic inline limit; 0 disables
    library=None,              # optional explicit library path
)
```

- `paste.base_path` — the configured combo URL prefix.
- `paste.page() -> PastePage` — a per-render page object.
- `paste.combo(path_and_query) -> ComboResponse` — resolves a request path
  (with optional `?query`) to `status`, `content_type`, `cache_control`,
  `cacheable`, `body`, `location`. `location` is `None` unless the response
  is a redirect to the canonical versioned URL.

### PastePage

```python
page = paste.page()
page.jam(css=["site.theme"], js=["paste.dom"])   # records, returns ""
page.dump_css()   # markup for all jams so far
page.dump_js()    # inlined runtime (once) + loader/inline tags
page.finish()     # validates and seals the page, returns ""
```

`jam(css=(), js=(), inline=False, sync=False, media=None)`. A `PastePage` is
not thread-safe — create one per render.

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
unknown module; the message is the library's. `ComboResponse` is a frozen
dataclass mirroring the library's JSON.

## Tests

```bash
PYTHONPATH=runtime/python/src python3 -m unittest discover -s runtime/python/tests -v
```

The tests resolve the library exactly like the package does at run time:
`PASTE_ASSETGRAPH_LIB` when set, else the bundled `native-lib/` copy the
package ships — so a plain run exercises the bundled package. To run them
against a source-built library, build the ffi crate and point the variable
at it:

```bash
cargo build -p paste-assetgraph-ffi
PYTHONPATH=runtime/python/src \
  PASTE_ASSETGRAPH_LIB=$PWD/target/debug/libpaste_assetgraph_ffi.dylib \
  python3 -m unittest discover -s runtime/python/tests -v
```
