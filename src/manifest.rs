use anyhow::Result;
use bincode::Options;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// unpinned lookups and never appear in a rendered group or rollup.
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
    /// How many old versions to retain (should match storage.keep_versions)
    pub max_history: usize,
    /// Stamp for a new version and for backfilling a carried-forward entry
    /// that has none — the build computes it from the source file's mtime,
    /// clamped to SOURCE_DATE_EPOCH when set
    pub stamp: DateTime<Utc>,
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
    /// Whether the resolved version is the current one
    pub current: bool,
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
}

/// Fixed header identifying the private bincode cache schema — bumped when
/// the serialized shape changes, so a cache written by a build with a
/// different layout never decodes into this one. An internal cache marker,
/// not a package or release version.
const MANIFEST_CACHE_FORMAT: [u8; 8] = *b"PASTEC04";

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
        let json = match fs::read(path) {
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
        if let Ok(file) = File::open(&bin_path) {
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
                    let _ = fs::remove_file(&bin_path);
                }
            }
        }

        Self::from_json_reader(json.as_slice())
    }

    /// Load manifest from its JSON form only.
    ///
    /// Request-path loaders (`render`, `combo`) use this: `load`
    /// prefers a `.bin` cache beside the path and deletes it when stale —
    /// side effects a read path must not have.
    pub fn load_json(path: &Path) -> Result<Self> {
        Self::from_json_reader(File::open(path)?)
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
    /// every unpinned lookup (rendering, rollups, loader URLs) goes through
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
    pub(crate) fn resolve_version(
        &self,
        module: &str,
        asset_type: &str,
        version: Option<u32>,
    ) -> Option<ResolvedVersion<'_>> {
        // A pin names an exact version — a retired asset may still serve it.
        // An unpinned lookup sees the active set only, so a retired module
        // cannot enter a fresh page or loader request.
        let entry = self
            .asset_key(module, asset_type, version.is_some())
            .and_then(|key| self.assets.get(&key))?;
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
                        current: false,
                    })
            }
            _ => Some(ResolvedVersion {
                version: entry.version,
                output: &entry.output,
                hash: &entry.hash,
                timestamp: &entry.timestamp,
                // A retired asset's pinned current version is historical for
                // ordering: the canonical list keeps the order the URL was
                // written in rather than treating it as an active member.
                current: !entry.retired,
            }),
        }
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

    /// Refuse to persist an asset whose recorded output carries the other
    /// type's path prefix: `OutputDirs::compiled_file` routes an output
    /// starting with `js/` or `css/` through that type's output directory,
    /// so a `js/` asset recorded under `/css/…` would be read from a tree
    /// the build never wrote. Outputs without a type prefix — beside-source
    /// layout, nested output roots — resolve under the base and are out of
    /// scope here.
    fn validate_output_prefixes(&self) -> Result<()> {
        for (key, entry) in &self.assets {
            let Some(kind) = key.split_once('/').map(|(t, _)| t) else {
                continue;
            };
            if !matches!(kind, "js" | "css") {
                continue;
            }
            for output in
                std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
            {
                let trimmed = output.trim_start_matches('/');
                let prefix = trimmed.split('/').next().unwrap_or("");
                if matches!(prefix, "js" | "css") && prefix != kind {
                    anyhow::bail!(
                        "manifest asset {key} records output {output} under the {prefix}/ prefix — a {kind}/ asset resolves through {kind}/ only"
                    );
                }
            }
        }
        Ok(())
    }

    /// The canonical JSON bytes of this manifest: sorted keys, pretty
    /// printed. `save` writes exactly these bytes and binds the bincode
    /// cache to their digest, so `load` accepts a cache only while it still
    /// describes the JSON on disk — a read-time check, not a guarantee
    /// covering builds that write the pair concurrently.
    fn canonical_json(&self) -> Result<Vec<u8>> {
        self.validate_output_prefixes()?;
        let mut value = serde_json::to_value(self)?;
        canonicalize_json(&mut value);
        Ok(serde_json::to_string_pretty(&value)?.into_bytes())
    }

    /// Write `json` to `path` atomically (write-to-temp + rename) so a
    /// partial write can never corrupt a manifest readers already hold.
    fn write_json(path: &Path, json: &[u8]) -> Result<()> {
        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json_tmp = path.with_extension("json.tmp");
        fs::write(&json_tmp, json)?;
        fs::rename(&json_tmp, path)?;
        Ok(())
    }

    /// Save manifest to both JSON and bincode formats.
    /// Uses atomic write-to-temp + rename to prevent corruption from
    /// partial writes (e.g., crash mid-write, concurrent server reads).
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = self.canonical_json()?;
        Self::write_json(path, &json)?;

        // Save the bincode cache (fast loading) — atomic via temp file +
        // rename, and bound to the digest of the JSON just written. The
        // BufWriter is flushed explicitly and the error propagated: Drop
        // discards flush errors, which could rename a torn cache into place.
        let bin_path = path.with_extension("bin");
        let bin_tmp = path.with_extension("bin.tmp");
        let file = File::create(&bin_tmp)?;
        let mut writer = BufWriter::new(file);
        let cache = ManifestCache {
            format: MANIFEST_CACHE_FORMAT,
            json_hash: md5::compute(&json).0,
            manifest: self.clone(),
        };
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        opts.serialize_into(&mut writer, &cache)?;
        writer.flush()?;
        fs::rename(&bin_tmp, &bin_path)?;

        Ok(())
    }

    /// Save only the JSON manifest, and drop any bincode cache beside it.
    ///
    /// Distributed output is committed and diffed, so it carries the
    /// reproducible artifact only: the bincode file is a load-time cache written
    /// from hash maps, and its bytes vary between runs. It is removed rather
    /// than left behind, because `load` prefers it and would otherwise serve a
    /// stale manifest for every later build.
    pub fn save_json(&self, path: &Path) -> Result<()> {
        Self::write_json(path, &self.canonical_json()?)
    }

    /// Remove the bincode cache beside `path`, if one exists.
    pub fn remove_cache(path: &Path) -> Result<()> {
        let bin_path = path.with_extension("bin");
        if bin_path.exists() {
            fs::remove_file(bin_path)?;
        }
        Ok(())
    }

    /// Update or create asset entry, preserving version history
    /// Output format: {name}.{hash}.v{version}.min.{ext}
    /// `update.max_history` controls how many old versions to retain (should match storage.keep_versions)
    /// `update.stamp` is the version's last-modified stamp: the source file's
    /// mtime clamped to SOURCE_DATE_EPOCH, computed once per asset at the
    /// build boundary. A new version takes it; an unchanged hash keeps the
    /// stamp it already carries.
    pub fn update_asset_with_history(&mut self, update: AssetUpdate<'_>) -> &AssetEntry {
        let AssetUpdate {
            key,
            source,
            hash,
            output_base,
            ext,
            dependencies,
            max_history,
            stamp,
        } = update;
        let timestamp = stamp.to_rfc3339();

        let existing = self.assets.get(key);

        // Unchanged means the same output bytes *and* the same dependency
        // metadata: a dependency-only edit still versions the asset — the
        // combo list a module expands into is part of what a version means —
        // while an ordinary comment the compiler discards stays unchanged.
        if let Some(entry) = existing {
            if entry.hash == hash && entry.dependencies == dependencies {
                // An unchanged hash keeps the version, but everything else
                // is synchronized: the output prefix follows this build's
                // configured path (a layout or build-dir change relocates
                // the file the version resolves to), the source label and
                // dependencies must not go stale, and history is trimmed to
                // the current retention bound — a manifest written with a
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
                entry.output = format!("{}.{}.v{}.min.{}", output_base, hash, entry.version, ext);
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
                return self.assets.get(key).unwrap();
            }
        }

        let new_version = existing.map_or(1, |e| e.version + 1);

        // Build versioned output path
        let output = format!("{}.{}.v{}.min.{}", output_base, hash, new_version, ext);

        // Preserve history for rolling deploys (keep last max_history
        // entries); each retained version carries the stamp it was created
        // under, backfilled when a manifest from before stamping had none.
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
        self.assets.get(key).unwrap()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture-facing update a test feeds the manifest: tests don't tune
    /// retention, so the default bound of 10 is fixed here, and the stamp
    /// defaults to STAMP_ONE — `update_asset_at` overrides it.
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
        }
    }

    /// `update_asset` with a caller-chosen stamp — for version-history tests.
    fn update_asset_at(manifest: &mut Manifest, stamp: DateTime<Utc>, update: AssetUpdate<'_>) {
        manifest.update_asset_with_history(AssetUpdate { stamp, ..update });
    }

    const STAMP_ONE: &str = "2024-01-15T10:00:00+00:00";
    const STAMP_TWO: &str = "2024-01-16T10:00:00+00:00";
    const STAMP_THREE: &str = "2024-01-17T10:00:00+00:00";

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
        manifest.update_asset_with_history(asset_update(
            key,
            source,
            hash,
            output_base,
            ext,
            dependencies,
        ));
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
    fn test_save_rejects_current_output_under_other_type_prefix() {
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
    fn test_save_rejects_history_output_under_other_type_prefix() {
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
    fn test_save_accepts_output_without_type_prefix() {
        use tempfile::tempdir;

        // Beside-source and nested-root layouts record outputs that carry no
        // js/ or css/ prefix; they resolve under the base directory and are
        // not this validation's concern.
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let mut manifest = Manifest::new();
        manifest.assets.insert(
            "js/app.js".to_string(),
            AssetEntry {
                source: "src/app.js".to_string(),
                output: "src/web/_paste_build/app.h.v1.min.js".to_string(),
                hash: "h".to_string(),
                version: 1,
                timestamp: String::new(),
                dependencies: vec![],
                history: vec![],
                retired: false,
            },
        );

        manifest.save(&path).unwrap();
        assert!(path.exists());
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
        // An exact pin resolves the retired entry, ordered as historical.
        let pinned = manifest.resolve_version("old", "js", Some(2)).unwrap();
        assert!(!pinned.current);
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
}
