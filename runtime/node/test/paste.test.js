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
      source: 'src/paste.js', output: '/js/paste.h.v1.min.js',
      hash: '7e9e92b0857c477a6e8aebb0ab2bd924',
      version: 1, timestamp: '2026-01-01T00:00:00Z', dependencies: [], history: [],
    },
    'js/app.js': {
      source: 'src/app.js', output: '/js/app.h.v1.min.js',
      hash: '0bdbb2c325525e986d55ef50ca66d056',
      version: 1, timestamp: '2026-01-01T00:00:00Z', dependencies: [], history: [],
    },
    'css/theme.css': {
      source: 'src/theme.scss', output: '/css/theme.h.v1.min.css',
      hash: '2c56c360580420d293172f42d85dfbed',
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

  const cssTag = page.dump('css');
  assert.match(cssTag, /stylesheet/);
  assert.match(cssTag, /site\.theme\+v1\.0/);

  const jsTags = page.dump('js');
  assert.match(jsTags, /^<script type="text\/javascript">PASTE_RT/);
  assert.match(jsTags, /\(function\(d,r\)\{/);
  assert.match(jsTags, /app\+v1\.0/);
}));

test('a no-argument dump renders every type, equal to the two typed dumps', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });

  const reference = paste.page();
  reference.jam({ css: ['site.theme'], js: ['app'] });
  const expected = `${reference.dump('css')}\n${reference.dump('js')}`;

  const page = paste.page();
  page.jam({ css: ['site.theme'], js: ['app'] });
  assert.equal(page.dump(), expected);
  assert.equal(page.finish(), '');
}));

test('a dump with a bad type rejects before any native call', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  page.jam({ js: ['app'] });

  // 'all' is the no-argument spelling; as a value it is not a valid type.
  for (const bad of ['all', 'css js', 'style', 0, null]) {
    assert.throws(
      () => page.dump(bad),
      (err) => err instanceof PasteError && /dump takes/.test(err.message)
    );
    assert.throws(
      () => page.dumpAsync(bad),
      (err) => err instanceof PasteError && /dump takes/.test(err.message)
    );
  }

  // Nothing was recorded: the jam still dumps and the page finishes.
  assert.match(page.dump('js'), /app\+v1\.0/);
  assert.equal(page.finish(), '');
}));

test('a conditional jam wraps tags and does not mark js loaded', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  page.jam({ js: ['app'], condition: 'lt IE 9' });
  const first = page.dump('js');
  assert.match(first, /<!--\[if lt IE 9\]>\n/);
  assert.match(first, /\n<!\[endif\]-->/);
  // A conditional group cannot carry the unconditional bootstrap.
  assert.doesNotMatch(first, /paste\.config/);

  // js stays unloaded — the unconditional jam serves it again, with
  // the bootstrap this time.
  page.jam({ js: ['app'] });
  const second = page.dump('js');
  assert.match(second, /paste\.config/);
  assert.match(second, /app\+v1\.0/);
  assert.doesNotMatch(second, /<!--\[if/);
  assert.equal(page.finish(), '');
}));

test('a conditional css jam still marks loaded', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  page.jam({ css: ['site.theme'], condition: 'lt IE 9' });
  const first = page.dump('css');
  assert.match(first, /<!--\[if lt IE 9\]>/);
  assert.match(first, /<link rel="stylesheet"/);

  // The stylesheet counted as delivered — nothing pending on re-jam.
  page.jam({ css: ['site.theme'] });
  assert.equal(page.dump('css'), '');
  assert.equal(page.finish(), '');
}));

test('a mistyped condition rejects before the event is recorded', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  for (const bad of [42, true, {}, ['x']]) {
    assert.throws(
      () => page.jam({ js: ['app'], condition: bad }),
      (err) => err instanceof PasteError && /condition must be a string/.test(err.message)
    );
  }
  page.jam({ js: ['app'], condition: 'IE 8' });
  assert.match(page.dump('js'), /<!--\[if IE 8\]>/);
  assert.equal(page.finish(), '');
}));

