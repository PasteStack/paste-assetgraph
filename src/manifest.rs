use anyhow::Result;
use bincode::Options;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

/// Single version of an asset (for rollback history)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetVersion {
    pub hash: String,
    pub version: u32,
    pub output: String,
    pub timestamp: String,
}

/// Single asset entry with version history
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetEntry {
    pub source: String,
    pub output: String,
    pub hash: String,
    pub version: u32,
    pub timestamp: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub history: Vec<AssetVersion>,
    /// True when the source was deleted and the entry survives only to serve
    /// exact pinned historical URLs; absent in older manifests means active.
    /// The field is always written — the bincode cache is positional, so an
    /// omitted field would shift the layout. Retired entries never satisfy
    /// unpinned lookups and never appear in a rendered group.
    #[serde(default)]
    pub retired: bool,
}

/// Inputs to [`Manifest::update_asset_with_history`].
pub struct AssetUpdate<'a> {
    /// Manifest asset key (e.g. "js/app.js")
    pub key: &'a str,
    /// Source label recorded on the entry
    pub source: &'a str,
    /// Content hash; an unchanged hash keeps the version and history
    pub hash: &'a str,
    /// Output path prefix (e.g. "/js/app")
    pub output_base: &'a str,
    /// File extension ("js" or "css")
    pub ext: &'a str,
    /// Declared module dependencies
    pub dependencies: Vec<String>,
    /// How many old versions to retain (the build passes
    /// storage.keep_versions; usize::MAX retains every version)
    pub max_history: usize,
    /// Stamp for a new version and for backfilling a carried-forward entry
    /// that has none — the build computes it from the source file's mtime,
    /// clamped to SOURCE_DATE_EPOCH when set
    pub stamp: DateTime<Utc>,
    /// The logical module name this build binds to the entry, when the
    /// source declared or derived one. A name has its own version identity:
    /// a pin `name+vN` issued under a different asset must keep meaning that
    /// asset, so when this claim lands on a version slot the name already
    /// recorded for other bytes the entry takes a version past every
    /// recorded one instead of aliasing the old pin.
    pub module_name: Option<&'a str>,
}

/// A content hash suitable for a URL qualifier: the full 32-hex MD5 the
/// build records — either case is accepted on a request; only the emitted
/// canonical form is lowercase. Never shortened — a prefix would alias.
pub(crate) fn is_content_hash(hash: &str) -> bool {
    hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The version token one module carries in a combo list: `+vN.0` in the
/// legacy form, `+vN.0-h<32 lowercase hex>` when qualified. A qualified
/// token needs a real recorded content hash — anything else is a manifest
/// error, never an unqualified fallback.
pub(crate) fn version_suffix(version: u32, hash: &str, qualified: bool) -> Result<String> {
    if qualified {
        anyhow::ensure!(
            is_content_hash(hash),
            "asset has no valid content hash — rebuild the manifest"
        );
        Ok(format!("+v{version}.0-h{}", hash.to_ascii_lowercase()))
    } else {
        Ok(format!("+v{version}.0"))
    }
}

/// The epoch seconds a RFC3339 stamp carries, or `None` when the field is
/// empty or unparseable — URL segments compare at second precision.
pub(crate) fn stamp_epoch(timestamp: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|t| t.timestamp())
}

/// A module's asset resolved at an exact version: the current entry or a
/// retained history entry.
pub(crate) struct ResolvedVersion<'a> {
    /// The version that resolved (a bare name resolves to the current one)
    pub version: u32,
    /// That version's recorded output path
    pub output: &'a str,
    /// That version's recorded content hash — the field a hash-qualified
    /// URL token is checked against
    pub hash: &'a str,
    /// That version's recorded stamp — history entries carry their own
    pub timestamp: &'a str,
}

/// Sorted dependency entry: (module_name, output_path, version)
pub type SortedDepEntry = (String, String, u32);

/// Sort every object's keys, recursively.
///
/// A manifest is written from HashMaps, and serde_json's map preserves
/// insertion order in this dependency tree, so the same sources would otherwise
/// produce different bytes on every run. Array order is the build's own and is
/// made deterministic where it is produced. test_saved_json_keys_are_sorted
/// covers this.
fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let entries = std::mem::take(object);
            let mut sorted = std::collections::BTreeMap::new();
            for (key, mut child) in entries {
                canonicalize_json(&mut child);
                sorted.insert(key, child);
            }
            *object = sorted.into_iter().collect();
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(canonicalize_json),
        _ => {}
    }
}

/// Asset manifest with hash-versioned paths
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub generated: String,
    /// Module name to file path mapping (from @module comments)
    #[serde(default)]
    pub modules: HashMap<String, String>,
    pub assets: HashMap<String, AssetEntry>,
    /// Pre-computed topologically sorted dependencies by content type
    /// Key: "js" or "css", Value: list of (module_name, output_path, version)
    #[serde(default)]
    pub sorted_deps: HashMap<String, Vec<SortedDepEntry>>,
    /// Project-relative directories this build may generate into — the
    /// configured output roots and the parent directories of recorded
    /// outputs, carried forward across layout and build-directory changes.
    /// Forward slashes only; the scanner excludes them so generated files
    /// can never re-enter discovery as sources, and an output-root move can
    /// find retained files in a finite set of manifest-owned locations.
    /// Always written — the bincode cache is positional, so an omitted field
    /// would shift the layout.
    #[serde(default)]
    pub generated_dirs: Vec<String>,
    /// Whether the build's rendered versioned URLs carry the content-hash
    /// qualifier (`+vN.0-h<32 hex>`). Always written for the same positional
    /// reason; absent in an old manifest means `false`, so pre-existing JSON
    /// keeps serving and emitting legacy `+vN.0` URLs until a build rewrites
    /// it — the manifest carries the URL policy so adapters need no setting.
    #[serde(default)]
    pub hash_urls: bool,
    /// Directories this and earlier builds recorded as type output roots:
    /// `"js"`/`"css"` → project-relative labels in `generated_dirs` form,
    /// accumulated so a root survives the config move that replaced it.
    /// This is the only provenance a later build accepts for removing a
    /// legacy `assetgraph.{type}` at a historical location — files found
    /// elsewhere are reported, never inferred into deletion targets.
    /// Always written for the same positional reason; an old manifest
    /// without it simply establishes no historical roots.
    #[serde(default)]
    pub output_roots: BTreeMap<String, Vec<String>>,
    /// Loader-URL history: each packages fingerprint a build has emitted →
    /// the pin each JS module name resolved at then. A saved page's runtime
    /// keeps fetching `{segment}/{module}.js` long after later builds moved
    /// the set on; this is the only record those requests can resolve their
    /// original bytes through. A segment whose pins retention has pruned is
    /// dropped with them — the record must not outlive what it can serve.
    /// Always written for the same positional reason; a manifest that
    /// predates it knows no historical fingerprints, so those URLs answer
    /// not-found rather than substituting current content.
    #[serde(default)]
    pub packages_segments: BTreeMap<String, BTreeMap<String, SegmentPin>>,
    /// The build's default URL prefix — `cdn.url` when `cdn.enabled` was
    /// set — carried by the manifest so emitted tags need no per-host
    /// configuration. A runtime's own `cdnBase` always overrides it; `None`
    /// means origin-relative URLs. The JSON omits the key entirely when
    /// absent (`canonical_json` strips it after serialization); the
    /// bincode cache is positional and keeps the field in place.
    #[serde(default)]
    pub cdn_base: Option<String>,
    /// Retained logical-name bindings: `"{type}:{name}"` → version → the
    /// pin that name resolved at while that version was current — asset
    /// key plus recorded content hash. This is the historical half of name
    /// resolution: `modules` says what a name means NOW; this map says what
    /// `name+vN` meant when it was issued, so a renamed or retired module
    /// keeps serving its saved pins without the old spelling staying a
    /// live alias — and a reused name can never alias an old pin to new
    /// bytes. A binding is dropped when the version it names is evicted.
    /// Always written for the positional bincode layout; a manifest that
    /// predates the field loads it empty and the next build seeds it from
    /// the state the older resolution could serve.
    #[serde(default)]
    pub historical_names: BTreeMap<String, BTreeMap<u32, SegmentPin>>,
    /// `"{type}:{name}:{version}"` slots a pre-pin manifest could not
    /// attribute to one identity: seeding found two different assets
    /// claiming the same name and version with different bytes, so the
    /// recorded pin cannot say which one `name+vN` meant. Resolution of
    /// that exact slot fails rather than guesses — a disputed pin is
    /// worse than no pin. Markers are never pruned: they are what keeps
    /// the slot from falling through to the name's current binding once
    /// its disputed record is evicted. Always written; absent in a
    /// manifest that predates the field.
    #[serde(default)]
    pub ambiguous_name_pins: BTreeSet<String>,
}

/// One recorded loader pin: the module name as the page's URL spelled it →
/// the asset key, version, and content hash it resolved to at that build.
/// The key rides along because the name→key mapping can retire or be
/// renamed after the record — only the pin remembers which asset the
/// page's name meant; the hash pins which bytes: a same-key, same-version
/// rebuild under different content cannot satisfy it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentPin {
    pub key: String,
    pub version: u32,
    /// Content hash of the pinned version. Records written before the pin
    /// carried an identity load with an empty hash, which verifies against
    /// nothing — the pin cannot serve and is pruned.
    #[serde(default)]
    pub hash: String,
}

/// Fixed header identifying the private bincode cache schema — bumped when
/// the serialized shape changes, so a cache written by a build with a
/// different layout never decodes into this one. An internal cache marker,
/// not a package or release version. PASTEC07: `SegmentPin` gains a
/// content hash. PASTEC08: `cdn_base` — the build's default URL origin —
/// is recorded on the manifest. PASTEC09: `historical_names` — retained
/// logical-name pins — join the manifest. PASTEC10:
/// `ambiguous_name_pins` — name+version slots seeding could not
/// attribute to one identity, recorded so they fail rather than guess.
/// The cache is disposable metadata, so a stale marker simply falls back
/// to the authoritative JSON.
const MANIFEST_CACHE_FORMAT: [u8; 8] = *b"PASTEC10";

/// The `.bin` beside a manifest: a deserialization cache for `Manifest`
/// bound to the fixed format header and the md5 digest of the exact JSON
/// bytes it was written from. `load` accepts the cache only when both
/// match; a cache written by another schema or for other JSON bytes is a
/// miss and the JSON is parsed instead.
#[derive(Serialize, Deserialize)]
struct ManifestCache {
    format: [u8; 8],
    json_hash: [u8; 16],
    manifest: Manifest,
}

