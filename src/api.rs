//! In-process entry points: the CLI's `render`/`combo` commands,
//! and the FFI library all call through here. Parsed manifests are cached
//! process-wide, keyed by the file's modification stamp; nothing else is
//! cached — every call re-resolves through `jam`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;

use anyhow::Result;
use serde::Deserialize;

use crate::jam;
use crate::manifest::Manifest;

fn default_base_path() -> String {
    "/paste".to_string()
}

fn default_inline_threshold() -> usize {
    1024
}

/// Everything a render or combo call needs — the shape each runtime posts
/// as its settings JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// Manifest JSON path.
    pub manifest: PathBuf,
    /// Compiled output directory.
    pub output_base: PathBuf,
    /// Compiled JS directory (default: `<outputBase>/js`).
    #[serde(default)]
    pub output_js: Option<PathBuf>,
    /// Compiled CSS directory (default: `<outputBase>/css`).
    #[serde(default)]
    pub output_css: Option<PathBuf>,
    /// URL prefix for emitted tags (a CDN origin).
    #[serde(default)]
    pub cdn_base: String,
    /// Combo URL path prefix.
    #[serde(default = "default_base_path")]
    pub base_path: String,
    /// Automatic inline limit in UTF-8 bytes (0 disables).
    #[serde(default = "default_inline_threshold")]
    pub inline_threshold: usize,
    /// Request each resolved module's versioned file in dependency order
    /// instead of one combo URL per group. A jam event's own `perModule`
    /// overrides this per group.
    #[serde(default)]
    pub per_module: bool,
}

impl Settings {
    /// The compiled-file layout: per-type directories default to
    /// `<outputBase>/js` and `<outputBase>/css`.
    pub fn output_dirs(&self) -> jam::OutputDirs {
        let mut dirs = jam::OutputDirs::under(&self.output_base);
        if let Some(js) = &self.output_js {
            dirs.js = js.clone();
        }
        if let Some(css) = &self.output_css {
            dirs.css = css.clone();
        }
        dirs
    }
}

/// stdin shape for `render`: `{"events":[{"jam":{...}},{"dump":"js"}]}`;
/// the dump scope is `"css"`, `"js"`, or `"all"` — css tags then js tags
/// in one dump — and a trailing `"finish"` validates the page.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderInput {
    events: Vec<jam::Event>,
}

/// The most manifests held at once — enough for a busy host without
/// letting a long-lived process accumulate an entry per file it has seen.
const MANIFEST_CACHE_LIMIT: usize = 128;

/// What identifies a manifest on disk. On Unix the inode identity joins
/// the modification time and length, so an atomic rename that preserves
/// both still reads as a different file. Elsewhere the file's bytes are
/// the stamp — no timestamp preservation can serve a stale manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
    #[cfg(not(unix))]
    content: Vec<u8>,
}

impl Stamp {
    /// The stamp of an already-open file's current metadata.
    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        Ok(Stamp {
            modified: meta.modified()?,
            len: meta.len(),
            identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        })
    }

    /// Whether two stamps of the same open inode describe the same bytes:
    /// only fields a write to the inode changes count. ctime is excluded —
    /// an atomic publish renaming a new file over the path bumps the held
    /// inode's ctime (its link count drops) without touching its bytes.
    #[cfg(unix)]
    fn same_content(&self, other: &Stamp) -> bool {
        (self.modified, self.len, self.identity.0, self.identity.1)
            == (
                other.modified,
                other.len,
                other.identity.0,
                other.identity.1,
            )
    }
}

/// Bounded manifest cache: `entries` holds the values, `order` remembers
/// first-seen insertion order for eviction. A hit only clones the `Arc`s
/// and never touches `order`; eviction happens only when a NEW key lands
/// at the limit. Each entry also stores the manifest's generation token
/// — the md5 of the exact bytes it was parsed from — so a page can pin
/// its event replay to one manifest generation.
struct ManifestCache {
    entries: HashMap<PathBuf, (Stamp, Arc<Manifest>, Arc<str>)>,
    order: VecDeque<PathBuf>,
}

