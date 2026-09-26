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
    // page's mutable cells: frozen arrays replaced per event, plus the
    // in-flight finish promise.
    let events = Object.freeze([]);
    // Async dump jobs not yet settled — finishAsync awaits them.
    let pendingDumps = Object.freeze([]);
    // Async dumps replay the page's event list, so the native calls must run
    // in record order — a concurrent dump that raced ahead of a failed one
    // would see the failed dump's marker and wrongly treat its modules as
    // delivered. The tail serializes only the native invocation: recording
    // stays immediate and nothing blocks the event loop.
    let dumpTail = Promise.resolve();
    // The in-flight finishAsync(), so a second call joins it rather than
    // sealing a second time.
    let finishPromise;
    // The finish marker is the bare string the wire format uses.
    const FINISH = 'finish';

    // The marker seals recording: once it is in the list — in flight or
    // committed — it is the last event and no new event may land.
    const closed = () => events[events.length - 1] === FINISH;
    const requireOpen = () => {
      if (closed()) {
        throw new PasteError(
          'page is finishing or finished — no more jams or dumps'
        );
      }
    };
    const retract = (event) => {
      events = Object.freeze(events.filter((e) => e !== event));
    };
    const record = (event) => {
      events = Object.freeze([...events, Object.freeze(event)]);
      return JSON.stringify({ events });
    };
    const dump = (assetType) => {
      requireOpen();
      // A sync dump cannot safely overtake queued async dumps — its render
      // would see markers whose native calls have not run yet.
      if (pendingDumps.length > 0 || finishPromise) {
        throw new PasteError(
          'async work is still in flight — await it or use the async dump'
        );
      }
      const event = Object.freeze({ dump: assetType });
      events = Object.freeze([...events, event]);
      try {
        return render(JSON.stringify({ events }));
      } catch (err) {
        // A failed dump never delivered — retract its event.
        retract(event);
        throw err;
      }
    };
    const dumpAsync = (assetType) => {
      requireOpen();
      const event = Object.freeze({ dump: assetType });
      events = Object.freeze([...events, event]);
      const job = dumpTail
        .then(() => {
          // The queue serializes the native calls, so every earlier failed
          // dump has already retracted its marker — but later events may
          // already be recorded. Replay only up to this event.
          const end = events.indexOf(event) + 1;
          if (end === 0) {
            throw new PasteError('internal: dump event missing from page');
          }
          return renderAsync(JSON.stringify({ events: events.slice(0, end) }));
        })
        .catch((err) => {
          // A failed dump never delivered — retract its event.
          retract(event);
          throw err;
        });
      // The tail swallows the outcome so one failure cannot poison the
      // chain — the job's own catch has already cleaned up before the next
      // queued dump computes its prefix.
      dumpTail = job.then(
        () => undefined,
        () => undefined
      );
      pendingDumps = Object.freeze([...pendingDumps, job]);
      const settled = () => {
        pendingDumps = Object.freeze(pendingDumps.filter((j) => j !== job));
      };
      // The derived promise absorbs the rejection so the bookkeeping branch
      // can never surface as an unhandled rejection.
      job.then(settled, settled);
      return job;
    };
    return Object.freeze({
      jam({ css = [], js = [], inline = false, sync = false, media } = {}) {
        requireOpen();
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
        return dump('css');
      },
      dumpJs() {
        return dump('js');
      },
      dumpCssAsync() {
        return dumpAsync('css');
      },
      dumpJsAsync() {
        return dumpAsync('js');
      },
      finish() {
        if (pendingDumps.length > 0 || finishPromise) {
          throw new PasteError(
            'async work is still in flight — await finishAsync() instead'
          );
        }
        if (closed()) return '';
        const candidate = Object.freeze([...events, FINISH]);
        render(JSON.stringify({ events: candidate }));
        // Only a successful validation seals the page.
        events = candidate;
        return '';
      },
      finishAsync() {
        if (finishPromise) return finishPromise;
        if (closed()) return Promise.resolve('');
        // Seal recording now — no new event may land mid-finish — then wait
        // for outstanding async dumps without blocking the loop, and
        // validate the CURRENT list: a failed dump retracts its own event.
        events = Object.freeze([...events, FINISH]);
        finishPromise = Promise.all(pendingDumps)
          .then(() => renderAsync(JSON.stringify({ events })))
          .then(
            () => {
              finishPromise = undefined;
              return '';
            },
            (err) => {
              // The page stays open: the missing dump can still be added
              // and finish retried.
              retract(FINISH);
              finishPromise = undefined;
              throw err;
            }
          );
        return finishPromise;
      },
    });
  }

  return Object.freeze({ basePath, page, combo });
}