test('a scalar module name rejects instead of spreading into characters', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  // 'app' is one name, not three modules 'a','p','p' — a scalar is not a
  // collection, and a collection's members must be strings. Each is a
  // PasteError thrown synchronously, before the event could be recorded.
  const circular = {};
  circular.self = circular;
  for (const bad of [
    { js: 'app' }, { css: 'app' }, { js: 'site.theme' },
    { js: 42 }, { js: { length: 1 } }, { js: ['app', 7] },
    { js: ['app', null] }, { css: [Symbol('m')] },
    // A circular value must still fail as PasteError — describing it for
    // the message cannot throw a TypeError of its own.
    { js: circular },
  ]) {
    assert.throws(
      () => page.jam(bad),
      (err) => err instanceof PasteError && /jam (css|js)/.test(err.message),
      'expected a jam-list rejection'
    );
  }
  // The options argument itself: an array is not an options object and
  // must reject rather than silently record an empty jam.
  for (const badOptions of [['app'], 'app', 42]) {
    assert.throws(
      () => page.jam(badOptions),
      (err) => err instanceof PasteError && /options object/.test(err.message),
      `expected a jam-options rejection for ${JSON.stringify(badOptions)}`
    );
  }

  // Nothing was recorded: a valid jam still dumps and the page finishes —
  // a malformed event cannot strand an otherwise usable page.
  page.jam({ js: ['app'] });
  assert.match(page.dump('js'), /app\+v1\.0/);
  assert.equal(page.finish(), '');
}));

test('jam option types are checked before the event is recorded', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  // 'false' the string is truthy — coercing it would flip the flag, so the
  // type itself is required. media is a string or absent; perModule a
  // boolean or absent (null means inherit, same as omitted).
  for (const [bad, name] of [
    [{ js: ['app'], inline: 'false' }, 'inline'],
    [{ js: ['app'], inline: 1 }, 'inline'],
    [{ js: ['app'], sync: 'yes' }, 'sync'],
    [{ js: ['app'], media: 42 }, 'media'],
    [{ js: ['app'], media: { type: 'print' } }, 'media'],
    [{ js: ['app'], perModule: 'true' }, 'perModule'],
  ]) {
    assert.throws(
      () => page.jam(bad),
      (err) => err instanceof PasteError && err.message.includes(`jam ${name}`),
      `expected a jam-${name} rejection`
    );
  }

  // Valid spellings — including the null/omitted inherit forms — record
  // and render normally after the rejections: the inline flag inlines the
  // body, so the module lands as its bytes rather than a pinned URL.
  page.jam({ js: ['app'], inline: true, sync: true, perModule: true });
  page.jam({ css: ['site.theme'], media: 'print', perModule: null });
  assert.match(page.dump('js'), /APP/);
  assert.match(page.dump('css'), /site\.theme\+v1\.0/);
  assert.equal(page.finish(), '');
}));

test('a misspelled jam option rejects before the event is recorded', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  // 'per_module' would silently inherit the default delivery and 'sycn'
  // the default ordering — an unknown name is a PasteError naming it,
  // never a recorded event.
  for (const [bad, name] of [
    [{ js: ['app'], per_module: true }, 'per_module'],
    [{ js: ['app'], sycn: true }, 'sycn'],
    [{ js: ['app'], medias: 'print' }, 'medias'],
    [{ js: ['app'], jsOptions: {} }, 'jsOptions'],
  ]) {
    assert.throws(
      () => page.jam(bad),
      (err) => err instanceof PasteError && err.message.includes(`no option "${name}"`),
      `expected a rejection naming ${name}`
    );
  }

  // Nothing was recorded: the perModule spelling lands per-module
  // delivery on the same page and the page still finishes.
  page.jam({ js: ['app'], perModule: true });
  const tags = page.dump('js');
  assert.match(tags, /app\.h\.v1\.min\.js/);
  assert.doesNotMatch(tags, /app\+v1\.0/);
  assert.equal(page.finish(), '');
}));