impl ManifestCache {
    fn new() -> Self {
        ManifestCache {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &Path, stamp: &Stamp) -> Option<(Arc<Manifest>, Arc<str>)> {
        match self.entries.get(key) {
            Some((cached, manifest, generation)) if cached == stamp => {
                Some((manifest.clone(), generation.clone()))
            }
            _ => None,
        }
    }

    fn insert(
        &mut self,
        key: PathBuf,
        stamp: Stamp,
        manifest: Arc<Manifest>,
        generation: Arc<str>,
    ) {
        if !self.entries.contains_key(&key) {
            if self.entries.len() >= MANIFEST_CACHE_LIMIT {
                while let Some(oldest) = self.order.pop_front() {
                    if self.entries.remove(&oldest).is_some() {
                        break;
                    }
                }
            }
            self.order.push_back(key.clone());
        }
        self.entries.insert(key, (stamp, manifest, generation));
    }
}

/// Parsed manifests shared process-wide. A read of the lock recovers a
/// poisoned lock rather than panicking — a panicking loader must not wedge
/// every later call.
fn cache() -> &'static RwLock<ManifestCache> {
    static CACHE: OnceLock<RwLock<ManifestCache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(ManifestCache::new()))
}

/// The manifest at `path`, cached by file identity: every call opens the
/// file, a stamp hit clones the shared `Arc`, anything else reparses. An
/// open, stat, or parse failure returns `Err` and caches nothing.
/// Concurrent misses may both parse — the last writer wins, no
/// single-flight.
pub fn manifest(path: &Path) -> Result<Arc<Manifest>> {
    Ok(manifest_with_generation(path)?.0)
}

/// The manifest plus its generation token — the md5 of the exact bytes
/// the parse came from. A page's event replay pins each step to the token
/// its first step was answered with, so a build landing mid-page can
/// never make one page mix two manifests' module maps.
pub(crate) fn manifest_with_generation(path: &Path) -> Result<(Arc<Manifest>, Arc<str>)> {
    manifest_with_cache(path, Path::new(""), cache())
}

/// `path` relative to `root` (the caller's cwd for `manifest`), served
/// from `cache`. Split out so tests can exercise a private cache and a
/// resolution root without moving the process cwd.
fn manifest_with_cache(
    path: &Path,
    root: &Path,
    cache: &RwLock<ManifestCache>,
) -> Result<(Arc<Manifest>, Arc<str>)> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    // The canonical key folds aliases (`./`, `..`, symlinks, a relative
    // spelling) into one entry; a path that cannot be canonicalized keeps
    // its spelling so the open below reports the failure.
    let key = joined.canonicalize().unwrap_or(joined);
    // Open before the lookup: the stamp and the bytes parsed below come
    // from the same open file, and no cache lock is held across the I/O.
    // The checked opener is required — an ordinary open stalls forever on
    // a FIFO planted at the manifest path.
    let file = crate::manifest::Manifest::open_manifest_file(&key)?;
    manifest_from_open_file(key, file, cache)
}

#[cfg(unix)]
fn manifest_from_open_file(
    key: PathBuf,
    file: std::fs::File,
    cache: &RwLock<ManifestCache>,
) -> Result<(Arc<Manifest>, Arc<str>)> {
    let stamp = Stamp::of(&file.metadata()?)?;
    {
        let guard = cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = guard.get(&key, &stamp) {
            return Ok(hit);
        }
    }
    let content = crate::manifest::Manifest::read_bounded_file(
        &file,
        &key,
        crate::manifest::Manifest::MANIFEST_READ_CAP,
    )?;
    let generation: Arc<str> = format!("{:x}", md5::compute(&content)).into();
    let manifest = Manifest::from_json_reader(content.as_slice())?;
    // The bytes just parsed must still be the bytes the stamp described.
    // Only fields a write to this inode can change are compared: an atomic
    // publish renames a new file over the path, which bumps the held
    // inode's ctime (its link count drops) without touching its bytes.
    if !Stamp::of(&file.metadata()?)?.same_content(&stamp) {
        anyhow::bail!("manifest {} changed while it was being read", key.display());
    }
    let manifest = Arc::new(manifest);
    let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
    guard.insert(key, stamp, manifest.clone(), generation.clone());
    Ok((manifest, generation))
}

#[cfg(not(unix))]
fn manifest_from_open_file(
    key: PathBuf,
    file: std::fs::File,
    cache: &RwLock<ManifestCache>,
) -> Result<(Arc<Manifest>, Arc<str>)> {
    let content = crate::manifest::Manifest::read_bounded_file(
        &file,
        &key,
        crate::manifest::Manifest::MANIFEST_READ_CAP,
    )?;
    let stamp = Stamp {
        modified: file.metadata()?.modified()?,
        len: file.metadata()?.len(),
        content,
    };
    {
        let guard = cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = guard.get(&key, &stamp) {
            return Ok(hit);
        }
    }
    let generation: Arc<str> = format!("{:x}", md5::compute(&stamp.content)).into();
    let manifest = Arc::new(Manifest::from_json_reader(stamp.content.as_slice())?);
    let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
    guard.insert(key, stamp, manifest.clone(), generation.clone());
    Ok((manifest, generation))
}

