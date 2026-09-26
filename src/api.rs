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
}

/// Bounded manifest cache: `entries` holds the values, `order` remembers
/// first-seen insertion order for eviction. A hit only clones the `Arc`
/// and never touches `order`; eviction happens only when a NEW key lands
/// at the limit.
struct ManifestCache {
    entries: HashMap<PathBuf, (Stamp, Arc<Manifest>)>,
    order: VecDeque<PathBuf>,
}

impl ManifestCache {
    fn new() -> Self {
        ManifestCache {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &Path, stamp: &Stamp) -> Option<Arc<Manifest>> {
        match self.entries.get(key) {
            Some((cached, manifest)) if cached == stamp => Some(manifest.clone()),
            _ => None,
        }
    }

    fn insert(&mut self, key: PathBuf, stamp: Stamp, manifest: Arc<Manifest>) {
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
        self.entries.insert(key, (stamp, manifest));
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
    manifest_with_cache(path, Path::new(""), cache())
}

/// `path` relative to `root` (the caller's cwd for `manifest`), served
/// from `cache`. Split out so tests can exercise a private cache and a
/// resolution root without moving the process cwd.
fn manifest_with_cache(
    path: &Path,
    root: &Path,
    cache: &RwLock<ManifestCache>,
) -> Result<Arc<Manifest>> {
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
    let file = std::fs::File::open(&key)?;
    manifest_from_open_file(key, file, cache)
}

#[cfg(unix)]
fn manifest_from_open_file(
    key: PathBuf,
    file: std::fs::File,
    cache: &RwLock<ManifestCache>,
) -> Result<Arc<Manifest>> {
    let stamp = Stamp::of(&file.metadata()?)?;
    {
        let guard = cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some(manifest) = guard.get(&key, &stamp) {
            return Ok(manifest);
        }
    }
    let manifest = Manifest::from_json_reader(&file)?;
    // The bytes just parsed must still be the bytes the stamp described.
    if Stamp::of(&file.metadata()?)? != stamp {
        anyhow::bail!("manifest {} changed while it was being read", key.display());
    }
    let manifest = Arc::new(manifest);
    let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
    guard.insert(key, stamp, manifest.clone());
    Ok(manifest)
}

#[cfg(not(unix))]
fn manifest_from_open_file(
    key: PathBuf,
    mut file: std::fs::File,
    cache: &RwLock<ManifestCache>,
) -> Result<Arc<Manifest>> {
    use std::io::Read;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;
    let stamp = Stamp {
        modified: file.metadata()?.modified()?,
        len: file.metadata()?.len(),
        content,
    };
    {
        let guard = cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some(manifest) = guard.get(&key, &stamp) {
            return Ok(manifest);
        }
    }
    let manifest = Arc::new(Manifest::from_json_reader(stamp.content.as_slice())?);
    let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
    guard.insert(key, stamp, manifest.clone());
    Ok(manifest)
}

/// Render the event list and return the last dump's HTML — the same bytes
/// the `render` command prints. An event list ending in `"finish"` is a
/// completion check instead: it validates that every jammed module was
/// dumped and returns an empty string, never markup.
pub fn render(settings: &Settings, events_json: &str) -> Result<String> {
    let manifest = manifest(&settings.manifest)?;
    let request: RenderInput = serde_json::from_str(events_json)?;
    if matches!(request.events.last(), Some(jam::Event::Finish)) {
        jam::validate_finished(&manifest, &request.events)?;
        return Ok(String::new());
    }
    let options = jam::RenderOptions {
        cdn_base: settings.cdn_base.clone(),
        base_path: settings.base_path.clone(),
        inline_threshold: settings.inline_threshold,
        // API-rendered tags are never materialized as files; one URL
        // component per combo regardless of length.
        url_component_limit: None,
    };
    let dirs = settings.output_dirs();
    let mut rendered = jam::render_all(&manifest, &dirs, &options, &request.events)?;
    Ok(rendered.pop().map(|r| r.html).unwrap_or_default())
}

/// Resolve a combo `path?query` string to its response — the same value
/// the `combo` command prints as JSON.
pub fn combo(settings: &Settings, path_and_query: &str) -> Result<jam::ComboResponse> {
    let manifest = manifest(&settings.manifest)?;
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    Ok(jam::combo(
        &manifest,
        &settings.output_dirs(),
        &settings.base_path,
        path,
        query,
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
                    "hash": "h",
                    "version": 1,
                    "timestamp": "2026-01-01T00:00:00Z",
                    "dependencies": [],
                    "history": []
                },
                "js/app.js": {
                    "source": "src/app.js",
                    "output": "/js/app.h.v1.min.js",
                    "hash": "h",
                    "version": 1,
                    "timestamp": "2026-01-01T00:00:00Z",
                    "dependencies": [],
                    "history": []
                },
                "css/site.css": {
                    "source": "src/site.css",
                    "output": "/css/site.h.v1.min.css",
                    "hash": "h",
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
            url_component_limit: None,
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
    fn manifest_path_aliases_share_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let settings = fixture(dir.path());
        let cache = RwLock::new(ManifestCache::new());
        // A relative spelling resolved against `root` and the absolute
        // path canonicalize to the same key.
        let relative = manifest_with_cache(Path::new("manifest.json"), dir.path(), &cache).unwrap();
        let absolute = manifest_with_cache(&settings.manifest, Path::new(""), &cache).unwrap();
        assert!(Arc::ptr_eq(&relative, &absolute));
        // A dotted spelling of the same file too.
        let dotted =
            manifest_with_cache(&dir.path().join("./manifest.json"), Path::new(""), &cache)
                .unwrap();
        assert!(Arc::ptr_eq(&relative, &dotted));
        assert_eq!(cache.read().unwrap().entries.len(), 1);
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
