# @pastestack/assetgraph

Thin Node.js loader for the `paste-assetgraph` native library. All tag, URL
and combo logic lives in the Rust library — this package locates it, loads
it through `koffi`, and calls `render`/`combo` in-process.

ESM, Node >= 18.

## API

```js
import { createPaste, resolveLibrary, PasteError } from '@pastestack/assetgraph';

const paste = createPaste({
  manifest: 'public/manifest.json',   // built by paste-assetgraph
  outputBase: 'public',               // compiled output root
  // outputJs, outputCss,             // optional, default <outputBase>/js|css
  cdnBase: '',                        // URL prefix for emitted tags
  basePath: '/paste',                 // combo URL prefix
  inlineThreshold: 1024,              // automatic inline limit; 0 disables
  // library,                         // optional explicit library path
});
```

- `paste.basePath` — the configured combo URL prefix.
- `paste.page()` — a per-render page object (see below) with synchronous and
  asynchronous dumps.
- `await paste.combo(pathAndQuery)` — resolves a request path (with optional
  `?query`) to a frozen `{ status, contentType, cacheControl, cacheable,
  body, location }`. `location` is `null` unless the response is a redirect
  to the canonical versioned URL. Runs on koffi's worker thread, so it never
  blocks the event loop.

### Page object

```js
const page = paste.page();
page.jam({ css: ['site.theme'], js: ['paste.dom'] });  // records, returns ''
page.dumpCss();   // markup for all jams so far
page.dumpJs();    // inlined runtime (once) + loader/inline tags
await page.dumpCssAsync();  // the same renders off the event loop
await page.dumpJsAsync();
```

Dumps come in sync and async forms over the same event list. The async forms
and `combo` run on koffi's worker and never block the event loop — use them
in async handlers and engines. The sync `dumpCss`/`dumpJs` call the library
in place — for synchronous template engines. A page holds the render's
mutable event list — create one per render, never share it across requests.

### `resolveLibrary(env = process.env, dir = <package dir>)`

Lookup order: `PASTE_ASSETGRAPH_LIB` → the bundled
`native-lib/{platform}/libpaste_assetgraph_ffi.{so,dylib}` (or
`paste_assetgraph_ffi.dll` on Windows) under `dir` → throws a `PasteError`
naming both places searched and the platform. The `{platform}` segment is
`{os}-{arch}`; on Linux it also carries the libc — `linux-amd64` on glibc
hosts, `linux-amd64-musl` on musl hosts such as Alpine — since a musl
process cannot load a glibc build. An override that is set — including an
empty value — but not a regular file throws a `PasteError` naming the
value rather than falling through.

### `PasteError`

Thrown when the library reports an error — an invalid settings JSON, an
unreadable manifest, or a render naming an unknown module; the message is
the library's.

## Tests

The tests compile real assets and resolve the library exactly like the
package does at run time. They need a built compiler —
`PASTE_ASSETGRAPH_TEST_BIN` names the `paste-assetgraph` CLI (the tests
fail without it; the variable is test-only and no production code path
spawns the compiler). Build it from the repository root:

```bash
cargo build --locked --workspace
```

To exercise the bundled `native-lib/` copy the package ships:

```bash
PASTE_ASSETGRAPH_TEST_BIN="$PWD/target/debug/paste-assetgraph" npm --prefix runtime/node test
```

To run against a source-built library instead, set `PASTE_ASSETGRAPH_LIB`
alongside it (the FFI filename varies by platform — `.dylib` on macOS,
`.so` on Linux, `.dll` on Windows):

```bash
PASTE_ASSETGRAPH_TEST_BIN="$PWD/target/debug/paste-assetgraph" \
PASTE_ASSETGRAPH_LIB="$PWD/target/debug/libpaste_assetgraph_ffi.dylib" \
  npm --prefix runtime/node test
```
