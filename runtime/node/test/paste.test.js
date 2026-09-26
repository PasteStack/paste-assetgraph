import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import { createPaste, resolveLibrary, PasteError } from '../index.js';

// The library comes from the production resolver: PASTE_ASSETGRAPH_LIB when
// set, else the bundled native-lib/ copy the package ships.
function library() {
  return resolveLibrary();
}

async function withTempDir(fn) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'paste-node-'));
  try {
    return await fn(dir);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

function write(file, content) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, content);
  return file;
}

const MANIFEST = JSON.stringify({
  version: '42',
  generated: '2026-01-01T00:00:00Z',
  modules: { paste: 'js/paste.js', app: 'js/app.js', 'site.theme': 'css/theme.css' },
  assets: {
    'js/paste.js': {
      source: 'src/paste.js', output: '/js/paste.h.v1.min.js', hash: 'h',
      version: 1, timestamp: '2026-01-01T00:00:00Z', dependencies: [], history: [],
    },
    'js/app.js': {
      source: 'src/app.js', output: '/js/app.h.v1.min.js', hash: 'h',
      version: 1, timestamp: '2026-01-01T00:00:00Z', dependencies: [], history: [],
    },
    'css/theme.css': {
      source: 'src/theme.scss', output: '/css/theme.h.v1.min.css', hash: 'h',
      version: 1, timestamp: '2026-01-01T00:00:00Z', dependencies: [], history: [],
    },
  },
  sorted_deps: {},
});

// A manifest file plus its compiled outputs, in the layout the build writes.
function e2eFixture(root) {
  const out = path.join(root, 'public');
  write(path.join(out, 'js/paste.h.v1.min.js'), 'PASTE_RT');
  write(path.join(out, 'js/app.h.v1.min.js'), 'APP');
  write(path.join(out, 'css/theme.h.v1.min.css'), 'CSS');
  const manifest = write(path.join(root, 'manifest.json'), MANIFEST);
  return { manifest, out };
}

test('page renders end-to-end through the library', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  assert.equal(page.jam({ css: ['site.theme'], js: ['app'] }), '');

  const cssTag = page.dumpCss();
  assert.match(cssTag, /stylesheet/);
  assert.match(cssTag, /site\.theme\+v1\.0/);

  const jsTags = page.dumpJs();
  assert.match(jsTags, /^<script type="text\/javascript">PASTE_RT/);
  assert.match(jsTags, /\(function\(d,r\)\{/);
  assert.match(jsTags, /app\+v1\.0/);
}));

test('combo passes through the library response, frozen', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, library: library() });

  // 1767225600 is the fixture stamp (2026-01-01T00:00:00Z).
  const versioned = await paste.combo('/paste/1767225600/app+v1.0.min.js');
  assert.equal(versioned.status, 200);
  assert.equal(versioned.cacheable, true);
  assert.equal(versioned.cacheControl, 'public, max-age=31536000');
  assert.equal(versioned.location, null);
  assert.equal(versioned.body, 'APP');
  assert.ok(Object.isFrozen(versioned));
  assert.throws(() => { versioned.body = 'X'; }, TypeError);

  const unversioned = await paste.combo('/paste/app.js');
  assert.equal(unversioned.status, 200);
  assert.equal(unversioned.cacheable, false);
  assert.equal(unversioned.cacheControl, 'no-cache');

  const missing = await paste.combo('/paste/1767225600/ghost+v1.0.min.js');
  assert.equal(missing.status, 404);
  assert.equal(missing.cacheable, false);
}));

test('a stale stamp answers a redirect carrying location', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, library: library() });

  const r = await paste.combo('/paste/1767225599/app+v1.0.min.js');
  assert.equal(r.status, 302);
  assert.equal(r.location, '/paste/1767225600/app+v1.0.min.js');
  assert.equal(r.cacheControl, 'no-cache');
  assert.equal(r.body, '');
}));