test('createPaste rejects a non-object settings argument before native load', () => {
  // No manifest/library is supplied at all — the argument-shape failure
  // precedes even locating the native library.
  for (const bad of [null, 'manifest.json', ['/x'], 42]) {
    assert.throws(
      () => createPaste(bad),
      (err) => err instanceof PasteError && err.message.includes('settings object'),
      `expected a settings-object rejection for ${JSON.stringify(bad)}`
    );
  }
});

test('createPaste rejects unknown top-level keys before native load', () => {
  // Every misspelling is rejected by name while the library path is
  // deliberately unresolvable — loadLibrary must never run for these.
  const nowhere = path.join('definitely', 'absent', 'native.so');
  for (const key of ['per_module', 'cdn_base', 'output_js', 'base_path']) {
    assert.throws(
      () => createPaste({
        manifest: '/x/manifest.json', outputBase: '/x/public',
        [key]: 'x', library: nowhere,
      }),
      (err) => err instanceof PasteError && err.message.includes(key),
      `expected a rejection naming ${key}`
    );
  }
});

test('createPaste requires non-empty manifest and outputBase paths', () => {
  const nowhere = path.join('definitely', 'absent', 'native.so');
  for (const [settings, name] of [
    [{ outputBase: '/x/public', library: nowhere }, 'manifest'],
    [{ manifest: '', outputBase: '/x/public', library: nowhere }, 'manifest'],
    [{ manifest: '/x/manifest.json', library: nowhere }, 'outputBase'],
    [{ manifest: '/x/manifest.json', outputBase: '', library: nowhere }, 'outputBase'],
  ]) {
    assert.throws(
      () => createPaste(settings),
      (err) => err instanceof PasteError && err.message.includes(name),
      `expected a ${name} rejection`
    );
  }
});

test('createPaste validates primitive settings before native load', () => {
  // A mistyped flag or threshold used to sail through construction and
  // surface mid-render inside the native deserializer. Every bad value
  // is a PasteError naming its option, and `library: nowhere` proves the
  // rejection precedes even locating the library.
  const nowhere = path.join('definitely', 'absent', 'native.so');
  const base = {
    manifest: '/x/manifest.json', outputBase: '/x/public', library: nowhere,
  };
  for (const [key, bad] of [
    ['perModule', 'false'],
    ['perModule', 1],
    ['perModule', null],
    ['inlineThreshold', -1],
    ['inlineThreshold', 1.5],
    ['inlineThreshold', '1024'],
    ['inlineThreshold', NaN],
    ['inlineThreshold', Infinity],
    ['inlineThreshold', true],
    ['basePath', null],
    ['basePath', 42],
    ['cdnBase', null],
    ['cdnBase', false],
  ]) {
    assert.throws(
      () => createPaste({ ...base, [key]: bad }),
      (err) => err instanceof PasteError && err.message.includes(key),
      `expected a ${key} rejection for ${JSON.stringify(bad)}`
    );
  }
});

test('createPaste rejects empty or mistyped output-directory overrides', () => {
  // Absent means "inherit <outputBase>/{js,css}"; explicitly present
  // means a usable directory path. An env-var expansion producing '' —
  // or a null, number, boolean or NUL-carrying value — must fail at the
  // call, never silently select the default.
  const nowhere = path.join('definitely', 'absent', 'native.so');
  const base = {
    manifest: '/x/manifest.json', outputBase: '/x/public', library: nowhere,
  };
  for (const key of ['outputJs', 'outputCss']) {
    for (const bad of ['', '   ', null, 42, true, {}, '/x\0y']) {
      assert.throws(
        () => createPaste({ ...base, [key]: bad }),
        (err) => err instanceof PasteError && err.message.includes(key),
        `expected a ${key} rejection for ${JSON.stringify(bad)}`
      );
    }
  }
});

