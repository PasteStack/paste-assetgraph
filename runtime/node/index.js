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

// The complete jam option vocabulary — a key outside this set is a
// misspelling that would otherwise be silently dropped from the event.
const JAM_OPTIONS = new Set(['css', 'js', 'inline', 'sync', 'media', 'condition', 'perModule']);

// The complete top-level createPaste vocabulary — a misspelled key like
// `per_module` must never fall through to the defaults silently.
const PASTE_SETTINGS = new Set([
  'manifest', 'outputBase', 'outputJs', 'outputCss',
  'cdnBase', 'basePath', 'inlineThreshold', 'perModule', 'library',
]);

// JSON.stringify throws on circular values — a rejection must still be a
// PasteError, so error messages describe unprintable input via String.
const describe = (value) => {
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
};

// A required filesystem path: a string only — never a boolean, number,
// array or object coerced into a spelling that would silently look
// somewhere else. Blank-only and NUL-carrying spellings are rejected;
// a valid path's bytes are returned untrimmed.
const requiredPath = (name, value) => {
  if (typeof value !== 'string' || value.trim() === '' || value.includes('\0')) {
    throw new PasteError(
      `createPaste ${name} must be a non-empty filesystem path — got ${describe(value)}`
    );
  }
  return value;
};

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
    case 'win32': {
      return 'paste_assetgraph_ffi.dll';
    }
    case 'darwin': {
      return 'libpaste_assetgraph_ffi.dylib';
    }
    default: {
      return 'libpaste_assetgraph_ffi.so';
    }
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
  const raw = env.PASTE_ASSETGRAPH_LIB;
  if (raw !== undefined) {
    // Absolute before both the check and the load: dlopen searches the
    // library path for a bare name, so the file checked here relative to
    // the working directory need not be the one that loads.
    const override = raw === '' ? raw : path.resolve(raw);
    if (isFile(override)) {
      return override;
    }
    throw new PasteError(
      `PASTE_ASSETGRAPH_LIB is set to ${override} but that is not a file`
    );
  }
  const platform = platformDir();
  const bundled = path.join(dir, 'native-lib', platform, libraryFileName());
  if (isFile(bundled)) {
    return bundled;
  }
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
    renderPage: lib.func('paste_render_page', heapStr, ['str', 'str', 'str']),
    combo: lib.func('paste_combo', heapStr, ['str', 'str']),
    comboConditional: lib.func('paste_combo_conditional', heapStr, [
      'str',
      'str',
      'str',
    ]),
  };
}

// The envelope's `ok` payload, or a PasteError carrying `error` — plus the
// machine `kind` when the library marks a failure an adapter must tell
// apart from the rest.
function unwrap(envelope) {
  if (envelope === null || envelope === undefined) {
    throw new PasteError('paste-assetgraph returned no response');
  }
  const parsed = JSON.parse(envelope);
  if (parsed.error !== undefined && parsed.error !== null) {
    const failure = new PasteError(parsed.error);
    if (parsed.kind !== undefined) {
      failure.kind = parsed.kind;
    }
    throw failure;
  }
  return parsed.ok;
}

// koffi admits at most 256 in-flight async calls per process and rejects
// the rest with a plain Error, so every async call in this process — across
// all createPaste instances — waits for one of these slots first. The work
// itself runs on the libuv threadpool (UV_THREADPOOL_SIZE, default 4).
const MAX_IN_FLIGHT = 64;
let inFlight = 0;
const waiting = [];

function releaseSlot() {
  const next = waiting.shift();
  if (next === undefined) {
    inFlight -= 1;
  } else {
    next();
  }
}

function callAsync(fn, ...args) {
  return new Promise((resolve, reject) => {
    const start = () => {
      try {
        fn.async(...args, (err, envelope) => {
          releaseSlot();
          if (err) {
            return reject(err instanceof PasteError ? err : new PasteError(String(err.message ?? err)));
          }
          try {
            resolve(unwrap(envelope));
          } catch (parseError) {
            reject(parseError);
          }
        });
      } catch (err) {
        releaseSlot();
        reject(err instanceof PasteError ? err : new PasteError(String(err.message ?? err)));
      }
    };
    if (inFlight < MAX_IN_FLIGHT) {
      inFlight += 1;
      start();
    } else {
      waiting.push(start);
    }
  });
}

