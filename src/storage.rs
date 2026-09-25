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

    /// Write `content` to an already-resolved file path. Both filesystem
    /// backends compare before writing: unchanged bytes stay untouched —
    /// committed dist output and local build output alike keep their mtime —
    /// while bytes that differ (a tampered or truncated file) are repaired.
    /// Compare actual bytes, not just length — same-length content with
    /// different bytes must still be written.
    pub fn write_file(&self, file: &Path, content: &[u8]) -> Result<()> {
        match self {
            StorageBackend::Local { .. } | StorageBackend::Dist { .. } => {
                if let Some(parent) = file.parent() {
                    fs::create_dir_all(parent)?;
                }
                let should_write = if file.exists() {
                    match fs::read(file) {
                        Ok(existing) => existing != content,
                        Err(_) => true,
                    }
                } else {
                    true
                };
                if should_write {
                    fs::write(file, content)?;
                }
                Ok(())
            }
            StorageBackend::S3 { .. } => {
                // Future: upload to S3
                eprintln!("S3 write not implemented: {}", file.display());
                Ok(())
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

    /// Prune old versions of the asset whose current version sits at
    /// `current` — a resolved compiled file named
    /// `{stem}.{32-hex hash}.v{N}.min.{ext}`.
    ///
    /// Matches are exact on the stem: pruning `site` never counts
    /// `site.theme`'s files the way a `"{stem}."` prefix did. The directory
    /// comes from the resolved file itself, so the same code prunes flat
    /// type dirs and beside-source build dirs. `retained` holds the resolved
    /// files the manifest's history still references — the caller derives it
    /// from the manifest, not from a count, so a stray file that happens to
    /// share a version number can never displace a version the manifest
    /// still serves. `current` and every retained path survive; a missing
    /// file is not an error.
    pub fn prune_versions(&self, current: &Path, retained: &[PathBuf]) -> Result<()> {
        if matches!(self, StorageBackend::S3 { .. }) {
            return Ok(());
        }
        let Some(name) = current.file_name().and_then(|n| n.to_str()) else {
            return Ok(());
        };
        let Some((file_stem, ext)) = Self::split_versioned_name(name) else {
            return Ok(());
        };
        let Some(scan_dir) = current.parent() else {
            return Ok(());
        };
        if !scan_dir.exists() {
            return Ok(());
        }

        let versioned = Regex::new(&format!(
            r"^{}\.[0-9a-f]{{32}}\.v\d+\.min\.{}$",
            regex::escape(file_stem),
            regex::escape(ext)
        ))?;
        for entry in fs::read_dir(scan_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !versioned.is_match(&name) {
                continue;
            }
            let path = entry.path();
            if path == current || retained.contains(&path) {
                continue;
            }
            match fs::remove_file(&path) {
                // Already gone — a file removed between the scan and the
                // delete is not a pruning failure.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                result => result?,
            }
        }

        Ok(())
    }

    /// Split a versioned filename `{stem}.{32-hex hash}.v{N}.min.{ext}` into
    /// (stem, ext); anything else is not a compiled asset and returns None.
    fn split_versioned_name(name: &str) -> Option<(&str, &str)> {
        thread_local! {
            static NAME_RE: Regex =
                Regex::new(r"^(.*)\.[0-9a-f]{32}\.v\d+\.min\.([^.]+)$").unwrap();
        }
        NAME_RE.with(|re| {
            let caps = re.captures(name)?;
            Some((caps.get(1)?.as_str(), caps.get(2)?.as_str()))
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

/// Whether `path`'s filename has the generated
/// `{stem}.{32-hex hash}.v{N}.min.{ext}` shape — the exact predicate pruning
/// uses. Source discovery consults it so generated output can never feed a
/// later build back in as a source, even from a build directory whose
/// configured name has since changed.
pub(crate) fn is_versioned_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| StorageBackend::split_versioned_name(name).is_some())
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

    /// A 32-hex-char hash for a versioned test filename.
    fn hash(ch: char) -> String {
        ch.to_string().repeat(32)
    }

    #[test]
    fn test_dist_prune_versions_keeps_current_plus_history() {
        // The manifest references the current file plus its retained
        // history entries — keep exactly the files it names.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 2);

        // Write 4 versions matching the versioned filename format
        for (version, ch) in [('1', 'a'), ('2', 'b'), ('3', 'c'), ('4', 'd')] {
            let name = format!("app.{}.v{version}.min.js", hash(ch));
            storage
                .write_asset(version.to_string().as_bytes(), "js", &name)
                .unwrap();
        }

        let current = dir.path().join(format!("js/app.{}.v4.min.js", hash('d')));
        let retained = vec![
            dir.path().join(format!("js/app.{}.v2.min.js", hash('b'))),
            dir.path().join(format!("js/app.{}.v3.min.js", hash('c'))),
        ];
        storage.prune_versions(&current, &retained).unwrap();

        // v2, v3 and v4 remain: the current file plus the two retained
        // history entries.
        assert!(
            !dir.path()
                .join(format!("js/app.{}.v1.min.js", hash('a')))
                .exists(),
            "v1 should be pruned"
        );
        assert!(
            dir.path()
                .join(format!("js/app.{}.v2.min.js", hash('b')))
                .exists(),
            "v2 should remain (a manifest history entry)"
        );
        assert!(
            dir.path()
                .join(format!("js/app.{}.v3.min.js", hash('c')))
                .exists(),
            "v3 should remain"
        );
        assert!(
            dir.path()
                .join(format!("js/app.{}.v4.min.js", hash('d')))
                .exists(),
            "v4 should remain"
        );
    }

    #[test]
    fn test_prune_versions_matches_the_exact_stem() {
        // `app.` was once a prefix match, so pruning `app` counted and
        // deleted `app.theme`'s file in the same directory.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 0); // keep current only

        let theme = format!("app.theme.{}.v1.min.js", hash('t'));
        storage.write_asset(b"theme", "js", &theme).unwrap();
        storage
            .write_asset(b"v1", "js", &format!("app.{}.v1.min.js", hash('a')))
            .unwrap();
        storage
            .write_asset(b"v2", "js", &format!("app.{}.v2.min.js", hash('b')))
            .unwrap();

        storage
            .prune_versions(
                &dir.path().join(format!("js/app.{}.v2.min.js", hash('b'))),
                &[],
            )
            .unwrap();

        assert!(
            !dir.path()
                .join(format!("js/app.{}.v1.min.js", hash('a')))
                .exists(),
            "app v1 should be pruned"
        );
        assert!(
            dir.path().join(format!("js/{theme}")).exists(),
            "app.theme must survive pruning app"
        );
    }

    #[test]
    fn test_local_prune_keeps_only_current() {
        let dir = tempdir().unwrap();
        let storage = StorageBackend::local(dir.path());

        // Write multiple versions
        for (version, ch) in [('1', 'a'), ('2', 'b'), ('3', 'c')] {
            let name = format!("app.{}.v{version}.min.js", hash(ch));
            storage
                .write_asset(version.to_string().as_bytes(), "js", &name)
                .unwrap();
        }

        let current = dir.path().join(format!("js/app.{}.v3.min.js", hash('c')));
        storage.prune_versions(&current, &[]).unwrap();

        // Only the current file should remain
        assert!(
            !dir.path()
                .join(format!("js/app.{}.v1.min.js", hash('a')))
                .exists(),
            "v1 should be pruned"
        );
        assert!(
            !dir.path()
                .join(format!("js/app.{}.v2.min.js", hash('b')))
                .exists(),
            "v2 should be pruned"
        );
        assert!(
            dir.path()
                .join(format!("js/app.{}.v3.min.js", hash('c')))
                .exists(),
            "v3 (current) should remain"
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
        // Modules like paste.dom produce files in js/paste/dom.{hash}.v{N}.min.js —
        // prune_versions takes the resolved current file and must scan the
        // nested subdirectory it sits in.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 2); // keep 2

        // Write 4 versions in a nested subdirectory (paste/)
        for (version, ch) in [('1', 'a'), ('2', 'b'), ('3', 'c'), ('4', 'd')] {
            let name = format!("paste/dom.{}.v{version}.min.js", hash(ch));
            storage
                .write_asset(version.to_string().as_bytes(), "js", &name)
                .unwrap();
        }

        let current = dir
            .path()
            .join(format!("js/paste/dom.{}.v4.min.js", hash('d')));
        let retained = vec![
            dir.path()
                .join(format!("js/paste/dom.{}.v2.min.js", hash('b'))),
            dir.path()
                .join(format!("js/paste/dom.{}.v3.min.js", hash('c'))),
        ];
        storage.prune_versions(&current, &retained).unwrap();

        // v2, v3 and v4 remain: current plus keep_versions history entries.
        assert!(
            !dir.path()
                .join(format!("js/paste/dom.{}.v1.min.js", hash('a')))
                .exists(),
            "v1 should be pruned"
        );
        assert!(
            dir.path()
                .join(format!("js/paste/dom.{}.v2.min.js", hash('b')))
                .exists(),
            "v2 should remain (a manifest history entry)"
        );
        assert!(
            dir.path()
                .join(format!("js/paste/dom.{}.v3.min.js", hash('c')))
                .exists(),
            "v3 should remain"
        );
        assert!(
            dir.path()
                .join(format!("js/paste/dom.{}.v4.min.js", hash('d')))
                .exists(),
            "v4 should remain"
        );
    }

    #[test]
    fn test_prune_versions_deletes_stray_same_version_files() {
        // A version number is not an identity: two files can share it with
        // different hashes (a stray from an interrupted or relocated build).
        // Keeping "the newest N" by version could displace a file the
        // manifest still references; pruning by retained path cannot.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 1);

        let retained_file = format!("app.{}.v1.min.js", hash('a'));
        let stray = format!("app.{}.v1.min.js", hash('e'));
        let sibling = format!("app2.{}.v1.min.js", hash('9'));
        storage.write_asset(b"kept", "js", &retained_file).unwrap();
        storage.write_asset(b"stray", "js", &stray).unwrap();
        storage.write_asset(b"sibling", "js", &sibling).unwrap();
        storage
            .write_asset(b"current", "js", &format!("app.{}.v2.min.js", hash('b')))
            .unwrap();

        let current = dir.path().join(format!("js/app.{}.v2.min.js", hash('b')));
        let retained = vec![dir.path().join(format!("js/{retained_file}"))];
        storage.prune_versions(&current, &retained).unwrap();

        assert!(
            !dir.path().join(format!("js/{stray}")).exists(),
            "a stray same-version file is pruned like any unreferenced version"
        );
        assert!(
            dir.path().join(format!("js/{retained_file}")).exists(),
            "the manifest-referenced version survives"
        );
        assert!(
            dir.path().join(format!("js/{sibling}")).exists(),
            "the exact-stem sibling is untouched"
        );
        assert!(current.exists(), "the current file survives");
    }
}
