// Planned feature stub — suppress dead code warnings
#![allow(dead_code)]

use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Storage backend for asset artifacts.
///
/// Supports different storage strategies:
/// - `Local`: Write to target/ (sbt-web style, gitignored, no rollback)
/// - `Dist`: Write to dist/ with versioned files (HTML sites, committed, rollback support)
/// - `S3`: Future - write to S3 bucket (external storage, rollback support)
#[derive(Debug, Clone)]
pub enum StorageBackend {
    /// Local filesystem storage (default for sbt)
    /// Files written to output path, overwritten on rebuild
    /// No version retention - relies on CI/CD for rollback
    Local {
        path: PathBuf,
        /// Per-type output directory overrides (e.g., "js" -> "/project/custom-js")
        type_dirs: HashMap<String, PathBuf>,
    },

    /// Dist storage for static sites
    /// Keeps versioned artifacts for rolling deploy support
    /// Artifacts should be committed to source control
    Dist {
        path: PathBuf,
        keep_versions: usize,
        /// Per-type output directory overrides (e.g., "js" -> "/project/custom-js")
        type_dirs: HashMap<String, PathBuf>,
    },

    /// S3 storage (future)
    /// External storage with version retention
    #[allow(dead_code)]
    S3 {
        bucket: String,
        prefix: String,
        keep_versions: usize,
    },
}

impl StorageBackend {
    /// Create local storage backend (sbt default)
    pub fn local(path: impl Into<PathBuf>) -> Self {
        StorageBackend::Local {
            path: path.into(),
            type_dirs: HashMap::new(),
        }
    }

    /// Create dist storage backend (HTML sites)
    pub fn dist(path: impl Into<PathBuf>, keep_versions: usize) -> Self {
        StorageBackend::Dist {
            path: path.into(),
            keep_versions,
            type_dirs: HashMap::new(),
        }
    }

    /// Parse from config, using per-type output directories
    pub fn from_config(
        backend: &str,
        path: &Path,
        keep_versions: usize,
        type_dirs: HashMap<String, PathBuf>,
    ) -> Self {
        match backend {
            "dist" => StorageBackend::Dist {
                path: path.into(),
                keep_versions,
                type_dirs,
            },
            "s3" => {
                // Future: parse S3 config
                eprintln!("S3 backend not yet implemented, falling back to local");
                StorageBackend::Local {
                    path: path.into(),
                    type_dirs,
                }
            }
            _ => StorageBackend::Local {
                path: path.into(),
                type_dirs,
            },
        }
    }

    pub fn is_dist(&self) -> bool {
        matches!(self, StorageBackend::Dist { .. })
    }

    /// Resolve the output directory for a given asset type.
    /// Uses per-type override if configured, otherwise falls back to base/type.
    pub fn resolve_dir(&self, asset_type: &str) -> PathBuf {
        match self {
            StorageBackend::Local { path, type_dirs }
            | StorageBackend::Dist {
                path, type_dirs, ..
            } => type_dirs
                .get(asset_type)
                .cloned()
                .unwrap_or_else(|| path.join(asset_type)),
            StorageBackend::S3 { .. } => PathBuf::new(),
        }
    }