/**
 * A wrapper around the library for one manifest/output pair.
 *
 * Returns a frozen `{ basePath, page(), combo(pathAndQuery) }`. The sync
 * `dump` calls the library in place; `dumpAsync` and `combo` run on koffi's
 * worker and never block the event loop. `combo` resolves to a frozen
 * response.
 */
export function createPaste(options = {}) {
  if (options === null || typeof options !== 'object' || Array.isArray(options)) {
    throw new PasteError(
      `createPaste takes a settings object — got ${describe(options)}`
    );
  }
  // A misspelled setting ('per_module', 'cdn_base') would silently fall
  // back to the default — delivery, origin, and resolver behaviour would
  // change without notice. Reject unknown keys before the native library
  // is even located.
  for (const key of Object.keys(options)) {
    if (!PASTE_SETTINGS.has(key)) {
      throw new PasteError(
        `createPaste has no option ${JSON.stringify(key)} — supported options are ${[...PASTE_SETTINGS].join(', ')}`
      );
    }
  }
  // Destructuring `options` itself would also read keys inherited
  // through the prototype — values the own-key validation above never
  // saw, silently overriding defaults the caller never set. Snapshot the
  // own enumerable entries once and read only from that.
  const supplied = Object.fromEntries(Object.entries(options));
  const {
    manifest,
    outputBase,
    outputJs,
    outputCss,
    cdnBase = '',
    basePath = '/paste',
    inlineThreshold = 1024,
    perModule = false,
    library,
  } = supplied;
  // The two required locations get the same strictness as the optional
  // overrides: String(undefined) posting the literal path "undefined" was
  // the original hazard, but String(42) or String({}) is just as wrong —
  // a mistyped host setting must fail here, never reach the renderer as
  // a different path.
  const manifestPath = requiredPath('manifest', manifest);
  const outputBasePath = requiredPath('outputBase', outputBase);
  // Primitive types are checked before the library is located: a
  // mistyped flag or threshold ('false', -1) would otherwise surface
  // mid-render at the native deserializer instead of at the call that
  // supplied it. What makes a basePath or cdnBase *valid* — the URL
  // grammar — stays with the library; only the type is checked here.
  if (typeof perModule !== 'boolean') {
    throw new PasteError(
      `createPaste perModule must be a boolean — got ${describe(perModule)}`
    );
  }
  if (!Number.isInteger(inlineThreshold) || inlineThreshold < 0) {
    throw new PasteError(
      `createPaste inlineThreshold must be a non-negative integer — got ${describe(inlineThreshold)}`
    );
  }
  for (const [name, value] of [['basePath', basePath], ['cdnBase', cdnBase]]) {
    if (typeof value !== 'string') {
      throw new PasteError(
        `createPaste ${name} must be a string — got ${describe(value)}`
      );
    }
  }
  // outputJs/outputCss distinguish absent — inherit <outputBase>/{js,css}
  // — from explicitly unusable: an env-var expansion producing '' must
  // fail loudly, never silently select the default directory.
  for (const [name, value] of [['outputJs', outputJs], ['outputCss', outputCss]]) {
    if (value === undefined) {
      continue;
    }
    if (typeof value !== 'string' || value.trim() === '' || value.includes('\0')) {
      throw new PasteError(
        `createPaste ${name} must be a non-empty directory path or absent — got ${describe(value)}`
      );
    }
  }
  const settings = JSON.stringify({
    manifest: manifestPath,
    outputBase: outputBasePath,
    ...(outputJs !== undefined ? { outputJs } : {}),
    ...(outputCss !== undefined ? { outputCss } : {}),
    cdnBase,
    basePath,
    inlineThreshold,
    perModule,
  });
  const lib = loadLibrary(library ?? resolveLibrary());

  async function combo(pathAndQuery, options) {
    // The path crosses the ABI as a C string: a NUL would silently
    // truncate it. A non-string caller gets a PasteError, not a
    // TypeError — and nothing is coerced into one.
    if (typeof pathAndQuery !== 'string') {
      throw new PasteError(
        `combo takes a path string — got ${describe(pathAndQuery)}`
      );
    }
    if (pathAndQuery.includes('\0')) {
      throw new PasteError('combo path must not contain NUL');
    }
    // A misspelled option would silently drop the validator — reject
    // anything but a plain object of known keys, as createPaste does.
    if (options !== undefined) {
      if (options === null || typeof options !== 'object' || Array.isArray(options)) {
        throw new PasteError(
          `combo options must be an object — got ${describe(options)}`
        );
      }
      for (const key of Object.keys(options)) {
        if (key !== 'ifModifiedSince') {
          throw new PasteError(
            `combo has no option ${JSON.stringify(key)} — the supported option is ifModifiedSince`
          );
        }
      }
    }
    // The conditional form is one Rust decision — the option only hands
    // the request's If-Modified-Since value to it.
    const validator = options?.ifModifiedSince;
    if (validator === undefined || validator === null) {
      return callAsync(lib.combo, settings, pathAndQuery).then(Object.freeze);
    }
    if (typeof validator !== 'string') {
      throw new PasteError(
        `combo ifModifiedSince takes a header string — got ${describe(validator)}`
      );
    }
    if (validator.includes('\0')) {
      throw new PasteError('combo ifModifiedSince must not contain NUL');
    }
    return callAsync(lib.comboConditional, settings, pathAndQuery, validator).then(
      Object.freeze
    );
  }

  function page() {
    // One page render's ordered events; not shared across pages. Jams arrive
    // from anywhere in a render — nested templates and components each add
    // their own — while dumps land where the layout emits them, so the page
    // accumulates the render's events: one page per render, one thread. The
    // page's mutable cells: one append-only events array (recording must
    // stay O(1) — a page can carry thousands of jams) and the in-flight
    // finish promise.
    let events = [];
    // The manifest generation the page's render calls are pinned to: the
    // first native call asks for whatever the manifest is (''), every
    // answer carries the token it was planned against, and each later call
    // posts that token back — so a build landing mid-page can never make
    // one page mix two manifests' module maps.
    let generation = '';
    // A manifest-generation error retires the page permanently: earlier
    // dumps were planned against a manifest that no longer exists, so no
    // later step may proceed — every subsequent call fails with it.
    let invalid = null;
    // Async dump jobs not yet settled — finishAsync awaits them. A Set:
    // enqueue and settle are O(1), and finishAsync snapshots it once.
    const pendingDumps = new Set();
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

    // No argument dumps every type in one dump; anything else must be
    // exactly 'css' or 'js'. Rejects before any native call or event
    // mutation — the name is validated here, never by the library.
    const dumpScope = (type) => {
      if (type === undefined) {
        return 'all';
      }
      if (type === 'css' || type === 'js') {
        return type;
      }
      throw new PasteError(
        `dump takes 'css', 'js', or no argument — got ${describe(type)}`
      );
    };

    // The marker seals recording: once it is in the list — in flight or
    // committed — it is the last event and no new event may land.
    const closed = () => events[events.length - 1] === FINISH;
    const requireOpen = () => {
      if (invalid !== null) {
        throw invalid;
      }
      if (closed()) {
        throw new PasteError(
          'page is finishing or finished — no more jams or dumps'
        );
      }
    };
    // One page step: the call posts the page's generation token, the answer
    // refreshes it, and `html` is the step's markup.
    const renderPage = (eventsJson) => {
      const result = unwrap(lib.renderPage(settings, eventsJson, generation));
      generation = result.generation;
      return result.html;
    };
    const renderPageAsync = (eventsJson) => callAsync(
      lib.renderPage, settings, eventsJson, generation
    ).then((result) => {
      generation = result.generation;
      return result.html;
    });
    // A mid-page manifest change is terminal for this page — record it so
    // every later call fails the same way — then always propagate.
    const retireOnMismatch = (err) => {
      if (err && err.kind === 'manifest-generation') {
        invalid = err;
      }
      throw err;
    };
    const retract = (event) => {
      events.splice(events.indexOf(event), 1);
    };
    // Recorded events serialize at the native-call boundary, not per jam —
    // a page may jam hundreds of groups but renders only at dump/finish.
    const record = (event) => {
      events.push(Object.freeze(event));
    };
    const dump = (assetType) => {
      requireOpen();
      // A sync dump cannot safely overtake queued async dumps — its render
      // would see markers whose native calls have not run yet.
      if (pendingDumps.size > 0 || finishPromise) {
        throw new PasteError(
          'async work is still in flight — await it or use the async dump'
        );
      }
      const event = Object.freeze({ dump: assetType });
      events.push(event);
      try {
        return renderPage(JSON.stringify({ events }));
      } catch (err) {
        // A failed dump never delivered — retract its event.
        retract(event);
        retireOnMismatch(err);
      }
    };
    const dumpAsync = (assetType) => {
      requireOpen();
      const event = Object.freeze({ dump: assetType });
      events.push(event);
      const job = dumpTail
        .then(() => {
          // A manifest-generation mismatch retires the page permanently:
          // a job queued before the mismatch must rethrow the stored
          // terminal error here rather than reach the native layer — the
          // generation the earlier call discovered is already gone.
          if (invalid !== null) {
            throw invalid;
          }
          // The queue serializes the native calls, so every earlier failed
          // dump has already retracted its marker — but later events may
          // already be recorded. Replay only up to this event.
          const end = events.indexOf(event) + 1;
          if (end === 0) {
            throw new PasteError('internal: dump event missing from page');
          }
          return renderPageAsync(
            JSON.stringify({ events: events.slice(0, end) })
          );
        })
        .catch((err) => {
          // A failed dump never delivered — retract its event. A stored
          // terminal error takes precedence so a pre-retired job surfaces
          // the mismatch it was queued behind, not a fresh error.
          retract(event);
          retireOnMismatch(invalid !== null ? invalid : err);
        });
      // The tail swallows the outcome so one failure cannot poison the
      // chain — the job's own catch has already cleaned up before the next
      // queued dump computes its prefix.
      dumpTail = job.then(
        () => undefined,
        () => undefined
      );
      pendingDumps.add(job);
      const settled = () => {
        pendingDumps.delete(job);
      };
      // The derived promise absorbs the rejection so the bookkeeping branch
      // can never surface as an unhandled rejection.
      job.then(settled, settled);
      return job;
    };
    return Object.freeze({
      jam(options = {}) {
        requireOpen();
        if (options === null || typeof options !== 'object' || Array.isArray(options)) {
          throw new PasteError(
            `jam takes an options object — got ${describe(options)}`
          );
        }
        // A misspelled key ('per_module', 'sycn') must never reach the
        // event silently: the caller asked for a different delivery than
        // the defaults it would get. Own enumerable keys only.
        for (const key of Object.keys(options)) {
          if (!JAM_OPTIONS.has(key)) {
            throw new PasteError(
              `jam has no option ${JSON.stringify(key)} — supported options are ${[...JAM_OPTIONS].join(', ')}`
            );
          }
        }
        // Same own-property rule as createPaste: destructure a snapshot
        // of the own enumerable entries — an inherited css list or
        // perModule flag must never reach the recorded event.
        const supplied = Object.fromEntries(Object.entries(options));
        const {
          css = [], js = [], inline = false, sync = false, media, condition, perModule,
        } = supplied;
        // Every value is checked before the event exists: an invalid jam
        // throws PasteError and changes no page state, so a later valid
        // jam on the same page is the straightforward recovery. A scalar
        // string is not a collection of module names — spreading 'app'
        // would record 'a', 'p', 'p'. Non-strings inside a collection and
        // mistyped options fail the same way, before any native call.
        const moduleList = (value, name) => {
          if (typeof value === 'string' || typeof value?.[Symbol.iterator] !== 'function') {
            throw new PasteError(
              `jam ${name} must be a collection of module names — got ${describe(value)}`
            );
          }
          const list = [...value];
          for (const entry of list) {
            if (typeof entry !== 'string') {
              throw new PasteError(
                `jam ${name} entries must be strings — got ${describe(entry)}`
              );
            }
          }
          return list;
        };
        const modulesCss = moduleList(css, 'css');
        const modulesJs = moduleList(js, 'js');
        for (const [name, value] of [['inline', inline], ['sync', sync]]) {
          if (typeof value !== 'boolean') {
            throw new PasteError(
              `jam ${name} must be a boolean — got ${describe(value)}`
            );
          }
        }
        if (media !== undefined && media !== null && typeof media !== 'string') {
          throw new PasteError(
            `jam media must be a string or absent — got ${describe(media)}`
          );
        }
        if (condition !== undefined && condition !== null && typeof condition !== 'string') {
          throw new PasteError(
            `jam condition must be a string or absent — got ${describe(condition)}`
          );
        }
        if (perModule !== undefined && perModule !== null && typeof perModule !== 'boolean') {
          throw new PasteError(
            `jam perModule must be a boolean or absent — got ${describe(perModule)}`
          );
        }
        // Copy and freeze: a caller mutating its arrays after jam() must not
        // rewrite this page's recorded events. condition and perModule post
        // only when set — absent means unconditional / inherits the render's
        // setting.
        record({
          jam: Object.freeze({
            css: Object.freeze(modulesCss),
            js: Object.freeze(modulesJs),
            inline,
            sync,
            media: media ?? null,
            ...(condition !== undefined && condition !== null
              ? { condition }
              : {}),
            ...(perModule !== undefined && perModule !== null
              ? { perModule }
              : {}),
          }),
        });
        return '';
      },
      dump(type) {
        return dump(dumpScope(type));
      },
      dumpAsync(type) {
        const scope = dumpScope(type);
        return dumpAsync(scope);
      },
      finish() {
        if (invalid !== null) {
          throw invalid;
        }
        if (pendingDumps.size > 0 || finishPromise) {
          throw new PasteError(
            'async work is still in flight — await finishAsync() instead'
          );
        }
        if (closed()) {
          return '';
        }
        const marker = FINISH;
        events.push(marker);
        try {
          renderPage(JSON.stringify({ events }));
        } catch (err) {
          // A failed finish leaves the page open — retract the marker.
          retract(marker);
          retireOnMismatch(err);
        }
        return '';
      },
      finishAsync() {
        if (invalid !== null) {
          return Promise.reject(invalid);
        }
        if (finishPromise) {
          return finishPromise;
        }
        if (closed()) {
          return Promise.resolve('');
        }
        // Seal recording now — no new event may land mid-finish — then
        // wait out every outstanding async dump: Promise.all would answer
        // on the FIRST rejection while later dumps still have native calls
        // in flight, unsealing a page those calls still write into. Drain
        // them all, then validate the CURRENT list — a failed dump has
        // already retracted its own event.
        events.push(FINISH);
        finishPromise = Promise.allSettled([...pendingDumps])
          .then((outcomes) => {
            // Everything queued has settled — a manifest-generation
            // mismatch discovered by any of them is terminal, so the
            // finish must not reach the native layer after the page was
            // already retired.
            if (invalid !== null) {
              throw invalid;
            }
            const failed = outcomes.find(
              (outcome) => outcome.status === 'rejected'
            );
            if (failed) {
              throw failed.reason;
            }
            return renderPageAsync(JSON.stringify({ events }));
          })
          .then(
            () => {
              finishPromise = undefined;
              return '';
            },
            (err) => {
              // The page stays open: the missing dump can still be added
              // and finish retried — unless the page is retired, in which
              // case every later call answers the recorded error.
              retract(FINISH);
              finishPromise = undefined;
              return retireOnMismatch(err);
            }
          );
        return finishPromise;
      },
    });
  }

  return Object.freeze({ basePath, page, combo });
}