/// Refuse a manifest destination whose final component is a symlink or a
/// non-regular file. Reads follow a manifest symlink, but the publication
/// rename replaces the link itself — the old target keeps stale bytes
/// while the configured path silently becomes an independent file — and a
/// dangling link reads as absent, which a build would mistake for a first
/// build and reset the history on. An absent entry is a legal first build;
/// an ordinary regular file is the normal case.
pub(crate) fn validate_writable_manifest_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => anyhow::bail!(
            "manifest path {} is a symlink — publishing would replace the link itself, not its target; configure a regular manifest file",
            path.display()
        ),
        Ok(meta) if !meta.is_file() => anyhow::bail!(
            "manifest path {} is not a regular file — refusing to publish over it",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

impl Manifest {
    /// Create new empty manifest
    pub fn new() -> Self {
        let now: DateTime<Utc> = Utc::now();
        Manifest {
            version: now.format("%Y.%m.%d.%H%M").to_string(),
            generated: now.to_rfc3339(),
            modules: HashMap::new(),
            assets: HashMap::new(),
            sorted_deps: HashMap::new(),
            generated_dirs: Vec::new(),
            hash_urls: true,
            output_roots: BTreeMap::new(),
            packages_segments: BTreeMap::new(),
            cdn_base: None,
            historical_names: BTreeMap::new(),
            ambiguous_name_pins: BTreeSet::new(),
        }
    }

    /// Load manifest from file (JSON or bincode)
    ///
    /// The JSON manifest is authoritative. The `.bin` beside it is only a
    /// cache of its exact bytes, bound by content hash rather than mtime:
    /// a checkout that refreshes the JSON without moving its timestamp must
    /// still win, and a cache whose recorded JSON hash differs from the
    /// bytes on disk is ignored. Without a JSON file there is nothing to
    /// bind a cache to — the manifest starts empty.
    pub fn load(path: &Path) -> Result<Self> {
        let json = match Self::read_bounded(path, Self::MANIFEST_READ_CAP) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(e.into()),
        };
        let json_hash = md5::compute(&json).0;

        // A cache is only as good as the JSON bytes it was written from:
        // an undecodable one is deleted so we don't retry it every load,
        // while a decodable one whose hash no longer matches stays on disk
        // — the JSON wins until a build rewrites it.
        let bin_path = path.with_extension("bin");
        if let Ok(file) = Self::open_cache_file(&bin_path) {
            // Cap deserialization at 256 MB to prevent OOM on corrupt data.
            // Without a limit, garbage length prefixes can cause multi-exabyte
            // allocation attempts that abort the process before returning Err.
            let opts = bincode::options().with_limit(256 * 1024 * 1024);
            match opts.deserialize_from::<_, ManifestCache>(BufReader::new(file)) {
                Ok(cache)
                    if cache.format == MANIFEST_CACHE_FORMAT && cache.json_hash == json_hash =>
                {
                    let mut manifest = cache.manifest;
                    manifest.sanitize_strings();
                    return Ok(manifest);
                }
                // Stale cache — a JSON replaced since it was written, or an
                // old-format cache (a bare Manifest never decodes as a
                // ManifestCache). It stays on disk; the JSON wins until a
                // build rewrites it. Bincode that failed to decode at all
                // (e.g. corruption) is deleted so we don't retry it.
                Ok(_) => {}
                Err(_) => {
                    // The unlink is anchored to the bin's parent opened
                    // now — a swapped-in link between the read and the
                    // removal cannot redirect it into a foreign tree.
                    let parent = crate::dirfd::parent_dir(&bin_path);
                    if let (Ok(dir), Some(name)) =
                        (crate::dirfd::OwnedDir::open(parent), bin_path.file_name())
                    {
                        let _ = dir.remove_file(name);
                    }
                }
            }
        }

        Self::from_json_reader(json.as_slice())
    }

    /// Load manifest from its JSON form only, with the same bounded read
    /// every other loader applies — no cache, no side effects.
    pub fn load_json(path: &Path) -> Result<Self> {
        let file = Self::open_manifest_file(path)?;
        let json = Self::read_bounded_file(&file, path, Self::MANIFEST_READ_CAP)?;
        Self::from_json_reader(json.as_slice())
    }

    /// Words for a refused anchor: not every Unix reports a link refusal
    /// as ELOOP (Darwin's `O_NOFOLLOW | O_DIRECTORY` open answers
    /// ENOTDIR), so the component's own metadata breaks the tie — the
    /// operation already failed closed; this is diagnostic only.
    fn describe_unsafe_parent(path: &Path, parent: &Path, e: std::io::Error) -> std::io::Error {
        if !crate::dirfd::is_unsafe_component(&e) {
            return e;
        }
        let through = if fs::symlink_metadata(parent)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            "a symlink"
        } else {
            "a non-directory"
        };
        std::io::Error::new(
            e.kind(),
            format!("{} resolves through {}", path.display(), through),
        )
    }

    /// The largest manifest read that can be legitimate — a substituted
    /// oversized file is rejected before it can grow a matching
    /// allocation. Shared with the runtime loader's read paths.
    pub(crate) const MANIFEST_READ_CAP: u64 = 256 * 1024 * 1024;

    /// Open `path` for reading through its pinned parent — nonblocking,
    /// following a trailing link (a deployed manifest may legitimately be
    /// one), and verified regular on the opened handle before a byte is
    /// read. A FIFO or device substituted for the manifest can neither
    /// stall the open nor pass the handle check.
    pub(crate) fn open_manifest_file(path: &Path) -> std::io::Result<File> {
        let parent = crate::dirfd::parent_dir(path);
        let dir = crate::dirfd::OwnedDir::open(parent)
            .map_err(|e| Self::describe_unsafe_parent(path, parent, e))?;
        let file = dir.open_file_follow(path.file_name().unwrap_or_default())?;
        if !crate::dirfd::OwnedDir::opened_file_is_regular(&file)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(file)
    }

    /// `path` read to completion under the manifest cap.
    fn read_bounded(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
        let file = Self::open_manifest_file(path)?;
        Self::read_bounded_file(&file, path, cap)
    }

    /// An already-open manifest file read to completion under `cap` —
    /// for callers that opened it themselves so the stamp and the bytes
    /// come from the same handle.
    pub(crate) fn read_bounded_file(
        file: &File,
        path: &Path,
        cap: u64,
    ) -> std::io::Result<Vec<u8>> {
        let mut limited = std::io::Read::take(file, cap + 1);
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut limited, &mut bytes)?;
        if bytes.len() as u64 > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} exceeds the {} byte manifest read limit",
                    path.display(),
                    cap
                ),
            ));
        }
        Ok(bytes)
    }

    /// The optional `.bin` cache opened the same way, except a trailing
    /// link is refused outright — a cache is disposable local state and a
    /// link-shaped one is simply a miss.
    pub(crate) fn open_cache_file(path: &Path) -> std::io::Result<File> {
        let parent = crate::dirfd::parent_dir(path);
        let dir = crate::dirfd::OwnedDir::open(parent)
            .map_err(|e| Self::describe_unsafe_parent(path, parent, e))?;
        let file = dir.open_file(path.file_name().unwrap_or_default())?;
        if !crate::dirfd::OwnedDir::opened_file_is_regular(&file)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(file)
    }

    /// Parse a manifest from any JSON source. The request-path cache parses
    /// through this too, so a file handle and a byte slice get the same
    /// sanitization either way.
    pub fn from_json_reader(reader: impl std::io::Read) -> Result<Self> {
        let mut manifest: Self = serde_json::from_reader(BufReader::new(reader))?;
        manifest.sanitize_strings();
        Ok(manifest)
    }

    /// Strip null bytes from all string fields.
    /// Corrupt bincode deserialization can introduce null-byte padding.
    fn sanitize_strings(&mut self) {
        fn clean(s: &str) -> String {
            s.chars().filter(|c| *c != '\0').collect()
        }
        self.version = clean(&self.version);
        self.generated = clean(&self.generated);
        // Clean modules map keys and values
        self.modules = self
            .modules
            .drain()
            .map(|(k, v)| (clean(&k), clean(&v)))
            .collect();
        // Clean sorted_deps: drain keys (null-padded keys break lookups),
        // then clean tuple values (module_name, output_path, version)
        self.sorted_deps = self
            .sorted_deps
            .drain()
            .map(|(k, mut deps)| {
                let clean_key = clean(&k);
                for entry in deps.iter_mut() {
                    entry.0 = clean(&entry.0);
                    entry.1 = clean(&entry.1);
                }
                (clean_key, deps)
            })
            .collect();
        // Clean asset entries: drain keys (null-padded keys break lookups),
        // clean all value fields including dependencies, history, and history output
        self.assets = self
            .assets
            .drain()
            .map(|(k, mut entry)| {
                let clean_key = clean(&k);
                entry.source = clean(&entry.source);
                entry.output = clean(&entry.output);
                entry.hash = clean(&entry.hash);
                entry.timestamp = clean(&entry.timestamp);
                for dep in entry.dependencies.iter_mut() {
                    *dep = clean(dep);
                }
                for hist in entry.history.iter_mut() {
                    hist.hash = clean(&hist.hash);
                    hist.output = clean(&hist.output);
                    hist.timestamp = clean(&hist.timestamp);
                }
                (clean_key, entry)
            })
            .collect();
        self.generated_dirs = self
            .generated_dirs
            .drain(..)
            .map(|dir| clean(&dir))
            .collect();
        fn clean_pin(pin: &mut SegmentPin) {
            pin.key = pin.key.chars().filter(|c| *c != '\0').collect();
            pin.hash = pin.hash.chars().filter(|c| *c != '\0').collect();
        }
        self.output_roots = std::mem::take(&mut self.output_roots)
            .into_iter()
            .map(|(k, mut roots)| {
                for root in roots.iter_mut() {
                    *root = clean(root);
                }
                (clean(&k), roots)
            })
            .collect();
        self.packages_segments = std::mem::take(&mut self.packages_segments)
            .into_iter()
            .map(|(k, segments)| {
                let segments = segments
                    .into_iter()
                    .map(|(name, mut pin)| {
                        clean_pin(&mut pin);
                        (clean(&name), pin)
                    })
                    .collect();
                (clean(&k), segments)
            })
            .collect();
        if let Some(base) = self.cdn_base.as_mut() {
            *base = clean(base);
        }
        self.historical_names = std::mem::take(&mut self.historical_names)
            .into_iter()
            .map(|(name, mut pins)| {
                for pin in pins.values_mut() {
                    clean_pin(pin);
                }
                (clean(&name), pins)
            })
            .collect();
        self.ambiguous_name_pins = std::mem::take(&mut self.ambiguous_name_pins)
            .into_iter()
            .map(|name| clean(&name))
            .collect();
    }

    /// Resolve a declared module name to the key of its asset of `asset_type`.
    ///
    /// The modules map is authoritative when it names an existing asset of that
    /// type — a stylesheet may declare a module name unrelated to its path. It
    /// is ignored when it does not, which is what lets a manifest written by an
    /// older build (whose modules map held source paths) still resolve through
    /// the convention key.
    pub fn asset_key_for_module(&self, module: &str, asset_type: &str) -> Option<String> {
        self.asset_key(module, asset_type, false)
    }

    /// The lookup behind [`asset_key_for_module`]: with `include_retired`,
    /// retired assets resolve too — the pinned-version path uses it, while
    /// every unpinned lookup (rendering, loader URLs) goes through
    /// the public wrapper and sees only the active set. The modules map may
    /// carry a retired alias; this filter is what keeps it out of new work.
    fn asset_key(&self, module: &str, asset_type: &str, include_retired: bool) -> Option<String> {
        let usable = |key: &String| {
            self.assets
                .get(key.as_str())
                .is_some_and(|entry| include_retired || !entry.retired)
        };
        let suffix = format!(".{}", asset_type);
        let declared = self
            .modules
            .get(module)
            .filter(|key| key.ends_with(&suffix) && usable(key))
            .cloned();
        declared.or_else(|| {
            let key = format!("{}/{}.{}", asset_type, module.replace('.', "/"), asset_type);
            usable(&key).then_some(key)
        })
    }

    /// Resolve a module's `asset_type` entry at `version`: the current entry
    /// for `None` or the current version, a retained history version's record
    /// for an older pin, else `None` when the module or version does not
    /// resolve.
    ///
    /// An exact pin resolves through `historical_names` — the record of
    /// which asset+hash the name carried when that version was issued —
    /// never through the name's current binding: a renamed module's old
    /// spelling still serves its retained versions, while a name since
    /// claimed by different bytes cannot alias the old pin. A recorded
    /// pin that fails to verify (evicted version, mismatched hash)
    /// resolves to nothing rather than substituting another identity.
    /// The legacy path remains for a manifest that predates the map.
    pub(crate) fn resolve_version(
        &self,
        module: &str,
        asset_type: &str,
        version: Option<u32>,
    ) -> Option<ResolvedVersion<'_>> {
        if let Some(v) = version {
            // A slot seeding could not attribute to one identity fails
            // rather than guesses — never let it fall through to the
            // name's current binding.
            if self
                .ambiguous_name_pins
                .contains(&format!("{asset_type}:{module}:{v}"))
            {
                return None;
            }
            if let Some(pins) = self.historical_names.get(&format!("{asset_type}:{module}")) {
                let pin = pins.get(&v)?;
                return self
                    .resolve_entry(&pin.key, Some(v))
                    .filter(|resolved| resolved.hash == pin.hash);
            }
        }
        // A pin names an exact version — a retired asset may still serve it.
        // An unpinned lookup sees the active set only, so a retired module
        // cannot enter a fresh page or loader request.
        let key = self.asset_key(module, asset_type, version.is_some())?;
        self.resolve_entry(&key, version)
    }

    /// Resolve a known asset key at `version`: the pinned form a recorded
    /// packages segment carries. The key is stored with the pin because
    /// the module→key mapping may have retired or been renamed since —
    /// a saved page asks for the name its build knew, and only the record
    /// remembers which asset that name compiled to.
    pub(crate) fn resolve_key_version(
        &self,
        key: &str,
        version: u32,
    ) -> Option<ResolvedVersion<'_>> {
        self.resolve_entry(key, Some(version))
    }

    fn resolve_entry(&self, key: &str, version: Option<u32>) -> Option<ResolvedVersion<'_>> {
        let entry = self.assets.get(key)?;
        match version {
            Some(v) if v != entry.version => {
                entry
                    .history
                    .iter()
                    .find(|h| h.version == v)
                    .map(|h| ResolvedVersion {
                        version: h.version,
                        output: &h.output,
                        hash: &h.hash,
                        timestamp: &h.timestamp,
                    })
            }
            _ => Some(ResolvedVersion {
                version: entry.version,
                output: &entry.output,
                hash: &entry.hash,
                timestamp: &entry.timestamp,
            }),
        }
    }

    /// The module name a key derives by convention — `js/a/b.js` ↔ `a.b`,
    /// `css/site/theme.css` ↔ `site.theme` — injective because a module
    /// name cannot carry a path separator. None outside a type prefix.
    fn conventional_name(key: &str) -> Option<String> {
        let (kind, rest) = key.split_once('/')?;
        let stem = rest.strip_suffix(&format!(".{kind}"))?;
        Some(stem.replace('/', "."))
    }

    /// Record the pin every resolvable module name carries right now: for
    /// each type, every name an unpinned lookup resolves — declared
    /// spellings from `modules` plus each active key's conventional name —
    /// gets `{type}:{name}` → `{key, version, hash}` under the entry's
    /// current version. This is the record a later `name+vN` request
    /// answers from when the name has since moved on. A slot already
    /// recorded for different bytes is a build error — the version floor
    /// in `update_asset_with_history` exists to prevent it, so landing on
    /// one anyway means the identity guarantee was broken upstream.
    pub(crate) fn record_name_pins(&mut self) -> Result<()> {
        let mut names: BTreeSet<String> = self.modules.keys().cloned().collect();
        for key in self.assets.keys() {
            if let Some(name) = Self::conventional_name(key) {
                names.insert(name);
            }
        }
        for name in names {
            for asset_type in ["js", "css"] {
                let Some(key) = self.asset_key_for_module(&name, asset_type) else {
                    continue;
                };
                let entry = &self.assets[&key];
                let bucket = self
                    .historical_names
                    .entry(format!("{asset_type}:{name}"))
                    .or_default();
                let pin = SegmentPin {
                    key: key.clone(),
                    version: entry.version,
                    hash: entry.hash.clone(),
                };
                if let Some(recorded) = bucket.get(&entry.version) {
                    // A name this key only answers to by its path-derived
                    // convention was never issued as this version's pin; when
                    // another identity already holds the slot (a deleted file
                    // that declared this name), that record stands.
                    let declared_here =
                        self.modules.get(&name).map(String::as_str) == Some(key.as_str());
                    if !declared_here && (recorded.key != pin.key || recorded.hash != pin.hash) {
                        continue;
                    }
                    if recorded.key != pin.key || recorded.hash != pin.hash {
                        anyhow::bail!(
                            "module {name} version {} already pins {}@{} — cannot record {}@{}",
                            entry.version,
                            recorded.key,
                            recorded.hash,
                            pin.key,
                            pin.hash
                        );
                    }
                    // The slot already records this exact key+hash — the
                    // identity is unchanged, keep the issued record. A
                    // different key claiming the slot is a bail above:
                    // the version floor in claim_version moves the name's
                    // rebind past it before this recording runs.
                } else {
                    bucket.insert(entry.version, pin);
                    // A slot this build just issued is no longer
                    // unattributable — the pin is the identity record.
                    self.ambiguous_name_pins
                        .remove(&format!("{asset_type}:{name}:{}", entry.version));
                }
            }
        }
        Ok(())
    }

    /// Drop every recorded name pin that can no longer serve verified
    /// bytes — its version evicted from the asset or the recorded hash no
    /// longer matching — and every bucket left empty. Runs after the
    /// build's retention pass so a record never outlives the file it names.
    pub(crate) fn prune_name_pins(&mut self) {
        let dead: Vec<(String, u32)> = self
            .historical_names
            .iter()
            .flat_map(|(name, pins)| {
                pins.iter()
                    .filter_map(|(version, pin)| {
                        let alive = self
                            .resolve_entry(&pin.key, Some(*version))
                            .is_some_and(|resolved| resolved.hash == pin.hash);
                        (!alive).then(|| (name.clone(), *version))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        for (name, version) in dead {
            if let Some(pins) = self.historical_names.get_mut(&name) {
                pins.remove(&version);
                if pins.is_empty() {
                    self.historical_names.remove(&name);
                }
            }
        }
    }

    /// Populate `historical_names` with every binding the loaded state
    /// could still resolve a pin through — each modules-map entry and
    /// each key's conventional name, recorded for every version of the
    /// key the old name→key→version lookup could serve, retired entries
    /// included. Runs at build start and merges (an existing record
    /// wins): a manifest written before the field existed gets its whole
    /// resolvable history recorded, and a partially recorded one fills
    /// its gaps.
    pub(crate) fn seed_historical_names(&mut self) {
        // Claims are visited in sorted order — declared names first, then
        // conventional ones — so which claimant seeds a contested slot never
        // depends on HashMap iteration (the committed-bytes contract).
        let mut claims: Vec<(String, String)> = self
            .modules
            .iter()
            .map(|(name, key)| (name.clone(), key.clone()))
            .collect();
        claims.sort();
        let mut keys: Vec<&String> = self.assets.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(name) = Self::conventional_name(key) {
                claims.push((name, key.clone()));
            }
        }
        for (name, key) in claims {
            let Some((kind, _)) = key.split_once('/') else {
                continue;
            };
            if !matches!(kind, "js" | "css") {
                continue;
            }
            let Some(entry) = self.assets.get(&key) else {
                continue;
            };
            let bucket = self
                .historical_names
                .entry(format!("{kind}:{name}"))
                .or_default();
            let ambiguous = &mut self.ambiguous_name_pins;
            let mut record = |version: u32, hash: &str| {
                match bucket.get(&version) {
                    Some(existing) if existing.hash != hash => {
                        // Two different assets claim this name+version
                        // with different bytes — the pin cannot say
                        // which one `name+vN` meant, so resolution of the
                        // slot must fail rather than guess. The recorded
                        // pin stays: it still anchors the version floor
                        // for any later claimant of the name.
                        ambiguous.insert(format!("{kind}:{name}:{version}"));
                    }
                    Some(_) => {}
                    None => {
                        bucket.insert(
                            version,
                            SegmentPin {
                                key: key.clone(),
                                version,
                                hash: hash.to_string(),
                            },
                        );
                    }
                }
            };
            record(entry.version, &entry.hash);
            for history in &entry.history {
                record(history.version, &history.hash);
            }
        }
    }

    /// The pin every JS module name currently resolves to: declared
    /// aliases from the modules map (a module can live under a
    /// non-conventional key) plus the conventional name every active JS
    /// asset key derives. Declared names win — an alias over a
    /// conventional name is the spelling a page's URL can carry. Each pin
    /// copies the entry's key, version, and content hash so a record
    /// verifies the bytes it serves, not just the name and sequence slot.
    /// Purely a view over current state — recording is
    /// `record_packages_segment`, pruning is `prune_packages_segments`.
    pub(crate) fn loader_pins(&self) -> BTreeMap<String, SegmentPin> {
        let mut pins: BTreeMap<String, SegmentPin> = BTreeMap::new();
        for (name, key) in &self.modules {
            if let Some(entry) = self
                .assets
                .get(key)
                .filter(|entry| key.starts_with("js/") && !entry.retired)
            {
                pins.insert(
                    name.clone(),
                    SegmentPin {
                        key: key.clone(),
                        version: entry.version,
                        hash: entry.hash.clone(),
                    },
                );
            }
        }
        for (key, entry) in &self.assets {
            if !key.starts_with("js/") || entry.retired {
                continue;
            }
            // `js/a/b.js` ↔ `a.b`: the derivation is injective because a
            // module name cannot contain a path separator.
            let Some(name) = key
                .strip_prefix("js/")
                .and_then(|rest| rest.strip_suffix(".js"))
                .map(|rest| rest.replace('/', "."))
            else {
                continue;
            };
            pins.entry(name).or_insert_with(|| SegmentPin {
                key: key.clone(),
                version: entry.version,
                hash: entry.hash.clone(),
            });
        }
        pins
    }

    /// Record this build's packages fingerprint → the pin each JS module
    /// name resolves at, so a saved page's `{segment}/{module}.js` loader
    /// URL keeps naming the bytes it was rendered against after later
    /// builds change the set. A recorded segment is immutable: the same
    /// fingerprint must always name the same pins, so a conflicting map
    /// errors rather than overwriting what saved pages resolve through.
    /// An empty pin set records nothing — no active JS means no loader
    /// URLs were ever minted.
    pub fn record_packages_segment(&mut self, segment: &str) -> Result<()> {
        let pins = self.loader_pins();
        if pins.is_empty() {
            return Ok(());
        }
        if let Some(existing) = self.packages_segments.get(segment) {
            if existing != &pins {
                anyhow::bail!(
                    "packages fingerprint {segment} already records different module pins"
                );
            }
        } else {
            self.packages_segments.insert(segment.to_string(), pins);
        }
        Ok(())
    }

    /// Drop the loader pins that can no longer serve verified bytes —
    /// bounded retention prunes the files, and the record must not outlive
    /// what it resolves to. A pin survives while the asset carries its
    /// version at its hash, current or retained history; a dead pin is
    /// removed individually so the survivors keep serving, and a segment
    /// left with no pins is dropped entirely.
    pub(crate) fn prune_packages_segments(&mut self) {
        let assets = &self.assets;
        self.packages_segments.retain(|_, pins| {
            pins.retain(|_, pin| {
                if !is_content_hash(&pin.hash) {
                    return false;
                }
                assets.get(&pin.key).is_some_and(|entry| {
                    if entry.version == pin.version {
                        entry.hash.eq_ignore_ascii_case(&pin.hash)
                    } else {
                        entry.history.iter().any(|v| {
                            v.version == pin.version && v.hash.eq_ignore_ascii_case(&pin.hash)
                        })
                    }
                })
            });
            !pins.is_empty()
        });
    }

    /// The output path a module's `asset_type` entry records for `version`:
    /// the current output for `None` or the current version, a history
    /// version's own output while it is retained, else `None`.
    pub fn output_for_version(
        &self,
        module: &str,
        asset_type: &str,
        version: Option<u32>,
    ) -> Option<&str> {
        self.resolve_version(module, asset_type, version)
            .map(|resolved| resolved.output)
    }

    /// Normalize metadata for static distribution builds.
    ///
    /// Static output must be reproducible across operating systems and build
    /// times. Content hashes and sorted keys already provide stable asset
    /// identity; the only field that still varies between identical builds is
    /// `generated`, so it is dropped and the manifest version becomes the
    /// fixed label "static". The asset and history timestamps stay: they are
    /// the last-modified stamps the combo URL segment is built from — the
    /// build stamps them from the source mtime (clamped by SOURCE_DATE_EPOCH
    /// when set), so a deterministic job pins that variable to make them
    /// byte-identical too. Source labels are already project-relative.
    pub fn make_deterministic(&mut self) {
        self.version = "static".to_string();
        self.generated.clear();
    }

    /// Validate compiled output pointers without inferring asset type from
    /// directory names: beside-source outputs may live under any source
    /// directory. Current and historical pointers must be safe paths and
    /// carry the extension of their asset key.
    fn validate_output_paths(&self) -> Result<()> {
        let dirs = crate::jam::OutputDirs::under(Path::new("."));
        for (key, entry) in &self.assets {
            let kind = match key.split_once('/').map(|(kind, _)| kind) {
                Some("js") => crate::jam::AssetType::Js,
                Some("css") => crate::jam::AssetType::Css,
                _ => continue,
            };
            for output in
                std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
            {
                anyhow::ensure!(
                    dirs.compiled_file(output, kind).is_some()
                        && Path::new(output)
                            .extension()
                            .is_some_and(|ext| ext == kind.ext()),
                    "manifest asset {key} records invalid compiled {} output path {output:?}",
                    kind.ext()
                );
            }
        }
        Ok(())
    }

    /// The canonical JSON bytes of this manifest: sorted keys, pretty
    /// printed. `save` writes exactly these bytes and binds the bincode
    /// cache to their digest, so `load` accepts a cache only while it still
    /// describes the JSON on disk — a read-time check, not a guarantee
    /// covering builds that write the pair concurrently.
    pub(crate) fn canonical_json(&self) -> Result<Vec<u8>> {
        self.validate_output_paths()?;
        let mut value = serde_json::to_value(self)?;
        canonicalize_json(&mut value);
        // A disabled/absent origin leaves no `cdn_base` key in the JSON —
        // the bincode cache stays positional (the Option always
        // serializes), so the omission happens on this value only.
        if let Some(object) = value.as_object_mut() {
            if object.get("cdn_base").is_some_and(|v| v.is_null()) {
                object.remove("cdn_base");
            }
        }
        let bytes = serde_json::to_string_pretty(&value)?.into_bytes();
        // Publish nothing a reader will refuse: every load path caps the
        // manifest at MANIFEST_READ_CAP, so a larger one would go live and
        // then fail every render, combo, and later build.
        anyhow::ensure!(
            bytes.len() as u64 <= Self::MANIFEST_READ_CAP,
            "manifest would be {} bytes — over the {} byte read limit; bound storage.keep-versions",
            bytes.len(),
            Self::MANIFEST_READ_CAP
        );
        Ok(bytes)
    }

    /// A temp suffix unique to this process and save call — writers that
    /// bypass the build lock can never rename each other's partial tmp
    /// into place.
    fn tmp_tag() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        #[cfg(test)]
        if let Some(next) = test_hooks::TMP_TAG_BASE.with(|c| c.get()) {
            test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(next + 1)));
            return format!("{}.{}", std::process::id(), next);
        }
        format!(
            "{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Open a fresh staging file beside `path` — `create_new` makes the
    /// open itself the ownership claim: a stale residue or a planted
    /// symlink at a reused PID/sequence name is a collision to retry,
    /// never a file to truncate or a link to follow. Only the file this
    /// call created is ever removed, and only on a failure before
    /// publication. The name is bounded under every filesystem's
    /// component limit however long `path` is configured to be.
    fn stage_tmp(path: &Path, kind: &str) -> Result<Staged> {
        /// Collisions are a stale file or another writer — retrying is
        /// correct, but a directory full of planted names must fail
        /// rather than spin forever.
        const MAX_STAGING_ATTEMPTS: usize = 64;
        let dir_path = crate::dirfd::parent_dir(path);
        let dir = crate::dirfd::OwnedDir::open(dir_path)?;
        for _ in 0..MAX_STAGING_ATTEMPTS {
            let name = crate::storage::staged_tmp_component(path, kind, &Self::tmp_tag());
            match dir.create_new(&name) {
                Ok(file) => return Ok(Staged { dir, name, file }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!(
            "could not allocate a staging file for {} — every candidate name was taken",
            path.display()
        )
    }

    /// Write `json` to `path` atomically (stage-temp + rename) so a
    /// partial write can never corrupt a manifest readers already hold.
    /// A tmp left by a crashed earlier save is simply left in place —
    /// the tagged `.paste-staged.<kind>.<pid>.<n>.<digest>` names are unique
    /// per attempt, so
    /// residue is inert, and a save must never unlink a file it cannot
    /// prove it wrote itself.
    fn write_json(path: &Path, json: &[u8]) -> Result<()> {
        // Create parent directories
        fs::create_dir_all(crate::dirfd::parent_dir(path))?;
        let staged = Self::stage_tmp(path, "json")?;
        let mut file = staged.file;
        // Durable before it is visible — the rename is the publication.
        if let Err(e) = file.write_all(json).and_then(|_| file.sync_all()) {
            let _ = staged.dir.remove_file(&staged.name);
            return Err(e.into());
        }
        drop(file);
        // The destination was not re-verified since the caller checked:
        // a link swapped in mid-write must still not be replaced.
        if let Err(e) = validate_writable_manifest_path(path) {
            let _ = staged.dir.remove_file(&staged.name);
            return Err(e);
        }
        let dest = path.file_name().unwrap_or_default();
        if let Err(e) = staged.dir.rename(&staged.name, dest) {
            let _ = staged.dir.remove_file(&staged.name);
            return Err(e.into());
        }
        // The swap already happened: the new manifest is live, so a failed
        // directory flush cannot be reported as a failed publication.
        if let Err(e) = staged.dir.sync() {
            eprintln!(
                "Warning: could not flush the directory of {}: {e}",
                path.display()
            );
        }
        Ok(())
    }

    /// Stage the bincode cache for `json` beside `path`: serialize to a
    /// unique tmp file, flush, and read the bytes back to prove they
    /// round-trip — everything the cache production can fail at happens
    /// here, before the JSON it describes may publish. `commit_cache`
    /// installs the result.
    pub fn prepare_cache(&self, path: &Path, json: &[u8]) -> Result<StagedCache> {
        fs::create_dir_all(crate::dirfd::parent_dir(path))?;
        let mut staged = Self::stage_tmp(path, "bin")?;
        let result = (|| -> Result<()> {
            let mut writer = BufWriter::new(&mut staged.file);
            let cache = ManifestCache {
                format: MANIFEST_CACHE_FORMAT,
                json_hash: md5::compute(json).0,
                manifest: self.clone(),
            };
            let opts = bincode::options().with_limit(256 * 1024 * 1024);
            opts.serialize_into(&mut writer, &cache)?;
            writer.flush()?;
            drop(writer);
            staged.file.sync_all()?;
            let round_trip: ManifestCache = bincode::options()
                .with_limit(256 * 1024 * 1024)
                .deserialize_from(BufReader::new(staged.dir.open_file(&staged.name)?))?;
            anyhow::ensure!(
                round_trip.format == cache.format && round_trip.json_hash == cache.json_hash,
                "staged manifest cache failed its read-back check"
            );
            #[cfg(test)]
            if test_hooks::FAIL_CACHE_STAGE.with(|f| f.replace(false)) {
                anyhow::bail!("injected manifest cache staging failure");
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(StagedCache::armed(staged)),
            Err(e) => {
                let _ = staged.dir.remove_file(&staged.name);
                Err(e)
            }
        }
    }

    /// The publication boundary: the atomic swap that makes `json` the
    /// manifest readers see — the single operation after which a new
    /// generation exists.
    pub fn publish_json(path: &Path, json: &[u8]) -> Result<()> {
        #[cfg(test)]
        if test_hooks::FAIL_PUBLISH.with(|f| f.replace(false)) {
            anyhow::bail!("injected manifest publication failure");
        }
        Self::write_json(path, json)
    }

    /// Install a staged cache beside `path`. Post-publication
    /// maintenance: a failure leaves an absent or stale cache that
    /// `load` detects by digest and bypasses for the authoritative JSON.
    /// The rename is anchored to the descriptor the stage pinned, so a
    /// directory swapped for a link meanwhile redirects nothing.
    pub fn commit_cache(staged: &StagedCache, path: &Path) -> Result<()> {
        #[cfg(test)]
        if test_hooks::FAIL_CACHE_COMMIT.with(|f| f.replace(false)) {
            anyhow::bail!("injected manifest cache install failure");
        }
        let bin = path.with_extension("bin");
        staged
            .inner
            .dir
            .rename(&staged.inner.name, bin.file_name().unwrap_or_default())?;
        Ok(())
    }

    /// Save manifest to both JSON and bincode formats.
    /// Uses atomic write-to-temp + rename to prevent corruption from
    /// partial writes (e.g., crash mid-write, concurrent server reads).
    ///
    /// The JSON swap is the publication boundary: a cache that cannot be
    /// installed afterwards leaves `load` reading the authoritative JSON
    /// anyway, so the failure is a diagnostic — never an error reported
    /// over a manifest that did publish.
    pub fn save(&self, path: &Path) -> Result<()> {
        validate_writable_manifest_path(path)?;
        let json = self.canonical_json()?;
        // Cache first: the JSON swap is the boundary past which its
        // production cannot be allowed to fail.
        // `staged` is armed until the rename installs it: a failed JSON
        // swap propagates while the guard removes the staged file, and a
        // failed install warns while the guard removes it — either way no
        // owned temp is abandoned and no unrelated file is touched.
        let staged = self.prepare_cache(path, &json)?;

        Self::publish_json(path, &json)?;
        if let Err(e) = Self::commit_cache(&staged, path) {
            eprintln!(
                "Warning: could not install the manifest cache beside {}: {e:#} — \
                 the published JSON remains authoritative",
                path.display()
            );
        } else {
            staged.disarm();
        }
        Ok(())
    }

    /// Save only the JSON manifest, and drop any bincode cache beside it.
    ///
    /// Distributed output is committed and diffed, so it carries the
    /// reproducible artifact only: the bincode file is a load-time cache written
    /// from hash maps, and its bytes vary between runs. It is removed before
    /// the JSON publishes rather than left behind — `load` prefers a present
    /// cache, so a stale one must never outlive the manifest it shadows. A
    /// removal failure aborts the save while the previous manifest is intact.
    pub fn save_json(&self, path: &Path) -> Result<()> {
        validate_writable_manifest_path(path)?;
        let json = self.canonical_json()?;
        Self::remove_cache(path)?;
        Self::write_json(path, &json)
    }

    /// Whether the `.bin` beside `path` is a usable cache for exactly
    /// these JSON bytes — the same format/digest gate `load` applies.
    /// `false` covers an absent, undecodable, wrong-format, or
    /// stale-hash cache alike: the caller's remedy is re-staging, not
    /// failing the build.
    pub(crate) fn cache_is_current(path: &Path, json: &[u8]) -> bool {
        let bin_path = path.with_extension("bin");
        let Ok(file) = Self::open_cache_file(&bin_path) else {
            return false;
        };
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        match opts.deserialize_from::<_, ManifestCache>(BufReader::new(file)) {
            Ok(cache) => {
                cache.format == MANIFEST_CACHE_FORMAT && cache.json_hash == md5::compute(json).0
            }
            Err(_) => false,
        }
    }

    /// Remove the bincode cache beside `path`, if one exists.
    ///
    /// `symlink_metadata` sees the directory entry itself, where
    /// `exists()` follows the link — a dangling `.bin` link would read
    /// as absent while its entry stays behind. `remove_file` unlinks
    /// the entry; a symlink's target is never touched. A non-file,
    /// non-link entry (a directory) reports its own error rather than
    /// passing as removed.
    pub fn remove_cache(path: &Path) -> Result<()> {
        let bin_path = path.with_extension("bin");
        let Some(bin_name) = bin_path.file_name() else {
            return Ok(());
        };
        // The open is the validation: the pinned descriptor is the
        // directory the unlinkat runs against, so a parent swapped for a
        // link after this point redirects nothing.
        let dir = match crate::dirfd::OwnedDir::open(crate::dirfd::parent_dir(&bin_path)) {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        match dir.entry_kind(bin_name) {
            Ok(_) => dir.remove_file(bin_name)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// The name's recorded pins for an update that claims `module_name` of
    /// type `ext` — the `"{ext}:{name}"` bucket in `historical_names`.
    fn name_pins(
        &self,
        ext: &str,
        module_name: Option<&str>,
    ) -> Option<&BTreeMap<u32, SegmentPin>> {
        module_name.and_then(|name| self.historical_names.get(&format!("{ext}:{name}")))
    }

    /// Whether the name's recorded pins already give `{name}+v{version}`
    /// to a different identity — a slot the claiming entry must not
    /// reuse, or a `name+vN` URL would change identity underneath a page
    /// holding it. The slot counts as taken when the recorded pin names
    /// a different asset key OR different bytes: same bytes under a new
    /// key still need a new version, because the pin resolves through
    /// the key that issued it — reusing the slot would emit a URL that
    /// resolves to the other asset's path. Only the same key carrying
    /// the same bytes is the identity continuing.
    fn name_slot_taken(
        &self,
        pins: Option<&BTreeMap<u32, SegmentPin>>,
        version: u32,
        key: &str,
        hash: &str,
    ) -> bool {
        pins.and_then(|pins| pins.get(&version))
            .is_some_and(|pin| pin.key != key || pin.hash != hash)
    }

    /// The version after `floor` that does not collide with any of the
    /// name's recorded pins — when the natural successor is already a
    /// recorded identity, the claim continues past the name's whole
    /// recorded history. Errors when no later version is representable.
    fn claim_version(
        &self,
        key: &str,
        ext: &str,
        module_name: Option<&str>,
        floor: u32,
        hash: &str,
    ) -> Result<u32> {
        let Some(pins) = self.name_pins(ext, module_name) else {
            return Ok(floor);
        };
        if !self.name_slot_taken(Some(pins), floor, key, hash) {
            return Ok(floor);
        }
        let &max = pins.keys().next_back().unwrap_or(&0);
        max.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!(
                "module {:?} has recorded versions through u32::MAX — no later version can be represented for {key}",
                module_name.unwrap_or_default(),
            )
        })
    }

    /// Update or create asset entry, preserving version history.
    /// Output format: {name}.{hash}.v{version}.min.{ext}
    /// `update.max_history` controls how many old versions to retain (the build
    /// passes storage.keep_versions; usize::MAX retains every version)
    /// `update.stamp` is the version's last-modified stamp: the source file's
    /// mtime clamped to SOURCE_DATE_EPOCH, computed once per asset at the
    /// build boundary. A new version takes it; an unchanged hash keeps the
    /// stamp it already carries.
    ///
    /// A version's recorded `output` is part of its identity — the public
    /// path a page may hold. When this build would place the same version
    /// at a different public path (a layout, output-root, build-dir, or
    /// relevant source-path move) the asset takes a NEW version at the new
    /// path and the old one retires into history verbatim; a retained
    /// version's `output` is never rewritten. A move that leaves the
    /// public path unchanged — only the file's physical home differs —
    /// keeps the version, and the file lands where the resolver maps it.
    pub fn update_asset_with_history(&mut self, update: AssetUpdate<'_>) -> Result<&AssetEntry> {
        let AssetUpdate {
            key,
            source,
            hash,
            output_base,
            ext,
            dependencies,
            max_history,
            stamp,
            module_name,
        } = update;
        let timestamp = stamp.to_rfc3339();

        // Unchanged means the same output bytes, the same dependency
        // metadata (a dependency-only edit still versions the asset — the
        // combo list a module expands into is part of what a version
        // means), AND the same public output path — a version whose path
        // would move is a new version, never a rewritten one. A module
        // name claim also rechecks the slot: an unchanged entry still
        // bumps when the name already gave this version to other bytes.
        if let Some(entry) = self.assets.get(key) {
            let candidate = format!("{}.{}.v{}.min.{}", output_base, hash, entry.version, ext);
            let taken =
                self.name_slot_taken(self.name_pins(ext, module_name), entry.version, key, hash);
            if entry.hash == hash
                && entry.dependencies == dependencies
                && entry.output == candidate
                && !taken
            {
                // An unchanged version keeps everything but the bookkeeping:
                // the source label must not go stale, and history is trimmed
                // to the current retention bound — a manifest written with a
                // larger keep_versions still holds files nobody references.
                // A retired entry whose source reappears is active again,
                // its version sequence intact.
                // Stamps keep their recorded values; a manifest written
                // before stamping (or a hand-edited one) carries empty or
                // unparseable values, and an entry without a stamp cannot
                // render a URL — this build's stamp backfills the current
                // version and every retained history entry that lacks one.
                let entry = self.assets.get_mut(key).expect("existing asset");
                entry.retired = false;
                entry.source = source.to_string();
                entry.dependencies = dependencies;
                if entry.history.len() > max_history {
                    entry.history.drain(..entry.history.len() - max_history);
                }
                if stamp_epoch(&entry.timestamp).is_none() {
                    entry.timestamp = timestamp.clone();
                }
                for history in &mut entry.history {
                    if stamp_epoch(&history.timestamp).is_none() {
                        history.timestamp = timestamp.clone();
                    }
                }
                return Ok(self.assets.get(key).unwrap());
            }
        }

        let existing = self.assets.get(key);
        let floor = match existing {
            Some(entry) => entry.version.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "asset {key} is already at version u32::MAX — no later version can be represented"
                )
            })?,
            None => 1,
        };
        let new_version = self.claim_version(key, ext, module_name, floor, hash)?;

        // Build versioned output path
        let output = format!("{}.{}.v{}.min.{}", output_base, hash, new_version, ext);

        // Preserve history for rolling deploys (keep last max_history
        // entries); each retained version carries the stamp it was created
        // under, backfilled when a manifest from before stamping had none.
        // The outgoing current version joins it whole — version, hash,
        // recorded output, timestamp — so its public path keeps resolving.
        let history = existing
            .map(|e| {
                let mut h = e.history.clone();
                h.push(AssetVersion {
                    hash: e.hash.clone(),
                    version: e.version,
                    output: e.output.clone(),
                    timestamp: e.timestamp.clone(),
                });
                for version in &mut h {
                    if stamp_epoch(&version.timestamp).is_none() {
                        version.timestamp = timestamp.clone();
                    }
                }
                if h.len() > max_history {
                    h.drain(0..h.len() - max_history);
                }
                h
            })
            .unwrap_or_default();

        let entry = AssetEntry {
            source: source.to_string(),
            output,
            hash: hash.to_string(),
            version: new_version,
            timestamp,
            dependencies,
            history,
            retired: false,
        };

        self.assets.insert(key.to_string(), entry);
        Ok(self.assets.get(key).unwrap())
    }

    /// Generate JAM URL for a module group.
    /// Format: {base_path}/{group_stamp}/{module+version~module+version}.{min.}{css|js}
    ///
    /// The URL names exactly `modules` — the caller expands dependencies and
    /// passes the delivered group — and the segment is the newest member
    /// stamp in epoch seconds: the source mtime each version carried when it
    /// was created, carried forward unchanged while the version lives. The
    /// segment moves only when a member of the group changes, which is what
    /// lets the versioned URL cache for a year.
    ///
    /// Every module must resolve to an asset of `asset_type` with a parseable
    /// stamp: a module with no asset is an error matching `expand`'s
    /// render-time rejection, and a member without a stamp means the manifest
    /// was written before stamping and must be rebuilt — a constant segment
    /// would silently serve stale content under a year-long cache.
    pub fn jam_url(
        &self,
        modules: &[&str],
        asset_type: &str,
        base_path: &str,
        debug: bool,
    ) -> Result<String> {
        self.jam_url_with_limit(modules, asset_type, base_path, debug, None)
    }

    /// As [`jam_url`], with an optional per-component byte limit on the URL
    /// path. Hash-qualified tokens are long — a full list can exceed a
    /// filesystem's 255-byte component limit — so when `component_limit` is
    /// set the module list continues over `/`-separated path components: each
    /// component carries complete `~`-joined tokens, and the
    /// `.min.{type}`/`{type}` suffix lands only on the last. The combo parser
    /// joins the components back into one ordered list, so one URL still
    /// resolves one combo. A token that cannot fit a component alone (token
    /// plus the reserved suffix) is an error — it is never truncated or split.
    pub(crate) fn jam_url_with_limit(
        &self,
        modules: &[&str],
        asset_type: &str,
        base_path: &str,
        debug: bool,
        component_limit: Option<usize>,
    ) -> Result<String> {
        let entries: Vec<&AssetEntry> = modules
            .iter()
            .map(|m| {
                self.asset_key_for_module(m, asset_type)
                    .and_then(|key| self.assets.get(&key))
                    .ok_or_else(|| {
                        anyhow::anyhow!("module {} has no {} asset in the manifest", m, asset_type)
                    })
            })
            .collect::<Result<_>>()?;

        let module_versions: Vec<String> = modules
            .iter()
            .zip(&entries)
            .map(|(m, entry)| {
                Ok(format!(
                    "{}{}",
                    m,
                    version_suffix(entry.version, &entry.hash, self.hash_urls)?
                ))
            })
            .collect::<Result<_>>()?;

        let mut group_stamp = 0i64;
        for (module, entry) in modules.iter().zip(&entries) {
            let stamp = stamp_epoch(&entry.timestamp).ok_or_else(|| {
                anyhow::anyhow!(
                    "module {} has no valid last-modified stamp — rebuild the manifest",
                    module
                )
            })?;
            group_stamp = group_stamp.max(stamp);
        }

        let suffix = if debug {
            format!(".{}", asset_type)
        } else {
            format!(".min.{}", asset_type)
        };

        // Every component must leave room for the suffix even though only
        // the last carries it: static hosting materializes each component as
        // a directory plus final file, and the limit applies to each name.
        let list_path = match component_limit {
            Some(limit) => {
                let usable = limit.checked_sub(suffix.len()).ok_or_else(|| {
                    anyhow::anyhow!("URL component limit is smaller than the suffix")
                })?;
                let mut components: Vec<String> = Vec::new();
                for token in &module_versions {
                    if token.len() > usable {
                        anyhow::bail!(
                            "module token {token} is {} bytes — over the {}-byte URL component limit",
                            token.len(),
                            usable
                        );
                    }
                    match components.last_mut() {
                        Some(component) if component.len() + 1 + token.len() <= usable => {
                            component.push('~');
                            component.push_str(token);
                        }
                        _ => components.push(token.clone()),
                    }
                }
                components.join("/")
            }
            None => module_versions.join("~"),
        };

        Ok(format!("{base_path}/{group_stamp}/{list_path}{suffix}"))
    }

    /// Get all dependencies for a module (recursive) for a given asset type
    pub fn get_dependencies_for_type(&self, module: &str, asset_type: &str) -> Vec<String> {
        let mut visited = std::collections::HashSet::new();
        let mut result = Vec::new();

        fn visit(
            manifest: &Manifest,
            module: &str,
            asset_type: &str,
            visited: &mut std::collections::HashSet<String>,
            result: &mut Vec<String>,
        ) {
            if visited.contains(module) {
                return;
            }
            visited.insert(module.to_string());

            let entry_opt = manifest
                .asset_key_for_module(module, asset_type)
                .and_then(|key| manifest.assets.get(&key));
            if let Some(entry) = entry_opt {
                for dep in &entry.dependencies {
                    visit(manifest, dep, asset_type, visited, result);
                }
                result.push(module.to_string());
            } else {
                // Module not in manifest — included without dependency
                // expansion so the caller can name it: the renderer fails on
                // it, the combo answers 404.
                result.push(module.to_string());
            }
        }

        visit(self, module, asset_type, &mut visited, &mut result);
        result
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

/// Ownership scope for one staged cache file — the path a
/// `Manifest::prepare_cache` call just created with `create_new`, and
/// only that path. Dropped while still armed it unlinks the staged file:
/// a publication or installation failure must not strand it beside the
/// manifest. `disarm` after a successful install keeps the renamed cache
/// in place. It never looks at a sibling file it did not stage itself.
/// A staged file still owned by the staging call: the directory
/// descriptor it was created through, its component name, the pathname
/// for diagnostics, and the open file itself. Operations on the staging
/// — read-back, install, cleanup — run through the descriptor, so a
/// directory replaced by a link after staging cannot redirect them.
struct Staged {
    dir: crate::dirfd::OwnedDir,
    name: std::ffi::OsString,
    file: File,
}

pub struct StagedCache {
    inner: Staged,
}

impl StagedCache {
    fn armed(staged: Staged) -> Self {
        Self { inner: staged }
    }

    /// Whether the staged file is still present — the staged-at-rest
    /// assertion tests make before publication.
    #[cfg(test)]
    fn exists(&self) -> bool {
        self.inner.dir.entry_kind(&self.inner.name).is_ok()
    }

    /// The staged bytes are installed (or independently handled) —
    /// dropping must not unlink anything.
    pub(crate) fn disarm(mut self) {
        self.inner.name = std::ffi::OsString::new();
    }
}

impl Drop for StagedCache {
    fn drop(&mut self) {
        if !self.inner.name.is_empty() {
            let _ = self.inner.dir.remove_file(&self.inner.name);
        }
    }
}

/// Deterministic failure injection for the cache stage/install seams —
/// a test arms a flag on its own thread and the next matching operation
/// on that thread fails once, standing in for a filesystem fault without
/// permission or timing tricks. Thread-local because the suite runs
/// parallel: one test's armed fault must never land in another's call.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::Cell;

    thread_local! {
        pub static FAIL_CACHE_STAGE: Cell<bool> = const { Cell::new(false) };
        pub static FAIL_PUBLISH: Cell<bool> = const { Cell::new(false) };
        pub static FAIL_CACHE_COMMIT: Cell<bool> = const { Cell::new(false) };
        /// When set, staging tags count up from this value — a test can
        /// predict the exact candidate names instead of racing a global
        /// counter it cannot observe.
        pub static TMP_TAG_BASE: Cell<Option<u64>> = const { Cell::new(None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture-facing update a test feeds the manifest: tests don't tune
    /// retention, so a fixed bound of 10 stands in here (the build-time
    /// default is unbounded), and the stamp defaults to STAMP_ONE —
    /// `update_asset_at` overrides it.
    fn asset_update<'a>(
        key: &'a str,
        source: &'a str,
        hash: &'a str,
        output_base: &'a str,
        ext: &'a str,
        dependencies: Vec<String>,
    ) -> AssetUpdate<'a> {
        AssetUpdate {
            key,
            source,
            hash,
            output_base,
            ext,
            dependencies,
            max_history: 10,
            stamp: stamp(STAMP_ONE),
            module_name: None,
        }
    }

    /// `update_asset` with a caller-chosen stamp — for version-history tests.
    fn update_asset_at(manifest: &mut Manifest, stamp: DateTime<Utc>, update: AssetUpdate<'_>) {
        manifest
            .update_asset_with_history(AssetUpdate { stamp, ..update })
            .unwrap();
    }

    const STAMP_ONE: &str = "2024-01-15T10:00:00+00:00";
    const STAMP_TWO: &str = "2024-01-16T10:00:00+00:00";
    const STAMP_THREE: &str = "2024-01-17T10:00:00+00:00";

    #[test]
    fn test_staged_publish_orders_cache_before_the_json_swap() {
        // The publication boundary is the JSON rename alone: every
        // fallible step of cache production must finish while the
        // previous manifest is still the live one. Staging writes and
        // reads back a tmp that only `commit_cache` moves into place.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let manifest = Manifest::new();
        let json = manifest.canonical_json().unwrap();

        let staged = manifest.prepare_cache(&path, &json).unwrap();
        assert!(staged.exists(), "the cache is fully staged pre-publish");
        assert!(
            !path.exists(),
            "staging must not publish: the JSON swap is the boundary"
        );

        Manifest::publish_json(&path, &json).unwrap();
        assert!(path.exists(), "the swap makes the manifest live");

        Manifest::commit_cache(&staged, &path).unwrap();
        // The installed cache is bound to the JSON's own digest, so a
        // load that prefers it serves exactly what was published.
        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.assets.len(), manifest.assets.len());
    }

    #[test]
    fn test_save_fails_without_publishing_when_cache_staging_fails() {
        // A pre-publication fault: no JSON swap may happen, the error
        // propagates, and the previous manifest stays byte-identical.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let mut manifest = Manifest::new();
        manifest.save(&path).unwrap();
        let before = fs::read(&path).unwrap();

        manifest.version = "next".to_string();
        crate::manifest::test_hooks::FAIL_CACHE_STAGE.with(|f| f.set(true));
        assert!(
            manifest.save(&path).is_err(),
            "a staging failure must fail the save"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "a failed save replaced the authoritative JSON"
        );
    }

    #[test]
    fn test_save_reports_published_generation_when_cache_install_fails() {
        // The JSON swap already made the new manifest authoritative — a
        // cache-install fault afterwards is maintenance, so save reports
        // the published generation (success) plus a warning, and readers
        // see exactly the JSON that published.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let mut manifest = Manifest::new();
        manifest.save(&path).unwrap();

        manifest.version = "next".to_string();
        crate::manifest::test_hooks::FAIL_CACHE_COMMIT.with(|f| f.set(true));
        manifest
            .save(&path)
            .expect("a post-publication cache fault is a warning, not a failed save");

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(
            loaded.version, "next",
            "load must read the manifest that actually published"
        );
        // The stale v1 cache stayed behind but can never serve: its
        // digest no longer matches the published JSON.
    }

    #[test]
    fn test_save_json_drops_the_stale_cache_before_publishing() {
        // `save_json`'s contract is the reproducible artifact only — the
        // stale bincode cache is removed before the JSON swap so no
        // mixed generation is ever visible to `load`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let bin = path.with_extension("bin");
        let mut manifest = Manifest::new();
        manifest.save(&path).unwrap();
        assert!(bin.exists(), "save installs the cache");

        manifest.version = "dist-ready".to_string();
        manifest.save_json(&path).unwrap();
        assert!(
            !bin.exists(),
            "the stale cache must be gone before the new JSON publishes"
        );
        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.version, "dist-ready");
    }

    #[test]
    fn test_save_and_save_json_reject_a_symlinked_destination() {
        // The public save boundary holds the same rule the build does:
        // a symlink reads its target but publication replaces the link
        // itself — never follow one at a writable destination.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("shared.json");
        fs::write(&target, b"shared bytes").unwrap();
        let path = dir.path().join("manifest.json");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, &path).is_ok();
        #[cfg(not(unix))]
        let linked = std::os::windows::fs::symlink_file(&target, &path).is_ok();
        if !linked {
            eprintln!("symlink creation refused by this environment — skipping");
            return;
        }

        let manifest = Manifest::new();
        for (name, result) in [
            ("save", manifest.save(&path)),
            ("save_json", manifest.save_json(&path)),
        ] {
            let error = result.expect_err(&format!("{name} must refuse a symlink"));
            assert!(
                format!("{error:#}").contains("symlink"),
                "{name} got: {error:#}"
            );
            assert!(
                fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{name} replaced the link"
            );
            assert_eq!(
                fs::read(&target).unwrap(),
                b"shared bytes",
                "{name} touched the target"
            );
        }
    }

    #[test]
    fn test_save_rejects_a_non_regular_destination() {
        // A directory at the manifest path is not a missing manifest —
        // refuse before a rename would replace it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        fs::create_dir(&path).unwrap();
        let error = Manifest::new()
            .save(&path)
            .expect_err("a directory destination must refuse");
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "got: {error:#}"
        );
        assert!(path.is_dir());
    }

    #[test]
    fn test_remove_cache_sees_directory_entries_not_targets() {
        // `exists()` follows links, so a dangling `.bin` link would read
        // as absent while its entry stays behind — cleanup works on the
        // entry itself, whatever it points at.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let bin = dir.path().join("manifest.bin");

        // Genuinely absent is a no-op, not an error.
        Manifest::remove_cache(&path).unwrap();

        // An ordinary file is removed.
        fs::write(&bin, b"cache").unwrap();
        Manifest::remove_cache(&path).unwrap();
        assert!(fs::symlink_metadata(&bin).is_err());

        // A dangling link loses its entry even though the target never
        // existed.
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(dir.path().join("missing-target"), &bin).is_ok();
        #[cfg(not(unix))]
        let linked =
            std::os::windows::fs::symlink_file(dir.path().join("missing-target"), &bin).is_ok();
        if linked {
            Manifest::remove_cache(&path).unwrap();
            assert!(
                fs::symlink_metadata(&bin).is_err(),
                "the dangling link survived removal"
            );
        }

        // A link to a populated file loses only the entry — the
        // target's bytes are never touched.
        let sentinel = dir.path().join("sentinel.bin");
        fs::write(&sentinel, b"outside").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&sentinel, &bin).is_ok();
        #[cfg(not(unix))]
        let linked = std::os::windows::fs::symlink_file(&sentinel, &bin).is_ok();
        if linked {
            Manifest::remove_cache(&path).unwrap();
            assert!(fs::symlink_metadata(&bin).is_err());
            assert_eq!(fs::read(&sentinel).unwrap(), b"outside");
        }

        // A directory is not a cache file — removal reports the real
        // problem rather than silently passing over it.
        fs::create_dir(&bin).unwrap();
        assert!(Manifest::remove_cache(&path).is_err());
        assert!(bin.is_dir());
    }

    #[test]
    fn test_save_replaces_an_ordinary_manifest_normally() {
        // The healthy path: a real first file installs, a real second
        // save atomically replaces it, and the bytes load back.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let mut manifest = Manifest::new();
        manifest.save(&path).unwrap();
        assert!(
            path.is_file()
                && !fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
        );

        manifest.version = "second".to_string();
        manifest.save(&path).unwrap();
        assert_eq!(Manifest::load(&path).unwrap().version, "second");

        manifest.version = "json-only".to_string();
        manifest.save_json(&path).unwrap();
        assert_eq!(Manifest::load(&path).unwrap().version, "json-only");
        // A genuinely absent destination still initializes.
        Manifest::new()
            .save(&dir.path().join("other.json"))
            .unwrap();
    }

    #[test]
    fn test_json_staging_retries_a_colliding_candidate_name() {
        // A file already sitting at the first candidate name — crash
        // residue or a same-PID coincidence — is never truncated or
        // renamed over: staging skips to the next tag and the planted
        // bytes survive byte-identical.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let pid = std::process::id();
        crate::manifest::test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(9_000_000)));
        // save() stages the cache first (bin.tmp.9000000), then the JSON
        // — so the JSON's first candidate is .9000001.
        let collision =
            crate::storage::staged_tmp_name(dir.path(), &path, "json", &format!("{pid}.9000001"));
        fs::write(&collision, b"planted").unwrap();

        Manifest::new().save(&path).unwrap();
        assert_eq!(
            fs::read(&collision).unwrap(),
            b"planted",
            "staging overwrote a path it never created"
        );
        Manifest::load(&path).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn test_json_staging_never_follows_a_symlinked_candidate() {
        // A planted link at the candidate name must not redirect the
        // staged write into a file the save does not own: create_new
        // refuses the link itself, the sentinel behind it keeps its
        // bytes, and publication completes on a fresh name.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let pid = std::process::id();
        crate::manifest::test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(9_000_000)));
        let sentinel = dir.path().join("elsewhere.txt");
        fs::write(&sentinel, b"not yours").unwrap();
        let link =
            crate::storage::staged_tmp_name(dir.path(), &path, "json", &format!("{pid}.9000001"));
        std::os::unix::fs::symlink(&sentinel, &link).unwrap();

        Manifest::new().save(&path).unwrap();
        assert_eq!(fs::read(&sentinel).unwrap(), b"not yours");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the staged open replaced the planted link"
        );
        Manifest::load(&path).unwrap();
    }

    #[test]
    fn test_cache_staging_retries_a_colliding_candidate_name() {
        // Same contract on the cache side: the first candidate taken by
        // prepare_cache is skipped, never truncated.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let pid = std::process::id();
        crate::manifest::test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(9_000_000)));
        let collision =
            crate::storage::staged_tmp_name(dir.path(), &path, "bin", &format!("{pid}.9000000"));
        fs::write(&collision, b"planted").unwrap();

        Manifest::new().save(&path).unwrap();
        assert_eq!(
            fs::read(&collision).unwrap(),
            b"planted",
            "cache staging overwrote a path it never created"
        );
        Manifest::load(&path).unwrap();
    }

    #[test]
    fn test_staging_failure_removes_only_the_temp_it_created() {
        // A failed stage unlinks its own fresh temp and nothing else:
        // the previous JSON stays byte-identical and a lookalike file
        // the save cannot prove it wrote is left alone.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let pid = std::process::id();
        let manifest = Manifest::new();
        manifest.save(&path).unwrap();
        let before = fs::read(&path).unwrap();
        let unowned = crate::storage::staged_tmp_name(dir.path(), &path, "bin", "4242.1");
        fs::write(&unowned, b"someone else's").unwrap();

        crate::manifest::test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(9_000_000)));
        crate::manifest::test_hooks::FAIL_CACHE_STAGE.with(|f| f.set(true));
        manifest.save(&path).unwrap_err();

        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(&unowned).unwrap(), b"someone else's");
        assert!(
            !crate::storage::staged_tmp_name(dir.path(), &path, "bin", &format!("{pid}.9000000"))
                .exists(),
            "a failed stage left its own temp behind"
        );
    }

    #[test]
    fn test_publish_failure_removes_only_the_staged_cache() {
        // JSON publication fails after the cache staged (the manifest
        // path is occupied by a directory, so the atomic rename cannot
        // land): the save reports the failure, the previous manifest is
        // untouched, the staged file this call created is gone, and a
        // lookalike temp it never created is left alone.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let pid = std::process::id();
        let unowned = crate::storage::staged_tmp_name(dir.path(), &path, "bin", "4242.1");
        fs::write(&unowned, b"someone else's").unwrap();
        fs::create_dir(&path).unwrap();

        crate::manifest::test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(9_000_000)));
        Manifest::new()
            .save(&path)
            .expect_err("a JSON swap that cannot land must fail the save");

        assert!(
            !crate::storage::staged_tmp_name(dir.path(), &path, "bin", &format!("{pid}.9000000"))
                .exists(),
            "a failed publication left its own staged cache behind"
        );
        assert_eq!(
            fs::read(&unowned).unwrap(),
            b"someone else's",
            "cleanup removed a file it never created"
        );
    }

    #[test]
    fn test_install_failure_removes_only_the_staged_cache() {
        // The JSON swap already published — a cache-install fault warns
        // and succeeds, but the staged file the call still owns must not
        // linger beside the manifest. A lookalike stays untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let pid = std::process::id();
        let mut manifest = Manifest::new();
        manifest.save(&path).unwrap();
        let unowned = crate::storage::staged_tmp_name(dir.path(), &path, "bin", "4242.1");
        fs::write(&unowned, b"someone else's").unwrap();

        manifest.version = "next".to_string();
        crate::manifest::test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(9_000_000)));
        crate::manifest::test_hooks::FAIL_CACHE_COMMIT.with(|f| f.set(true));
        manifest
            .save(&path)
            .expect("a post-publication cache fault is a warning, not a failed save");

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.version, "next");
        assert!(
            !crate::storage::staged_tmp_name(dir.path(), &path, "bin", &format!("{pid}.9000000"))
                .exists(),
            "a failed install left its own staged cache behind"
        );
        assert_eq!(
            fs::read(&unowned).unwrap(),
            b"someone else's",
            "cleanup removed a file it never created"
        );
    }

    #[test]
    fn test_version_overflow_fails_instead_of_wrapping() {
        // An entry already at u32::MAX has no later version: the next
        // update must refuse deterministically rather than wrap to 0 and
        // resurrect a version a saved page could hold.
        let mut manifest = Manifest::new();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash1",
            "/js/app",
            "js",
            vec![],
        );
        manifest.assets.get_mut("js/app.js").unwrap().version = u32::MAX;

        let error = manifest
            .update_asset_with_history(asset_update(
                "js/app.js",
                "src/app.js",
                "hash2",
                "/js/app",
                "js",
                vec![],
            ))
            .expect_err("u32::MAX has no successor");
        assert!(
            error.to_string().contains("u32::MAX"),
            "the failure names the bound: {error:#}"
        );
        // The entry is untouched — no wrap, no partial update.
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, u32::MAX);
        assert_eq!(entry.hash, "hash1");
        assert!(entry.history.is_empty());
    }

    #[test]
    fn test_output_path_change_is_a_new_version_with_the_old_one_retained() {
        // Same bytes, same deps, different output prefix: the version's
        // public path is part of its identity, so the move issues a new
        // version and the old one keeps its recorded output.
        let mut manifest = Manifest::new();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash",
            "/js/app",
            "js",
            vec![],
        );
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash",
            "/assets/app",
            "js",
            vec![],
        );
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 2, "a path change bumps the version");
        assert_eq!(entry.output, "/assets/app.hash.v2.min.js");
        assert_eq!(entry.history.len(), 1);
        assert_eq!(
            entry.history[0].output, "/js/app.hash.v1.min.js",
            "the old version keeps its recorded public path"
        );
        assert_eq!(entry.history[0].version, 1);
    }

    fn stamp(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().to_utc()
    }

    fn update_asset(
        manifest: &mut Manifest,
        key: &str,
        source: &str,
        hash: &str,
        output_base: &str,
        ext: &str,
        dependencies: Vec<String>,
    ) {
        manifest
            .update_asset_with_history(asset_update(
                key,
                source,
                hash,
                output_base,
                ext,
                dependencies,
            ))
            .unwrap();
    }

    #[test]
    fn test_saved_json_keys_are_sorted() {
        use tempfile::tempdir;

        // A committed manifest must not change shape between builds of the same
        // sources, and the JSON map in this dependency tree preserves insertion
        // order rather than sorting.
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let mut manifest = Manifest::new();
        for key in ["js/zulu.js", "js/alpha.js", "js/mike.js"] {
            update_asset(
                &mut manifest,
                key,
                "src.js",
                "hash",
                &format!("/{key}"),
                "js",
                vec![],
            );
            manifest
                .modules
                .insert(key.replace(".js", "").replace('/', "."), key.to_string());
        }
        manifest.save(&path).unwrap();

        let json = fs::read_to_string(&path).unwrap();
        let asset_keys: Vec<&str> = json
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("\"js/"))
            .collect();
        let mut sorted = asset_keys.clone();
        sorted.sort();
        assert_eq!(
            asset_keys, sorted,
            "object keys must be written in sorted order:\n{json}"
        );
    }

    #[test]
    fn test_save_rejects_current_output_with_wrong_extension() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut manifest = Manifest::new();
        manifest.assets.insert(
            "js/app.js".to_string(),
            AssetEntry {
                source: "src/app.js".to_string(),
                output: "/css/app.h.v1.min.css".to_string(),
                hash: "h".to_string(),
                version: 1,
                timestamp: String::new(),
                dependencies: vec![],
                history: vec![],
                retired: false,
            },
        );

        let err = manifest
            .save(&dir.path().join("manifest.json"))
            .unwrap_err();
        assert!(
            err.to_string().contains("js/app.js"),
            "error must name the offending asset key: {err}"
        );
    }

    #[test]
    fn test_save_rejects_history_output_with_wrong_extension() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut manifest = Manifest::new();
        manifest.assets.insert(
            "js/app.js".to_string(),
            AssetEntry {
                source: "src/app.js".to_string(),
                output: "/js/app.h.v2.min.js".to_string(),
                hash: "h2".to_string(),
                version: 2,
                timestamp: String::new(),
                dependencies: vec![],
                history: vec![AssetVersion {
                    hash: "h1".to_string(),
                    version: 1,
                    output: "/css/app.h.v1.min.css".to_string(),
                    timestamp: String::new(),
                }],
                retired: false,
            },
        );

        let err = manifest
            .save(&dir.path().join("manifest.json"))
            .unwrap_err();
        assert!(
            err.to_string().contains("js/app.js"),
            "error must name the offending asset key: {err}"
        );
    }

    #[test]
    fn test_save_rejects_unsafe_output_paths_in_current_and_history() {
        use tempfile::tempdir;

        // The shared resolver's safety rules apply to every recorded
        // pointer: traversal, empty segments, separators a URL cannot
        // carry, and an empty pointer are all refused wherever they appear.
        let invalid = [
            "/js/../escape.js",
            "/js//app.js",
            "/js/dir\\name.js",
            "/js/dir:name.js",
            "",
        ];
        for bad in invalid {
            for in_history in [false, true] {
                let dir = tempdir().unwrap();
                let mut manifest = Manifest::new();
                let (output, history) = if in_history {
                    (
                        "/js/app.h.v2.min.js".to_string(),
                        vec![AssetVersion {
                            hash: "h1".to_string(),
                            version: 1,
                            output: bad.to_string(),
                            timestamp: String::new(),
                        }],
                    )
                } else {
                    (bad.to_string(), vec![])
                };
                manifest.assets.insert(
                    "js/app.js".to_string(),
                    AssetEntry {
                        source: "src/app.js".to_string(),
                        output,
                        hash: "h".to_string(),
                        version: 2,
                        timestamp: String::new(),
                        dependencies: vec![],
                        history,
                        retired: false,
                    },
                );

                let err = manifest
                    .save(&dir.path().join("manifest.json"))
                    .unwrap_err();
                assert!(
                    err.to_string().contains("js/app.js"),
                    "pointer {bad:?} (history={in_history}) must be rejected: {err}"
                );
            }
        }
    }

    #[test]
    fn test_save_accepts_outputs_under_any_source_directory() {
        use tempfile::tempdir;

        // Beside-source outputs live under whatever directory held the
        // source: a js asset may record a path under css/ and a css asset
        // a path under js/. Directory names carry no type meaning — the
        // pointer only has to be a safe path with the right extension.
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let mut manifest = Manifest::new();
        manifest.assets.insert(
            "js/app.js".to_string(),
            AssetEntry {
                source: "css/app.js".to_string(),
                output: "/css/_paste_build/app.h2.v2.min.js".to_string(),
                hash: "h2".to_string(),
                version: 2,
                timestamp: String::new(),
                dependencies: vec![],
                history: vec![AssetVersion {
                    hash: "h1".to_string(),
                    version: 1,
                    output: "/css/_paste_build/app.h1.v1.min.js".to_string(),
                    timestamp: String::new(),
                }],
                retired: false,
            },
        );
        manifest.assets.insert(
            "css/site.css".to_string(),
            AssetEntry {
                source: "js/site.scss".to_string(),
                output: "/js/_paste_build/site.h2.v2.min.css".to_string(),
                hash: "h2".to_string(),
                version: 2,
                timestamp: String::new(),
                dependencies: vec![],
                history: vec![AssetVersion {
                    hash: "h1".to_string(),
                    version: 1,
                    output: "/js/_paste_build/site.h1.v1.min.css".to_string(),
                    timestamp: String::new(),
                }],
                retired: false,
            },
        );
        manifest.assets.insert(
            "js/nested.js".to_string(),
            AssetEntry {
                source: "src/web/nested.js".to_string(),
                output: "src/web/_paste_build/nested.h.v1.min.js".to_string(),
                hash: "h".to_string(),
                version: 1,
                timestamp: String::new(),
                dependencies: vec![],
                history: vec![],
                retired: false,
            },
        );

        // Read the canonical JSON and the binary cache explicitly.
        manifest.save(&path).unwrap();
        let loaded = Manifest::load_json(&path).unwrap();
        assert_eq!(
            loaded.assets["js/app.js"].output,
            "/css/_paste_build/app.h2.v2.min.js"
        );
        assert_eq!(
            loaded.assets["css/site.css"].history[0].output,
            "/js/_paste_build/site.h1.v1.min.css"
        );
        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(
            loaded.assets["js/nested.js"].output,
            "src/web/_paste_build/nested.h.v1.min.js"
        );
    }

    #[test]
    fn test_output_for_version_resolves_current_history_and_unknown() {
        let mut manifest = Manifest::new();
        manifest.assets.insert(
            "js/a.js".to_string(),
            AssetEntry {
                source: "src/a.js".to_string(),
                output: "/js/a.h.v2.min.js".to_string(),
                hash: "h2".to_string(),
                version: 2,
                timestamp: String::new(),
                dependencies: vec![],
                history: vec![AssetVersion {
                    hash: "h1".to_string(),
                    version: 1,
                    output: "/js/a.h.v1.min.js".to_string(),
                    timestamp: String::new(),
                }],
                retired: false,
            },
        );

        // No pin and the current version both resolve to the current output.
        assert_eq!(
            manifest.output_for_version("a", "js", None),
            Some("/js/a.h.v2.min.js")
        );
        assert_eq!(
            manifest.output_for_version("a", "js", Some(2)),
            Some("/js/a.h.v2.min.js")
        );
        // A retained history version resolves to its own output.
        assert_eq!(
            manifest.output_for_version("a", "js", Some(1)),
            Some("/js/a.h.v1.min.js")
        );
        // A version the manifest does not know resolves to nothing.
        assert_eq!(manifest.output_for_version("a", "js", Some(3)), None);
        // So do an unknown module and a type the module has no asset for.
        assert_eq!(manifest.output_for_version("ghost", "js", None), None);
        assert_eq!(manifest.output_for_version("a", "css", None), None);
    }

    // ============================================================
    // Timestamp parsing: RFC3339 to Unix epoch conversion
    // ============================================================

    fn create_test_manifest_with_timestamps() -> Manifest {
        let mut manifest = Manifest::new();
        // Legacy token shape — these tests predate the hash-qualified form.
        manifest.hash_urls = false;

        // Add assets with RFC3339 timestamps (the actual format used)
        manifest.assets.insert(
            "js/paste/dom.js".to_string(),
            AssetEntry {
                source: "src/paste/dom.js".to_string(),
                output: "/js/paste/dom.abc123.v1.min.js".to_string(),
                hash: "abc123".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:30:00+00:00".to_string(), // RFC3339
                dependencies: vec![],
                history: vec![],
                retired: false,
            },
        );

        manifest.assets.insert(
            "js/paste/event.js".to_string(),
            AssetEntry {
                source: "src/paste/event.js".to_string(),
                output: "/js/paste/event.def456.v2.min.js".to_string(),
                hash: "def456".to_string(),
                version: 2,
                timestamp: "2024-01-16T14:00:00+00:00".to_string(), // Later timestamp
                dependencies: vec!["paste.dom".to_string()],
                history: vec![],
                retired: false,
            },
        );

        manifest
    }

    #[test]
    fn test_jam_url_uses_group_timestamp() {
        // When RFC3339 timestamps are present, the segment derives from the
        // latest asset's stamp — the newest member wins.
        let manifest = create_test_manifest_with_timestamps();

        let url = manifest
            .jam_url(&["paste.dom", "paste.event"], "js", "/paste", false)
            .unwrap();

        // The URL should contain module versions
        assert!(
            url.contains("paste.dom+v1.0"),
            "Should contain paste.dom version: {}",
            url
        );
        assert!(
            url.contains("paste.event+v2.0"),
            "Should contain paste.event version: {}",
            url
        );

        // URL format: /paste/{timestamp}/{modules}.min.js
        assert!(
            url.starts_with("/paste/"),
            "Should start with /paste/: {}",
            url
        );
        assert!(url.ends_with(".min.js"), "Should end with .min.js: {}", url);
    }

    #[test]
    fn test_jam_url_timestamp_is_parsed_correctly() {
        // Verify that RFC3339 timestamps are parsed correctly using chrono
        let manifest = create_test_manifest_with_timestamps();

        // Get an entry's timestamp
        let entry = manifest.assets.get("js/paste/dom.js").unwrap();

        // RFC3339 should be parseable by chrono
        let parsed = chrono::DateTime::parse_from_rfc3339(&entry.timestamp);
        assert!(
            parsed.is_ok(),
            "RFC3339 timestamp should be parseable by chrono"
        );

        // The JAM URL should now contain the Unix timestamp (not the manifest version)
        let url = manifest
            .jam_url(&["paste.dom"], "js", "/paste", false)
            .unwrap();

        // URL should contain a numeric timestamp (Unix epoch seconds)
        // The timestamp for 2024-01-15T10:30:00+00:00 is 1705314600
        assert!(
            url.contains("1705314600"),
            "JAM URL should contain Unix timestamp 1705314600, got: {}",
            url
        );
    }

    #[test]
    fn test_jam_url_timestamp_is_max_over_listed_modules() {
        // The URL names exactly the modules it is given — the caller expands
        // dependencies — and its timestamp is the max over those modules'
        // entries. An unlisted dependency moves nothing.
        let mut manifest = create_test_manifest_with_timestamps();

        // app depends on paste.event (2024-01-16T14:00Z), which is newer than
        // app itself (2024-01-10T00:00Z)
        manifest.assets.insert(
            "js/app.js".to_string(),
            AssetEntry {
                source: "src/app.js".to_string(),
                output: "/js/app.a1.v1.min.js".to_string(),
                hash: "a1".to_string(),
                version: 1,
                timestamp: "2024-01-10T00:00:00+00:00".to_string(),
                dependencies: vec!["paste.event".to_string()],
                history: vec![],
                retired: false,
            },
        );

        // app alone is stamped with its own epoch (1704844800) — the
        // dependency is not expanded inside jam_url.
        let url = manifest.jam_url(&["app"], "js", "/paste", false).unwrap();
        assert!(url.contains("app+v1.0"), "Listed module in URL: {}", url);
        assert!(
            !url.contains("paste.event+v"),
            "Unlisted dependencies stay out of the URL: {}",
            url
        );
        assert!(
            url.contains("1704844800"),
            "Timestamp should be app's own epoch: {}",
            url
        );

        // When the dependency is listed, its newer epoch (1705413600) wins.
        let url = manifest
            .jam_url(&["paste.event", "app"], "js", "/paste", false)
            .unwrap();
        assert!(
            url.contains("1705413600"),
            "Timestamp should be the max over the listed modules: {}",
            url
        );
    }

    #[test]
    fn test_manifest_new_has_valid_timestamp() {
        let manifest = Manifest::new();

        // generated should be RFC3339 format
        assert!(
            !manifest.generated.is_empty(),
            "generated should not be empty"
        );

        // Should be parseable by chrono
        let parsed = chrono::DateTime::parse_from_rfc3339(&manifest.generated);
        assert!(
            parsed.is_ok(),
            "generated should be valid RFC3339: {}",
            manifest.generated
        );
    }

    // ============================================================
    // History: replaced entry's timestamp is preserved, not recomputed
    // ============================================================

    #[test]
    fn test_update_asset_history_preserves_correct_timestamp() {
        let mut manifest = Manifest::new();

        // Version 1
        update_asset_at(
            &mut manifest,
            stamp(STAMP_ONE),
            asset_update(
                "js/app.js",
                "src/app.js",
                "hash_v1",
                "/js/app",
                "js",
                vec![],
            ),
        );
        let v1_timestamp = manifest.assets.get("js/app.js").unwrap().timestamp.clone();

        // Version 2 (hash changed)
        update_asset_at(
            &mut manifest,
            stamp(STAMP_TWO),
            asset_update(
                "js/app.js",
                "src/app.js",
                "hash_v2",
                "/js/app",
                "js",
                vec![],
            ),
        );
        let entry_v2 = manifest.assets.get("js/app.js").unwrap();

        // History should have 1 entry (the old v1)
        assert_eq!(entry_v2.history.len(), 1, "Should have 1 history entry");

        // The history entry's timestamp should be v1's timestamp, NOT manifest.generated
        let history_ts = &entry_v2.history[0].timestamp;
        assert_eq!(
            history_ts, &v1_timestamp,
            "History timestamp should be the replaced entry's timestamp, got: {}",
            history_ts
        );
        assert_eq!(entry_v2.history[0].version, 1);
        assert_eq!(entry_v2.history[0].hash, "hash_v1");
    }

    #[test]
    fn test_update_asset_history_chain_timestamps() {
        let mut manifest = Manifest::new();

        // Create 3 versions with different hashes
        update_asset_at(
            &mut manifest,
            stamp(STAMP_ONE),
            asset_update(
                "js/app.js",
                "src/app.js",
                "hash_v1",
                "/js/app",
                "js",
                vec![],
            ),
        );
        let ts_v1 = manifest.assets.get("js/app.js").unwrap().timestamp.clone();

        update_asset_at(
            &mut manifest,
            stamp(STAMP_TWO),
            asset_update(
                "js/app.js",
                "src/app.js",
                "hash_v2",
                "/js/app",
                "js",
                vec![],
            ),
        );
        let ts_v2 = manifest.assets.get("js/app.js").unwrap().timestamp.clone();

        update_asset_at(
            &mut manifest,
            stamp(STAMP_THREE),
            asset_update(
                "js/app.js",
                "src/app.js",
                "hash_v3",
                "/js/app",
                "js",
                vec![],
            ),
        );
        let entry = manifest.assets.get("js/app.js").unwrap();

        // Should have 2 history entries
        assert_eq!(entry.history.len(), 2, "Should have 2 history entries");

        // history[0] = v1, should have v1's timestamp
        assert_eq!(
            entry.history[0].timestamp, ts_v1,
            "history[0] should have v1 timestamp"
        );
        assert_eq!(entry.history[0].version, 1);

        // history[1] = v2, should have v2's timestamp (NOT v1's!)
        assert_eq!(
            entry.history[1].timestamp, ts_v2,
            "history[1] should have v2 timestamp, not v1's"
        );
        assert_eq!(entry.history[1].version, 2);
    }

    #[test]
    fn test_update_asset_no_version_bump_on_same_hash() {
        let mut manifest = Manifest::new();

        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "same_hash",
            "/js/app",
            "js",
            vec![],
        );
        let v1 = manifest.assets.get("js/app.js").unwrap().version;

        // Same hash should not bump version
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "same_hash",
            "/js/app",
            "js",
            vec![],
        );
        let v2 = manifest.assets.get("js/app.js").unwrap().version;

        assert_eq!(v1, v2, "Version should not bump when hash is unchanged");
        assert_eq!(
            manifest.assets.get("js/app.js").unwrap().history.len(),
            0,
            "No history should be added when hash is unchanged"
        );
    }

    // ============================================================
    // Diamond dependency graph: A→B→D and A→C→D must not produce duplicates
    // ============================================================

    #[test]
    fn test_get_dependencies_diamond_no_duplicates() {
        let mut manifest = Manifest::new();

        // Create diamond: A depends on B and C, both depend on D
        // D (no deps)
        manifest.assets.insert(
            "js/d.js".to_string(),
            AssetEntry {
                source: "src/d.js".to_string(),
                output: "/js/d.hash.v1.min.js".to_string(),
                hash: "hash_d".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec![],
                history: vec![],
                retired: false,
            },
        );

        // B depends on D
        manifest.assets.insert(
            "js/b.js".to_string(),
            AssetEntry {
                source: "src/b.js".to_string(),
                output: "/js/b.hash.v1.min.js".to_string(),
                hash: "hash_b".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec!["d".to_string()],
                history: vec![],
                retired: false,
            },
        );

        // C depends on D
        manifest.assets.insert(
            "js/c.js".to_string(),
            AssetEntry {
                source: "src/c.js".to_string(),
                output: "/js/c.hash.v1.min.js".to_string(),
                hash: "hash_c".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec!["d".to_string()],
                history: vec![],
                retired: false,
            },
        );

        // A depends on B and C
        manifest.assets.insert(
            "js/a.js".to_string(),
            AssetEntry {
                source: "src/a.js".to_string(),
                output: "/js/a.hash.v1.min.js".to_string(),
                hash: "hash_a".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec!["b".to_string(), "c".to_string()],
                history: vec![],
                retired: false,
            },
        );

        let deps = manifest.get_dependencies_for_type("a", "js");

        // Should include d, b, c, a in some topological order
        // D should appear exactly once
        let d_count = deps.iter().filter(|m| *m == "d").count();
        assert_eq!(
            d_count, 1,
            "D should appear exactly once in deps, got: {:?}",
            deps
        );

        // All 4 modules should be present
        assert!(deps.contains(&"a".to_string()), "a should be in deps");
        assert!(deps.contains(&"b".to_string()), "b should be in deps");
        assert!(deps.contains(&"c".to_string()), "c should be in deps");
        assert!(deps.contains(&"d".to_string()), "d should be in deps");
        assert_eq!(deps.len(), 4, "Should have exactly 4 deps");

        // D should come before B and C (topological order)
        let d_idx = deps.iter().position(|m| m == "d").unwrap();
        let b_idx = deps.iter().position(|m| m == "b").unwrap();
        let c_idx = deps.iter().position(|m| m == "c").unwrap();
        let a_idx = deps.iter().position(|m| m == "a").unwrap();

        assert!(d_idx < b_idx, "d should come before b");
        assert!(d_idx < c_idx, "d should come before c");
        assert!(b_idx < a_idx, "b should come before a");
        assert!(c_idx < a_idx, "c should come before a");
    }

    // ============================================================
    // Dependencies update even when content hash is unchanged
    // ============================================================

    #[test]
    fn test_update_asset_updates_deps_on_same_hash() {
        let mut manifest = Manifest::new();

        // Version 1 with deps [A]
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "same_hash",
            "/js/app",
            "js",
            vec!["dep_a".to_string()],
        );
        let deps_v1 = manifest
            .assets
            .get("js/app.js")
            .unwrap()
            .dependencies
            .clone();
        assert_eq!(deps_v1, vec!["dep_a"]);

        // Same hash but different deps [A, B]
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "same_hash",
            "/js/app",
            "js",
            vec!["dep_a".to_string(), "dep_b".to_string()],
        );
        let deps_v2 = manifest
            .assets
            .get("js/app.js")
            .unwrap()
            .dependencies
            .clone();

        // Dependencies should be updated even though hash didn't change
        assert_eq!(
            deps_v2,
            vec!["dep_a", "dep_b"],
            "Dependencies should update even when hash is unchanged, got: {:?}",
            deps_v2
        );
    }

    // ============================================================
    // JAM URL: a module with no asset of the dump's type is an error
    // ============================================================

    #[test]
    fn test_jam_url_includes_missing_modules() {
        // Callers validate the group through expand before asking for a URL,
        // so a module without a manifest asset is a hard error naming it.
        let manifest = create_test_manifest_with_timestamps();

        let err = manifest
            .jam_url(&["paste.dom", "paste.unknown"], "js", "/paste", false)
            .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("paste.unknown") && message.contains("js"),
            "Error should name the module and asset type: {}",
            message
        );
    }

    // ============================================================
    // Stale .bin cleanup on deserialization failure
    // ============================================================

    #[test]
    fn test_manifest_load_cleans_stale_bincode() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        // Write a valid JSON manifest
        let manifest = Manifest::new();
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        std::fs::write(&json_path, &json).unwrap();

        // Write garbage bincode (simulates schema change / corruption)
        std::fs::write(&bin_path, b"corrupted bincode data").unwrap();
        assert!(bin_path.exists(), "Precondition: .bin file should exist");

        // load() should succeed by falling back to JSON
        let loaded = Manifest::load(&json_path).unwrap();
        assert!(
            !loaded.generated.is_empty(),
            "Should have loaded valid manifest from JSON"
        );

        // Stale .bin should be deleted so subsequent loads skip the failed attempt
        assert!(
            !bin_path.exists(),
            "Stale .bin file should be cleaned up after failed deserialization"
        );
    }

    /// A special file planted as the cache is a miss, never a stall:
    /// the open is nonblocking and only a verified regular file is
    /// deserialized — the JSON beside it always answers.
    #[test]
    #[cfg(unix)]
    fn test_manifest_load_treats_a_fifo_cache_as_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");
        std::fs::write(
            &json_path,
            serde_json::to_string_pretty(&Manifest::new()).unwrap(),
        )
        .unwrap();
        {
            use std::os::unix::ffi::OsStrExt;
            let c = std::ffi::CString::new(bin_path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        }

        let loaded = Manifest::load(&json_path).unwrap();
        assert!(!loaded.generated.is_empty(), "the JSON lost to a FIFO");
    }

    /// The freshness probe takes the same checked open as the load —
    /// a FIFO `.bin` is a prompt miss, not a blocked reader.
    #[test]
    #[cfg(unix)]
    fn cache_is_current_treats_a_fifo_as_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");
        std::fs::write(&json_path, "{}").unwrap();
        {
            use std::os::unix::ffi::OsStrExt;
            let c = std::ffi::CString::new(bin_path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        }

        assert!(!Manifest::cache_is_current(&json_path, b"{}"));
    }

    /// A link-shaped cache is refused at the open — a symlinked `.bin`
    /// is a miss, and the authoritative JSON still loads.
    #[test]
    #[cfg(unix)]
    fn test_manifest_load_treats_a_symlinked_cache_as_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");
        std::fs::write(
            &json_path,
            serde_json::to_string_pretty(&Manifest::new()).unwrap(),
        )
        .unwrap();
        // Even pointing at the JSON itself, a link is not a cache.
        std::os::unix::fs::symlink(&json_path, &bin_path).unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert!(!loaded.generated.is_empty(), "the JSON lost to a link");
        assert!(
            bin_path
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink(),
            "the refused cache link was removed as if corrupt"
        );
    }

    #[test]
    fn test_jam_url_cross_type_resolution_via_modules_map() {
        // asset_key_for_module is type-filtered: a module whose only asset is
        // JS has no css asset, so a css URL errors while the js URL resolves.
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.version = "2024.01.15".to_string();

        // modules map: paste.ui.heroscroll → js/paste/ui/heroscroll.js
        manifest.modules.insert(
            "paste.ui.heroscroll".to_string(),
            "js/paste/ui/heroscroll.js".to_string(),
        );

        // Only a JS asset entry exists (no css/paste/ui/heroscroll.css)
        manifest.assets.insert(
            "js/paste/ui/heroscroll.js".to_string(),
            AssetEntry {
                source: "src/js/paste/ui/heroscroll.js".to_string(),
                output: "/js/paste/ui/heroscroll.v1.min.js".to_string(),
                hash: "h1".to_string(),
                version: 1,
                timestamp: "2024-06-15T10:00:00+00:00".to_string(),
                dependencies: vec![],
                history: vec![],
                retired: false,
            },
        );

        // A CSS URL for a JS-only module is an error — no cross-type fallback.
        let err = manifest
            .jam_url(&["paste.ui.heroscroll"], "css", "/paste", false)
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("paste.ui.heroscroll") && message.contains("css"),
            "Error should name the module and asset type: {}",
            message
        );

        // Request JS URL — resolves through the modules map entry.
        let js_url = manifest
            .jam_url(&["paste.ui.heroscroll"], "js", "/paste", false)
            .unwrap();
        assert!(
            js_url.contains("paste.ui.heroscroll+v1.0"),
            "JS URL should find module: {}",
            js_url
        );
    }

    #[test]
    fn test_jam_url_missing_stamp_is_a_render_error() {
        // Known modules whose timestamps are empty or unparseable — the
        // shape a manifest written before stamping carries — have no epoch
        // to name: the URL is a render error naming the module rather than a
        // constant segment that could cache stale content for a year.
        let mut manifest = create_test_manifest_with_timestamps();
        for entry in manifest.assets.values_mut() {
            entry.timestamp.clear();
        }

        let err = manifest
            .jam_url(&["paste.dom", "paste.event"], "js", "/paste", false)
            .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("paste.dom") && message.contains("rebuild"),
            "Error should name the module and say the manifest must be rebuilt: {}",
            message
        );
    }

    // ============================================================
    // Stamps: source-mtime semantics carried in update_asset_with_history
    // ============================================================

    #[test]
    fn test_stamp_carried_forward_when_hash_unchanged() {
        // A rebuild of unchanged content keeps the version's stamp — the
        // URL segment only moves when a member changes.
        let mut manifest = Manifest::new();
        update_asset_at(
            &mut manifest,
            stamp(STAMP_ONE),
            asset_update(
                "js/app.js",
                "src/app.js",
                "same_hash",
                "/js/app",
                "js",
                vec![],
            ),
        );

        // A later build — different stamp offered — must not move it.
        update_asset_at(
            &mut manifest,
            stamp(STAMP_TWO),
            asset_update(
                "js/app.js",
                "src/app.js",
                "same_hash",
                "/js/app",
                "js",
                vec![],
            ),
        );

        assert_eq!(
            manifest.assets.get("js/app.js").unwrap().timestamp,
            STAMP_ONE,
            "an unchanged hash keeps its version's stamp"
        );
    }

    #[test]
    fn test_new_version_takes_the_build_stamp() {
        let mut manifest = Manifest::new();
        update_asset_at(
            &mut manifest,
            stamp(STAMP_ONE),
            asset_update(
                "js/app.js",
                "src/app.js",
                "hash_v1",
                "/js/app",
                "js",
                vec![],
            ),
        );
        update_asset_at(
            &mut manifest,
            stamp(STAMP_TWO),
            asset_update(
                "js/app.js",
                "src/app.js",
                "hash_v2",
                "/js/app",
                "js",
                vec![],
            ),
        );

        let entry = manifest.assets.get("js/app.js").unwrap();
        assert_eq!(entry.version, 2);
        assert_eq!(entry.timestamp, STAMP_TWO, "the new version is stamped");
        assert_eq!(
            entry.history[0].timestamp, STAMP_ONE,
            "the retired version keeps its own stamp in history"
        );
    }

    #[test]
    fn test_build_backfills_missing_stamps() {
        // A manifest written before stamping carries empty (or unparseable)
        // timestamps; the next build fills them with the build stamp — on
        // the current entry and on every retained history version alike.
        let mut manifest = Manifest::new();
        manifest.assets.insert(
            "js/app.js".to_string(),
            AssetEntry {
                source: "src/app.js".to_string(),
                output: "/js/app.h1.v1.min.js".to_string(),
                hash: "h1".to_string(),
                version: 1,
                timestamp: String::new(),
                dependencies: vec![],
                history: vec![AssetVersion {
                    hash: "h0".to_string(),
                    version: 0,
                    output: "/js/app.h0.v0.min.js".to_string(),
                    timestamp: "not a timestamp".to_string(),
                }],
                retired: false,
            },
        );

        update_asset_at(
            &mut manifest,
            stamp(STAMP_THREE),
            asset_update("js/app.js", "src/app.js", "h1", "/js/app", "js", vec![]),
        );

        let entry = manifest.assets.get("js/app.js").unwrap();
        assert_eq!(entry.version, 1, "unchanged hash still bumps nothing");
        assert_eq!(entry.timestamp, STAMP_THREE);
        assert_eq!(entry.history[0].timestamp, STAMP_THREE);
    }

    #[test]
    fn test_make_deterministic_keeps_stamps() {
        // Dist output must stay reproducible, but the stamps are the URL
        // segment: clearing them would move every rendered URL or fail the
        // render. Only `generated` is dropped.
        let mut manifest = create_test_manifest_with_timestamps();
        manifest.make_deterministic();

        assert!(manifest.generated.is_empty(), "generated is cleared");
        assert_eq!(manifest.version, "static", "version gets the fixed label");
        for entry in manifest.assets.values() {
            assert!(
                stamp_epoch(&entry.timestamp).is_some(),
                "asset stamps survive make_deterministic"
            );
        }
    }

    // ============================================================
    // .bin freshness: the cache is bound to the JSON's bytes, not its mtime
    // ============================================================

    #[test]
    fn test_manifest_load_prefers_json_when_bin_is_older() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        // The bincode cache of a previous manifest — version "stale".
        let mut stale = Manifest::new();
        stale.version = "stale".to_string();
        stale.save(&json_path).unwrap();
        assert!(bin_path.exists());

        // The checkout refreshes the JSON — version "fresh" — but leaves
        // the .bin behind, still timestamped before it.
        let mut fresh = Manifest::new();
        fresh.version = "fresh".to_string();
        std::fs::write(&json_path, serde_json::to_string(&fresh).unwrap()).unwrap();
        filetime::set_file_mtime(&bin_path, filetime::FileTime::from_unix_time(1, 0)).unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(
            loaded.version, "fresh",
            "a .bin older than the JSON must not win"
        );
    }

    #[test]
    fn test_manifest_load_prefers_json_replaced_at_the_same_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut stale = Manifest::new();
        stale.version = "stale".to_string();
        stale.save(&json_path).unwrap();

        // A replaced JSON with the cache's own timestamp: freshness by
        // mtime cannot tell them apart, but the content binding can.
        let mut fresh = Manifest::new();
        fresh.version = "fresh".to_string();
        std::fs::write(&json_path, serde_json::to_string(&fresh).unwrap()).unwrap();
        let bin_mtime =
            filetime::FileTime::from_last_modification_time(&fs::metadata(&bin_path).unwrap());
        filetime::set_file_mtime(&json_path, bin_mtime).unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(
            loaded.version, "fresh",
            "a JSON replaced at the same mtime must win over the cache"
        );
    }

    #[test]
    fn test_manifest_load_prefers_json_when_bin_is_newer() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut stale = Manifest::new();
        stale.version = "stale".to_string();
        stale.save(&json_path).unwrap();

        // The JSON changes AND the cache ends up newer — content binding
        // still lets the JSON win where an mtime comparison would not.
        let mut fresh = Manifest::new();
        fresh.version = "fresh".to_string();
        std::fs::write(&json_path, serde_json::to_string(&fresh).unwrap()).unwrap();
        filetime::set_file_mtime(
            &bin_path,
            filetime::FileTime::from_unix_time(9_999_999_999, 0),
        )
        .unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(
            loaded.version, "fresh",
            "a newer .bin caching different JSON must not win"
        );
    }

    #[test]
    fn test_manifest_load_uses_the_bin_cache_for_unchanged_json() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");

        // save() writes JSON then a .bin bound to its bytes; the unchanged
        // pair still loads through the cache.
        let mut manifest = Manifest::new();
        manifest.version = "cached".to_string();
        manifest.save(&json_path).unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.version, "cached");
    }

    #[test]
    fn test_bin_cache_round_trips_the_full_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut manifest = Manifest::new();
        manifest.version = "roundtrip".to_string();
        manifest.generated_dirs = vec!["static/js".to_string(), "dist".to_string()];
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash1",
            "/js/app",
            "js",
            vec![],
        );
        update_asset(
            &mut manifest,
            "js/gone.js",
            "src/gone.js",
            "hash2",
            "/js/gone",
            "js",
            vec!["app".to_string()],
        );
        manifest.assets.get_mut("js/gone.js").unwrap().retired = true;
        manifest.sorted_deps.insert(
            "js".to_string(),
            vec![("app".to_string(), "/js/app.hash1.v1.min.js".to_string(), 1)],
        );
        manifest.output_roots.insert(
            "js".to_string(),
            vec!["dist/js".to_string(), "dist/scripts".to_string()],
        );
        manifest
            .output_roots
            .insert("css".to_string(), vec!["dist/css".to_string()]);
        manifest.record_packages_segment("0123456789ab").unwrap();
        manifest.cdn_base = Some("https://cdn.example/assets".to_string());
        manifest.historical_names.insert(
            "old.theme".to_string(),
            [(
                2,
                SegmentPin {
                    key: "css/old-theme.css".to_string(),
                    version: 2,
                    hash: "deadbee5".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        );
        manifest
            .ambiguous_name_pins
            .insert("shared.name/js/1".to_string());
        manifest.save(&json_path).unwrap();

        // Decode the cache the same way load does and compare the manifest
        // it carried — a positional layout drift must surface here, not just
        // as a load-time miss.
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        let cache: ManifestCache = opts
            .deserialize_from(BufReader::new(File::open(&bin_path).unwrap()))
            .unwrap();
        assert_eq!(cache.format, MANIFEST_CACHE_FORMAT);
        assert_eq!(
            cache.json_hash,
            md5::compute(fs::read(&json_path).unwrap()).0,
            "the cache must stay bound to the exact JSON bytes"
        );
        let decoded = cache.manifest;
        assert_eq!(decoded.generated_dirs, manifest.generated_dirs);
        assert_eq!(decoded.sorted_deps, manifest.sorted_deps);
        assert_eq!(decoded.hash_urls, manifest.hash_urls);
        assert_eq!(decoded.output_roots, manifest.output_roots);
        assert_eq!(
            decoded.packages_segments, manifest.packages_segments,
            "loader metadata round-trips the bincode cache"
        );
        assert_eq!(
            decoded.cdn_base.as_deref(),
            Some("https://cdn.example/assets"),
            "the build's default origin round-trips the bincode cache"
        );
        assert_eq!(
            decoded.historical_names, manifest.historical_names,
            "historical name pins round-trip the bincode cache"
        );
        assert_eq!(
            decoded.ambiguous_name_pins, manifest.ambiguous_name_pins,
            "ambiguous pin records round-trip the bincode cache"
        );
        let decoded_app = &decoded.assets["js/app.js"];
        assert_eq!(decoded_app.source, "src/app.js");
        assert!(!decoded_app.retired);
        let decoded_gone = &decoded.assets["js/gone.js"];
        assert!(decoded_gone.retired);
        assert_eq!(decoded_gone.dependencies, vec!["app".to_string()]);
        assert_eq!(
            decoded.canonical_json().unwrap(),
            manifest.canonical_json().unwrap()
        );

        // load accepts the matching cache and leaves it in place.
        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.version, "roundtrip");
        assert!(loaded.assets["js/gone.js"].retired);
        assert_eq!(loaded.output_roots, manifest.output_roots);
        assert_eq!(
            loaded.packages_segments, manifest.packages_segments,
            "loader metadata round-trips the JSON"
        );
        assert_eq!(loaded.cdn_base, manifest.cdn_base);
        assert_eq!(loaded.historical_names, manifest.historical_names);
        assert_eq!(loaded.ambiguous_name_pins, manifest.ambiguous_name_pins);
        assert!(bin_path.exists(), "a matching cache must stay for reuse");

        // An empty manifest round-trips the same layout.
        let empty_path = dir.path().join("empty.json");
        let empty_bin = empty_path.with_extension("bin");
        Manifest::new().save(&empty_path).unwrap();
        let cache: ManifestCache = opts
            .deserialize_from(BufReader::new(File::open(&empty_bin).unwrap()))
            .unwrap();
        assert_eq!(cache.format, MANIFEST_CACHE_FORMAT);
        assert!(cache.manifest.assets.is_empty());
        assert!(cache.manifest.generated_dirs.is_empty());
        assert!(cache.manifest.hash_urls);
        assert!(
            cache.manifest.cdn_base.is_none(),
            "absent cdn_base round-trips as absent"
        );
        assert!(Manifest::load(&empty_path).unwrap().assets.is_empty());
        assert!(empty_bin.exists());

        // The legacy policy round-trips too — no field is ever skipped.
        let legacy_path = dir.path().join("legacy.json");
        let mut legacy = Manifest::new();
        legacy.hash_urls = false;
        legacy.save(&legacy_path).unwrap();
        assert!(!Manifest::load(&legacy_path).unwrap().hash_urls);
    }

    #[test]
    fn test_load_strips_nul_bytes_from_every_string_field() {
        // The loader's stated contract is no NUL byte survives in any
        // string field — including the fields added since the sanitizer
        // was written: pins, segment maps, and set members are all part of
        // lookup identity, so a padded byte must never reach them.
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        fs::write(
            &json_path,
            r#"{
                "version": "0.3.1",
                "generated": "ge\u0000nerated",
                "modules": {"a\u0000pp": "js/a\u0000pp.js"},
                "sorted_deps": {"j\u0000s": [["ap\u0000p", "/js/ap\u0000p.v1.js", 1]]},
                "generated_dirs": ["di\u0000st"],
                "output_roots": {"j\u0000s": ["dist/j\u0000s"]},
                "packages_segments": {"0123456789a\u0000b": {"seg\u0000ment": {"key": "js/x\u0000.js", "version": 1, "hash": "aa\u0000bb"}}},
                "cdn_base": "https://cdn.exam\u0000ple",
                "historical_names": {"old.na\u0000me": {"2": {"key": "css/o\u0000ld.css", "version": 2, "hash": "deadb\u0000eef"}}},
                "ambiguous_name_pins": ["shar\u0000ed/js/1"],
                "assets": {"js/a\u0000pp.js": {"source": "src/ap\u0000p.js", "output": "js/ap\u0000p.js", "hash": "h\u00001", "timestamp": "2024-01-01T00:00:00Z", "version": 1, "retired": false, "dependencies": ["de\u0000p"], "history": [{"hash": "h\u00000", "output": "js/ap\u0000p.v0.js", "timestamp": "2023-01-01T00:00:00Z", "version": 0}]}}
            }"#,
        )
        .unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.generated, "generated");
        assert!(loaded.modules.contains_key("app"));
        assert_eq!(loaded.modules["app"], "js/app.js");
        assert!(loaded.sorted_deps.contains_key("js"));
        assert_eq!(loaded.sorted_deps["js"][0].0, "app");
        assert_eq!(loaded.generated_dirs, vec!["dist".to_string()]);
        assert!(loaded.output_roots.contains_key("js"));
        assert_eq!(loaded.output_roots["js"], vec!["dist/js".to_string()]);
        let segments = &loaded.packages_segments["0123456789ab"];
        assert_eq!(segments["segment"].key, "js/x.js");
        assert_eq!(segments["segment"].hash, "aabb");
        assert_eq!(loaded.cdn_base.as_deref(), Some("https://cdn.example"));
        let pins = &loaded.historical_names["old.name"];
        assert_eq!(pins[&2].key, "css/old.css");
        assert_eq!(pins[&2].hash, "deadbeef");
        assert!(loaded.ambiguous_name_pins.contains("shared/js/1"));
        let asset = &loaded.assets["js/app.js"];
        assert_eq!(asset.source, "src/app.js");
        assert_eq!(asset.dependencies, vec!["dep".to_string()]);
        assert_eq!(asset.history[0].hash, "h0");

        // No NUL byte survives anywhere a reader can observe.
        let canonical = loaded.canonical_json().unwrap();
        assert!(
            !canonical.contains(&b'\0'),
            "the loaded manifest is NUL-free"
        );
    }

    #[test]
    fn test_bincode_cache_load_strips_nul_bytes_from_new_fields() {
        // The same invariant holds on the cache path: a manifest whose
        // pin records carry NUL bytes must load cleaned — the sanitizer
        // runs on the decoded manifest, not only on JSON text.
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let mut manifest = Manifest::new();
        manifest.cdn_base = Some("https://cdn.exa\u{0}mple".to_string());
        manifest.historical_names.insert(
            "old.na\u{0}me".to_string(),
            [(
                1,
                SegmentPin {
                    key: "js/o\u{0}ld.js".to_string(),
                    version: 1,
                    hash: "aa\u{0}bb".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        );
        manifest
            .ambiguous_name_pins
            .insert("sh\u{0}ared/js/1".to_string());
        manifest.save(&json_path).unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.cdn_base.as_deref(), Some("https://cdn.example"));
        assert_eq!(loaded.historical_names["old.name"][&1].key, "js/old.js");
        assert_eq!(loaded.historical_names["old.name"][&1].hash, "aabb");
        assert!(loaded.ambiguous_name_pins.contains("shared/js/1"));
        assert!(!loaded.canonical_json().unwrap().contains(&b'\0'));
    }

    #[test]
    fn test_stale_format_bin_cache_falls_back_to_json() {
        // A .bin written under a previous cache format must never win over
        // the JSON beside it: the format marker is checked before the
        // payload is trusted, a decodable stale cache stays on disk, and
        // the JSON is parsed instead.
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut manifest = Manifest::new();
        manifest.version = "fresh".to_string();
        manifest.save_json(&json_path).unwrap();
        let json_bytes = fs::read(&json_path).unwrap();

        let mut cached = Manifest::new();
        cached.version = "cached".to_string();
        cached
            .output_roots
            .insert("js".to_string(), vec!["old/js".to_string()]);
        let stale = ManifestCache {
            format: *b"PASTEC04",
            json_hash: md5::compute(&json_bytes).0,
            manifest: cached,
        };
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        opts.serialize_into(BufWriter::new(File::create(&bin_path).unwrap()), &stale)
            .unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.version, "fresh", "the JSON stays authoritative");
        assert!(
            loaded.output_roots.is_empty(),
            "the stale cache's records must not appear"
        );
        assert!(
            bin_path.exists(),
            "a decodable stale cache stays for inspection, unlike corrupt data"
        );
    }

    /// The `PASTEC04` payload shape: the manifest serialized before
    /// `output_roots` existed — the same fields in the same order, minus
    /// the trailing field. Bincode is positional, so a cache carrying
    /// this layout cannot decode into the current `Manifest`.
    #[derive(Serialize)]
    struct PreFieldManifestCache {
        format: [u8; 8],
        json_hash: [u8; 16],
        manifest: PreFieldManifest,
    }

    #[derive(Serialize)]
    struct PreFieldManifest {
        version: String,
        generated: String,
        modules: HashMap<String, String>,
        assets: HashMap<String, AssetEntry>,
        sorted_deps: HashMap<String, Vec<SortedDepEntry>>,
        generated_dirs: Vec<String>,
        hash_urls: bool,
    }

    #[test]
    fn test_pre_field_cache_payload_falls_back_to_json() {
        // A cache written by a build whose Manifest had no output_roots
        // field is a shorter payload: decoding it into the current struct
        // fails, the loader drops the unusable .bin, and the authoritative
        // JSON supplies every asset, version, and history entry.
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut manifest = Manifest::new();
        manifest.version = "fresh".to_string();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash1",
            "/js/app",
            "js",
            vec![],
        );
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash2",
            "/js/app",
            "js",
            vec![],
        );
        manifest.save_json(&json_path).unwrap();
        let json_bytes = fs::read(&json_path).unwrap();
        let expected = manifest.assets["js/app.js"].clone();

        // A faithfully-shaped previous-format payload: the old marker and
        // a manifest serialized without the trailing field.
        let legacy = PreFieldManifestCache {
            format: *b"PASTEC04",
            json_hash: md5::compute(&json_bytes).0,
            manifest: PreFieldManifest {
                version: "cached".to_string(),
                generated: manifest.generated.clone(),
                modules: manifest.modules.clone(),
                assets: manifest.assets.clone(),
                sorted_deps: manifest.sorted_deps.clone(),
                generated_dirs: manifest.generated_dirs.clone(),
                hash_urls: manifest.hash_urls,
            },
        };
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        opts.serialize_into(BufWriter::new(File::create(&bin_path).unwrap()), &legacy)
            .unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.version, "fresh", "the JSON stays authoritative");
        assert_eq!(
            loaded.assets["js/app.js"], expected,
            "assets, versions, and history survive the fallback intact"
        );
        assert!(loaded.output_roots.is_empty());
        assert!(
            !bin_path.exists(),
            "a payload that cannot decode is dropped rather than retried every load"
        );
    }

    /// The `PASTEC05` payload shape: `output_roots` present but no
    /// `packages_segments` — the manifest as serialized before loader-URL
    /// history existed. Bincode is positional, so a cache carrying this
    /// layout cannot decode into the current `Manifest`.
    #[derive(Serialize)]
    struct PreSegmentsManifestCache {
        format: [u8; 8],
        json_hash: [u8; 16],
        manifest: PreSegmentsManifest,
    }

    #[derive(Serialize)]
    struct PreSegmentsManifest {
        version: String,
        generated: String,
        modules: HashMap<String, String>,
        assets: HashMap<String, AssetEntry>,
        sorted_deps: HashMap<String, Vec<SortedDepEntry>>,
        generated_dirs: Vec<String>,
        hash_urls: bool,
        output_roots: BTreeMap<String, Vec<String>>,
    }

    #[test]
    fn test_pre_segments_cache_payload_falls_back_to_json() {
        // A cache written by a build whose Manifest had no
        // packages_segments field is a shorter payload: decoding it into
        // the current struct fails, the loader drops the unusable .bin,
        // and the authoritative JSON supplies every asset, version, and
        // history entry — with no historical fingerprints recorded.
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut manifest = Manifest::new();
        manifest.version = "fresh".to_string();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash1",
            "/js/app",
            "js",
            vec![],
        );
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash2",
            "/js/app",
            "js",
            vec![],
        );
        manifest.save_json(&json_path).unwrap();
        let json_bytes = fs::read(&json_path).unwrap();
        let expected = manifest.assets["js/app.js"].clone();

        let legacy = PreSegmentsManifestCache {
            format: *b"PASTEC05",
            json_hash: md5::compute(&json_bytes).0,
            manifest: PreSegmentsManifest {
                version: "cached".to_string(),
                generated: manifest.generated.clone(),
                modules: manifest.modules.clone(),
                assets: manifest.assets.clone(),
                sorted_deps: manifest.sorted_deps.clone(),
                generated_dirs: manifest.generated_dirs.clone(),
                hash_urls: manifest.hash_urls,
                output_roots: manifest.output_roots.clone(),
            },
        };
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        opts.serialize_into(BufWriter::new(File::create(&bin_path).unwrap()), &legacy)
            .unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.version, "fresh", "the JSON stays authoritative");
        assert_eq!(
            loaded.assets["js/app.js"], expected,
            "assets, versions, and history survive the fallback intact"
        );
        assert!(
            loaded.packages_segments.is_empty(),
            "a pre-field manifest knows no historical fingerprints"
        );
        assert!(
            !bin_path.exists(),
            "a payload that cannot decode is dropped rather than retried every load"
        );
    }

    /// The `PASTEC06` payload shape: `packages_segments` present with
    /// key/version pins — the manifest as serialized before a pin carried
    /// a content hash. Bincode is positional, so a cache carrying this
    /// layout cannot decode into the current `SegmentPin`.
    #[derive(Serialize)]
    struct PreHashManifestCache {
        format: [u8; 8],
        json_hash: [u8; 16],
        manifest: PreHashManifest,
    }

    #[derive(Serialize)]
    struct PreHashPin {
        key: String,
        version: u32,
    }

    #[derive(Serialize)]
    struct PreHashManifest {
        version: String,
        generated: String,
        modules: HashMap<String, String>,
        assets: HashMap<String, AssetEntry>,
        sorted_deps: HashMap<String, Vec<SortedDepEntry>>,
        generated_dirs: Vec<String>,
        hash_urls: bool,
        output_roots: BTreeMap<String, Vec<String>>,
        packages_segments: BTreeMap<String, BTreeMap<String, PreHashPin>>,
    }

    #[test]
    fn test_pre_hash_pins_cache_payload_falls_back_to_json() {
        // A cache written by a build whose SegmentPin had no hash field is
        // a differently-shaped payload: decoding it into the current
        // struct fails, the loader drops the unusable .bin, and the
        // authoritative JSON supplies every asset, history entry, and
        // loader pin.
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut manifest = Manifest::new();
        manifest.version = "fresh".to_string();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash1",
            "/js/app",
            "js",
            vec![],
        );
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash2",
            "/js/app",
            "js",
            vec![],
        );
        manifest
            .output_roots
            .insert("js".to_string(), vec!["dist/js".to_string()]);
        manifest.record_packages_segment("0123456789ab").unwrap();
        manifest.save_json(&json_path).unwrap();
        let json_bytes = fs::read(&json_path).unwrap();
        let expected = manifest.assets["js/app.js"].clone();
        let expected_segments = manifest.packages_segments.clone();

        let legacy = PreHashManifestCache {
            format: *b"PASTEC06",
            json_hash: md5::compute(&json_bytes).0,
            manifest: PreHashManifest {
                version: "cached".to_string(),
                generated: manifest.generated.clone(),
                modules: manifest.modules.clone(),
                assets: manifest.assets.clone(),
                sorted_deps: manifest.sorted_deps.clone(),
                generated_dirs: manifest.generated_dirs.clone(),
                hash_urls: manifest.hash_urls,
                output_roots: manifest.output_roots.clone(),
                packages_segments: manifest
                    .packages_segments
                    .iter()
                    .map(|(segment, pins)| {
                        (
                            segment.clone(),
                            pins.iter()
                                .map(|(name, pin)| {
                                    (
                                        name.clone(),
                                        PreHashPin {
                                            key: pin.key.clone(),
                                            version: pin.version,
                                        },
                                    )
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            },
        };
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        opts.serialize_into(BufWriter::new(File::create(&bin_path).unwrap()), &legacy)
            .unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.version, "fresh", "the JSON stays authoritative");
        assert_eq!(
            loaded.assets["js/app.js"], expected,
            "assets, versions, and history survive the fallback intact"
        );
        assert_eq!(
            loaded.packages_segments, expected_segments,
            "loader metadata survives the fallback intact"
        );
        assert!(
            !bin_path.exists(),
            "a payload that cannot decode is dropped rather than retried every load"
        );
    }

    /// The `PASTEC07` payload shape: `packages_segments` with
    /// key/version/hash pins but no `cdn_base` — the manifest as
    /// serialized before the build's default origin was recorded. Bincode
    /// is positional, so a cache carrying this layout cannot decode into
    /// the current `Manifest`.
    #[derive(Serialize)]
    struct PreCdnManifestCache {
        format: [u8; 8],
        json_hash: [u8; 16],
        manifest: PreCdnManifest,
    }

    #[derive(Serialize)]
    struct PreCdnManifest {
        version: String,
        generated: String,
        modules: HashMap<String, String>,
        assets: HashMap<String, AssetEntry>,
        sorted_deps: HashMap<String, Vec<SortedDepEntry>>,
        generated_dirs: Vec<String>,
        hash_urls: bool,
        output_roots: BTreeMap<String, Vec<String>>,
        packages_segments: BTreeMap<String, BTreeMap<String, SegmentPin>>,
    }

    #[test]
    fn test_pre_cdn_cache_payload_falls_back_to_json() {
        // A cache written by a build whose Manifest had no cdn_base field
        // is a shorter payload: decoding it into the current struct fails,
        // the loader drops the unusable .bin, and the authoritative JSON
        // supplies the origin the stale cache could not carry.
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut manifest = Manifest::new();
        manifest.version = "fresh".to_string();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash1",
            "/js/app",
            "js",
            vec![],
        );
        manifest
            .output_roots
            .insert("js".to_string(), vec!["dist/js".to_string()]);
        manifest.record_packages_segment("0123456789ab").unwrap();
        manifest.cdn_base = Some("https://cdn.example/assets".to_string());
        manifest.save_json(&json_path).unwrap();
        let json_bytes = fs::read(&json_path).unwrap();
        let expected = manifest.assets["js/app.js"].clone();
        let expected_segments = manifest.packages_segments.clone();

        let legacy = PreCdnManifestCache {
            format: *b"PASTEC07",
            // Bound to the current JSON's digest: only the payload shape
            // predates cdn_base.
            json_hash: md5::compute(&json_bytes).0,
            manifest: PreCdnManifest {
                version: "cached".to_string(),
                generated: manifest.generated.clone(),
                modules: manifest.modules.clone(),
                assets: manifest.assets.clone(),
                sorted_deps: manifest.sorted_deps.clone(),
                generated_dirs: manifest.generated_dirs.clone(),
                hash_urls: manifest.hash_urls,
                output_roots: manifest.output_roots.clone(),
                packages_segments: manifest.packages_segments.clone(),
            },
        };
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        opts.serialize_into(BufWriter::new(File::create(&bin_path).unwrap()), &legacy)
            .unwrap();

        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.version, "fresh", "the JSON stays authoritative");
        assert_eq!(
            loaded.cdn_base.as_deref(),
            Some("https://cdn.example/assets"),
            "the JSON's cdn_base is read, not a stale cache's missing field"
        );
        assert_eq!(
            loaded.assets["js/app.js"], expected,
            "assets, versions, and history survive the fallback intact"
        );
        assert_eq!(
            loaded.packages_segments, expected_segments,
            "loader metadata survives the fallback intact"
        );
        assert!(
            !bin_path.exists(),
            "a payload that cannot decode is dropped rather than retried every load"
        );
    }

    #[test]
    fn test_pre_hash_json_pins_parse_but_cannot_serve() {
        // JSON written before pins carried a hash still parses — serde's
        // default fills the missing field — but a pin without a verifiable
        // content identity can never serve and pruning removes it rather
        // than letting it resolve a same-key/same-version substitute.
        let json = r#"{
            "version": "1",
            "generated": "2026-01-01T00:00:00Z",
            "modules": {},
            "assets": {
                "js/app.js": {
                    "source": "src/app.js",
                    "output": "/js/app.h.v1.min.js",
                    "hash": "0123456789abcdef0123456789abcdef",
                    "version": 1,
                    "timestamp": "2026-01-01T00:00:00Z"
                }
            },
            "sorted_deps": {},
            "packages_segments": {
                "0123456789ab": {
                    "app": {"key": "js/app.js", "version": 1}
                }
            }
        }"#;
        let mut manifest: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.packages_segments["0123456789ab"]["app"].hash, "");

        manifest.prune_packages_segments();
        assert!(
            manifest.packages_segments.is_empty(),
            "a pin with no verifiable identity is pruned, map and all"
        );
    }

    #[test]
    fn test_prune_packages_segments_drops_dead_pins_and_empty_maps() {
        // Retention prunes individually: a pin survives only while its
        // version is still recorded at its hash — current or retained
        // history. Dead pins leave the map; a map with none left is
        // dropped.
        let mut manifest = Manifest::new();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "0123456789abcdef0123456789abcdef",
            "/js/app",
            "js",
            vec![],
        );
        update_asset(
            &mut manifest,
            "js/lib.js",
            "src/lib.js",
            "fedcba9876543210fedcba9876543210",
            "/js/lib",
            "js",
            vec![],
        );
        update_asset(
            &mut manifest,
            "js/lib.js",
            "src/lib.js",
            "001122334455667788990011aabbccdd",
            "/js/lib",
            "js",
            vec![],
        );
        let live = SegmentPin {
            key: "js/app.js".to_string(),
            version: 1,
            hash: "0123456789abcdef0123456789abcdef".to_string(),
        };
        let historical = SegmentPin {
            key: "js/lib.js".to_string(),
            version: 1,
            hash: "fedcba9876543210fedcba9876543210".to_string(),
        };
        let dead = SegmentPin {
            key: "js/lib.js".to_string(),
            // Version matches but the content moved on — the pin must not
            // resolve the rebuilt bytes.
            version: 2,
            hash: "ffffffffffffffffffffffffffffffff".to_string(),
        };
        manifest.packages_segments.insert(
            "aaaaaaaaaaaa".to_string(),
            [
                ("app".to_string(), live.clone()),
                ("lib".to_string(), historical.clone()),
                ("lib2".to_string(), dead),
            ]
            .into_iter()
            .collect(),
        );
        manifest.packages_segments.insert(
            "bbbbbbbbbbbb".to_string(),
            [(
                "gone".to_string(),
                SegmentPin {
                    key: "js/gone.js".to_string(),
                    version: 1,
                    hash: "0123456789abcdef0123456789abcdef".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        );

        manifest.prune_packages_segments();

        let pins = &manifest.packages_segments["aaaaaaaaaaaa"];
        assert_eq!(pins.get("app"), Some(&live));
        assert_eq!(
            pins.get("lib"),
            Some(&historical),
            "a retained history version still verifies its pin"
        );
        assert!(
            !pins.contains_key("lib2"),
            "a same-version different-hash pin is dead"
        );
        assert!(
            !manifest.packages_segments.contains_key("bbbbbbbbbbbb"),
            "a segment with no serving pins left is dropped"
        );
    }

    #[test]
    fn test_record_packages_segment_refuses_conflicting_pins() {
        // A recorded fingerprint is immutable: it names one pin set for
        // every saved page holding that URL. Recording the same segment
        // under different pins is an error, and the original record
        // survives — a silent overwrite would retarget those pages.
        let mut manifest = Manifest::new();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "0123456789abcdef0123456789abcdef",
            "/js/app",
            "js",
            vec![],
        );
        manifest.record_packages_segment("0123456789ab").unwrap();
        let original = manifest.packages_segments["0123456789ab"].clone();

        // The same pins again are idempotent — an unchanged state records
        // over its own record without complaint.
        manifest.record_packages_segment("0123456789ab").unwrap();
        assert_eq!(manifest.packages_segments["0123456789ab"], original);

        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "fedcba9876543210fedcba9876543210",
            "/js/app",
            "js",
            vec![],
        );
        assert!(
            manifest.record_packages_segment("0123456789ab").is_err(),
            "a different pin set under one fingerprint must error"
        );
        assert_eq!(manifest.packages_segments["0123456789ab"], original);
    }

    #[test]
    fn test_manifest_load_without_json_ignores_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        // A cache with no JSON beside it binds to nothing — the manifest is
        // a fresh empty one, not the cached content.
        let mut manifest = Manifest::new();
        manifest.version = "cached".to_string();
        manifest.save(&json_path).unwrap();
        std::fs::remove_file(&json_path).unwrap();
        assert!(bin_path.exists(), "Precondition: cache outlives the JSON");

        let loaded = Manifest::load(&json_path).unwrap();
        assert_ne!(
            loaded.version, "cached",
            "a .bin without its JSON must not be trusted"
        );
        assert!(loaded.assets.is_empty(), "a fresh manifest has no assets");
    }

    #[test]
    fn test_manifest_load_propagates_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        // Even with a valid cache beside it, corrupt JSON is an error —
        // never a silent reset that loses version history.
        let mut manifest = Manifest::new();
        manifest.version = "cached".to_string();
        manifest.save(&json_path).unwrap();
        assert!(bin_path.exists());
        let corrupt = b"{ not json";
        std::fs::write(&json_path, corrupt).unwrap();

        assert!(
            Manifest::load(&json_path).is_err(),
            "corrupt JSON must be an error"
        );
        assert_eq!(
            fs::read(&json_path).unwrap(),
            corrupt,
            "a failed load must not modify the JSON"
        );
    }

    #[test]
    fn test_retired_entry_resolves_only_at_an_exact_pin() {
        let mut manifest = Manifest::new();
        manifest.assets.insert(
            "js/old.js".to_string(),
            AssetEntry {
                source: "src/old.js".to_string(),
                output: "/js/old.bbb.v2.min.js".to_string(),
                hash: "bbb".to_string(),
                version: 2,
                timestamp: STAMP_TWO.to_string(),
                dependencies: vec![],
                history: vec![AssetVersion {
                    hash: "aaa".to_string(),
                    version: 1,
                    output: "/js/old.aaa.v1.min.js".to_string(),
                    timestamp: STAMP_ONE.to_string(),
                }],
                retired: true,
            },
        );
        manifest
            .modules
            .insert("old".to_string(), "js/old.js".to_string());

        // Unpinned lookups see the active set only — the retired alias in
        // the module map does not leak into rendering or loader requests.
        assert!(manifest.asset_key_for_module("old", "js").is_none());
        assert!(manifest.output_for_version("old", "js", None).is_none());
        // An exact pin resolves the retired entry.
        let pinned = manifest.resolve_version("old", "js", Some(2)).unwrap();
        assert_eq!(pinned.output, "/js/old.bbb.v2.min.js");
        assert_eq!(
            manifest
                .resolve_version("old", "js", Some(1))
                .unwrap()
                .output,
            "/js/old.aaa.v1.min.js"
        );
        assert!(manifest.resolve_version("old", "js", Some(9)).is_none());
    }

    #[test]
    fn test_seeded_ambiguous_name_pin_fails_instead_of_guessing() {
        // A manifest written before logical-name pins existed can hold
        // two different assets that both claim `site.theme` at v1 with
        // different bytes — a `site.theme+v1` request cannot be
        // attributed to one identity, so it must fail rather than serve
        // whichever claimant happened to seed first.
        let mut manifest = Manifest::new();
        for (key, hash, output) in [
            ("css/theme.css", "aaa", "/css/theme.aaa.v1.min.css"),
            (
                "css/site/theme.css",
                "bbb",
                "/css/site/theme.bbb.v1.min.css",
            ),
        ] {
            manifest.assets.insert(
                key.to_string(),
                AssetEntry {
                    source: format!("src/{key}"),
                    output: output.to_string(),
                    hash: hash.to_string(),
                    version: 1,
                    timestamp: STAMP_ONE.to_string(),
                    dependencies: vec![],
                    history: vec![],
                    retired: false,
                },
            );
        }
        // The declared binding points at the first claimant; the second
        // claims the same spelling by path convention.
        manifest
            .modules
            .insert("site.theme".to_string(), "css/theme.css".to_string());

        manifest.seed_historical_names();

        assert!(
            manifest.ambiguous_name_pins.contains("css:site.theme:1"),
            "the disputed slot is recorded as unattributable"
        );
        assert!(
            manifest
                .resolve_version("site.theme", "css", Some(1))
                .is_none(),
            "a name+version two identities claim cannot pick one"
        );
        // The marker is what blocks the fall-through — the unpinned
        // current binding stays available on its own terms.
        assert!(manifest
            .resolve_version("site.theme", "css", None)
            .is_some());
        // An unrelated version still fails on its own: v2 was never
        // recorded for the name at all.
        assert!(manifest
            .resolve_version("site.theme", "css", Some(2))
            .is_none());
    }

    #[test]
    fn test_same_version_name_reuse_resolves_the_recorded_identity() {
        // If a name is ever bound to two different assets at the SAME
        // version — a state builds refuse to mint but a hand-edited
        // manifest could carry — the recorded pin, not the current
        // binding, decides which asset `name+vN` serves.
        let mut manifest = Manifest::new();
        manifest.assets.insert(
            "css/theme.css".to_string(),
            AssetEntry {
                source: "src/theme.scss".to_string(),
                output: "/css/theme.aaa.v1.min.css".to_string(),
                hash: "aaa".to_string(),
                version: 1,
                timestamp: STAMP_ONE.to_string(),
                dependencies: vec![],
                history: vec![],
                retired: true,
            },
        );
        manifest.assets.insert(
            "css/site/theme.css".to_string(),
            AssetEntry {
                source: "src/site/theme.scss".to_string(),
                output: "/css/site/theme.bbb.v1.min.css".to_string(),
                hash: "bbb".to_string(),
                version: 1,
                timestamp: STAMP_ONE.to_string(),
                dependencies: vec![],
                history: vec![],
                retired: false,
            },
        );
        manifest
            .modules
            .insert("site.theme".to_string(), "css/site/theme.css".to_string());
        // The recorded pin says `site.theme+v1` meant the first asset.
        manifest.historical_names.insert(
            "css:site.theme".to_string(),
            BTreeMap::from([(
                1,
                SegmentPin {
                    key: "css/theme.css".to_string(),
                    version: 1,
                    hash: "aaa".to_string(),
                },
            )]),
        );

        let resolved = manifest
            .resolve_version("site.theme", "css", Some(1))
            .unwrap();
        assert_eq!(resolved.hash, "aaa");
        assert_eq!(resolved.output, "/css/theme.aaa.v1.min.css");
        // The current binding's own version resolves under its recorded
        // pin only — there is none, and the retired asset's pin keeps the
        // slot claimed, so this lookup cannot land on the wrong bytes.
    }

    #[test]
    fn test_name_rebind_to_identical_bytes_claims_a_new_version() {
        // `theme` moves from a.scss to b.scss with byte-identical output
        // and a different stamp. The v1 pin still resolves through
        // css/a.css's recorded path, so reusing `theme+v1` for the new
        // key would emit a URL whose name+version resolves to the other
        // asset's output — a fresh URL that immediately 404s. The slot
        // is taken by identity (key), not only by bytes: the rebind
        // claims v2 and both URLs keep serving their own files.
        let mut manifest = Manifest::new();
        manifest
            .modules
            .insert("theme".to_string(), "css/a.css".to_string());
        manifest
            .update_asset_with_history(AssetUpdate {
                module_name: Some("theme"),
                ..asset_update("css/a.css", "src/a.scss", "same", "/css/a", "css", vec![])
            })
            .unwrap();
        manifest.record_name_pins().unwrap();

        manifest
            .modules
            .insert("theme".to_string(), "css/b.css".to_string());
        manifest
            .update_asset_with_history(AssetUpdate {
                module_name: Some("theme"),
                stamp: stamp(STAMP_TWO),
                ..asset_update("css/b.css", "src/b.scss", "same", "/css/b", "css", vec![])
            })
            .unwrap();
        manifest.record_name_pins().unwrap();

        let b = &manifest.assets["css/b.css"];
        assert_eq!(
            b.version, 2,
            "a slot pinned to another asset key is taken even at equal bytes"
        );
        assert_eq!(b.output, "/css/b.same.v2.min.css");

        let retained = manifest.resolve_version("theme", "css", Some(1)).unwrap();
        assert_eq!(
            retained.output, "/css/a.same.v1.min.css",
            "a page holding theme+v1 keeps the asset that issued it"
        );
        let emitted = manifest.resolve_version("theme", "css", Some(2)).unwrap();
        assert_eq!(emitted.output, "/css/b.same.v2.min.css");
        let current = manifest.resolve_version("theme", "css", None).unwrap();
        assert_eq!(current.output, "/css/b.same.v2.min.css");
    }

    #[test]
    fn test_manifests_written_before_retirement_stay_active() {
        // `retired` is absent in manifests written by older builds; those
        // entries must deserialize as active, not as retired.
        let entry: AssetEntry = serde_json::from_str(
            r#"{"source":"src/a.js","output":"/js/a.x.v1.min.js","hash":"x",
            "version":1,"timestamp":"2024-01-15T10:00:00+00:00",
            "dependencies":[],"history":[]}"#,
        )
        .unwrap();
        assert!(!entry.retired);
        // The field is always written: the bincode cache is positional, so
        // an omitted field would shift its layout.
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"retired\":false"));
    }

    #[test]
    fn test_bin_cache_without_format_marker_is_ignored() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("manifest.json");
        let bin_path = dir.path().join("manifest.bin");

        let mut manifest = Manifest::new();
        manifest.version = "json".to_string();
        update_asset(
            &mut manifest,
            "js/app.js",
            "src/app.js",
            "hash",
            "/js/app",
            "js",
            vec![],
        );
        manifest.save(&json_path).unwrap();

        // A cache written by an older build has no format header — it
        // cannot decode into the marker-checked shape, so load falls back
        // to the JSON instead of trusting it.
        let old_format = bincode::serialize(&manifest).unwrap();
        fs::write(&bin_path, old_format).unwrap();
        let loaded = Manifest::load(&json_path).unwrap();
        assert_eq!(loaded.version, "json");
        assert!(loaded.assets.contains_key("js/app.js"));
    }

    #[test]
    fn test_dependency_change_alone_creates_a_new_version() {
        let mut manifest = Manifest::new();
        update_asset(
            &mut manifest,
            "js/site.js",
            "src/site.js",
            "samehash",
            "/js/site",
            "js",
            vec!["dep.a".to_string()],
        );
        // Identical bytes with a new dependency list are still a new
        // version — the dep ordering is semantic content.
        update_asset(
            &mut manifest,
            "js/site.js",
            "src/site.js",
            "samehash",
            "/js/site",
            "js",
            vec!["dep.b".to_string()],
        );
        let entry = &manifest.assets["js/site.js"];
        assert_eq!(entry.version, 2);
        assert_eq!(entry.hash, "samehash");
        assert_eq!(entry.history.len(), 1);
        assert_eq!(entry.history[0].version, 1);
        // The same deps again leave the version alone.
        update_asset(
            &mut manifest,
            "js/site.js",
            "src/site.js",
            "samehash",
            "/js/site",
            "js",
            vec!["dep.b".to_string()],
        );
        assert_eq!(manifest.assets["js/site.js"].version, 2);
        // A re-seen retired key clears the flag and keeps its sequence.
        manifest.assets.get_mut("js/site.js").unwrap().retired = true;
        update_asset(
            &mut manifest,
            "js/site.js",
            "src/site.js",
            "otherhash",
            "/js/site",
            "js",
            vec![],
        );
        let entry = &manifest.assets["js/site.js"];
        assert!(!entry.retired);
        assert_eq!(entry.version, 3);
    }

    // ============================================================
    // Hash-qualified URL policy
    // ============================================================

    #[test]
    fn test_new_manifest_defaults_hash_urls_on() {
        assert!(Manifest::new().hash_urls);
    }

    #[test]
    fn test_old_json_without_hash_urls_deserializes_as_legacy() {
        // A manifest written before the field existed must keep serving and
        // emitting legacy URLs — serde defaults the missing field to false.
        let manifest = Manifest::new();
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(json.contains("\"hash_urls\":true"), "{json}");

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("hash_urls");
        let old: Manifest = serde_json::from_value(value).unwrap();
        assert!(
            !old.hash_urls,
            "a pre-hash manifest missing the field must stay legacy"
        );
    }

    #[test]
    fn test_hash_url_toggle_preserves_assets_and_stamp() {
        // Toggling the policy changes rendered tokens only — the asset
        // records the build wrote are untouched.
        let mut manifest = create_test_manifest_with_timestamps();
        manifest.hash_urls = true;
        // Real recorded hashes — the qualified token needs them.
        manifest.assets.get_mut("js/paste/dom.js").unwrap().hash =
            format!("{:x}", md5::compute(b"dom bytes"));
        manifest.assets.get_mut("js/paste/event.js").unwrap().hash =
            format!("{:x}", md5::compute(b"event bytes"));

        let assets = manifest.assets.clone();
        let sorted = manifest.sorted_deps.clone();

        let qualified = manifest
            .jam_url(&["paste.dom", "paste.event"], "js", "/paste", false)
            .unwrap();
        let dom_hash = &manifest.assets["js/paste/dom.js"].hash;
        let event_hash = &manifest.assets["js/paste/event.js"].hash;
        assert!(
            qualified.contains(&format!("paste.dom+v1.0-h{dom_hash}")),
            "{qualified}"
        );
        assert!(
            qualified.contains(&format!("paste.event+v2.0-h{event_hash}")),
            "{qualified}"
        );
        assert_eq!(dom_hash.len(), 32);
        // The segment is still the max recorded stamp — event's, not dom's.
        let stamp = stamp_epoch("2024-01-16T14:00:00+00:00").unwrap();
        assert!(
            qualified.starts_with(&format!("/paste/{stamp}/")),
            "{qualified}"
        );

        manifest.hash_urls = false;
        let legacy = manifest
            .jam_url(&["paste.dom", "paste.event"], "js", "/paste", false)
            .unwrap();
        assert_eq!(
            legacy,
            format!("/paste/{stamp}/paste.dom+v1.0~paste.event+v2.0.min.js")
        );

        // The toggle itself wrote nothing into the asset records.
        assert_eq!(manifest.assets, assets);
        assert_eq!(manifest.sorted_deps, sorted);
        assert!(!manifest.hash_urls);
    }

    #[test]
    fn test_jam_url_invalid_hash_is_an_error_under_qualified_policy() {
        // A manifest claiming qualified URLs but recording a bad hash cannot
        // fall back to an unqualified token — the render is an error.
        let mut manifest = create_test_manifest_with_timestamps();
        manifest.hash_urls = true;
        let err = manifest
            .jam_url(&["paste.dom"], "js", "/paste", false)
            .unwrap_err();
        assert!(
            format!("{err}").contains("content hash"),
            "expected the hash error, got: {err}"
        );
    }

    #[test]
    fn test_version_suffix_forms() {
        let hash = "0123456789abcdef0123456789ABCDEF";
        assert_eq!(
            version_suffix(3, hash, true).unwrap(),
            "+v3.0-h0123456789abcdef0123456789abcdef"
        );
        assert_eq!(version_suffix(3, hash, false).unwrap(), "+v3.0");
        assert!(version_suffix(1, "h", true).is_err());
        assert!(version_suffix(1, "0123456789abcdef0123456789abc", true).is_err());
        assert!(version_suffix(1, "0123456789abcdef0123456789abcg", true).is_err());
        assert!(is_content_hash("0123456789abcdef0123456789abcdef"));
        assert!(!is_content_hash(""));
        assert!(!is_content_hash("0123456789abcdef0123456789abcde"));
    }

    #[test]
    fn test_jam_url_with_limit_segments_long_lists() {
        // Each component — token run plus the reserved suffix — stays under
        // the limit; the list continues over `/`-separated components.
        let mut manifest = create_test_manifest_with_timestamps();
        manifest.hash_urls = true;
        manifest.assets.get_mut("js/paste/dom.js").unwrap().hash =
            format!("{:x}", md5::compute(b"dom"));
        manifest.assets.get_mut("js/paste/event.js").unwrap().hash =
            format!("{:x}", md5::compute(b"event"));

        let unlimited = manifest
            .jam_url(&["paste.dom", "paste.event"], "js", "/paste", false)
            .unwrap();
        // Without a limit the list is one component: /paste/<stamp>/<list>.
        assert_eq!(unlimited.matches('/').count(), 3, "{unlimited}");

        // A limit smaller than the single-segment list splits it.
        let segmented = manifest
            .jam_url_with_limit(
                &["paste.dom", "paste.event"],
                "js",
                "/paste",
                false,
                Some(100),
            )
            .unwrap();
        let stamp = stamp_epoch("2024-01-16T14:00:00+00:00").unwrap();
        let prefix = format!("/paste/{stamp}/");
        let after_base = segmented
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("unexpected URL {segmented}"));
        assert_eq!(after_base.matches('/').count(), 1, "{segmented}");
        assert!(segmented.ends_with(".min.js"));
        for component in segmented.split('/').skip(1) {
            assert!(
                component.len() <= 100,
                "{component} too long in {segmented}"
            );
        }
        assert!(after_base.contains("paste.dom+v1.0-h"), "{segmented}");
        assert!(after_base.contains("paste.event+v2.0-h"), "{segmented}");

        // A token too long for one component alone is a hard error — never
        // truncated or split mid-token.
        let err = manifest
            .jam_url_with_limit(
                &["paste.dom", "paste.event"],
                "js",
                "/paste",
                false,
                Some(20),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("component limit"), "{err}");
    }

    /// `Manifest::load("manifest.json")` must open the file in the working
    /// directory — `Path::parent` answers `Some("")` for a bare name, and an
    /// empty path is not a directory `openat` accepts. The child owns the
    /// cwd change so parallel tests keep theirs.
    #[test]
    fn bare_relative_manifest_paths() {
        const CHILD_FLAG: &str = "PASTE_BARE_RELATIVE_MANIFEST_CHILD";
        if std::env::var_os(CHILD_FLAG).is_some() {
            let mut manifest = Manifest::new();
            manifest.version = "sentinel-version".to_string();
            manifest.save_json(Path::new("manifest.json")).unwrap();
            manifest.save(Path::new("manifest-full.json")).unwrap();

            for name in ["manifest.json", "manifest-full.json"] {
                let loaded = Manifest::load(Path::new(name)).unwrap();
                assert_eq!(loaded.version, "sentinel-version", "{name}");
                let loaded = Manifest::load_json(Path::new(name)).unwrap();
                assert_eq!(loaded.version, "sentinel-version", "{name}");
            }
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "manifest::tests::bare_relative_manifest_paths",
                "--exact",
                "--test-threads=1",
            ])
            .env(CHILD_FLAG, "1")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "bare-path child failed: {status}");
    }
}
