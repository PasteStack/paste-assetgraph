# Using paste-assetgraph with Node/Express

The `@pastestack/assetgraph` package is a thin loader for the Rust shared
library: all tag, URL and combo logic lives in the library. `createPaste`
calls `paste_render_page` and `paste_combo` in-process through `koffi` with your
manifest and output paths.

The package is ESM and requires Node >= 18. Its only runtime dependency is
`koffi`; the shared library ships inside the package under `native-lib/`.

## Installation

```bash
npm install @pastestack/assetgraph
```

`resolveLibrary()` looks up `PASTE_ASSETGRAPH_LIB` first, then the bundled
`native-lib/{platform}` copy — a set-but-invalid override throws rather than
falling through.

```js
// app.js (ESM)
import { createPaste } from '@pastestack/assetgraph';

const paste = createPaste({
  manifest: 'static/manifest.json',
  outputBase: 'static',
  // optional: outputJs, outputCss, cdnBase = '' (empty inherits the
  //           manifest's cdn_base), basePath = '/paste',
  //           inlineThreshold = 1024,
  //           perModule = false — one versioned-file request per
  //           resolved module in dependency order; a jam's perModule
  //           overrides it
});
```

## Express combo middleware

`paste.combo(pathAndQuery)` is asynchronous — the library call runs on
koffi's worker and never blocks the event loop. Mount a middleware scoped to
`paste.basePath` and pass the request's original URL through; the library
parses the path and query itself.

```js
import express from 'express';

const app = express();

// Per-module dumps request each versioned file in dependency order,
// naming its output path — /js/... and /css/... under the default flat
// layout. The combo route does not serve them; map those paths to the
// compiled output through the host's ordinary static file handling.
app.use('/js', express.static('static/js'));
app.use('/css', express.static('static/css'));

app.use(paste.basePath, async (req, res, next) => {
  try {
    // Forwarding the request's If-Modified-Since lets the library answer
    // 304 for an unchanged immutable response — pass it, never your own.
    // If-None-Match outranks the date condition (RFC 9110 §13.2.2): the
    // library evaluates no entity tags, so the date must not decide while
    // a tag condition stands unevaluated.
    const ifModifiedSince = req.headers['if-none-match'] === undefined
      ? req.headers['if-modified-since']
      : undefined;
    const r = await paste.combo(req.originalUrl, { ifModifiedSince });
    if (r.location !== null) {
      res.set('Location', r.location);
    }
    if (r.lastModified !== null) {
      res.set('Last-Modified', r.lastModified);
    }
    // end(), not send(): send() adds an ETag and re-runs Express's own
    // freshness check, which would turn the library's deliberate 200 for a
    // mutable unversioned or require combo into a stale 304.
    res.status(r.status)
       .set('Content-Type', r.contentType)
       .set('Cache-Control', r.cacheControl)
       .end(r.body);
  } catch (err) {
    next(err);   // PasteError carries the library's message
  }
});
```

Versioned URLs serve each `+vN` pin exactly — the current version or a
retained history version — and under the default `jam.hash-urls` policy
each pin also carries the version's full content hash (`-h{hash}`), which
a request must match exactly or get a `no-cache` 404; successful
versioned responses get `Cache-Control: public, max-age=31536000`;
unversioned and `require` responses succeed with 200 and `no-cache`; a
non-canonical versioned URL (a stale stamp, an unpinned
name, a non-numeric segment, a legacy `+vN` token under a qualified
manifest) is a `302` whose `location` field is the
canonical URL — a fully pinned list is served in the order written, so
two orders of the same members are two different canonical addresses; and a version or module the manifest cannot serve, or a stamp
newer than the manifest knows, is a `no-cache` 404 — the response is a frozen
object whose fields pass through unchanged. The wrapper
adds no cache of its own — every call reaches the library, which keeps a
bounded manifest cache that reloads when the manifest file changes.

The static mounts are the host's responsibility and must match the emitted
output paths: the flat layout above emits `/js/…` and `/css/…` paths
relative to `outputBase`, so `/js` maps to `static/js`. Custom
`outputJs`/`outputCss` settings change the filesystem roots behind `/js`
and `/css`; they do not rewrite manifest output paths. A CDN prefix or
beside-source layout requires matching URL routing at the static origin;
the emitted URL is the manifest's `output` pointer with one leading slash,
percent-encoded per segment.

## Page rendering

`paste.page()` returns a per-render event list. Dumps come in two forms over
the same event list: `dumpAsync(type)` may be invoked concurrently
— each records its event immediately — while their native calls queue per
page and run in record order on koffi's worker, never blocking the event
loop. Use them in async handlers. The **synchronous** `dump(type)`
calls the library in place for synchronous template engines; they throw while
async dump or finish work is outstanding — await it first. `type` is `'css'`
or `'js'`; no argument dumps both in one event — css markup first, then js.

```js
app.get('/', async (req, res, next) => {
  try {
    const page = paste.page();
    page.jam({ css: ['site.theme'], js: ['paste.dom', 'paste.ui.heroscroll'] });
    const css = await page.dumpAsync('css');
    const js = await page.dumpAsync('js');
    await page.finishAsync();   // validates before the response is sent
    res.render('home', { title: 'My Site', css, js });
  } catch (err) {
    next(err);
  }
});
```

`page.jam({ css, js, inline, sync, media, condition, perModule })` records a group and returns `''`;
the dumps emit the markup — the inlined runtime once, exact-group loader URLs
with `isLoading` marks, threshold-based inline groups. Insert them unescaped
in your template engine of choice (e.g. `<%- css %>` in EJS, `{{{css}}}` in
Handlebars).

Finish the page after the last dump: `finishAsync()` awaits any in-flight
async dumps, then the library verifies every jammed module was emitted — a
jam queued after its type's final dump (a css jam below the head's css dump)
throws a `PasteError` naming the module rather than dropping it silently. In
synchronous handlers `finish()` does the same check; it throws instead of
blocking when async work is still in flight. Both are idempotent — a
repeated call returns `''` — and a failed finish leaves the page open so the
missing dump can still be added and finish retried. Delivery is tracked per
type (and per `media` for css), so jams and dumps of the two types compose.

## Project Structure

```
my-app/
├── paste-assetgraph.conf
├── static/
│   ├── manifest.json          # Generated by paste-assetgraph
│   ├── js/                    # Compiled output
│   └── css/
├── views/
│   └── home.ejs
└── app.js
```

## Build & Run

```bash
# Build assets
paste-assetgraph build

# Run
node app.js

# Development: rebuild on change in a second terminal
paste-assetgraph watch
```

## Tests

```bash
cd runtime/node
node --test
```