test('createPaste rejects non-string, blank and NUL required paths', () => {
  // String(42) or String({}) used to serialize as "42"/"[object Object]"
  // — a different path than the host meant, failing only at first render.
  // Every invalid value is a PasteError naming its key before the library
  // is even located (`library: nowhere` proves the order).
  const nowhere = path.join('definitely', 'absent', 'native.so');
  for (const key of ['manifest', 'outputBase']) {
    for (const bad of [undefined, null, '', '   ', 42, false, true, [], {}, 'bad\0path']) {
      assert.throws(
        () => createPaste({
          manifest: '/x/manifest.json', outputBase: '/x/public',
          [key]: bad, library: nowhere,
        }),
        (err) => err instanceof PasteError && err.message.includes(key),
        `expected a ${key} rejection for ${JSON.stringify(bad)}`
      );
    }
  }
});

test('required paths are forwarded byte-equivalently, spaces and all', () => withTempDir((root) => {
  // A valid path is never trimmed or rewritten: deliberately awkward
  // names — interior spaces and non-ASCII — and a real render proves the
  // native layer received them unaltered.
  const dir = path.join(root, 'fixture dir with spaces');
  const { manifest, out } = e2eFixture(dir);
  const manifestSpaced = path.join(dir, 'manifest spåce.json');
  fs.renameSync(manifest, manifestSpaced);
  const paste = createPaste({
    manifest: manifestSpaced, outputBase: out, inlineThreshold: 0, library: library(),
  });
  const page = paste.page();
  page.jam({ js: ['app'] });
  assert.match(page.dump('js'), /app\+v1\.0/);
  assert.equal(page.finish(), '');
}));

test('outputJs and outputCss select their configured directories', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  // Each override replaces the whole <outputBase>/{js,css} directory.
  // The integrity-checked bytes live only in the custom dirs while a
  // byte trap sits at the defaults — silently resolving a default would
  // read the wrong bytes and fail the recorded hash.
  const customJs = path.join(root, 'custom-js');
  const customCss = path.join(root, 'custom-css');
  write(path.join(customJs, 'paste.h.v1.min.js'), 'PASTE_RT');
  write(path.join(customJs, 'app.h.v1.min.js'), 'APP');
  write(path.join(customCss, 'theme.h.v1.min.css'), 'CSS');
  write(path.join(out, 'js/paste.h.v1.min.js'), 'TRAP');
  write(path.join(out, 'js/app.h.v1.min.js'), 'TRAP');
  write(path.join(out, 'css/theme.h.v1.min.css'), 'TRAP');

  const paste = createPaste({
    manifest, outputBase: out, outputJs: customJs, outputCss: customCss,
    library: library(),
  });
  const page = paste.page();
  page.jam({ js: ['app'], css: ['site.theme'], inline: true });
  assert.match(page.dump('js'), /APP/);
  assert.match(page.dump('css'), /CSS/);
  assert.equal(page.finish(), '');
}));

test('inherited settings are never forwarded to the native boundary', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  // Recognized option names carried on the prototype must not reach the
  // settings envelope: validation iterates own keys only, so an
  // inherited perModule or cdnBase would silently override a default
  // the caller never supplied.
  const options = Object.create({
    perModule: true,
    cdnBase: 'https://cdn.invalid/',
    basePath: '/inherited',
    inlineThreshold: 0,
  });
  Object.assign(options, {
    manifest, outputBase: out, inlineThreshold: 0, library: library(),
  });
  const paste = createPaste(options);

  assert.equal(paste.basePath, '/paste', 'an inherited basePath overrode the default');
  const page = paste.page();
  page.jam({ js: ['app'] });
  const tags = page.dump('js');
  // Defaults held: combo tokens, not the per-module file URLs an
  // inherited perModule would have emitted, and no inherited CDN origin.
  assert.match(tags, /app\+v1\.0/);
  assert.doesNotMatch(tags, /app\.h\.v1\.min\.js/);
  assert.doesNotMatch(tags, /cdn\.invalid/);
  assert.equal(page.finish(), '');
}));

test('an inherited library path is never loaded', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  // No own `library`: resolution falls to PASTE_ASSETGRAPH_LIB or the
  // bundled artifact — never to the prototype's planted path, which
  // would fail or load a different binary entirely.
  const options = Object.create({ library: '/inherited/absent.so' });
  Object.assign(options, { manifest, outputBase: out, inlineThreshold: 0 });
  const paste = createPaste(options);
  const page = paste.page();
  page.jam({ js: ['app'] });
  assert.match(page.dump('js'), /app\+v1\.0/);
  assert.equal(page.finish(), '');
}));