// The hash-qualified URL contract is decided in the library — the wrapper
// only forwards paths. The fixture pins the content hashes the contract
// records for these fixture bytes (golden vectors — hash calculation is
// the compiler's, tested in Rust).
const HASH = {
  APP: '0bdbb2c325525e986d55ef50ca66d056',
  PASTE_RT: '7e9e92b0857c477a6e8aebb0ab2bd924',
  CSS: '2c56c360580420d293172f42d85dfbed',
  BOGUS: '90affbd9a1954ec9ff029b7ad7183a16',
};
test('hash-qualified URLs render, serve, redirect, and validate', async () => withTempDir(async (root) => {
  const appHash = HASH.APP;
  const hashed = JSON.parse(MANIFEST);
  hashed.hash_urls = true;
  hashed.assets['js/paste.js'].hash = HASH.PASTE_RT;
  hashed.assets['js/app.js'].hash = appHash;
  hashed.assets['css/theme.css'].hash = HASH.CSS;

  const out = path.join(root, 'public');
  write(path.join(out, 'js/paste.h.v1.min.js'), 'PASTE_RT');
  write(path.join(out, 'js/app.h.v1.min.js'), 'APP');
  write(path.join(out, 'css/theme.h.v1.min.css'), 'CSS');
  const manifest = write(path.join(root, 'manifest.json'), JSON.stringify(hashed));
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });

  // Render emits the qualified token.
  const page = paste.page();
  page.jam({ js: ['app'] });
  assert.match(page.dumpJs(), new RegExp(`app\\+v1\\.0-h${appHash}`));

  // The qualified combo serves 200.
  const qualified = await paste.combo(`/paste/1767225600/app+v1.0-h${appHash}.min.js`);
  assert.equal(qualified.status, 200);
  assert.equal(qualified.body, 'APP');
  assert.equal(qualified.cacheControl, 'public, max-age=31536000');

  // A legacy request redirects to the qualified canonical URL.
  const legacy = await paste.combo('/paste/1767225600/app+v1.0.min.js');
  assert.equal(legacy.status, 302);
  assert.equal(legacy.location, `/paste/1767225600/app+v1.0-h${appHash}.min.js`);

  // A modified hash is a 404 — never a redirect onto different bytes.
  const wrong = await paste.combo(`/paste/1767225600/app+v1.0-h${HASH.BOGUS}.min.js`);
  assert.equal(wrong.status, 404);
  assert.equal(wrong.location, null);
  assert.equal(wrong.cacheControl, 'no-cache');

  // Opted out, the same manifest serves and renders the legacy form — the
  // version is unchanged — while a correctly qualified URL still resolves.
  hashed.hash_urls = false;
  const optedOut = createPaste({
    manifest: write(path.join(root, 'manifest-legacy.json'), JSON.stringify(hashed)),
    outputBase: out,
    inlineThreshold: 0,
    library: library(),
  });
  const optedOutCombo = await optedOut.combo('/paste/1767225600/app+v1.0.min.js');
  assert.equal(optedOutCombo.status, 200);
  assert.equal(optedOutCombo.body, 'APP');
  const stillQualified = await optedOut.combo(`/paste/1767225600/app+v1.0-h${appHash}.min.js`);
  assert.equal(stillQualified.status, 200);
  const page2 = optedOut.page();
  page2.jam({ js: ['app'] });
  const tags = page2.dumpJs();
  assert.match(tags, /app\+v1\.0\.min\.js/);
  assert.doesNotMatch(tags, /-h[0-9a-f]{32}/);
}));

test('finish seals the page; a jam its type never dumped fails', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  // The css dump already ran when the jam lands: site.theme is queued for
  // a dump that never comes, and finishing on the js dump must refuse —
  // not drop the stylesheet silently.
  page.dumpCss();
  page.jam({ css: ['site.theme'], js: ['app'] });
  page.dumpJs();
  assert.throws(() => page.finish(), (err) => err instanceof PasteError && /site\.theme/.test(err.message));

  // A failed finish leaves the page open: deliver the missing dump, then
  // finish — and finish again is a no-op.
  page.dumpCss();
  assert.equal(page.finish(), '');
  assert.equal(page.finish(), '');

  // A sealed page accepts no more events.
  assert.throws(() => page.jam({ js: ['app'] }), (err) => err instanceof PasteError && /finished/.test(err.message));
  assert.throws(() => page.dumpJs(), (err) => err instanceof PasteError && /finished/.test(err.message));
}));

