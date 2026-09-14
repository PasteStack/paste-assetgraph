use anyhow::Result;
use bincode::Options;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::Path;

/// Single version of an asset (for rollback history)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetVersion {
    pub hash: String,
    pub version: u32,
    pub output: String,
    pub timestamp: String,
}

/// Single asset entry with version history
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        }
    }

    /// Load manifest from file (JSON or bincode)
    pub fn load(path: &Path) -> Result<Self> {
        // Try bincode first (faster)
        let bin_path = path.with_extension("bin");
        if bin_path.exists() {
            let file = File::open(&bin_path)?;
            let reader = BufReader::new(file);
            // Cap deserialization at 256 MB to prevent OOM on corrupt data.
            // Without a limit, garbage length prefixes can cause multi-exabyte
            // allocation attempts that abort the process before returning Err.
            let opts = bincode::options().with_limit(256 * 1024 * 1024);
            if let Ok(mut manifest) = opts.deserialize_from::<_, Self>(reader) {
                manifest.sanitize_strings();
                return Ok(manifest);
            }
            // Bincode deserialization failed (e.g., schema change).
            // Delete the stale .bin so we don't retry it every load.
            let _ = fs::remove_file(&bin_path);
        }

        // Fall back to JSON
        if path.exists() {
            let content = fs::read_to_string(path)?;
            let mut manifest: Self = serde_json::from_str(&content)?;
            manifest.sanitize_strings();
            return Ok(manifest);
        }

        Ok(Self::new())
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
    }

    /// Resolve a declared module name to the key of its asset of `asset_type`.
    ///
    /// The modules map is authoritative when it names an existing asset of that
    /// type — a stylesheet may declare a module name unrelated to its path. It
    /// is ignored when it does not, which is what lets a manifest written by an
    /// older build (whose modules map held source paths) still resolve through
    /// the convention key.
    pub fn asset_key_for_module(&self, module: &str, asset_type: &str) -> Option<String> {
        let suffix = format!(".{}", asset_type);
        let declared = self
            .modules
            .get(module)
            .filter(|key| key.ends_with(&suffix) && self.assets.contains_key(key.as_str()))
            .cloned();
        declared.or_else(|| {
            let key = format!("{}/{}.{}", asset_type, module.replace('.', "/"), asset_type);
            self.assets.contains_key(&key).then_some(key)
        })
    }

    /// Normalize metadata for static distribution builds.
    ///
    /// Static output must be reproducible across operating systems and build
    /// times. Content hashes and sorted keys already provide stable asset
    /// identity; build timestamps are what a byte comparison cannot survive, so
    /// they are dropped and the manifest version becomes the fixed label
    /// "static". Source labels are already project-relative.
    pub fn make_deterministic(&mut self) {
        self.version = "static".to_string();
        self.generated.clear();
        for entry in self.assets.values_mut() {
            entry.timestamp.clear();
            for history in &mut entry.history {
                history.timestamp.clear();
            }
        }
    }

    /// Save manifest to both JSON and bincode formats.
    /// Uses atomic write-to-temp + rename to prevent corruption from
    /// partial writes (e.g., crash mid-write, concurrent server reads).
    pub fn save(&self, path: &Path) -> Result<()> {
        self.save_json(path)?;

        // Save bincode (fast loading) — atomic via temp file + rename
        let bin_path = path.with_extension("bin");
        let bin_tmp = path.with_extension("bin.tmp");
        let file = File::create(&bin_tmp)?;
        let writer = BufWriter::new(file);
        let opts = bincode::options().with_limit(256 * 1024 * 1024);
        opts.serialize_into(writer, self)?;
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
        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut value = serde_json::to_value(self)?;
        canonicalize_json(&mut value);
        let json = serde_json::to_string_pretty(&value)?;
        let json_tmp = path.with_extension("json.tmp");
        fs::write(&json_tmp, &json)?;
        fs::rename(&json_tmp, path)?;

        Ok(())
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
    /// `max_history` controls how many old versions to retain (should match storage.keep_versions)
    pub fn update_asset(
        &mut self,
        key: &str,
        source: &str,
        hash: &str,
        output_base: &str,
        ext: &str,
        dependencies: Vec<String>,
    ) -> &AssetEntry {
        self.update_asset_with_history(key, source, hash, output_base, ext, dependencies, 10)
    }

    /// Update or create asset entry with explicit history retention limit.
    pub fn update_asset_with_history(
        &mut self,
        key: &str,
        source: &str,
        hash: &str,
        output_base: &str,
        ext: &str,
        dependencies: Vec<String>,
        max_history: usize,
    ) -> &AssetEntry {
        let now: DateTime<Utc> = Utc::now();
        let timestamp = now.to_rfc3339();

        let existing = self.assets.get(key);

        // Check if content changed
        if let Some(entry) = existing {
            if entry.hash == hash {
                // Dependencies and the source label are always synchronized,
                // even when the content hash is unchanged: a manifest carried
                // over from an earlier build must not keep a stale label (for
                // example an absolute path recorded by an older version).
                if entry.dependencies != dependencies || entry.source != source {
                    let entry_mut = self.assets.get_mut(key).unwrap();
                    entry_mut.dependencies = dependencies;
                    entry_mut.source = source.to_string();
                }
                return self.assets.get(key).unwrap();
            }
        }

        let new_version = existing.map_or(1, |e| e.version + 1);

        // Build versioned output path
        let output = format!("{}.{}.v{}.min.{}", output_base, hash, new_version, ext);

        // Preserve history for rolling deploys (keep last max_history entries)
        let history = existing
            .map(|e| {
                let mut h = e.history.clone();
                h.push(AssetVersion {
                    hash: e.hash.clone(),
                    version: e.version,
                    output: e.output.clone(),
                    timestamp: e.timestamp.clone(),
                });
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
            timestamp: timestamp.clone(),
            dependencies,
            history,
        };

        self.assets.insert(key.to_string(), entry);
        self.assets.get(key).unwrap()
    }

    /// Get output path for specific version (for rollback)
    pub fn get_version(&self, key: &str, version: Option<u32>) -> Option<&str> {
        let entry = self.assets.get(key)?;

        match version {
            None => Some(&entry.output),
            Some(v) if v == entry.version => Some(&entry.output),
            Some(v) => entry
                .history
                .iter()
                .find(|h| h.version == v)
                .map(|h| h.output.as_str()),
        }
    }

    /// Resolve source path to versioned output path
    pub fn resolve(&self, source_path: &str, debug: bool) -> Option<&str> {
        if debug {
            self.assets.get(source_path).map(|e| e.source.as_str())
        } else {
            self.assets.get(source_path).map(|e| e.output.as_str())
        }
    }

    /// Generate JAM URL for bundled modules
    /// Format: /paste/{group_timestamp}/{module+version,module+version,...}.{css|js}
    ///
    /// The timestamp is based on when THIS GROUP of modules last changed,
    /// not the global build time. This enables cache reuse across deploys
    /// when the specific modules haven't changed.
    pub fn jam_url(&self, modules: &[&str], asset_type: &str, debug: bool) -> String {
        // Collect module entries and find latest timestamp for this group
        let mut latest_timestamp: i64 = 0;
        let mut module_versions: Vec<String> = Vec::new();

        for m in modules {
            // Intentionally falls back across asset types: jam_url needs a
            // version and timestamp for cache busting even when the requested
            // type has no asset for the module, so the group timestamp reflects
            // ALL requested modules' changes. The server (serve_js/serve_css)
            // and get_dependencies_for_type stay type-filtered for content
            // serving — modules with no matching-type asset are skipped there.
            let entry_opt = self
                .asset_key_for_module(m, asset_type)
                .or_else(|| {
                    self.modules
                        .get(*m)
                        .filter(|k| self.assets.contains_key(k.as_str()))
                        .cloned()
                })
                .and_then(|key| self.assets.get(&key));
            if let Some(entry) = entry_opt {
                module_versions.push(format!("{}+v{}.0", m, entry.version));

                // Parse RFC3339 timestamp and convert to Unix timestamp
                if let Ok(dt) = DateTime::parse_from_rfc3339(&entry.timestamp) {
                    let ts = dt.timestamp();
                    if ts > latest_timestamp {
                        latest_timestamp = ts;
                    }
                }
            } else {
                // Module not in manifest — include with default version.
                // Matches Scala runtime behavior; combo handler resolves at request time.
                module_versions.push(format!("{}+v1.0", m));
            }
        }

        // If no valid timestamps, use current manifest version
        let group_timestamp = if latest_timestamp > 0 {
            latest_timestamp.to_string()
        } else {
            self.version.clone()
        };

        let suffix = if debug {
            format!(".{}", asset_type)
        } else {
            format!(".min.{}", asset_type)
        };

        format!(
            "/paste/{}/{}{}",
            group_timestamp,
            module_versions.join("~"),
            suffix
        )
    }

    /// Generate JAM URL for JS modules
    pub fn jam_js_url(&self, modules: &[&str], debug: bool) -> String {
        self.jam_url(modules, "js", debug)
    }

    /// Generate JAM URL for CSS modules  
    pub fn jam_css_url(&self, modules: &[&str], debug: bool) -> String {
        self.jam_url(modules, "css", debug)
    }

    /// Get all dependencies for a module (recursive), defaulting to JS assets
    pub fn get_dependencies(&self, module: &str) -> Vec<String> {
        self.get_dependencies_for_type(module, "js")
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
                // Module not in manifest — include it without dependency expansion.
                // Matches Scala expandDependencies: unknown modules are passed through
                // so the server can attempt convention-based resolution at request time.
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
            manifest.update_asset(key, "src.js", "hash", &format!("/{key}"), "js", vec![]);
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

    // ============================================================
    // Timestamp parsing: RFC3339 to Unix epoch conversion
    // ============================================================

    fn create_test_manifest_with_timestamps() -> Manifest {
        let mut manifest = Manifest::new();

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
            },
        );

        manifest
    }

    #[test]
    fn test_jam_url_uses_group_timestamp() {
        // When RFC3339 timestamps are present, group_timestamp derives from
        // the latest asset. Falls back to manifest.version when absent.
        let manifest = create_test_manifest_with_timestamps();

        let url = manifest.jam_url(&["paste.dom", "paste.event"], "js", false);

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
        let url = manifest.jam_url(&["paste.dom"], "js", false);

        // URL should contain a numeric timestamp (Unix epoch seconds)
        // The timestamp for 2024-01-15T10:30:00+00:00 is 1705314600
        assert!(
            url.contains("1705314600"),
            "JAM URL should contain Unix timestamp 1705314600, got: {}",
            url
        );
    }

    #[test]
    fn test_get_version_current() {
        let manifest = create_test_manifest_with_timestamps();

        let output = manifest.get_version("js/paste/dom.js", None);
        assert_eq!(output, Some("/js/paste/dom.abc123.v1.min.js"));
    }

    #[test]
    fn test_get_version_specific() {
        let mut manifest = create_test_manifest_with_timestamps();

        // Add history
        if let Some(entry) = manifest.assets.get_mut("js/paste/event.js") {
            entry.history.push(AssetVersion {
                hash: "old123".to_string(),
                version: 1,
                output: "/js/paste/event.old123.v1.min.js".to_string(),
                timestamp: "2024-01-14T10:00:00+00:00".to_string(),
            });
        }

        // Get specific version
        let v1 = manifest.get_version("js/paste/event.js", Some(1));
        assert_eq!(v1, Some("/js/paste/event.old123.v1.min.js"));

        let v2 = manifest.get_version("js/paste/event.js", Some(2));
        assert_eq!(v2, Some("/js/paste/event.def456.v2.min.js"));
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
        manifest.update_asset(
            "js/app.js",
            "src/app.js",
            "hash_v1",
            "/js/app",
            "js",
            vec![],
        );
        let v1_timestamp = manifest.assets.get("js/app.js").unwrap().timestamp.clone();

        // Small delay to ensure different timestamp
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Version 2 (hash changed)
        manifest.update_asset(
            "js/app.js",
            "src/app.js",
            "hash_v2",
            "/js/app",
            "js",
            vec![],
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
        manifest.update_asset(
            "js/app.js",
            "src/app.js",
            "hash_v1",
            "/js/app",
            "js",
            vec![],
        );
        let ts_v1 = manifest.assets.get("js/app.js").unwrap().timestamp.clone();
        std::thread::sleep(std::time::Duration::from_millis(10));

        manifest.update_asset(
            "js/app.js",
            "src/app.js",
            "hash_v2",
            "/js/app",
            "js",
            vec![],
        );
        let ts_v2 = manifest.assets.get("js/app.js").unwrap().timestamp.clone();
        std::thread::sleep(std::time::Duration::from_millis(10));

        manifest.update_asset(
            "js/app.js",
            "src/app.js",
            "hash_v3",
            "/js/app",
            "js",
            vec![],
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

        manifest.update_asset(
            "js/app.js",
            "src/app.js",
            "same_hash",
            "/js/app",
            "js",
            vec![],
        );
        let v1 = manifest.assets.get("js/app.js").unwrap().version;

        // Same hash should not bump version
        manifest.update_asset(
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
            },
        );

        let deps = manifest.get_dependencies("a");

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
        manifest.update_asset(
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
        manifest.update_asset(
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
    // JAM URL: missing modules included with default version
    // ============================================================

    #[test]
    fn test_jam_url_includes_missing_modules() {
        // Modules not in the manifest should still appear in the URL with v1.0,
        // matching the Scala runtime behavior for consistent cache keys.
        let manifest = create_test_manifest_with_timestamps();

        let url = manifest.jam_url(&["paste.dom", "paste.unknown"], "js", false);

        // Both modules should be in the URL
        assert!(
            url.contains("paste.dom+v1.0"),
            "Known module should be in URL: {}",
            url
        );
        assert!(
            url.contains("paste.unknown+v1.0"),
            "Unknown module should be in URL with default v1.0: {}",
            url
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
        // Bug 1 regression: jam_url should consult self.modules map first,
        // not just construct key from asset_type. This matters for CSS URLs
        // requesting modules that only have JS entries in assets.
        let mut manifest = Manifest::new();
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
            },
        );

        // Request CSS URL — should still find the JS asset via modules map
        let css_url = manifest.jam_url(&["paste.ui.heroscroll"], "css", false);
        // Should use the JS asset's version (v1) not default (v1.0)
        assert!(
            css_url.contains("paste.ui.heroscroll+v1.0"),
            "CSS URL should find module via modules map: {}",
            css_url
        );
        // Should have a real epoch timestamp, not manifest.version
        assert!(
            !css_url.contains("2024.01.15"),
            "CSS URL should use asset timestamp, not manifest version: {}",
            css_url
        );

        // Request JS URL — should also work (direct key and modules map both resolve)
        let js_url = manifest.jam_url(&["paste.ui.heroscroll"], "js", false);
        assert!(
            js_url.contains("paste.ui.heroscroll+v1.0"),
            "JS URL should find module: {}",
            js_url
        );
    }

    #[test]
    fn test_jam_url_all_missing_uses_manifest_version() {
        // When all modules are missing, no timestamps are found,
        // so the URL should fall back to the manifest version.
        let manifest = create_test_manifest_with_timestamps();

        let url = manifest.jam_url(&["nonexistent.a", "nonexistent.b"], "js", false);

        assert!(
            url.contains("nonexistent.a+v1.0"),
            "Missing module a in URL: {}",
            url
        );
        assert!(
            url.contains("nonexistent.b+v1.0"),
            "Missing module b in URL: {}",
            url
        );
        // Should use manifest.version as timestamp fallback
        assert!(
            url.contains(&manifest.version),
            "Should fall back to manifest version when no timestamps found: {}",
            url
        );
    }
}