test('a null-prototype settings dictionary is accepted', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const options = Object.assign(Object.create(null), {
    manifest, outputBase: out, inlineThreshold: 0, library: library(),
  });
  const paste = createPaste(options);
  const page = paste.page();
  page.jam({ js: ['app'] });
  assert.match(page.dump('js'), /app\+v1\.0/);
  assert.equal(page.finish(), '');
}));

test('a settings getter is read exactly once', () => {
  // Validation reads keys; the snapshot reads each value a single time.
  // A getter invoked twice — once to validate, once to destructure —
  // would be caller-visible side effects the API must not have.
  let reads = 0;
  const options = { outputBase: '/x/public', library: library() };
  Object.defineProperty(options, 'manifest', {
    enumerable: true,
    get: () => {
      reads += 1;
      return '/x/manifest.json';
    },
  });
  createPaste(options);
  assert.equal(reads, 1);
});

test('inherited jam options never reach the recorded event', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  // An empty own-options jam whose prototype carries recognized keys:
  // the inherited js list and flags must not record or apply.
  const options = Object.create({ js: ['app'], perModule: true, inline: true });
  assert.equal(page.jam(options), '');
  const tags = page.dump('js');
  // Neither the module token ('app+v1.0') nor the inlined bytes ('APP')
  // may appear — the inherited list and flags never recorded.
  assert.doesNotMatch(tags, /app/i);
  // An inherited misspelling still rejects by name — the own-key rule
  // applies to what the caller set, never what the prototype happens
  // to carry.
  const misspelled = Object.assign(Object.create({ sycn: true }), { js: ['app'] });
  page.jam(misspelled); // own keys are all recognized — records normally
  assert.match(page.dump('js'), /app\+v1\.0/);
  assert.equal(page.finish(), '');
}));

test('jam records exactly the declared names, caller copies included', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  // Order and repetition are the caller's contract — preserved verbatim
  // until the library resolves the dependency order.
  const js = ['app', 'app'];
  page.jam({ js, css: ['site.theme'] });
  js.push('ghost'); // an aliased array would make the dump name an unknown module
  page.dump();
  assert.equal(page.finish(), '');
}));

test('a manifest rewrite mid-page retires the page permanently', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  page.jam({ css: ['site.theme'], js: ['app'] });
  page.dump('css');

  // A build lands between the page's dumps — the manifest's bytes move
  // and so does its generation token (the stamp needs the mtime bump:
  // the rewrite is the same length).
  fs.writeFileSync(manifest, MANIFEST.replace('"version":"42"', '"version":"43"'));
  const future = new Date(Date.now() + 60000);
  fs.utimesSync(manifest, future, future);

  assert.throws(
    () => page.dump('js'),
    (err) => err instanceof PasteError
      && err.kind === 'manifest-generation'
      && /generation/.test(err.message)
  );
  // The failure is terminal: every later step answers the recorded
  // error rather than rendering against a manifest the page's earlier
  // dumps were not planned with.
  for (const step of [
    () => page.jam({ js: ['app'] }),
    () => page.dump(),
    () => page.finish(),
  ]) {
    assert.throws(step, (err) => err.kind === 'manifest-generation');
  }
}));

test('a manifest rewrite retires the page across async steps too', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  page.jam({ css: ['site.theme'], js: ['app'] });
  await page.dumpAsync('css');

  fs.writeFileSync(manifest, MANIFEST.replace('"version":"42"', '"version":"43"'));
  const future = new Date(Date.now() + 60000);
  fs.utimesSync(manifest, future, future);

  await assert.rejects(
    () => page.dumpAsync('js'),
    (err) => err.kind === 'manifest-generation'
  );
  await assert.rejects(
    () => page.finishAsync(),
    (err) => err.kind === 'manifest-generation'
  );
  assert.throws(
    () => page.jam({ js: ['app'] }),
    (err) => err.kind === 'manifest-generation'
  );
}));