test('finishAsync waits for pending dumps and joins concurrent callers', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  page.jam({ js: ['app'] });
  const dump = page.dumpJsAsync();
  // Neither a sync finish nor a sync dump may overtake the async dump in
  // flight — the dump would render a list whose marker is not yet delivered.
  assert.throws(() => page.finish(), (err) => err instanceof PasteError && /finishAsync/.test(err.message));
  assert.throws(() => page.dumpJs(), (err) => err instanceof PasteError && /in flight/.test(err.message));
  // Concurrent finishAsync calls share the one in-flight validation, and
  // the seal already blocks new events before the worker answers.
  const first = page.finishAsync();
  const second = page.finishAsync();
  assert.strictEqual(first, second);
  assert.throws(() => page.jam({ js: ['app'] }), (err) => err instanceof PasteError && /finished/.test(err.message));
  assert.throws(() => page.dumpJsAsync(), (err) => err instanceof PasteError && /finished/.test(err.message));
  assert.equal(await first, '');
  assert.equal(await dump.then((html) => html.startsWith('<script')), true);
  // After a successful finish the call resolves immediately.
  assert.equal(await page.finishAsync(), '');
}));

test('a failed finish leaves the page open for the missing dump', async () => withTempDir(async (root) => {
  const out = path.join(root, 'public');
  write(path.join(out, 'js/paste.h.v1.min.js'), 'PASTE_RT');
  write(path.join(out, 'js/app.h.v1.min.js'), 'APP');
  write(path.join(out, 'css/theme.h.v1.min.css'), 'CSS');
  // The manifest is absent until restored below — the first dump fails in
  // the library.
  const manifestPath = path.join(root, 'manifest.json');
  const paste = createPaste({ manifest: manifestPath, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  page.jam({ css: ['site.theme'], js: ['app'] });
  const dump = page.dumpJsAsync();
  const fin = page.finishAsync();
  await assert.rejects(fin);
  await assert.rejects(dump);

  // A failed finish is no false seal: the page accepts events again.
  page.jam({ js: ['app'] });

  // Restore the manifest — the page still owes its undumped modules.
  write(manifestPath, MANIFEST);
  await assert.rejects(page.finishAsync(), /site\.theme|app/);
  page.dumpCss();
  page.dumpJs();
  assert.equal(page.finish(), '');
}));

test('concurrent same-type async dumps render in record order', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  page.jam({ js: ['app'] });
  const first = page.dumpJsAsync();
  const second = page.dumpJsAsync();
  const [firstHtml, secondHtml] = await Promise.all([first, second]);
  // A queued dump replays only its own event prefix: the first delivers
  // the jammed group, and the second — running after it — sees the group
  // already delivered and emits nothing.
  assert.match(firstHtml, /app\+v1\.0/);
  assert.equal(secondHtml, '');
  assert.equal(await page.finishAsync(), '');
}));