    /// Write an asset to storage
    /// Returns the relative URL path for the asset
    pub fn write_asset(
        &self,
        content: &[u8],
        asset_type: &str, // "js", "css", "images"
        filename: &str,   // "app.abc123.v1.min.js"
    ) -> Result<String> {
        match self {
            StorageBackend::Local { .. } => {
                let dir = self.resolve_dir(asset_type);
                let file_path = dir.join(filename);
                // Create parent directories (filename may contain subdirs, e.g., "paste/dom.v1.min.js")
                if let Some(parent) = file_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&file_path, content)?;
                Ok(format!("/{}/{}", asset_type, filename))
            }

            StorageBackend::Dist { .. } => {
                let dir = self.resolve_dir(asset_type);
                let file_path = dir.join(filename);

                // Create parent directories (filename may contain subdirs, e.g., "paste/dom.v1.min.js")
                if let Some(parent) = file_path.parent() {
                    fs::create_dir_all(parent)?;
                }

                // Write if file doesn't exist, or if existing content differs.
                // Compare actual bytes, not just length — same-length content
                // with different bytes must still be written (e.g., hash collision).
                let should_write = if file_path.exists() {
                    match fs::read(&file_path) {
                        Ok(existing) => existing != content,
                        Err(_) => true,
                    }
                } else {
                    true
                };
                if should_write {
                    fs::write(&file_path, content)?;
                }

                Ok(format!("/{}/{}", asset_type, filename))
            }

            StorageBackend::S3 { bucket, prefix, .. } => {
                // Future: upload to S3
                eprintln!(
                    "S3 write not implemented: s3://{}/{}{}/{}",
                    bucket, prefix, asset_type, filename
                );
                Ok(format!("/{}/{}", asset_type, filename))
            }
        }
    }

    /// Check if an asset version exists
    pub fn asset_exists(&self, asset_type: &str, filename: &str) -> bool {
        match self {
            StorageBackend::Local { .. } | StorageBackend::Dist { .. } => {
                self.resolve_dir(asset_type).join(filename).exists()
            }
            StorageBackend::S3 { .. } => {
                // Future: check S3
                false
            }
        }
    }

    /// Prune old versions, keeping only the most recent N
    pub fn prune_versions(&self, asset_type: &str, base_name: &str) -> Result<()> {
        match self {
            StorageBackend::Local { .. } => {
                // Local keeps only the current version — prune all old versions
                let dir = self.resolve_dir(asset_type);
                if !dir.exists() {
                    return Ok(());
                }

                // base_name may contain subdirectories (e.g., "paste/dom").
                let (scan_dir, file_stem) = if let Some(slash_pos) = base_name.rfind('/') {
                    (
                        dir.join(&base_name[..slash_pos]),
                        &base_name[slash_pos + 1..],
                    )
                } else {
                    (dir, base_name)
                };

                if !scan_dir.exists() {
                    return Ok(());
                }

                // Find all versions of this asset and keep only the latest
                let prefix = format!("{}.", file_stem);
                let mut versions: Vec<(u32, PathBuf)> = Vec::new();

                for entry in fs::read_dir(&scan_dir)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();

                    if !name.starts_with(&prefix) {
                        continue;
                    }

                    if let Some(version) = Self::extract_version(&name) {
                        versions.push((version, entry.path()));
                    }
                }

                // Sort by version descending, remove all but the latest
                versions.sort_by(|a, b| b.0.cmp(&a.0));
                for (_, path) in versions.iter().skip(1) {
                    if let Err(e) = fs::remove_file(path) {
                        eprintln!("Warning: failed to prune {}: {}", path.display(), e);
                    }
                }

                Ok(())
            }

            StorageBackend::Dist { keep_versions, .. } => {
                let dir = self.resolve_dir(asset_type);
                if !dir.exists() {
                    return Ok(());
                }

                // base_name may contain subdirectories (e.g., "paste/dom").
                // Split into subdir + file stem so we scan the correct directory.
                let (scan_dir, file_stem) = if let Some(slash_pos) = base_name.rfind('/') {
                    (
                        dir.join(&base_name[..slash_pos]),
                        &base_name[slash_pos + 1..],
                    )
                } else {
                    (dir, base_name)
                };

                if !scan_dir.exists() {
                    return Ok(());
                }

                // Find all versions of this asset
                // Filename format: {file_stem}.{hash}.v{N}.min.{ext}
                let prefix = format!("{}.", file_stem);
                let mut versions: Vec<(u32, PathBuf)> = Vec::new();

                for entry in fs::read_dir(&scan_dir)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();

                    // Must start with file_stem followed by a dot
                    if !name.starts_with(&prefix) {
                        continue;
                    }

                    // Extract version number from ".v{N}." pattern (last match)
                    if let Some(version) = Self::extract_version(&name) {
                        versions.push((version, entry.path()));
                    }
                }

                // Sort by version descending
                versions.sort_by(|a, b| b.0.cmp(&a.0));

                // Remove old versions beyond keep_versions
                for (_, path) in versions.iter().skip(*keep_versions) {
                    if let Err(e) = fs::remove_file(path) {
                        eprintln!("Warning: failed to prune {}: {}", path.display(), e);
                    }
                }

                Ok(())
            }

            StorageBackend::S3 { .. } => {
                // Future: prune S3 objects
                Ok(())
            }
        }
    }

    /// Extract version number from a filename matching the `.v{N}.` pattern.
    /// Searches for the *last* occurrence to avoid false matches like `.vue`.
    fn extract_version(name: &str) -> Option<u32> {
        thread_local! {
            static VERSION_RE: Regex = Regex::new(r"\.v(\d+)\.").unwrap();
        }
        VERSION_RE.with(|re| {
            // Take the last match to avoid false positives like `.vue-fixes`
            re.find_iter(name).last().and_then(|m| {
                // Extract digits between ".v" and "."
                let matched = m.as_str();
                matched[2..matched.len() - 1].parse::<u32>().ok()
            })
        })
    }

    /// Get the base path for this storage
    pub fn base_path(&self) -> &Path {
        match self {
            StorageBackend::Local { path, .. } | StorageBackend::Dist { path, .. } => path,
            StorageBackend::S3 { .. } => Path::new(""),
        }
    }

    /// Whether this backend supports rollback (keeps old versions)
    pub fn supports_rollback(&self) -> bool {
        match self {
            StorageBackend::Local { .. } => false,
            StorageBackend::Dist { .. } | StorageBackend::S3 { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_local_storage() {
        let dir = tempdir().unwrap();
        let storage = StorageBackend::local(dir.path());

        let url = storage
            .write_asset(b"test content", "js", "app.v1.js")
            .unwrap();
        assert_eq!(url, "/js/app.v1.js");
        assert!(dir.path().join("js/app.v1.js").exists());
    }

    #[test]
    fn test_dist_storage_preserves_versions() {
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 5);

        storage.write_asset(b"v1", "js", "app.v1.js").unwrap();
        storage.write_asset(b"v2", "js", "app.v2.js").unwrap();

        assert!(dir.path().join("js/app.v1.js").exists());
        assert!(dir.path().join("js/app.v2.js").exists());
    }

    #[test]
    fn test_dist_prune_versions_keeps_n_most_recent() {
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 2); // keep 2

        // Write 4 versions matching the versioned filename format
        storage
            .write_asset(b"v1", "js", "app.aaa.v1.min.js")
            .unwrap();
        storage
            .write_asset(b"v2", "js", "app.bbb.v2.min.js")
            .unwrap();
        storage
            .write_asset(b"v3", "js", "app.ccc.v3.min.js")
            .unwrap();
        storage
            .write_asset(b"v4", "js", "app.ddd.v4.min.js")
            .unwrap();

        storage.prune_versions("js", "app").unwrap();

        // Only v3 and v4 should remain
        assert!(
            !dir.path().join("js/app.aaa.v1.min.js").exists(),
            "v1 should be pruned"
        );
        assert!(
            !dir.path().join("js/app.bbb.v2.min.js").exists(),
            "v2 should be pruned"
        );
        assert!(
            dir.path().join("js/app.ccc.v3.min.js").exists(),
            "v3 should remain"
        );
        assert!(
            dir.path().join("js/app.ddd.v4.min.js").exists(),
            "v4 should remain"
        );
    }

    #[test]
    fn test_local_prune_keeps_only_latest() {
        let dir = tempdir().unwrap();
        let storage = StorageBackend::local(dir.path());

        // Write multiple versions
        storage
            .write_asset(b"v1", "js", "app.aaa.v1.min.js")
            .unwrap();
        storage
            .write_asset(b"v2", "js", "app.bbb.v2.min.js")
            .unwrap();
        storage
            .write_asset(b"v3", "js", "app.ccc.v3.min.js")
            .unwrap();

        storage.prune_versions("js", "app").unwrap();

        // Only the latest version should remain
        assert!(
            !dir.path().join("js/app.aaa.v1.min.js").exists(),
            "v1 should be pruned"
        );
        assert!(
            !dir.path().join("js/app.bbb.v2.min.js").exists(),
            "v2 should be pruned"
        );
        assert!(
            dir.path().join("js/app.ccc.v3.min.js").exists(),
            "v3 (latest) should remain"
        );
    }

    #[test]
    fn test_dist_write_same_length_different_content() {
        // Regression: Dist backend must write when content differs even if byte length is the same.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 5);

        // Write initial content
        storage.write_asset(b"aaaa", "js", "app.v1.js").unwrap();
        assert_eq!(fs::read(dir.path().join("js/app.v1.js")).unwrap(), b"aaaa");

        // Write different content with the same length
        storage.write_asset(b"bbbb", "js", "app.v1.js").unwrap();
        assert_eq!(
            fs::read(dir.path().join("js/app.v1.js")).unwrap(),
            b"bbbb",
            "Dist should overwrite when content differs, even if length is the same"
        );
    }

    #[test]
    fn test_local_storage_nested_module() {
        let dir = tempdir().unwrap();
        let storage = StorageBackend::local(dir.path());

        // Modules like paste.dom produce files in js/paste/dom.{hash}.v{N}.min.js
        let url = storage
            .write_asset(b"nested content", "js", "paste/dom.abc123.v1.min.js")
            .unwrap();
        assert_eq!(url, "/js/paste/dom.abc123.v1.min.js");
        assert!(dir.path().join("js/paste/dom.abc123.v1.min.js").exists());
    }

    #[test]
    fn test_dist_prune_versions_nested_module() {
        // Modules like paste.dom produce files in js/paste/dom.{hash}.v{N}.min.js
        // prune_versions must handle the nested subdirectory correctly.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 2); // keep 2

        // Write 4 versions in a nested subdirectory (paste/)
        storage
            .write_asset(b"v1", "js", "paste/dom.aaa.v1.min.js")
            .unwrap();
        storage
            .write_asset(b"v2", "js", "paste/dom.bbb.v2.min.js")
            .unwrap();
        storage
            .write_asset(b"v3", "js", "paste/dom.ccc.v3.min.js")
            .unwrap();
        storage
            .write_asset(b"v4", "js", "paste/dom.ddd.v4.min.js")
            .unwrap();

        // base_name is "paste/dom" (as computed by graph.rs from key "js/paste/dom.js")
        storage.prune_versions("js", "paste/dom").unwrap();

        // Only v3 and v4 should remain
        assert!(
            !dir.path().join("js/paste/dom.aaa.v1.min.js").exists(),
            "v1 should be pruned"
        );
        assert!(
            !dir.path().join("js/paste/dom.bbb.v2.min.js").exists(),
            "v2 should be pruned"
        );
        assert!(
            dir.path().join("js/paste/dom.ccc.v3.min.js").exists(),
            "v3 should remain"
        );
        assert!(
            dir.path().join("js/paste/dom.ddd.v4.min.js").exists(),
            "v4 should remain"
        );
    }
}