test('a dump queued before a generation mismatch never reaches native', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  page.jam({ css: ['site.theme'], js: ['app'] });
  await page.dumpAsync('css');

  // A build lands between the page's steps.
  fs.writeFileSync(manifest, MANIFEST.replace('"version":"42"', '"version":"43"'));
  const future = new Date(Date.now() + 60000);
  fs.utimesSync(manifest, future, future);

  // Dump A fails against the changed generation; dump B and the finish
  // are queued behind it before the mismatch lands. A retired page must
  // surface the SAME stored error — if B or the finish replayed a stale
  // event list into the native layer, each would come back as a fresh
  // error object instead.
  const a = page.dumpAsync('js');
  const b = page.dumpAsync();
  const fin = page.finishAsync();

  let errA;
  await assert.rejects(a, (err) => {
    errA = err;
    return err.kind === 'manifest-generation';
  });
  await assert.rejects(b, (err) => {
    assert.equal(
      err,
      errA,
      'a job queued before retirement rethrows the stored error — no second native call'
    );
    return true;
  });
  await assert.rejects(fin, (err) => {
    assert.equal(
      err,
      errA,
      'finish waits out the queue, then answers the stored error without calling native'
    );
    return true;
  });
}));

test('a no-argument async dump renders every type', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  page.jam({ css: ['site.theme'], js: ['app'] });

  const html = await page.dumpAsync();
  assert.match(html, /stylesheet/);
  assert.match(html, /site\.theme\+v1\.0/);
  assert.match(html, /app\+v1\.0/);
  assert.equal(await page.finishAsync(), '');
}));

test('combo rejects unknown or non-object options', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, library: library() });
  await assert.rejects(paste.combo('/paste/app.js', { ifModifedSince: 'x' }), PasteError);
  await assert.rejects(paste.combo('/paste/app.js', 'Thu, 01 Jan 2026 00:00:00 GMT'), PasteError);
  assert.equal((await paste.combo('/paste/app.js', {})).status, 200);
}));

test('a bare PASTE_ASSETGRAPH_LIB name resolves against the working directory', () => {
  const resolved = resolveLibrary({ PASTE_ASSETGRAPH_LIB: path.relative(process.cwd(), library()) });
  assert.ok(path.isAbsolute(resolved), resolved);
});

test('a burst beyond koffi\'s async cap queues instead of failing', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, library: library() });
  const results = await Promise.allSettled(
    Array.from({ length: 600 }, () => paste.combo('/paste/1767225600/app+v1.0.min.js')),
  );
  const rejected = results.filter((r) => r.status === 'rejected');
  assert.equal(rejected.length, 0, rejected[0]?.reason?.message);
  assert.ok(results.every((r) => r.value.status === 200 && r.value.body === 'APP'));
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

// The conditional form is the same library decision with the request's
// validator forwarded — the fixture group stamps 2026-01-01T00:00:00Z.
test('combo with a validator answers 304 or 200 by the recorded stamp', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, library: library() });
  const url = '/paste/1767225600/app+v1.0.min.js';

  // At or after the group stamp → 304, empty body, stamped header.
  for (const ims of ['Thu, 01 Jan 2026 00:00:00 GMT', 'Thu, 01 Jan 2026 00:00:01 GMT']) {
    const r = await paste.combo(url, { ifModifiedSince: ims });
    assert.equal(r.status, 304, ims);
    assert.equal(r.body, '');
    assert.equal(r.lastModified, 'Thu, 01 Jan 2026 00:00:00 GMT');
    assert.equal(r.cacheControl, 'public, max-age=31536000');
    assert.ok(Object.isFrozen(r));
  }

  // Before it, absent, or unparsable → the ordinary 200.
  for (const ims of ['Wed, 01 Jan 2025 00:00:00 GMT', 'not a date', undefined]) {
    const r = await paste.combo(url, ims === undefined ? {} : { ifModifiedSince: ims });
    assert.equal(r.status, 200, ims);
    assert.equal(r.body, 'APP');
    assert.equal(r.lastModified, 'Thu, 01 Jan 2026 00:00:00 GMT');
  }

  // A non-canonical URL still redirects first — the validator never
  // suppresses canonicalization.
  const r = await paste.combo('/paste/1767225599/app+v1.0.min.js', { ifModifiedSince: 'Thu, 01 Jan 2026 00:00:01 GMT' });
  assert.equal(r.status, 302);
  assert.equal(r.location, url);

  // A NUL in the validator is rejected at the adapter, not by the library.
  await assert.rejects(
    paste.combo(url, { ifModifiedSince: 'Thu\0' }),
    (err) => err instanceof PasteError && err.message === 'combo ifModifiedSince must not contain NUL',
  );
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
  assert.match(page.dump('js'), new RegExp(`app\\+v1\\.0-h${appHash}`));

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
  const tags = page2.dump('js');
  assert.match(tags, /app\+v1\.0\.min\.js/);
  assert.doesNotMatch(tags, /-h[0-9a-f]{32}/);
}));