test('a failed async dump cannot launder delivery into a concurrent dump', async () => withTempDir(async (root) => {
  const out = path.join(root, 'public');
  write(path.join(out, 'js/paste.h.v1.min.js'), 'PASTE_RT');
  write(path.join(out, 'js/app.h.v1.min.js'), 'APP');
  write(path.join(out, 'css/theme.h.v1.min.css'), 'CSS');
  // The manifest is absent until restored below: the first dump fails.
  const manifestPath = path.join(root, 'manifest.json');
  const paste = createPaste({ manifest: manifestPath, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  page.jam({ js: ['app'] });
  const first = page.dumpJsAsync();
  // Restoring the manifest is registered directly on the first failure,
  // before the second dump starts — its queued native call must see it.
  const restored = first.catch((err) => {
    write(manifestPath, MANIFEST);
    return err;
  });
  const second = page.dumpJsAsync();
  const secondOutcome = second.then(
    (html) => ({ ok: true, html }),
    (error) => ({ ok: false, error })
  );

  const firstError = await restored;
  assert.ok(firstError instanceof PasteError);
  const outcome = await secondOutcome;
  // The second dump replays only its own event — the failed first dump was
  // retracted — and renders the group the manifest now resolves. Rejection
  // or empty markup would both mean the failure leaked into it.
  assert.equal(outcome.ok, true, outcome.ok ? '' : String(outcome.error));
  assert.match(outcome.html, /app\+v1\.0/);
  assert.equal(await page.finishAsync(), '');
}));

// Compiles `source` with the real compiler and executes both the source and
// the emitted file in separate vm contexts, returning what each left on
// `globalThis.check`. PASTE_ASSETGRAPH_TEST_BIN names the compiler binary;
// it is required — the regression exercises minified output and would pass
// vacuously against a stale or missing binary.
function compileAndCheck(root, source) {
  const bin = process.env.PASTE_ASSETGRAPH_TEST_BIN;
  assert.ok(bin, 'PASTE_ASSETGRAPH_TEST_BIN must name a built paste-assetgraph CLI');
  write(path.join(root, 'assets/probe.js'), source);
  write(path.join(root, 'config.json'), JSON.stringify({
    source: { dirs: ['assets'] },
    output: {
      base: 'public', js: 'public/js', css: 'public/css', images: 'public/images',
    },
    manifest_path: 'public/manifest.json',
    minify: true,
    source_maps: false,
  }));
  const build = spawnSync(bin, ['build', '--config', 'config.json', '--cwd', root], { encoding: 'utf8' });
  assert.equal(
    build.status, 0,
    `compiler exited ${build.status}\nstdout: ${build.stdout}\nstderr: ${build.stderr}`
  );
  const manifest = JSON.parse(fs.readFileSync(path.join(root, 'public/manifest.json'), 'utf8'));
  const entry = manifest.assets['js/probe.js'];
  const emittedPath = path.join(root, 'public', entry.output.replace(/^\/+/, ''));
  const emitted = fs.readFileSync(emittedPath, 'utf8');
  const run = (code) => {
    const context = vm.createContext({});
    vm.runInContext(code, context);
    return context.check;
  };
  return { sourceCheck: run(source), emittedCheck: run(emitted), emitted };
}

test('the compiler preserves __proto__ shorthand semantics', () => withTempDir((root) => {
  // `{__proto__}` shorthand is an own data property named __proto__; the
  // mangler cannot rename the local binding without rewriting the key, so
  // the build must emit the source verbatim instead of minified output.
  const source = "(function () { 'use strict'; var __proto__ = {marker: 1}; var object = {__proto__}; globalThis.check = [Object.prototype.hasOwnProperty.call(object, '__proto__'), object.marker, object.__proto__.marker]; }());\n";
  const { sourceCheck, emittedCheck, emitted } = compileAndCheck(root, source);
  assert.deepEqual(Array.from(sourceCheck), [true, undefined, 1]);
  assert.deepEqual(Array.from(emittedCheck), [true, undefined, 1]);
  assert.equal(emitted, source, 'the emitted file must be the original source bytes');
}));

test('the compiler preserves an escaped __proto__ shorthand', () => withTempDir((root) => {
  // `\\u005f_proto__` decodes to `__proto__` — same property name, same
  // required fallback.
  const source = "(function () { 'use strict'; var \\u005f_proto__ = {marker: 1}; var object = {\\u005f_proto__}; globalThis.check = [Object.prototype.hasOwnProperty.call(object, '\\u005f_proto__'), object.marker, object.__proto__.marker]; }());\n";
  const { sourceCheck, emittedCheck, emitted } = compileAndCheck(root, source);
  assert.deepEqual(Array.from(sourceCheck), [true, undefined, 1]);
  assert.deepEqual(Array.from(emittedCheck), [true, undefined, 1]);
  assert.equal(emitted, source, 'the emitted file must be the original source bytes');
}));

test('the compiler preserves ordinary shorthand semantics', () => withTempDir((root) => {
  // The binding is a local — the mangler renames it, so this control proves
  // mangled shorthand round-trips correctly for every key but __proto__.
  const source = '(function () { var value = 42; var object = {value}; globalThis.check = [object.value]; }());\n';
  const { sourceCheck, emittedCheck } = compileAndCheck(root, source);
  assert.deepEqual(Array.from(sourceCheck), [42]);
  assert.deepEqual(Array.from(emittedCheck), [42]);
}));

test('a render naming an unknown module raises PasteError naming it', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  page.jam({ js: ['ghost'] });
  assert.throws(
    () => page.dumpJs(),
    (err) => err instanceof PasteError && /ghost/.test(err.message)
  );
}));

