/*!
 * Copyright (c) 2017–present Tomshley LLC
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 * 
 *     https://www.apache.org/licenses/LICENSE-2.0
 * 
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import url from 'node:url';
import vm from 'node:vm';
import { createHash } from 'node:crypto';
import { createPaste, PasteError } from '../index.js';

// The determinism fixture's module sources, executed under the real paste
// runtime they ship with. Byte-identical output on two platforms cannot
// show that the modules run — this executes the sources in order (runtime
// first, then dependency, then dependent) and calls the export the page
// would call. Source execution is the whole scope here: the built-output
// loader behavior a page exercises is covered separately.
const features = path.join(
  path.dirname(url.fileURLToPath(import.meta.url)),
  '..',
  '..',
  '..',
  'features'
);

test('determinism fixture modules execute under the real runtime', () => {
  // The runtime is a browser script: it binds to window and reads
  // document.head while seeding. A truthy head short-circuits the
  // getElementsByTagName fallback, which is all the loader needs here —
  // define never touches the DOM, and the dependency is already defined
  // before the dependent module runs, so require() is never invoked.
  const context = vm.createContext({});
  context.window = context;
  context.document = { head: {} };

  for (const name of ['paste.js', 'helper.js', 'determinism.js']) {
    const source = fs.readFileSync(path.join(features, name), 'utf8');
    vm.runInContext(source, context, { filename: name });
  }

  assert.strictEqual(typeof context.helper, 'function');
  assert.strictEqual(context.helper(), 'helper');
  assert.strictEqual(typeof context.determinism, 'function');
  assert.strictEqual(
    context.determinism(),
    true,
    'the dependent factory receives the dependency export, not the module namespace'
  );
});

// Below here: the shared CLI-built fixture — one built distribution every
// runner verifies against its frozen reference.json. Response bodies and
// page values compare by SHA-256; the reference is adapter consistency,
// not legacy-parity proof.
const sha256 = value => createHash('sha256').update(value).digest('hex');

// Resolve a fixture metadata path beneath the selected root: reference.json
// members must be relative and must stay inside the root after symlinks
// resolve — the operator-chosen root itself is the intended input.
function fixturePath(root, relative) {
  assert.equal(typeof relative, 'string');
  assert.ok(!path.isAbsolute(relative), 'fixture paths must be relative');
  const file = fs.realpathSync(path.join(root, relative));
  const rest = path.relative(root, file);
  assert.ok(
    rest !== '..' && !rest.startsWith('..' + path.sep) && !path.isAbsolute(rest),
    'fixture path escapes its root'
  );
  return file;
}

function sharedFixture() {
  assert.ok(
    process.env.PASTE_ASSETGRAPH_TEST_FIXTURE,
    'Build tests/build_shared_fixture.py once and set PASTE_ASSETGRAPH_TEST_FIXTURE'
  );
  const root = fs.realpathSync(process.env.PASTE_ASSETGRAPH_TEST_FIXTURE);
  const reference = JSON.parse(
    fs.readFileSync(fixturePath(root, 'reference.json'), 'utf8')
  );
  assert.equal(reference.schema, 1);
  return { root, cases: reference.cases };
}

function treeDigests(root) {
  const files = {};
  function visit(dir) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const file = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        visit(file);
      } else {
        files[path.relative(root, file).split(path.sep).join('/')] = sha256(
          fs.readFileSync(file)
        );
      }
    }
  }
  visit(root);
  return files;
}

test('shared CLI-built distribution agrees through Node sync and async pages', async () => {
  const fixture = sharedFixture();
  assert.throws(() => fixturePath(fixture.root, '..'));
  for (const item of fixture.cases) {
    const outputBase = fixturePath(fixture.root, item.outputBase);
    assert.deepEqual(treeDigests(outputBase), item.files);
    const paste = createPaste({
      manifest: fixturePath(fixture.root, item.manifest),
      outputBase,
      inlineThreshold: 0,
    });
    for (const request of item.responses) {
      const { body, ...actual } = await paste.combo(request.path);
      assert.deepEqual(
        { ...actual, bodySha256: sha256(body) },
        request.expected,
        `${item.name}: ${request.name}`
      );
    }
    for (const asyncMode of [false, true]) {
      for (const scenario of item.pages) {
        const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'paste-shared-'));
        try {
          const manifest = path.join(temp, 'manifest.json');
          const manifestSource = fixturePath(fixture.root, item.manifest);
          // The scenario's recorded manifest default, applied only when
          // it names one — otherwise the fixture bytes are copied as-is.
          if (scenario.manifestCdnBase !== null &&
              scenario.manifestCdnBase !== undefined) {
            const manifestJson = JSON.parse(fs.readFileSync(manifestSource, 'utf8'));
            manifestJson.cdn_base = scenario.manifestCdnBase;
            fs.writeFileSync(manifest, JSON.stringify(manifestJson));
          } else {
            fs.copyFileSync(manifestSource, manifest);
          }
          // Only the known render settings are mapped through — no
          // arbitrary scenario keys reach the wrapper.
          const settings = scenario.settings || {};
          const page = createPaste({
            manifest,
            outputBase,
            inlineThreshold: settings.inlineThreshold ?? 0,
            cdnBase: settings.cdnBase ?? '',
            perModule: settings.perModule ?? false,
          }).page();
          for (const step of scenario.steps) {
            if (step.hideManifest) {
              fs.renameSync(manifest, manifest + '.held');
              continue;
            }
            if (step.restoreManifest) {
              fs.renameSync(manifest + '.held', manifest);
              continue;
            }
            const operation = async () => {
              if (step.jam) {
                return page.jam(step.jam);
              }
              if (step.finish) {
                return asyncMode ? page.finishAsync() : page.finish();
              }
              const type = step.dump === 'all' ? undefined : step.dump;
              return asyncMode ? page.dumpAsync(type) : page.dump(type);
            };
            if (step.error || step.errorContains) {
              await assert.rejects(
                operation,
                error =>
                  error instanceof PasteError &&
                  (!step.errorContains || error.message.includes(step.errorContains))
              );
            } else {
              assert.equal(
                sha256(await operation()),
                step.valueSha256,
                `${item.name}: ${scenario.name}`
              );
            }
          }
        } finally {
          fs.rmSync(temp, { recursive: true, force: true });
        }
      }
    }
    assert.deepEqual(treeDigests(outputBase), item.files);
  }
});

// A minimal DOM the vendored runtime can drive in a VM: loader-created
// script nodes land on `queue` for the test to deliver.
function domContext() {
  const queue = [];
  const context = vm.createContext({ setTimeout, clearTimeout });
  context.window = context;
  context.document = {
    head: {
      appendChild(node) {
        queue.push(...(node.children || [node]));
        return node;
      },
      removeChild() {},
    },
    createDocumentFragment() {
      return {
        children: [],
        appendChild(node) {
          this.children.push(node);
          return node;
        },
      };
    },
    createElement() {
      const listeners = new Map();
      return {
        addEventListener(type, listener) {
          listeners.set(type, listener);
        },
        removeEventListener(type) {
          listeners.delete(type);
        },
        loaded() {
          const fn = listeners.get('load');
          if (fn) {
            fn({ target: this });
          }
        },
      };
    },
  };
  return { context, queue };
}

// The same fixture's compiled modules through the vendored runtime's real
// loader: the page markup is rewritten to a still-recorded historical
// packages segment, the mock document hands every loader request to the
// library's combo, and paste.require fetches a module the page did not
// name upfront. This exercises require/DOM-loader scheduling in a VM —
// it is not a real browser or HTTP deployment.
test('compiled runtime loads a saved page feature through its historical URL', async () => {
  const fixture = sharedFixture();
  for (const item of fixture.cases) {
    const outputBase = fixturePath(fixture.root, item.outputBase);
    const paste = createPaste({
      manifest: fixturePath(fixture.root, item.manifest),
      outputBase,
      inlineThreshold: 0,
    });
    const page = paste.page();
    page.jam({ js: ['helper'] });
    const markup = page
      .dump('js')
      .replace(item.browser.currentPackages, item.browser.oldPackages);
    assert.equal(sha256(markup), item.browser.oldMarkupSha256);
    const { context, queue } = domContext();
    const requested = [];
    async function drain() {
      let consumed = 0;
      while (queue.length) {
        assert.ok(++consumed <= 20, 'unexpected loader request loop');
        const node = queue.shift();
        requested.push(node.src);
        let response = await paste.combo(node.src);
        for (let redirects = 0; response.status === 302 && redirects < 5; redirects++) {
          response = await paste.combo(response.location);
        }
        assert.equal(response.status, 200, node.src);
        vm.runInContext(response.body, context, { filename: node.src });
        node.loaded();
      }
    }
    for (const script of markup.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g)) {
      vm.runInContext(script[1], context);
    }
    await drain();
    assert.equal(context.helper(), 'helper');
    assert.equal(context.later, undefined);
    assert.equal(context.unused, undefined);
    vm.runInContext(
      "paste.require('later', function (module, later) { window.dynamicResult = later(); });",
      context
    );
    await drain();
    assert.equal(context.dynamicResult, item.browser.dynamicValue);
    assert.ok(requested.includes(item.browser.oldPackages + 'later.js'));
    assert.ok(!requested.some(url => url.includes('unused')));
    assert.deepEqual(treeDigests(outputBase), item.files);
  }
});

// Per-module output executes through the same runtime but names real
// files: one loader block marks every pending member loading, then
// requests the versioned files in dependency order through
// paste.require(url, callback) — each file's load fires the next
// request, so at most one is pending at a time and draining the queue
// delivers them in emitted order. The isLoading marks are what stop a
// dependent's define from triggering a bare-name packagesUrl fetch.
// VM execution against files read from the fixture — not a real browser
// or HTTP deployment.
test('per-module tags execute against the versioned files they name', async () => {
  const fixture = sharedFixture();
  for (const item of fixture.cases) {
    const outputBase = fixturePath(fixture.root, item.outputBase);
    const manifest = JSON.parse(
      fs.readFileSync(fixturePath(fixture.root, item.manifest), 'utf8')
    );
    const outputs = new Set();
    for (const entry of Object.values(manifest.assets)) {
      outputs.add(entry.output);
      for (const version of entry.history || []) {
        outputs.add(version.output);
      }
    }
    const paste = createPaste({
      manifest: fixturePath(fixture.root, item.manifest),
      outputBase,
      inlineThreshold: 0,
      perModule: true,
    });
    const page = paste.page();
    page.jam({ js: ['app'] });
    const markup = page.dump('js');
    const { context, queue } = domContext();
    const requested = [];
    function executeFile(source) {
      requested.push(source);
      const file = source.split('/').map(decodeURIComponent).join('/');
      assert.ok(outputs.has(file), `${source} is not a manifest output`);
      assert.ok(file.startsWith('/'));
      vm.runInContext(
        fs.readFileSync(fixturePath(outputBase, file.slice(1)), 'utf8'),
        context,
        { filename: source }
      );
    }
    for (const tag of markup.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)) {
      const src = /src="([^"]*)"/.exec(tag[1]);
      if (src) {
        executeFile(src[1]);
      } else {
        vm.runInContext(tag[2], context);
      }
    }
    let consumed = 0;
    while (queue.length) {
      assert.ok(++consumed <= 20, 'unexpected loader request loop');
      const node = queue.pop();
      executeFile(node.src);
      if (node.loaded) {
        node.loaded();
      }
    }
    assert.equal(context.app(), 'v2:helper');
    // The fixtures inline the runtime, so the exact request set is the
    // two module files — a duplicate or extra request fails this.
    const expected = ['helper', 'app'].map(name => manifest.assets[manifest.modules[name]].output);
    assert.deepEqual([...requested].sort(), expected.sort());
    assert.ok(!requested.some(url => url.includes('/paste/')), `${requested}`);
    assert.ok(!requested.some(url => url.includes('unused')));
    assert.deepEqual(treeDigests(outputBase), item.files);
  }
});

// A CLI-built project of raw-global modules — no define() calls, just
// top-level `var`/`window` writes chained by @requires — proves the
// ordered loader really evaluates files in dependency order at global
// scope: `var answer` inside app.js must land on the global object for
// final.js to read it, which any callback-wrapping delivery would break.
// helper is padded past the inline threshold so threshold 1024 splits
// the group after it — the dependency still must not be overtaken.
// Built once into a temp dir; not a browser or HTTP deployment.
test('per-module ordered delivery evaluates raw globals on a CLI-built page', async () => {
  const cli = process.env.PASTE_ASSETGRAPH_TEST_BIN;
  assert.ok(cli, 'PASTE_ASSETGRAPH_TEST_BIN names the built CLI');
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'paste-ordered-'));
  try {
    fs.mkdirSync(path.join(temp, 'src'));
    fs.copyFileSync(
      path.join(features, 'paste.js'),
      path.join(temp, 'src', 'paste.js')
    );
    fs.writeFileSync(
      path.join(temp, 'src', 'helper.js'),
      '// @module helper\nwindow.dependencyValue = 41;\n' +
        '/*' + 'x'.repeat(1500) + '*/\n'
    );
    fs.writeFileSync(
      path.join(temp, 'src', 'app.js'),
      '// @module app\n// @requires helper\nvar answer = window.dependencyValue + 1;\n'
    );
    fs.writeFileSync(
      path.join(temp, 'src', 'final.js'),
      '// @module final\n// @requires app\nwindow.finalAnswer = answer + 1;\n'
    );
    fs.writeFileSync(
      path.join(temp, 'config.json'),
      JSON.stringify({
        source: { dirs: ['src'] },
        output: {
          base: 'dist',
          js: 'dist/js',
          css: 'dist/css',
          images: 'dist/images',
        },
        manifest_path: 'dist/manifest.json',
        minify: false,
        source_maps: false,
        storage: { backend: 'dist', keep_versions: 'all' },
      })
    );
    const build = spawnSync(
      cli,
      ['build', '--cwd', temp, '--config', 'config.json'],
      { encoding: 'utf8' }
    );
    assert.equal(build.status, 0, build.stderr);

    const outputBase = path.join(temp, 'dist');
    const manifestPath = path.join(outputBase, 'manifest.json');
    const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
    const output = name => manifest.assets[manifest.modules[name]].output;
    const outputs = new Set(
      Object.values(manifest.assets).map(entry => entry.output)
    );

    // A requested file URL resolves only to a manifest-recorded output
    // confined under outputBase.
    function readOutput(source) {
      const file = source
        .split('?')[0]
        .split('/')
        .map(decodeURIComponent)
        .join('/');
      assert.ok(outputs.has(file), `${source} is not a manifest output`);
      const resolved = path.join(outputBase, file.slice(1));
      const rest = path.relative(outputBase, resolved);
      assert.ok(
        rest !== '..' && !rest.startsWith('..' + path.sep) && !path.isAbsolute(rest),
        `${source} escapes outputBase`
      );
      return fs.readFileSync(resolved, 'utf8');
    }

    // Play one page of markup against a fresh DOM: inline code and
    // synchronous src tags execute in order; each queued async request
    // delivers through fetch() and its load event issues the next.
    async function play(markup, fetch) {
      const { context, queue } = domContext();
      const requested = [];
      async function deliver(source) {
        requested.push(source);
        vm.runInContext(await fetch(source), context, { filename: source });
      }
      for (const tag of markup.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)) {
        const src = /src="([^"]*)"/.exec(tag[1]);
        if (src) {
          await deliver(src[1]);
        } else {
          vm.runInContext(tag[2], context);
        }
      }
      let consumed = 0;
      while (queue.length) {
        assert.ok(++consumed <= 20, 'unexpected loader request loop');
        const node = queue.pop();
        await deliver(node.src);
        if (node.loaded) {
          node.loaded();
        }
      }
      return { context, requested };
    }

    for (const inlineThreshold of [0, 1024]) {
      for (const [label, perModule, sync] of [
        ['combo', false, false],
        ['per-module async', true, false],
        ['per-module sync', true, true],
      ]) {
        for (const asyncDump of [false, true]) {
          const paste = createPaste({
            manifest: manifestPath,
            outputBase,
            inlineThreshold,
            perModule,
          });
          const page = paste.page();
          page.jam({ js: ['final'], sync });
          const markup = asyncDump
            ? await page.dumpAsync('js')
            : page.dump('js');
          const mode = `${label} t=${inlineThreshold} ${asyncDump ? 'dumpAsync' : 'dump'}`;
          const { context, requested } = await play(markup, async source => {
            if (source.startsWith('/paste/')) {
              let response = await paste.combo(source);
              for (let redirects = 0; response.status === 302 && redirects < 5; redirects++) {
                response = await paste.combo(response.location);
              }
              assert.equal(response.status, 200, source);
              return response.body;
            }
            return readOutput(source);
          });

          assert.equal(context.window.answer, 42, mode);
          assert.equal(context.window.finalAnswer, 43, mode);

          // Unique requests; the expected set is the planned external
          // files — or the single combo URL — and nothing bare-named.
          assert.equal(
            new Set(requested).size,
            requested.length,
            `${mode}: ${requested}`
          );
          if (perModule) {
            const expected =
              sync && inlineThreshold > 0
                ? [output('helper')]
                : ['helper', 'app', 'final'].map(output);
            assert.deepEqual(
              [...requested].sort(),
              expected.sort(),
              `${mode}: ${requested}`
            );
            assert.ok(
              !requested.some(u => u.includes('/paste/')),
              `${mode}: ${requested}`
            );
          } else {
            assert.equal(requested.length, 1, `${mode}: ${requested}`);
            assert.ok(requested[0].startsWith('/paste/'), `${mode}: ${requested}`);
          }
          // No loader scheduling fallback fetched a module by bare name —
          // a static host cannot answer those URLs.
          assert.ok(
            !requested.some(u => /\/(helper|app|final)\.js$/.test(u)),
            `${mode}: ${requested}`
          );
        }
      }
    }
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
});