test('perModule emits per-module tags and a jam overrides it', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, perModule: true, library: library() });

  // Global on: the dump names each module's versioned file — one
  // ordered loader block per async group, no combo token.
  const page = paste.page();
  page.jam({ js: ['app'] });
  const perModuleTags = page.dump('js');
  assert.match(perModuleTags, /\/js\/app\.h\.v1\.min\.js/);
  assert.match(perModuleTags, /paste\.require/);
  assert.doesNotMatch(perModuleTags, /app\+v1\.0/);
  assert.equal(page.finish(), '');

  // perModule: false on a jam renders the combo group the setting would
  // suppress — the override goes both ways.
  const combo = paste.page();
  combo.jam({ js: ['app'], perModule: false });
  const comboTags = combo.dump('js');
  assert.match(comboTags, /app\+v1\.0/);
  assert.doesNotMatch(comboTags, /\/js\/app\.h\.v1\.min\.js/);
  assert.equal(combo.finish(), '');

  // And perModule: true flips a combo-setting page.
  const comboSetting = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const flipped = comboSetting.page();
  flipped.jam({ js: ['app'], perModule: true });
  assert.match(flipped.dump('js'), /\/js\/app\.h\.v1\.min\.js/);
  assert.equal(flipped.finish(), '');
}));

test('finish seals the page; a jam its type never dumped fails', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  // The css dump already ran when the jam lands: site.theme is queued for
  // a dump that never comes, and finishing on the js dump must refuse —
  // not drop the stylesheet silently.
  page.dump('css');
  page.jam({ css: ['site.theme'], js: ['app'] });
  page.dump('js');
  assert.throws(() => page.finish(), (err) => err instanceof PasteError && /site\.theme/.test(err.message));

  // A failed finish leaves the page open: deliver the missing dump, then
  // finish — and finish again is a no-op.
  page.dump('css');
  assert.equal(page.finish(), '');
  assert.equal(page.finish(), '');

  // A sealed page accepts no more events.
  assert.throws(() => page.jam({ js: ['app'] }), (err) => err instanceof PasteError && /finished/.test(err.message));
  assert.throws(() => page.dump('js'), (err) => err instanceof PasteError && /finished/.test(err.message));
}));

test('finishAsync waits for pending dumps and joins concurrent callers', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  page.jam({ js: ['app'] });
  const dump = page.dumpAsync('js');
  // Neither a sync finish nor a sync dump may overtake the async dump in
  // flight — the dump would render a list whose marker is not yet delivered.
  assert.throws(() => page.finish(), (err) => err instanceof PasteError && /finishAsync/.test(err.message));
  assert.throws(() => page.dump('js'), (err) => err instanceof PasteError && /in flight/.test(err.message));
  // Concurrent finishAsync calls share the one in-flight validation, and
  // the seal already blocks new events before the worker answers.
  const first = page.finishAsync();
  const second = page.finishAsync();
  assert.strictEqual(first, second);
  assert.throws(() => page.jam({ js: ['app'] }), (err) => err instanceof PasteError && /finished/.test(err.message));
  assert.throws(() => page.dumpAsync('js'), (err) => err instanceof PasteError && /finished/.test(err.message));
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
  const dump = page.dumpAsync('js');
  const fin = page.finishAsync();
  await assert.rejects(fin);
  await assert.rejects(dump);

  // A failed finish is no false seal: the page accepts events again.
  page.jam({ js: ['app'] });

  // Restore the manifest — the page still owes its undumped modules.
  write(manifestPath, MANIFEST);
  await assert.rejects(page.finishAsync(), /site\.theme|app/);
  page.dump('css');
  page.dump('js');
  assert.equal(page.finish(), '');
}));