test('async dumps render the same markup as the sync dumps', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  page.jam({ css: ['site.theme'], js: ['app'] });

  const cssTag = await page.dumpCssAsync();
  const jsTags = await page.dumpJsAsync();
  assert.match(cssTag, /stylesheet/);
  assert.match(cssTag, /site\.theme\+v1\.0/);
  assert.match(jsTags, /^<script type="text\/javascript">PASTE_RT/);
  assert.match(jsTags, /app\+v1\.0/);
  // The js dump emits no css module, and vice versa.
  assert.doesNotMatch(jsTags, /site\.theme/);

  const syncPage = paste.page();
  syncPage.jam({ css: ['site.theme'], js: ['app'] });
  assert.equal(cssTag, syncPage.dumpCss());
  assert.equal(jsTags, syncPage.dumpJs());
}));

test('jam does not alias caller arrays', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });

  const css = ['site.theme'];
  const js = ['app'];
  const page = paste.page();
  page.jam({ css, js });
  // Mutating the caller's arrays must not rewrite the recorded event: an
  // aliased array would render 'ghost', which is not in the manifest.
  css.length = 0;
  js.length = 0;
  js.push('ghost');

  const jsTags = page.dumpJs();
  assert.match(jsTags, /app\+v1\.0/);
  assert.doesNotMatch(jsTags, /ghost/);
}));

test('resolveLibrary prefers the env override and fails a bad one loudly', () => withTempDir((root) => {
  const lib = write(path.join(root, 'fake-lib.dylib'), 'x');

  assert.equal(resolveLibrary({ PASTE_ASSETGRAPH_LIB: lib }), lib);

  // An override that is set but not a regular file fails loudly — it never
  // falls through to the bundled copy.
  const missing = path.join(root, 'missing');
  assert.throws(
    () => resolveLibrary({ PASTE_ASSETGRAPH_LIB: missing }),
    (err) => err instanceof PasteError && err.message.includes(missing)
  );
  const dir = path.join(root, 'dir');
  fs.mkdirSync(dir);
  assert.throws(
    () => resolveLibrary({ PASTE_ASSETGRAPH_LIB: dir }),
    (err) => err instanceof PasteError && err.message.includes(dir)
  );

  // An override that is set but empty is configured — it must fail rather
  // than silently fall through to the bundled copy.
  assert.throws(
    () => resolveLibrary({ PASTE_ASSETGRAPH_LIB: '' }, root),
    (err) => err instanceof PasteError && /PASTE_ASSETGRAPH_LIB/.test(err.message)
  );
}));

// The platform directory resolveLibrary looks under — mirrors index.js:
// Linux bundles split by libc, so the fixture must name the same variant.
function bundledPlatformDir() {
  const os = { darwin: 'darwin', linux: 'linux', win32: 'windows' }[process.platform];
  const arch = { arm64: 'aarch64', x64: 'amd64' }[process.arch];
  const libc = os === 'linux'
      && !process.report.getReport().header.glibcVersionRuntime
    ? '-musl'
    : '';
  return `${os}-${arch}${libc}`;
}

test('resolveLibrary finds the bundled copy under the package dir', () => withTempDir((root) => {
  const name = {
    darwin: 'libpaste_assetgraph_ffi.dylib',
    linux: 'libpaste_assetgraph_ffi.so',
    win32: 'paste_assetgraph_ffi.dll',
  }[process.platform];
  const bundled = write(path.join(root, 'native-lib', bundledPlatformDir(), name), 'x');
  assert.equal(resolveLibrary({}, root), bundled);
}));

test('resolveLibrary names both places and the platform when nothing resolves', () => withTempDir((root) => {
  assert.throws(
    () => resolveLibrary({}, root),
    (err) =>
      err instanceof PasteError &&
      /PASTE_ASSETGRAPH_LIB/.test(err.message) &&
      /native-lib/.test(err.message) &&
      /platform/.test(err.message)
  );
}));

test('a combo path containing NUL is rejected, not truncated', () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, library: library() });

  await assert.rejects(
    paste.combo('/paste/app.js\0/paste/1770000000/app+v1.0.min.js'),
    (err) => err instanceof PasteError && err.message === 'combo path must not contain NUL'
  );
}));

test('repeated createPaste loads do not exhaust koffi type registrations', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  for (let i = 0; i < 100; i++) {
    const paste = createPaste({ manifest, outputBase: out, library: library() });
    assert.equal(paste.basePath, '/paste');
  }
}));
