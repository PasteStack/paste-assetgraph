// Thin loader for the paste-assetgraph native library: all tag, URL and
// combo logic lives in the library; this package only calls it through its
// C ABI.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import koffi from 'koffi';

export class PasteError extends Error {
  constructor(message) {
    super(message);
    this.name = 'PasteError';
  }
}

const HERE = path.dirname(fileURLToPath(import.meta.url));

function platformDir() {
  const os = { darwin: 'darwin', linux: 'linux', win32: 'windows' }[process.platform]
    ?? 'unknown';
  const arch = { arm64: 'aarch64', x64: 'amd64' }[process.arch] ?? 'unknown';
  // Linux bundles are per-libc: musl (Alpine) cannot load a glibc build. The
  // runtime report names glibc only when it actually provides libc.
  const libc = os === 'linux'
      && !process.report.getReport().header.glibcVersionRuntime
    ? '-musl'
    : '';
  return `${os}-${arch}${libc}`;
}

function libraryFileName() {
  switch (process.platform) {
    case 'win32': return 'paste_assetgraph_ffi.dll';
    case 'darwin': return 'libpaste_assetgraph_ffi.dylib';
    default: return 'libpaste_assetgraph_ffi.so';
  }
}

function isFile(file) {
  try {
    return fs.statSync(file).isFile();
  } catch {
    return false;
  }
}

/**
 * Locate the paste-assetgraph native library: `PASTE_ASSETGRAPH_LIB`, then
 * the bundled `native-lib/<platform>/<file>` under `dir` (this package's
 * directory by default). An override that is set but not a regular file
 * throws a PasteError naming the value — an explicit override never falls
 * through silently; a missing library throws naming both places searched
 * and the platform.
 */
export function resolveLibrary(env = process.env, dir = HERE) {
  const override = env.PASTE_ASSETGRAPH_LIB;
  if (override !== undefined) {
    if (isFile(override)) return override;
    throw new PasteError(
      `PASTE_ASSETGRAPH_LIB is set to ${override} but that is not a file`
    );
  }
  const platform = platformDir();
  const bundled = path.join(dir, 'native-lib', platform, libraryFileName());
  if (isFile(bundled)) return bundled;
  throw new PasteError(
    `paste-assetgraph native library not found for platform ${platform}. ` +
    `Searched: the PASTE_ASSETGRAPH_LIB environment variable and ${bundled}`
  );
}

// The C ABI as koffi sees it: UTF-8 JSON in, a heap C string of envelope
// JSON out. The disposable type reads the string and hands the pointer to
// paste_free, so every result is released exactly once. It is anonymous —
// koffi type names are process-global, so a named type would register once
// per load forever.
function loadLibrary(libraryPath) {
  const lib = koffi.load(libraryPath);
  const free = lib.func('void paste_free(void *ptr)');
  const heapStr = koffi.disposable('str', free);
  return {
    render: lib.func('paste_render', heapStr, ['str', 'str']),
    combo: lib.func('paste_combo', heapStr, ['str', 'str']),
  };
}

// The envelope's `ok` payload, or a PasteError carrying `error`.
function unwrap(envelope) {
  if (envelope == null) throw new PasteError('paste-assetgraph returned no response');
  const parsed = JSON.parse(envelope);
  if (parsed.error !== undefined) throw new PasteError(parsed.error);
  return parsed.ok;
}

function callAsync(fn, ...args) {
  return new Promise((resolve, reject) => {
    fn.async(...args, (err, envelope) => {
      if (err) return reject(err);
      try {
        resolve(unwrap(envelope));
      } catch (parseError) {
        reject(parseError);
      }
    });
  });
}

/**
 * A wrapper around the library for one manifest/output pair.
 *
 * Returns a frozen `{ basePath, page(), combo(pathAndQuery) }`. The sync
 * dumps (`dumpCss`/`dumpJs`) call the library in place; the async dumps
 * (`dumpCssAsync`/`dumpJsAsync`) and `combo` run on koffi's worker and never
 * block the event loop. `combo` resolves to a frozen response.
 */
export function createPaste({
  manifest,
  outputBase,
  outputJs,
  outputCss,
  cdnBase = '',
  basePath = '/paste',
  inlineThreshold = 1024,
  library,
} = {}) {
  const settings = JSON.stringify({
    manifest: String(manifest),
    outputBase: String(outputBase),
    ...(outputJs ? { outputJs: String(outputJs) } : {}),
    ...(outputCss ? { outputCss: String(outputCss) } : {}),
    cdnBase,
    basePath,
    inlineThreshold,
  });
  const lib = loadLibrary(library ?? resolveLibrary());

  function render(eventsJson) {
    return unwrap(lib.render(settings, eventsJson));
  }

  function renderAsync(eventsJson) {
    return callAsync(lib.render, settings, eventsJson);
  }

  async function combo(pathAndQuery) {
    // The path crosses the ABI as a C string: a NUL would silently truncate it.
    if (pathAndQuery.includes('\0')) {
      throw new PasteError('combo path must not contain NUL');
    }
    return Object.freeze(await callAsync(lib.combo, settings, pathAndQuery));
  }

  function page() {
    // One page render's ordered events; not shared across pages. Jams arrive
    // from anywhere in a render — nested templates and components each add
    // their own — while dumps land where the layout emits them, so the page
    // accumulates the render's events: one page per render, one thread. The
    // page's single mutable cell: a frozen array replaced per event.
    let events = Object.freeze([]);
    const record = (event) => {
      events = Object.freeze([...events, Object.freeze(event)]);
      return JSON.stringify({ events });
    };
    return Object.freeze({
      jam({ css = [], js = [], inline = false, sync = false, media } = {}) {
        // Copy and freeze: a caller mutating its arrays after jam() must not
        // rewrite this page's recorded events.
        record({
          jam: Object.freeze({
            css: Object.freeze([...css]),
            js: Object.freeze([...js]),
            inline,
            sync,
            media: media ?? null,
          }),
        });
        return '';
      },
      dumpCss() {
        return render(record({ dump: 'css' }));
      },
      dumpJs() {
        return render(record({ dump: 'js' }));
      },
      dumpCssAsync() {
        return renderAsync(record({ dump: 'css' }));
      },
      dumpJsAsync() {
        return renderAsync(record({ dump: 'js' }));
      },
    });
  }

  return Object.freeze({ basePath, page, combo });
}