test('finishAsync waits out every pending dump before it answers', async () => withTempDir(async (root) => {
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
  const first = page.dumpAsync('js');
  // Restoring the manifest on the first failure means the queued second
  // dump succeeds — but its native call is still running when the first
  // failure lands, so an answering finish would unseal a page that call
  // still writes into.
  const restored = first.catch((err) => {
    write(manifestPath, MANIFEST);
    return err;
  });
  const second = page.dumpAsync('css');
  let secondSettled = false;
  second.then(
    () => { secondSettled = true; },
    () => { secondSettled = true; }
  );
  const fin = page.finishAsync();

  await assert.rejects(fin);
  assert.ok(
    secondSettled,
    'finish must not answer while a queued dump still has native work in flight'
  );
  assert.match(await second, /stylesheet/);
  assert.ok((await restored) instanceof PasteError);
}));

test('concurrent same-type async dumps render in record order', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();

  page.jam({ js: ['app'] });
  const first = page.dumpAsync('js');
  const second = page.dumpAsync('js');
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
  const first = page.dumpAsync('js');
  // Restoring the manifest is registered directly on the first failure,
  // before the second dump starts — its queued native call must see it.
  const restored = first.catch((err) => {
    write(manifestPath, MANIFEST);
    return err;
  });
  const second = page.dumpAsync('js');
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
    () => page.dump('js'),
    (err) => err instanceof PasteError && /ghost/.test(err.message)
  );
}));

test('async dumps render the same markup as the sync dumps', async () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, inlineThreshold: 0, library: library() });
  const page = paste.page();
  page.jam({ css: ['site.theme'], js: ['app'] });

  const cssTag = await page.dumpAsync('css');
  const jsTags = await page.dumpAsync('js');
  assert.match(cssTag, /stylesheet/);
  assert.match(cssTag, /site\.theme\+v1\.0/);
  assert.match(jsTags, /^<script type="text\/javascript">PASTE_RT/);
  assert.match(jsTags, /app\+v1\.0/);
  // The js dump emits no css module, and vice versa.
  assert.doesNotMatch(jsTags, /site\.theme/);

  const syncPage = paste.page();
  syncPage.jam({ css: ['site.theme'], js: ['app'] });
  assert.equal(cssTag, syncPage.dump('css'));
  assert.equal(jsTags, syncPage.dump('js'));
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

  const jsTags = page.dump('js');
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

test('combo rejects a non-string path before the native call', () => withTempDir(async (root) => {
  const { manifest, out } = e2eFixture(root);
  const paste = createPaste({ manifest, outputBase: out, library: library() });

  // A null, number, array or object used to die as a TypeError inside
  // .includes — the contract is a PasteError, and nothing is coerced.
  for (const bad of [null, undefined, 42, ['/x'], { path: '/x' }, true]) {
    await assert.rejects(
      paste.combo(bad),
      (err) => err instanceof PasteError && err.message.includes('combo'),
      `expected a combo rejection for ${JSON.stringify(bad)}`
    );
  }
}));

test('repeated createPaste loads do not exhaust koffi type registrations', () => withTempDir((root) => {
  const { manifest, out } = e2eFixture(root);
  for (let i = 0; i < 100; i++) {
    const paste = createPaste({ manifest, outputBase: out, library: library() });
    assert.equal(paste.basePath, '/paste');
  }
}));