/// Render the event list and return the last dump's HTML — the same bytes
/// the `render` command prints. An event list ending in `"finish"` is a
/// completion check instead: it validates that every jammed module was
/// dumped and returns an empty string, never markup.
pub fn render(settings: &Settings, events_json: &str) -> Result<String> {
    let manifest = manifest(&settings.manifest)?;
    render_events(&manifest, settings, events_json)
}

/// The render shared by `render` and `render_page`: parse the event list
/// and answer the last dump (or validate a trailing finish) against
/// `manifest`.
fn render_events(manifest: &Manifest, settings: &Settings, events_json: &str) -> Result<String> {
    let request: RenderInput = serde_json::from_str(events_json)?;
    if matches!(request.events.last(), Some(jam::Event::Finish)) {
        jam::validate_finished(manifest, &request.events)?;
        return Ok(String::new());
    }
    let options = jam::RenderOptions {
        // The runtime's cdn_base wins; an empty one inherits the default
        // the build recorded in the manifest.
        cdn_base: if settings.cdn_base.is_empty() {
            manifest.cdn_base.clone().unwrap_or_default()
        } else {
            settings.cdn_base.clone()
        },
        base_path: settings.base_path.clone(),
        inline_threshold: settings.inline_threshold,
        per_module: settings.per_module,
        // API-rendered tags are never materialized as files; one URL
        // component per combo regardless of length.
    };
    let dirs = settings.output_dirs();
    // The whole history still plans — expansion, ordering and loaded-set
    // dedup need it — but only the terminal dump is emitted: its
    // predecessors were already delivered, and re-rendering them would
    // reopen compiled files this answer does not need.
    Ok(jam::render_last(manifest, &dirs, &options, &request.events)?.html)
}

/// A render result bound to the manifest generation it was planned
/// against — the token a page carries between its steps.
#[derive(Debug)]
pub struct PageRender {
    /// The rendered markup (empty for a finish validation).
    pub html: String,
    /// The generation token of the manifest this step used — the caller
    /// stores it verbatim and posts it back on the next step.
    pub generation: String,
}

/// A page's stored generation token no longer matches the manifest on
/// disk: a build landed between the page's steps, so the event replay
/// must not continue against a different module map than the earlier
/// steps were answered from.
#[derive(Debug)]
pub struct GenerationMismatch {
    /// The token the caller supplied.
    pub expected: String,
    /// The manifest's current token.
    pub actual: String,
}

impl std::fmt::Display for GenerationMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "manifest generation changed mid-page: expected {}, found {}",
            self.expected, self.actual
        )
    }
}

impl std::error::Error for GenerationMismatch {}

/// Render one page step against the manifest, pinned to `expected` when
/// supplied: a generation mismatch is a [`GenerationMismatch`] error
/// raised before any event replays, so markup never mixes two manifests.
/// The returned token identifies the manifest actually used — a page
/// obtains it from a successful call and posts it back on the next step.
pub fn render_page(
    settings: &Settings,
    events_json: &str,
    expected: Option<&str>,
) -> Result<PageRender> {
    let (manifest, generation) = manifest_with_generation(&settings.manifest)?;
    if let Some(expected) = expected {
        if expected != generation.as_ref() {
            return Err(GenerationMismatch {
                expected: expected.to_string(),
                actual: generation.to_string(),
            }
            .into());
        }
    }
    let html = render_events(&manifest, settings, events_json)?;
    Ok(PageRender {
        html,
        generation: generation.to_string(),
    })
}

/// Resolve a combo `path?query` string to its response — the same value
/// the `combo` command prints as JSON.
pub fn combo(settings: &Settings, path_and_query: &str) -> Result<jam::ComboResponse> {
    combo_conditional(settings, path_and_query, None)
}

/// `combo` with the request's `If-Modified-Since` value — one Rust
/// decision for every adapter: a validator at or after the group's
/// recorded stamp answers 304; anything else resolves exactly as
/// `combo` does.
pub fn combo_conditional(
    settings: &Settings,
    path_and_query: &str,
    if_modified_since: Option<&str>,
) -> Result<jam::ComboResponse> {
    let manifest = manifest(&settings.manifest)?;
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    Ok(jam::combo_conditional(
        &manifest,
        &settings.output_dirs(),
        &settings.base_path,
        path,
        query,
        if_modified_since,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn manifest_json() -> &'static str {
        r#"{
            "version": "42",
            "generated": "2026-01-01T00:00:00Z",
            "modules": { "paste": "js/paste.js", "app": "js/app.js", "site": "css/site.css" },
            "assets": {
                "js/paste.js": {
                    "source": "src/paste.js",
                    "output": "/js/paste.h.v1.min.js",
                    "hash": "7e9e92b0857c477a6e8aebb0ab2bd924",
                    "version": 1,
                    "timestamp": "2026-01-01T00:00:00Z",
                    "dependencies": [],
                    "history": []
                },
                "js/app.js": {
                    "source": "src/app.js",
                    "output": "/js/app.h.v1.min.js",
                    "hash": "0bdbb2c325525e986d55ef50ca66d056",
                    "version": 1,
                    "timestamp": "2026-01-01T00:00:00Z",
                    "dependencies": [],
                    "history": []
                },
                "css/site.css": {
                    "source": "src/site.css",
                    "output": "/css/site.h.v1.min.css",
                    "hash": "2c56c360580420d293172f42d85dfbed",
                    "version": 1,
                    "timestamp": "2026-01-01T00:00:00Z",
                    "dependencies": [],
                    "history": []
                }
            },
            "sorted_deps": {}
        }"#
    }

    /// A manifest file plus its compiled outputs, in the layout the build
    /// writes; returns the settings describing them.
    fn fixture(root: &Path) -> Settings {
        write(&root.join("public/js/paste.h.v1.min.js"), "PASTE_RT");
        write(&root.join("public/js/app.h.v1.min.js"), "APP");
        write(&root.join("manifest.json"), manifest_json());
        Settings {
            manifest: root.join("manifest.json"),
            output_base: root.join("public"),
            output_js: None,
            output_css: None,
            cdn_base: String::new(),
            base_path: "/paste".to_string(),
            inline_threshold: 1024,
            per_module: false,
        }
    }

    #[test]
    fn render_returns_the_last_dump_html() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let events = r#"{"events":[{"jam":{"js":["app"]}},{"dump":"css"},{"dump":"js"}]}"#;
        let manifest = Manifest::load_json(&settings.manifest).unwrap();
        let options = jam::RenderOptions {
            cdn_base: String::new(),
            base_path: "/paste".to_string(),
            inline_threshold: 1024,
            per_module: false,
        };
        let dirs = settings.output_dirs();
        let request: Vec<jam::Event> = serde_json::from_str::<RenderInput>(events).unwrap().events;
        let rendered = jam::render_all(&manifest, &dirs, &options, &request).unwrap();
        assert_eq!(
            render(&settings, events).unwrap(),
            rendered.last().unwrap().html
        );
    }

    #[test]
    fn render_all_scope_returns_one_combined_dump() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let events = r#"{"events":[{"jam":{"css":["site"],"js":["app"]}},{"dump":"all"}]}"#;
        let html = render(&settings, events).unwrap();
        assert!(html.contains("stylesheet"), "{html}");
        assert!(html.contains("<script"), "{html}");
        // The css tag leads; both arrive in the one string a page emits.
        assert!(
            html.find("<link").unwrap() < html.find("<script").unwrap(),
            "{html}"
        );
    }

    #[test]
    fn render_with_finish_validates_and_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());

        // A finished page is a completion check, not markup: the events
        // were validated and the render returns empty.
        let events = r#"{"events":[{"jam":{"css":["site"],"js":["app"]}},{"dump":"css"},{"dump":"js"},"finish"]}"#;
        assert_eq!(render(&settings, events).unwrap(), "");

        // A jam no dump emits fails the finish instead of being dropped.
        let events = r#"{"events":[{"dump":"css"},{"jam":{"css":["site"],"js":["app"]}},{"dump":"js"},"finish"]}"#;
        let err = render(&settings, events).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("site"), "{msg}");

        // Events after finish — and finish missing entirely — keep the
        // ordinary event-list errors.
        let events = r#"{"events":[{"dump":"css"},"finish",{"dump":"js"}]}"#;
        assert!(render(&settings, events).is_err());
        let events = r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#;
        assert!(
            render(&settings, events).is_ok(),
            "a bare dump still renders"
        );
    }

    #[test]
    fn render_page_does_not_reopen_already_delivered_dumps() {
        // The original lifecycle is incremental: a dump delivered to the
        // browser was already read and served. A later dump must plan
        // the whole history — for loaded-set and ordering — but must not
        // re-emit it: a css file that vanished after its own successful
        // dump cannot fail an unrelated js dump on the same generation.
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        write(&dir.path().join("public/css/site.h.v1.min.css"), "CSS");

        // inline forces a real read of the css bytes at dump time.
        let events = r#"{"events":[{"jam":{"css":["site"],"inline":true}},{"dump":"css"}]}"#;
        let step = render_page(&settings, events, None).unwrap();
        assert!(step.html.contains("CSS"), "{}", step.html);

        std::fs::remove_file(dir.path().join("public/css/site.h.v1.min.css")).unwrap();

        let events = r#"{"events":[{"jam":{"css":["site"],"inline":true}},{"dump":"css"},{"jam":{"js":["app"],"inline":true}},{"dump":"js"}]}"#;
        let step = render_page(&settings, events, Some(&step.generation)).unwrap();
        assert!(step.html.contains("APP"), "{}", step.html);
        assert!(!step.html.contains("CSS"), "{}", step.html);

        // And finish still validates — planning consults the manifest,
        // never the files.
        let events = r#"{"events":[{"jam":{"css":["site"],"inline":true}},{"dump":"css"},{"jam":{"js":["app"],"inline":true}},{"dump":"js"},"finish"]}"#;
        render_page(&settings, events, Some(&step.generation)).unwrap();
    }

    #[test]
    fn render_last_matches_the_last_render_all_dump() {
        // Only emission changes — planning is identical — so the terminal
        // dump is byte-for-byte the same as render_all's last element
        // across delivery modes, media groups and mixed event lists.
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        write(&dir.path().join("public/css/site.h.v1.min.css"), "CSS");
        let manifest = Manifest::load_json(&settings.manifest).unwrap();
        let dirs = settings.output_dirs();

        let events_lists = [
            r#"[{"jam":{"css":["site"],"js":["app"]}},{"dump":"css"},{"dump":"js"}]"#,
            r#"[{"jam":{"css":["site"],"media":"print"}},{"jam":{"css":["site"]}},{"dump":"css"},{"jam":{"js":["app"]}},{"dump":"all"}]"#,
            r#"[{"jam":{"js":["app"],"inline":true,"sync":true}},{"dump":"js"},{"jam":{"js":["app"]}},{"dump":"all"}]"#,
            r#"[{"jam":{"css":["site"],"js":["app"],"perModule":true}},{"dump":"all"}]"#,
            r#"[{"jam":{"css":["site"],"js":["app"]}},{"dump":"all"},{"jam":{"css":["site"]}},{"dump":"css"}]"#,
        ];
        for per_module in [false, true] {
            for inline_threshold in [0, 1024, usize::MAX] {
                let options = jam::RenderOptions {
                    cdn_base: String::new(),
                    base_path: "/paste".to_string(),
                    inline_threshold,
                    per_module,
                };
                for events in events_lists {
                    let parsed =
                        serde_json::from_str::<RenderInput>(&format!("{{\"events\":{events}}}"))
                            .unwrap()
                            .events;
                    let expected = jam::render_all(&manifest, &dirs, &options, &parsed)
                        .unwrap()
                        .pop()
                        .unwrap()
                        .html;
                    let actual = jam::render_last(&manifest, &dirs, &options, &parsed)
                        .unwrap()
                        .html;
                    assert_eq!(
                        actual, expected,
                        "per_module={per_module} threshold={inline_threshold} events={events}"
                    );
                }
            }
        }
    }

    #[test]
    fn render_last_keeps_render_alls_errors() {
        // A malformed or unresolvable history fails identically whether
        // the caller asked for every dump or just the last.
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let manifest = Manifest::load_json(&settings.manifest).unwrap();
        let dirs = settings.output_dirs();
        let options = jam::RenderOptions {
            cdn_base: String::new(),
            base_path: "/paste".to_string(),
            inline_threshold: 0,
            per_module: false,
        };
        for events in [
            r#"[{"jam":{"js":["app"]}}]"#,
            r#"[{"jam":{"js":["ghost"]}},{"dump":"js"}]"#,
            r#"[{"dump":"js"},"finish",{"dump":"css"}]"#,
        ] {
            let parsed = serde_json::from_str::<RenderInput>(&format!("{{\"events\":{events}}}"))
                .unwrap()
                .events;
            let all = jam::render_all(&manifest, &dirs, &options, &parsed)
                .err()
                .map(|e| format!("{e:#}"));
            let last = jam::render_last(&manifest, &dirs, &options, &parsed)
                .err()
                .map(|e| format!("{e:#}"));
            assert_eq!(last, all, "events={events}");
            assert!(last.is_some(), "events={events} must fail");
        }
    }

    #[test]
    fn cdn_base_precedence_manifest_default_then_settings_override() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = fixture(dir.path());
        // Threshold 0 keeps the tag external so its URL shows the
        // resolved prefix.
        settings.inline_threshold = 0;
        let events = r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#;

        // No manifest default, no settings value → origin-relative URL.
        let html = render(&settings, events).unwrap();
        assert!(html.contains("r('/paste/"), "{html}");

        // A manifest-recorded default supplies the prefix.
        let with_cdn = manifest_json().replace(
            "\"sorted_deps\": {}",
            "\"cdn_base\": \"https://cdn.example\", \"sorted_deps\": {}",
        );
        write(&settings.manifest, &with_cdn);
        let html = render(&settings, events).unwrap();
        assert!(html.contains("r('https://cdn.example/paste/"), "{html}");

        // A runtime cdnBase always wins over the manifest default.
        settings.cdn_base = "https://other.example".to_string();
        let html = render(&settings, events).unwrap();
        assert!(html.contains("r('https://other.example/paste/"), "{html}");

        // Combo resolution ignores the prefix in every case — the same
        // request answers identically under all three.
        let plain = combo(&settings, "/paste/app.js?require").unwrap();
        settings.cdn_base = String::new();
        let inheriting = combo(&settings, "/paste/app.js?require").unwrap();
        assert_eq!(plain.body, inheriting.body);
        assert_eq!(plain.status, inheriting.status);
    }

    #[test]
    fn render_rejects_a_malformed_runtime_cdn_base() {
        // settings.cdn_base lands verbatim in emitted tag URLs — a
        // malformed authority must fail validation before any HTML, and
        // never degrade to the local origin (that would hide the
        // misconfigured host).
        let dir = tempfile::tempdir().unwrap();
        let mut settings = fixture(dir.path());
        // Threshold 0 keeps the tag external so its URL shows the
        // resolved prefix.
        settings.inline_threshold = 0;
        let events = r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#;

        for bad in [
            "https://cdn.example:bad/assets",
            "https://user@cdn.example/x",
            "//cdn.example:/assets",
            "https://cdn.%30/assets",
            "https://cdn.%C3/assets",
        ] {
            settings.cdn_base = bad.to_string();
            let err = render(&settings, events).unwrap_err();
            assert!(
                format!("{err:#}").contains("cdn_base"),
                "{bad}: got {err:#}"
            );
        }

        // Control: a valid authority renders the joined origin URL.
        settings.cdn_base = "https://cdn.example:8443/assets".to_string();
        let html = render(&settings, events).unwrap();
        assert!(
            html.contains("https://cdn.example:8443/assets/paste/"),
            "{html}"
        );
        // A valid UTF-8-encoded IDN remains a renderable origin.
        settings.cdn_base = "https://cdn.%C3%A9xample/assets".to_string();
        let html = render(&settings, events).unwrap();
        assert!(
            html.contains("https://cdn.%C3%A9xample/assets/paste/"),
            "{html}"
        );
    }

    #[test]
    fn render_rejects_unknown_event_option_keys() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        // A misspelled jam option must fail the parse — never fall back
        // to a default that changes delivery silently.
        for events in [
            r#"{"events":[{"jam":{"js":["app"],"per_module":true}},{"dump":"js"}]}"#,
            r#"{"events":[{"jam":{"js":["app"],"sycn":true}},{"dump":"js"}]}"#,
            r#"{"events":[{"jam":{"js":["app"],"inlne":true}},{"dump":"js"}]}"#,
        ] {
            let err = render(&settings, events).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("unknown field"), "{events}: {msg}");
        }
        // An extra top-level envelope key is never ignored either.
        let err = render(&settings, r#"{"events":[{"dump":"js"}],"eventz":[]}"#).unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"));
    }

    #[test]
    fn render_per_module_override_parses_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = fixture(dir.path());
        // Threshold 0 keeps every tag external so the URL shows which
        // delivery mode the event selected.
        settings.inline_threshold = 0;
        // true → individual versioned file tags; false and null → the
        // page-level default (combo).
        for (flag, needle) in [
            ("true", "app.h.v1.min.js"),
            ("false", "app+v1.0"),
            ("null", "app+v1.0"),
        ] {
            let events = format!(
                r#"{{"events":[{{"jam":{{"js":["app"],"perModule":{flag}}}}},{{"dump":"js"}}]}}"#
            );
            let html = render(&settings, &events).unwrap();
            assert!(html.contains(needle), "perModule {flag}: {html}");
        }
    }

    #[test]
    fn combo_splits_the_query_string() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let with_query = combo(&settings, "/paste/app.js?require").unwrap();
        let require_form = combo(&settings, "/paste/require/app.js").unwrap();
        assert_eq!(with_query.body, require_form.body);
    }

    #[test]
    fn manifest_is_cached_by_modification_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let first = manifest(&settings.manifest).unwrap();
        let second = manifest(&settings.manifest).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn manifest_reloads_after_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let first = manifest(&settings.manifest).unwrap();
        // A rewrite lands only when the stamp moves; bump the mtime past
        // the filesystem's granularity explicitly.
        write(&settings.manifest, manifest_json());
        std::fs::File::options()
            .write(true)
            .open(&settings.manifest)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        let second = manifest(&settings.manifest).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn render_page_pins_the_manifest_generation() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let events = r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#;

        // The first step issues the token the manifest currently carries.
        let page = render_page(&settings, events, None).unwrap();
        assert!(!page.generation.is_empty());
        // Posting it back validates — the manifest has not moved.
        let page2 = render_page(&settings, events, Some(&page.generation)).unwrap();
        assert_eq!(page.html, page2.html);

        // A stale token fails before any event replays.
        let err = render_page(&settings, events, Some("deadbeef")).unwrap_err();
        assert!(err.downcast_ref::<GenerationMismatch>().is_some());
    }

    #[test]
    fn render_page_detects_a_manifest_rewrite_mid_page() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let events = r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#;
        let page = render_page(&settings, events, None).unwrap();

        // A build lands between the page's steps: the bytes change, the
        // stamp moves, and the stored token is rejected.
        let rewritten = manifest_json().replace("\"version\": \"42\"", "\"version\": \"43\"");
        write(&settings.manifest, &rewritten);
        std::fs::File::options()
            .write(true)
            .open(&settings.manifest)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        let err = render_page(&settings, events, Some(&page.generation)).unwrap_err();
        let mismatch = err
            .downcast_ref::<GenerationMismatch>()
            .expect("a manifest rewrite is a generation mismatch");
        assert_eq!(mismatch.expected, page.generation);

        // The new generation answers its own token.
        let fresh = render_page(&settings, events, None).unwrap();
        assert_ne!(fresh.generation, page.generation);
    }

    #[test]
    fn manifest_path_aliases_share_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let cache = RwLock::new(ManifestCache::new());
        // A relative spelling resolved against `root` and the absolute
        // path canonicalize to the same key.
        let (relative, _) =
            manifest_with_cache(Path::new("manifest.json"), dir.path(), &cache).unwrap();
        let (absolute, _) = manifest_with_cache(&settings.manifest, Path::new(""), &cache).unwrap();
        assert!(Arc::ptr_eq(&relative, &absolute));
        // A dotted spelling of the same file too.
        let (dotted, _) =
            manifest_with_cache(&dir.path().join("./manifest.json"), Path::new(""), &cache)
                .unwrap();
        assert!(Arc::ptr_eq(&relative, &dotted));
        assert_eq!(cache.read().unwrap().entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_publish_during_the_read_does_not_fail_the_held_handle() {
        // A build renames a new manifest over the path while a request is
        // mid-read: the held inode's ctime moves (its link count drops),
        // but its bytes are complete and consistent.
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let file = crate::manifest::Manifest::open_manifest_file(&settings.manifest).unwrap();
        let before = Stamp::of(&file.metadata().unwrap()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let next = dir.path().join("next.json");
        std::fs::write(&next, manifest_json()).unwrap();
        std::fs::rename(&next, &settings.manifest).unwrap();
        let after = Stamp::of(&file.metadata().unwrap()).unwrap();
        assert_ne!(
            before, after,
            "the rename-over moved the held inode's ctime"
        );
        assert!(after.same_content(&before), "but not its content identity");
    }

    #[cfg(unix)]
    #[test]
    fn manifest_reloads_after_an_atomic_rename_preserving_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let first = manifest(&settings.manifest).unwrap();
        let old_meta = std::fs::metadata(&settings.manifest).unwrap();

        // Same length, same mtime — only the inode identity changes when
        // the sibling is renamed over the original.
        let replacement = manifest_json().replace("\"version\": \"42\"", "\"version\": \"43\"");
        assert_eq!(old_meta.len() as usize, replacement.len());
        let tmp = dir.path().join("manifest.json.new");
        write(&tmp, &replacement);
        std::fs::File::options()
            .write(true)
            .open(&tmp)
            .unwrap()
            .set_modified(old_meta.modified().unwrap())
            .unwrap();
        std::fs::rename(&tmp, &settings.manifest).unwrap();

        let second = manifest(&settings.manifest).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.version, "43");
    }

    #[test]
    fn the_cache_evicts_the_oldest_key_at_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("probe.json");
        write(&probe, manifest_json());
        #[cfg(unix)]
        let stamp = Stamp::of(&std::fs::metadata(&probe).unwrap()).unwrap();
        #[cfg(not(unix))]
        let stamp = Stamp {
            modified: std::fs::metadata(&probe).unwrap().modified().unwrap(),
            len: 0,
            content: Vec::new(),
        };

        let mut cache = ManifestCache::new();
        for i in 0..MANIFEST_CACHE_LIMIT + 1 {
            let key = dir.path().join(format!("m{i}.json"));
            write(&key, manifest_json());
            cache.insert(
                key.canonicalize().unwrap(),
                stamp.clone(),
                Arc::new(Manifest::new()),
                "gen".into(),
            );
        }
        assert_eq!(cache.entries.len(), MANIFEST_CACHE_LIMIT);
        assert_eq!(cache.order.len(), MANIFEST_CACHE_LIMIT);
        let evicted = dir.path().join("m0.json").canonicalize().unwrap();
        let newest = dir
            .path()
            .join(format!("m{MANIFEST_CACHE_LIMIT}.json"))
            .canonicalize()
            .unwrap();
        assert!(!cache.entries.contains_key(&evicted));
        assert!(cache.entries.contains_key(&newest));

        // A hit clones the Arc without touching the eviction order.
        let survivor = dir.path().join("m1.json").canonicalize().unwrap();
        assert!(cache.get(&survivor, &stamp).is_some());
        assert_eq!(cache.order.front().unwrap(), &survivor);
        assert_eq!(cache.order.back().unwrap(), &newest);
    }

    #[test]
    fn a_failed_manifest_load_caches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        write(&settings.manifest, "{not json");
        assert!(manifest(&settings.manifest).is_err());
        write(&settings.manifest, manifest_json());
        std::fs::File::options()
            .write(true)
            .open(&settings.manifest)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        assert!(manifest(&settings.manifest).is_ok());
        assert!(manifest(&settings.manifest).is_ok());
    }

    #[test]
    fn settings_reject_unknown_fields() {
        let err =
            serde_json::from_str::<Settings>(r#"{"manifest":"m","outputBase":"o","bogus":true}"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("bogus"), "{err}");
    }

    /// A FIFO at the manifest path is a prompt error, never a stall —
    /// the open is nonblocking and the handle check refuses the
    /// non-regular file before a byte is read.
    #[test]
    #[cfg(unix)]
    fn manifest_path_fifo_errors_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        std::fs::remove_file(&settings.manifest).unwrap();
        {
            use std::os::unix::ffi::OsStrExt;
            let c = std::ffi::CString::new(settings.manifest.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        }
        assert!(manifest(&settings.manifest).is_err());
    }

    #[test]
    fn settings_defaults_fill_the_optional_fields() {
        let settings: Settings =
            serde_json::from_str(r#"{"manifest":"m","outputBase":"o"}"#).unwrap();
        assert_eq!(settings.base_path, "/paste");
        assert_eq!(settings.inline_threshold, 1024);
        assert_eq!(settings.cdn_base, "");
        assert_eq!(settings.output_js, None);
        let dirs = settings.output_dirs();
        assert_eq!(dirs.js, PathBuf::from("o/js"));
        assert_eq!(dirs.css, PathBuf::from("o/css"));
    }
}
