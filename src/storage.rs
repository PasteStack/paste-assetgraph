// Copyright (c) 2017–present Tomshley LLC
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::Result;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The largest read a compiled asset can legitimately need — a build
/// that emits beyond it publishes a file the runtime refuses to serve
/// (the verified read stops at this cap), so the bound is shared by
/// write and read paths alike: anything over it is a stand-in or an
/// invalid asset, never a real output.
pub(crate) const MAX_COMPILED_ASSET_BYTES: u64 = 64 * 1024 * 1024;

/// Storage backend for asset artifacts.
///
/// Supported strategies:
/// - `Local`: Write to target/ (sbt-web style, gitignored, no rollback)
/// - `Dist`: Write to dist/ with versioned files (HTML sites, committed, rollback support)
///
/// S3 remains a roadmap item — it is deliberately NOT a variant here:
/// a backend that reports success without persisting bytes is worse
/// than none.
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
}

impl StorageBackend {
    // Test-only: production writes and prunes through `write_file` and
    // `prune_versions_set` with the build's ownership boundary.
    #[cfg(test)]
    /// Create local storage backend (sbt default)
    pub(crate) fn local(path: impl Into<PathBuf>) -> Self {
        StorageBackend::Local {
            path: path.into(),
            type_dirs: HashMap::new(),
        }
    }

    // Test-only: production writes and prunes through `write_file` and
    // `prune_versions_set` with the build's ownership boundary.
    #[cfg(test)]
    /// Create dist storage backend (HTML sites)
    pub(crate) fn dist(path: impl Into<PathBuf>, keep_versions: usize) -> Self {
        StorageBackend::Dist {
            path: path.into(),
            keep_versions,
            type_dirs: HashMap::new(),
        }
    }

    /// Parse from config, using per-type output directories. Fails
    /// closed: only "local" and "dist" construct a backend — any other
    /// string is an error, never a silent local fallback.
    pub fn from_config(
        backend: &str,
        path: &Path,
        keep_versions: usize,
        type_dirs: HashMap<String, PathBuf>,
    ) -> Result<Self> {
        match backend {
            "local" => Ok(StorageBackend::Local {
                path: path.into(),
                type_dirs,
            }),
            "dist" => Ok(StorageBackend::Dist {
                path: path.into(),
                keep_versions,
                type_dirs,
            }),
            other => anyhow::bail!(
                "unsupported storage backend {other:?} — supported: \"local\", \"dist\""
            ),
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
        }
    }

    // Test-only: production writes and prunes through `write_file` and
    // `prune_versions_set` with the build's ownership boundary.
    #[cfg(test)]
    /// Write an asset to storage
    /// Returns the relative URL path for the asset
    pub(crate) fn write_asset(
        &self,
        content: &[u8],
        asset_type: &str, // "js", "css", "images"
        filename: &str,   // "app.abc123.v1.min.js"
    ) -> Result<String> {
        match self {
            StorageBackend::Local { .. } | StorageBackend::Dist { .. } => {
                // Both segments name positions inside storage, so each
                // must be ordinary path components: a `..`, absolute, or
                // empty tail would escape the resolved type directory or
                // rename it into a caller-chosen location.
                anyhow::ensure!(
                    matches!(
                        Path::new(asset_type)
                            .components()
                            .collect::<Vec<_>>()
                            .as_slice(),
                        [std::path::Component::Normal(_)]
                    ),
                    "invalid asset type {asset_type:?} — expected a single path component like `js`"
                );
                anyhow::ensure!(
                    !filename.is_empty()
                        && Path::new(filename)
                            .components()
                            .all(|c| matches!(c, std::path::Component::Normal(_))),
                    "invalid asset filename {filename:?} — it must be a relative name beneath the asset type's output directory"
                );
                let dir = self.resolve_dir(asset_type);
                let file_path = dir.join(filename);
                // Both filesystem backends share the write-if-different
                // rule: unchanged bytes keep their mtime, different bytes
                // repair the file. The resolved type directory is the
                // configured write location, so it anchors the ownership
                // check for everything beneath it.
                self.write_file(&file_path, content, &dir)?;
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
    ///
    /// `boundary` is the trusted anchor the file's parent chain must sit
    /// under — the resolved output root or base_dir the caller derived it
    /// beneath. A symlinked component between them, or a symlinked file at
    /// the destination, means the write would land in a tree this build
    /// never owned, so it refuses rather than follow. The write itself is
    /// staged in an owned temporary file and renamed into place, so a
    /// partial write can never be served and the destination is replaced
    /// atomically — never opened, truncated, or followed.
    pub fn write_file(&self, file: &Path, content: &[u8], boundary: &Path) -> Result<()> {
        self.write_file_in(file, content, boundary, false)
    }

    /// [`Self::write_file`], following a link at `boundary` itself when it
    /// is the project root (`follow_anchor`).
    pub(crate) fn write_file_in(
        &self,
        file: &Path,
        content: &[u8],
        boundary: &Path,
        follow_anchor: bool,
    ) -> Result<()> {
        // Publish nothing the runtime cannot serve — an asset past the
        // shared read cap would build green and 404 at request time.
        if content.len() as u64 > MAX_COMPILED_ASSET_BYTES {
            anyhow::bail!(
                "refusing to write {} — {} bytes exceeds the {} byte compiled-asset limit the runtime enforces on reads",
                file.display(),
                content.len(),
                MAX_COMPILED_ASSET_BYTES
            );
        }
        match self {
            StorageBackend::Local { .. } | StorageBackend::Dist { .. } => {
                let name = file.file_name().unwrap_or_default();
                let parent_dir = ensure_owned_parent_in(
                    crate::dirfd::parent_dir(file),
                    boundary,
                    follow_anchor,
                )?;
                let should_write = match parent_dir.entry_kind(name) {
                    Ok(crate::dirfd::EntryKind::Symlink) => {
                        anyhow::bail!(
                            "refusing to write {} — the existing path is a symlink into a tree this build does not own",
                            file.display()
                        )
                    }
                    // Only a regular file gets a byte comparison — and
                    // only one that still is a regular file once opened
                    // (the handle's own metadata, not the earlier name
                    // lookup). Anything else — a FIFO planted to stall
                    // the open, a socket, a directory — is never opened:
                    // the staged rename either replaces it or refuses
                    // at the syscall, both without waiting on it.
                    Ok(crate::dirfd::EntryKind::Regular) => {
                        match parent_dir
                            .open_file(name)
                            .and_then(|f| compare_existing(&f, content))
                        {
                            Ok(identical) => !identical,
                            Err(_) => true,
                        }
                    }
                    Ok(_) => true,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                    Err(e) => return Err(e.into()),
                };
                if should_write {
                    write_staged(&parent_dir, file, content)?;
                }
                Ok(())
            }
        }
    }

    // Test-only: production writes and prunes through `write_file` and
    // `prune_versions_set` with the build's ownership boundary.
    #[cfg(test)]
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
    ///
    /// `current` must sit beneath an established output root — a
    /// configured per-type directory or the backend base — proven by
    /// ordinary path components, so a caller-supplied path can never
    /// steer the version scan into an unrelated directory.
    pub(crate) fn prune_versions(&self, current: &Path, retained: &[PathBuf]) -> Result<()> {
        let mut roots: Vec<&PathBuf> = match self {
            StorageBackend::Local { path, type_dirs }
            | StorageBackend::Dist {
                path, type_dirs, ..
            } => type_dirs.values().chain(std::iter::once(path)).collect(),
        };
        // Deepest root first: two configured roots may overlap (a type dir
        // beneath the base), and the narrowest established root is the
        // boundary — HashMap iteration order must never pick between them.
        roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
        let Some(boundary) = roots.into_iter().find(|root| {
            current.strip_prefix(root).is_ok_and(|rel| {
                rel.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
            })
        }) else {
            anyhow::bail!(
                "refusing to prune versions for {} — outside every established output root",
                current.display()
            );
        };
        // The selected root anchors a destructive scan, so it gets the
        // write path's own check: `symlink_metadata`, never `metadata` —
        // a symlinked or non-directory configured root would redirect the
        // unlinks into a tree this storage never owned. An absent root
        // stays a no-op: the scan directory beneath it cannot exist.
        match fs::symlink_metadata(boundary) {
            Ok(meta) if meta.file_type().is_symlink() => anyhow::bail!(
                "refusing to prune versions under {} — the configured output root is a symlink",
                boundary.display()
            ),
            Ok(meta) if !meta.is_dir() => anyhow::bail!(
                "refusing to prune versions under {} — the configured output root is not a directory",
                boundary.display()
            ),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.prune_versions_set(current, &retained.iter().cloned().collect(), boundary)
    }

    /// `prune_versions` with the retained set already built: membership is a
    /// set lookup — a scan compares every directory entry against every
    /// retained file, and a linear check would square that work with the
    /// history a bound keeps.
    ///
    /// `boundary` is the trusted directory anchor the scan dir resolves
    /// under — base_dir for output inside the project, an established
    /// output root outside it. A symlinked component between boundary and
    /// scan dir means the unlink would act inside a tree this build never
    /// owned, so pruning refuses rather than follow it.
    #[cfg(test)]
    pub(crate) fn prune_versions_set(
        &self,
        current: &Path,
        retained: &HashSet<PathBuf>,
        boundary: &Path,
    ) -> Result<()> {
        let failed = self.prune_versions_set_in(current, retained, None, boundary, false)?;
        match failed.first() {
            Some(path) => anyhow::bail!("could not remove {}", path.display()),
            None => Ok(()),
        }
    }

    /// [`Self::prune_versions_set`], following a link at `boundary` itself
    /// when it is the project root (`follow_anchor`). Returns the files
    /// whose unlink failed — the scan goes on past each one, so the caller
    /// can record them for a later build to retry.
    ///
    /// `prunable`, when given, is the only set of files the scan may
    /// delete: the resolved files a loaded manifest recorded. A build
    /// passes it so pruning removes only what retention retired from that
    /// manifest — a versioned file no loaded manifest names (the manifest
    /// was lost or replaced by an older one, a copy came from elsewhere)
    /// may still back a page and is left in place. The match is by
    /// spelling only: a case-only variant of a recorded name is kept, the
    /// conservative answer. `None` treats every unreferenced versioned
    /// file in the group as prunable.
    pub(crate) fn prune_versions_set_in(
        &self,
        current: &Path,
        retained: &HashSet<PathBuf>,
        prunable: Option<&HashSet<PathBuf>>,
        boundary: &Path,
        follow_anchor: bool,
    ) -> Result<Vec<PathBuf>> {
        // Keep-all dist retires nothing, so a versioned file the loaded
        // manifest does not name — a lost or replaced manifest, a copy
        // from elsewhere — may still back a page minted earlier: the scan
        // never deletes under keep-all. Files the manifest itself moved
        // away from are removed by the caller, not here.
        if matches!(self, StorageBackend::Dist { keep_versions, .. } if *keep_versions == usize::MAX)
        {
            return Ok(Vec::new());
        }
        let Some(name) = current.file_name().and_then(|n| n.to_str()) else {
            return Ok(Vec::new());
        };
        let Some((file_stem, ext)) = Self::split_versioned_name(name) else {
            return Ok(Vec::new());
        };
        let Some(scan_dir) = current.parent() else {
            return Ok(Vec::new());
        };
        let scanned = match owned_dir_beneath_in(scan_dir, boundary, follow_anchor)? {
            crate::dirfd::Descend::Anchored(dir) => dir,
            crate::dirfd::Descend::Missing => return Ok(Vec::new()),
            crate::dirfd::Descend::Unsafe => anyhow::bail!(
                "refusing to prune versions in {} — a symlinked component redirects the directory outside the established output location",
                scan_dir.display()
            ),
        };

        #[cfg(test)]
        PRUNE_VERSION_SCANS.with(|count| count.set(count.get() + 1));
        let versioned = Regex::new(&format!(
            r"^{}\.[0-9a-f]{{32}}\.v\d+\.min\.{}$",
            regex::escape(file_stem),
            regex::escape(ext)
        ))?;
        // On a case-insensitive filesystem two spellings name one file, so
        // a spelling mismatch alone never proves a file unreferenced: the
        // identities of the kept files decide, computed only once there is
        // something to delete.
        let mut kept_ids: Option<HashSet<u64>> = None;
        let mut failed = Vec::new();
        for name in scanned.entries()? {
            let Some(name) = name.to_str() else { continue };
            if !versioned.is_match(name) {
                continue;
            }
            let path = scan_dir.join(name);
            if path == current || retained.contains(&path) {
                continue;
            }
            if prunable.is_some_and(|prunable| !prunable.contains(&path)) {
                continue;
            }
            let ids = kept_ids.get_or_insert_with(|| {
                std::iter::once(current)
                    .chain(retained.iter().map(PathBuf::as_path))
                    .filter_map(crate::graph::bound_identity)
                    .collect()
            });
            if crate::graph::bound_identity(&path).is_some_and(|id| ids.contains(&id)) {
                continue;
            }
            match scanned.remove_file(std::ffi::OsStr::new(name)) {
                Ok(()) => {}
                // Already gone — a file removed between the scan and the
                // delete is not a pruning failure.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => failed.push(path),
            }
        }

        Ok(failed)
    }

    /// Split a versioned filename `{stem}.{32-hex hash}.v{N}.min.{ext}` into
    /// (stem, ext); anything else is not a compiled asset and returns None.
    /// The caller's dedup key for directory scans is (parent, stem, ext) —
    /// every version of one asset shares it.
    pub(crate) fn split_versioned_name(name: &str) -> Option<(&str, &str)> {
        thread_local! {
            static NAME_RE: Regex =
                Regex::new(r"^(.*)\.[0-9a-f]{32}\.v\d+\.min\.([^.]+)$").unwrap();
        }
        NAME_RE.with(|re| {
            let caps = re.captures(name)?;
            Some((caps.get(1)?.as_str(), caps.get(2)?.as_str()))
        })
    }

    // Test-only: production writes and prunes through `write_file` and
    // `prune_versions_set` with the build's ownership boundary.
    #[cfg(test)]
    /// Whether this backend supports rollback (keeps old versions)
    pub(crate) fn supports_rollback(&self) -> bool {
        matches!(self, StorageBackend::Dist { .. })
    }
}

/// Pin `dir` — physically beneath `boundary` — as an `OwnedDir` for a
/// destructive unlink: the returned descriptor anchors the deletion to
/// the inode validated right now, so a component swapped for a link
/// afterward redirects nothing — the unlinkat goes through the held fd.
/// `boundary` is the trusted anchor and is itself exempt from the
/// link-refusal carried by the open (the deployment's own path may
/// legitimately sit behind a mount alias — its own open still refuses a
/// link at use time, which matches the previous check).
///
/// Containment must be proven twice over: the components beneath the
/// boundary must be ordinary names (a `boundary/../elsewhere` spelling
/// escapes while keeping the prefix), and the descent must start at the
/// boundary — a `dir` that never sat beneath it reports `Unsafe`, never
/// an anchor from a tree ownership was never established in. A missing
/// ancestor answers `Missing`: there is nothing beneath to act on.
#[cfg(test)]
pub(crate) fn owned_dir_beneath(dir: &Path, boundary: &Path) -> Result<crate::dirfd::Descend> {
    owned_dir_beneath_in(dir, boundary, false)
}

/// Open a boundary: strictly (a symlinked boundary is refused) unless the
/// caller marks it as the project root, whose own link — a deploy-style
/// `current` alias, macOS `/tmp` — is how the operator named the project.
/// Every component beneath the boundary is no-follow either way.
fn open_boundary(boundary: &Path, follow_anchor: bool) -> std::io::Result<crate::dirfd::OwnedDir> {
    if follow_anchor {
        crate::dirfd::OwnedDir::open_anchor(boundary)
    } else {
        crate::dirfd::OwnedDir::open(boundary)
    }
}

/// [`owned_dir_beneath`], following a link at the boundary itself when it
/// is the project root (`follow_anchor`).
pub(crate) fn owned_dir_beneath_in(
    dir: &Path,
    boundary: &Path,
    follow_anchor: bool,
) -> Result<crate::dirfd::Descend> {
    let relative = match dir.strip_prefix(boundary) {
        Ok(relative)
            if relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))) =>
        {
            relative
        }
        _ => return Ok(crate::dirfd::Descend::Unsafe),
    };
    let base = match open_boundary(boundary, follow_anchor) {
        Ok(base) => base,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(crate::dirfd::Descend::Missing)
        }
        Err(e) if crate::dirfd::is_unsafe_component(&e) => {
            return Ok(crate::dirfd::Descend::Unsafe)
        }
        Err(e) => return Err(e.into()),
    };
    Ok(base.descend(relative)?)
}

/// Open `dir` — physically beneath `boundary` — as an `OwnedDir`,
/// creating missing components as it goes. Every existing component is
/// opened with directory-only, no-follow semantics *at use time*, so a
/// component swapped for a link between here and the caller's write
/// cannot redirect anything: the caller's operations run against the
/// returned descriptor, not the path.
///
/// Containment is proven by components, not by the lexical prefix:
/// `boundary/../elsewhere` keeps the prefix while leaving the owned
/// tree, so the path beneath the boundary must be ordinary names only —
/// no `..`, root, or platform prefix. The boundary itself is checked
/// too: a symlinked configured output root would redirect every write
/// beneath it, while aliases *above* the boundary remain the
/// deployment's own business. When the boundary does not exist yet it is
/// created by path — there is nothing beneath it to pin — then opened,
/// so a planted link at the anchor still fails closed.
#[cfg(test)]
pub(crate) fn ensure_owned_parent(dir: &Path, boundary: &Path) -> Result<crate::dirfd::OwnedDir> {
    ensure_owned_parent_in(dir, boundary, false)
}

/// [`ensure_owned_parent`], following a link at the boundary itself when
/// it is the project root (`follow_anchor`).
pub(crate) fn ensure_owned_parent_in(
    dir: &Path,
    boundary: &Path,
    follow_anchor: bool,
) -> Result<crate::dirfd::OwnedDir> {
    let relative = dir.strip_prefix(boundary).map_err(|_| {
        anyhow::anyhow!(
            "refusing to create {} — outside the established output location {}",
            dir.display(),
            boundary.display()
        )
    })?;
    anyhow::ensure!(
        relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "refusing to create {} — path escapes the established output location {}",
        dir.display(),
        boundary.display()
    );
    let mut current = match open_boundary(boundary, follow_anchor) {
        Ok(base) => base,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(boundary)?;
            match open_boundary(boundary, follow_anchor) {
                Ok(base) => base,
                Err(e) => return Err(unsafe_component(dir, boundary, e)),
            }
        }
        Err(e) => return Err(unsafe_component(dir, boundary, e)),
    };
    let mut cursor = boundary.to_path_buf();
    for component in relative.components() {
        let name = component.as_os_str();
        cursor.push(name);
        current = match current.open_dir(name) {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match current.mkdir(name) {
                Ok(child) => child,
                Err(e) => return Err(unsafe_component(dir, &cursor, e)),
            },
            Err(e) => return Err(unsafe_component(dir, &cursor, e)),
        };
    }
    Ok(current)
}

/// The `ensure_owned_parent` refusal for an opened component that cannot
/// carry the tree — a link or a non-directory — preserving the original
/// message text for each; anything else is passed through unchanged.
/// Not every Unix reports a link refusal as ELOOP (Darwin's
/// `O_NOFOLLOW|O_DIRECTORY` open answers ENOTDIR), so the message's
/// evidence comes from the component's own metadata when the errno is
/// ambiguous — diagnostic only; the operation already failed closed.
fn unsafe_component(dir: &Path, component: &Path, e: std::io::Error) -> anyhow::Error {
    #[cfg(unix)]
    let symlink = e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
        || (e.kind() == std::io::ErrorKind::NotADirectory
            && fs::symlink_metadata(component)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false));
    #[cfg(not(unix))]
    let symlink = crate::dirfd::is_symlink_refusal(&e);
    if symlink {
        anyhow::anyhow!(
            "refusing to create {} — component {} is a symlink",
            dir.display(),
            component.display()
        )
    } else if e.kind() == std::io::ErrorKind::NotADirectory {
        anyhow::anyhow!(
            "refusing to create {} — component {} is not a directory",
            dir.display(),
            component.display()
        )
    } else {
        e.into()
    }
}

/// A bounded staging component beside `dest`: `.paste-staged.{kind}.
/// {tag}.{digest}` — under every filesystem's component limit no matter
/// how long the legal destination name is. `tag` carries the attempt's
/// uniqueness (pid + counter); the FNV-1a digest of the destination
/// name keeps same-directory staging for different outputs distinct.
pub(crate) fn staged_tmp_component(dest: &Path, kind: &str, tag: &str) -> std::ffi::OsString {
    let digest = dest
        .file_name()
        .unwrap_or_default()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    std::ffi::OsString::from(format!(".paste-staged.{kind}.{tag}.{digest:016x}"))
}

/// [`staged_tmp_component`] as a full path — for tests that reason
/// about the staged name on disk rather than operate on it.
#[cfg(test)]
pub(crate) fn staged_tmp_name(dir: &Path, dest: &Path, kind: &str, tag: &str) -> PathBuf {
    dir.join(staged_tmp_component(dest, kind, tag))
}

/// Whether the opened `file` is a regular file whose bytes already equal
/// `content`. The type check runs on the handle itself, so a name swapped
/// between the earlier kind lookup and the open cannot pass a special
/// file off as the compared target; the read is bounded by the expected
/// length, so an oversized stand-in can neither allocate nor read past
/// what a matching file could contain.
fn compare_existing(file: &std::fs::File, content: &[u8]) -> std::io::Result<bool> {
    if !crate::dirfd::OwnedDir::opened_file_is_regular(file)? {
        return Ok(false);
    }
    let mut limited = std::io::Read::take(file, content.len() as u64 + 1);
    let mut existing = Vec::with_capacity(content.len().saturating_add(1));
    std::io::Read::read_to_end(&mut limited, &mut existing)?;
    Ok(existing == content)
}

/// Stage `content` in an owned temporary file beside `file`, then
/// rename it over the destination — both operations anchored to `dir`,
/// the descriptor `ensure_owned_parent` pinned open, so a directory
/// swapped for a link after validation cannot redirect either one.
/// The temp name is unique per attempt, bounded well under every
/// filesystem's component-length limit — a legal destination name near
/// 255 bytes must not fail staging — and opened with `create_new`, so
/// it can only ever be the file this call just made — never a
/// pre-existing link — and the rename replaces the destination's
/// directory entry atomically without opening it.
fn write_staged(dir: &crate::dirfd::OwnedDir, file: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    let dest = file.file_name().unwrap_or_default();
    for attempt in 0..100u32 {
        let tmp =
            staged_tmp_component(file, "file", &format!("{}.{}", std::process::id(), attempt));
        match dir.create_new(&tmp) {
            Ok(mut handle) => {
                // Durable before it is visible: a crash after the rename must
                // not leave a published name pointing at unwritten bytes.
                if let Err(e) = handle.write_all(content).and_then(|_| handle.sync_all()) {
                    let _ = dir.remove_file(&tmp);
                    return Err(e.into());
                }
                drop(handle);
                match dir.rename(&tmp, dest) {
                    Ok(()) => {
                        dir.sync()?;
                        return Ok(());
                    }
                    Err(e) => {
                        let _ = dir.remove_file(&tmp);
                        return Err(e.into());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("could not stage output for {}", file.display())
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

// Directory scans `prune_versions` has performed on the calling thread.
// A bounded history can still be long, so a rebuild's scan count — not its
// timing — is what a regression test asserts; keep-all never scans.
// Thread-local rather than process-wide so the parallel test runner cannot
// move the measurement. Test-only.
#[cfg(test)]
thread_local! {
    pub(crate) static PRUNE_VERSION_SCANS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
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
        let retained = HashSet::from([
            dir.path().join(format!("js/app.{}.v2.min.js", hash('b'))),
            dir.path().join(format!("js/app.{}.v3.min.js", hash('c'))),
        ]);
        storage
            .prune_versions_set(&current, &retained, dir.path())
            .unwrap();

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
    fn test_keep_all_prune_leaves_files_the_manifest_does_not_name() {
        // Under keep-all a versioned file absent from the loaded manifest
        // (a deleted or replaced manifest) may still back a minted page:
        // pruning leaves it in place.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), usize::MAX);
        for (version, ch) in [('1', 'a'), ('2', 'b')] {
            let name = format!("app.{}.v{version}.min.js", hash(ch));
            storage
                .write_asset(version.to_string().as_bytes(), "js", &name)
                .unwrap();
        }
        storage
            .prune_versions_set(
                &dir.path().join(format!("js/app.{}.v2.min.js", hash('b'))),
                &HashSet::new(),
                dir.path(),
            )
            .unwrap();
        assert!(dir
            .path()
            .join(format!("js/app.{}.v1.min.js", hash('a')))
            .exists());
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
            .prune_versions_set(
                &dir.path().join(format!("js/app.{}.v2.min.js", hash('b'))),
                &HashSet::new(),
                dir.path(),
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
        storage
            .prune_versions_set(&current, &HashSet::new(), dir.path())
            .unwrap();

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
    fn test_prune_versions_public_slice_api_keeps_current_and_history() {
        // The public entry point takes a retained slice: the current file
        // plus the named historical version survive.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::dist(dir.path(), 2);

        for (version, ch) in [('1', 'a'), ('2', 'b'), ('3', 'c')] {
            let name = format!("app.{}.v{version}.min.js", hash(ch));
            storage
                .write_asset(version.to_string().as_bytes(), "js", &name)
                .unwrap();
        }

        let current = dir.path().join(format!("js/app.{}.v3.min.js", hash('c')));
        let historical = dir.path().join(format!("js/app.{}.v1.min.js", hash('a')));
        storage.prune_versions(&current, &[historical]).unwrap();

        assert!(
            dir.path()
                .join(format!("js/app.{}.v1.min.js", hash('a')))
                .exists(),
            "v1 should remain (a retained historical version)"
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
        let retained = HashSet::from([
            dir.path()
                .join(format!("js/paste/dom.{}.v2.min.js", hash('b'))),
            dir.path()
                .join(format!("js/paste/dom.{}.v3.min.js", hash('c'))),
        ]);
        storage
            .prune_versions_set(&current, &retained, dir.path())
            .unwrap();

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
        let retained = HashSet::from([dir.path().join(format!("js/{retained_file}"))]);
        storage
            .prune_versions_set(&current, &retained, dir.path())
            .unwrap();

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

    #[test]
    fn test_from_config_fails_closed_on_unsupported_backends() {
        // `s3` is a roadmap name, not a backend — constructing one must not
        // silently produce local storage, and neither may an arbitrary
        // string. A backend that reports success without persisting bytes
        // is worse than an error.
        let dir = tempdir().unwrap();
        for backend in ["s3", "S3", "bogus", "", "local ", "distx"] {
            let result = StorageBackend::from_config(backend, dir.path(), 10, HashMap::new());
            assert!(
                result.is_err(),
                "{backend:?} must not construct a backend: {result:?}"
            );
        }
        // Nothing was written anywhere — construction failure is total.
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "a rejected backend must not touch the output directory"
        );

        let local = StorageBackend::from_config("local", dir.path(), 10, HashMap::new()).unwrap();
        assert!(matches!(local, StorageBackend::Local { .. }));
        assert!(!local.supports_rollback());
        let dist = StorageBackend::from_config("dist", dir.path(), 3, HashMap::new()).unwrap();
        assert!(matches!(
            dist,
            StorageBackend::Dist {
                keep_versions: 3,
                ..
            }
        ));
        assert!(dist.supports_rollback());
    }

    #[test]
    fn test_write_asset_rejects_escaping_type_and_filename() {
        // `..`, absolute, and empty tails name positions outside the
        // resolved output directory — refused before any mutation. A
        // caller could otherwise compose `base/..` into a write
        // anywhere on the filesystem.
        let dir = tempdir().unwrap();
        let storage = StorageBackend::local(dir.path().join("dist"));
        let sentinel = dir.path().join("sentinel.js");
        fs::write(&sentinel, b"leave me").unwrap();

        for bad_type in ["..", "../x", "/abs", ""] {
            let error = storage
                .write_asset(b"new bytes", bad_type, "app.js")
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("invalid asset type"),
                "{bad_type:?}: got {error:#}"
            );
        }
        for bad_name in ["../sentinel.js", "/abs.js", "..", ""] {
            let error = storage
                .write_asset(b"new bytes", "js", bad_name)
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("invalid asset filename"),
                "{bad_name:?}: got {error:#}"
            );
        }
        assert_eq!(fs::read(&sentinel).unwrap(), b"leave me");
        assert!(
            !sentinel.with_file_name("app.js").exists(),
            "a rejected write still created a file beside the sentinel"
        );

        // The ordinary valid write still lands and reports its URL.
        let name = "app.0123456789abcdef0123456789abcdef.v1.min.js";
        let url = storage.write_asset(b"ok", "js", name).unwrap();
        assert_eq!(url, format!("/js/{name}"));
        assert_eq!(
            fs::read(dir.path().join("dist/js").join(name)).unwrap(),
            b"ok"
        );
    }

    #[test]
    fn test_prune_versions_refuses_targets_outside_its_roots() {
        // A `current` path beneath no configured output root must never
        // steer the version scan into an unrelated directory — a
        // sibling tree holding lookalike versioned names is not this
        // storage's to prune.
        let dir = tempdir().unwrap();
        let dist = dir.path().join("dist");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let storage = StorageBackend::dist(&dist, 10);

        let hash = "0123456789abcdef0123456789abcdef";
        let v1 = outside.join(format!("app.{hash}.v1.min.js"));
        let v2 = outside.join(format!("app.{hash}.v2.min.js"));
        fs::write(&v1, b"v1").unwrap();
        fs::write(&v2, b"v2").unwrap();

        let error = storage.prune_versions(&v2, &[]).unwrap_err();
        assert!(
            format!("{error:#}").contains("outside every established output root"),
            "got: {error:#}"
        );
        // A `..` spelling that keeps the lexical prefix is refused too.
        let escaped = dist
            .join("../outside")
            .join(format!("app.{hash}.v2.min.js"));
        let error = storage.prune_versions(&escaped, &[]).unwrap_err();
        assert!(
            format!("{error:#}").contains("outside every established output root"),
            "got: {error:#}"
        );
        assert!(v1.exists() && v2.exists(), "outside sentinels were pruned");
    }

    #[test]
    fn test_prune_versions_allows_a_configured_type_override_outside_base() {
        // Per-type output roots outside the backend base are established
        // locations too — pruning under one evicts only unreferenced
        // versions of the exact stem.
        let dir = tempdir().unwrap();
        let base = dir.path().join("dist");
        let js_root = dir.path().join("elsewhere/js");
        fs::create_dir_all(&js_root).unwrap();
        let mut type_dirs = HashMap::new();
        type_dirs.insert("js".to_string(), js_root.clone());
        let storage = StorageBackend::from_config("dist", &base, 10, type_dirs).unwrap();

        let h1 = "11111111111111111111111111111111";
        let h2 = "22222222222222222222222222222222";
        let old = js_root.join(format!("app.{h1}.v1.min.js"));
        let current = js_root.join(format!("app.{h2}.v2.min.js"));
        let other_stem = js_root.join(format!("app.theme.{h1}.v1.min.js"));
        fs::write(&old, b"v1").unwrap();
        fs::write(&current, b"v2").unwrap();
        fs::write(&other_stem, b"other").unwrap();

        storage.prune_versions(&current, &[]).unwrap();
        assert!(!old.exists(), "the unreferenced old version survived");
        assert!(current.exists(), "the current version was pruned");
        assert!(other_stem.exists(), "a different stem was pruned");
    }

    #[test]
    #[cfg(unix)]
    fn test_prune_versions_refuses_a_symlinked_output_root() {
        // The write path refuses a symlinked configured root; the public
        // prune must not accept what writing would reject — `current`
        // spelled beneath the link would scan the destination and unlink
        // matching versioned files inside it.
        let dir = tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let dist = dir.path().join("dist");
        fs::create_dir_all(&dist).unwrap();
        let js_link = dist.join("js");
        std::os::unix::fs::symlink(&outside, &js_link).unwrap();
        let mut type_dirs = HashMap::new();
        type_dirs.insert("js".to_string(), js_link.clone());
        let storage = StorageBackend::from_config("dist", &dist, 10, type_dirs).unwrap();

        let hash = "0123456789abcdef0123456789abcdef";
        let v1 = outside.join(format!("app.{hash}.v1.min.js"));
        let v2 = outside.join(format!("app.{hash}.v2.min.js"));
        fs::write(&v1, b"v1").unwrap();
        fs::write(&v2, b"v2").unwrap();

        let error = storage
            .prune_versions(&js_link.join(format!("app.{hash}.v2.min.js")), &[])
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("output root is a symlink"),
            "got: {error:#}"
        );
        assert_eq!(fs::read(&v1).unwrap(), b"v1");
        assert_eq!(fs::read(&v2).unwrap(), b"v2");

        // The backend base itself behind a link refuses the same way.
        let base_link = dir.path().join("linked-dist");
        std::os::unix::fs::symlink(&outside, &base_link).unwrap();
        let storage = StorageBackend::dist(&base_link, 10);
        let error = storage
            .prune_versions(&base_link.join(format!("app.{hash}.v2.min.js")), &[])
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("output root is a symlink"),
            "got: {error:#}"
        );
        assert_eq!(fs::read(&v1).unwrap(), b"v1");
        assert_eq!(fs::read(&v2).unwrap(), b"v2");
    }

    #[test]
    fn test_prune_versions_refuses_a_non_directory_output_root() {
        // A regular file parked at the configured root is not a scan
        // directory — refuse by name rather than fail ambiguously inside
        // the walk.
        let dir = tempdir().unwrap();
        let dist = dir.path().join("dist");
        fs::create_dir_all(&dist).unwrap();
        let js_file = dist.join("js");
        fs::write(&js_file, b"not a directory").unwrap();
        let mut type_dirs = HashMap::new();
        type_dirs.insert("js".to_string(), js_file.clone());
        let storage = StorageBackend::from_config("dist", &dist, 10, type_dirs).unwrap();

        let hash = "0123456789abcdef0123456789abcdef";
        let error = storage
            .prune_versions(&js_file.join(format!("app.{hash}.v2.min.js")), &[])
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("not a directory"),
            "got: {error:#}"
        );
    }

    #[test]
    fn test_owned_dir_beneath_requires_reaching_the_boundary() {
        // A directory outside the boundary was never established as
        // owned — failing to descend from the anchor is a refusal, not
        // proof.
        let dir = tempdir().unwrap();
        let owned = dir.path().join("dist");
        let other = dir.path().join("other");
        fs::create_dir_all(&other).unwrap();

        assert!(matches!(
            owned_dir_beneath(&other, &owned).unwrap(),
            crate::dirfd::Descend::Unsafe
        ));
        assert!(matches!(
            owned_dir_beneath(&owned.join("../other"), &owned).unwrap(),
            crate::dirfd::Descend::Unsafe
        ));
        // A missing but contained target reports Missing: there is
        // nothing inside it to unlink.
        assert!(matches!(
            owned_dir_beneath(&owned.join("js"), &owned).unwrap(),
            crate::dirfd::Descend::Missing
        ));
        fs::create_dir_all(owned.join("js")).unwrap();
        assert!(matches!(
            owned_dir_beneath(&owned.join("js"), &owned).unwrap(),
            crate::dirfd::Descend::Anchored(_)
        ));
        assert!(matches!(
            owned_dir_beneath(&owned, &owned).unwrap(),
            crate::dirfd::Descend::Anchored(_)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn test_write_file_refuses_a_symlinked_output_root() {
        // The boundary is checked when the walk reaches it: a symlink at
        // the configured output root would redirect every write beneath
        // it into a tree this build never owned.
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let storage = StorageBackend::local(dir.path());
        let file = link.join("app.js");
        let error = storage.write_file(&file, b"x", &link).unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "got: {error:#}");
        assert!(
            !real.join("app.js").exists(),
            "the write followed the symlinked root"
        );
    }

    /// A destination name near the filesystem's component limit is legal
    /// output; staging must not push a longer temporary name over the
    /// edge and fail a write the destination itself allows.
    #[test]
    fn test_write_file_stages_beside_a_near_limit_name() {
        let dir = tempdir().unwrap();
        let storage = StorageBackend::local(dir.path());
        let long_name = "x".repeat(240) + ".js";
        let file = dir.path().join(&long_name);
        storage
            .write_file(&file, b"content", dir.path())
            .expect("a legal 245-byte destination name must stage");
        assert_eq!(fs::read(&file).unwrap(), b"content");
        // Nothing staged is stranded beside it.
        let stray: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".paste-staged"))
            .collect();
        assert!(stray.is_empty(), "staging left residue: {stray:?}");
    }

    /// The check-to-use window a pathname-only guard cannot close: after
    /// the directory is validated and pinned, its name is replaced by a
    /// symlink into a foreign tree. The staged create and rename run
    /// against the pinned descriptor, so the sentinel must be untouched
    /// and the output must land in the real directory.
    #[test]
    #[cfg(unix)]
    fn test_staged_write_survives_a_post_validation_directory_swap() {
        let dir = tempdir().unwrap();
        let owned = dir.path().join("dist");
        let js = owned.join("js");
        let sentinel = dir.path().join("sentinel");
        fs::create_dir_all(&js).unwrap();
        fs::create_dir_all(&sentinel).unwrap();
        fs::write(sentinel.join("watch.txt"), b"sentinel").unwrap();
        let target = js.join("out.js");

        // Validation pins the directory …
        let pinned = ensure_owned_parent(&js, &owned).unwrap();
        // … then the name is swapped for a link into foreign territory —
        // the real directory moves aside, keeping the inode reachable.
        let moved = dir.path().join("js-real");
        fs::rename(&js, &moved).unwrap();
        std::os::unix::fs::symlink(&sentinel, &js).unwrap();

        write_staged(&pinned, &target, b"body").unwrap();

        assert_eq!(fs::read(sentinel.join("watch.txt")).unwrap(), b"sentinel");
        assert!(
            !sentinel.join("out.js").exists(),
            "the staged write crossed the swapped symlink"
        );
        assert_eq!(
            fs::read(moved.join("out.js")).unwrap(),
            b"body",
            "the write did not land in the pinned directory"
        );
    }

    /// The same window for a destructive unlink: the scan directory is
    /// validated and pinned, then its name is replaced by a symlink into
    /// a sentinel directory holding an identically-named file. The
    /// unlink must hit the pinned inode's entry — never the sentinel's.
    #[test]
    #[cfg(unix)]
    fn test_anchored_unlink_survives_a_post_validation_directory_swap() {
        let dir = tempdir().unwrap();
        let owned = dir.path().join("dist");
        let js = owned.join("js");
        let sentinel = dir.path().join("sentinel");
        fs::create_dir_all(&js).unwrap();
        fs::create_dir_all(&sentinel).unwrap();
        let name = "mod.0123456789abcdef0123456789abcdef.v1.min.js";
        fs::write(js.join(name), b"owned").unwrap();
        fs::write(sentinel.join(name), b"sentinel").unwrap();

        let pinned = match owned_dir_beneath(&js, &owned).unwrap() {
            crate::dirfd::Descend::Anchored(dir) => dir,
            _ => panic!("the owned scan dir must pin"),
        };
        let moved = dir.path().join("js-real");
        fs::rename(&js, &moved).unwrap();
        std::os::unix::fs::symlink(&sentinel, &js).unwrap();

        pinned.remove_file(std::ffi::OsStr::new(name)).unwrap();

        assert_eq!(
            fs::read(sentinel.join(name)).unwrap(),
            b"sentinel",
            "the unlink crossed the swapped symlink"
        );
        assert!(!moved.join(name).exists(), "the pinned entry still exists");
    }

    /// A special file at the destination is never opened for
    /// comparison: a FIFO planted there cannot stall the build, and the
    /// staged rename replaces it like any other non-regular occupant.
    /// A blocking implementation would hang this test rather than fail
    /// it — the return is the assertion.
    #[test]
    #[cfg(unix)]
    fn test_write_file_replaces_a_fifo_destination_without_blocking() {
        let dir = tempdir().unwrap();
        let owned = dir.path().join("dist");
        let js = owned.join("js");
        fs::create_dir_all(&js).unwrap();
        let target = js.join("out.js");
        mkfifo(&target);

        StorageBackend::local(&owned)
            .write_file(&target, b"body", &owned)
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"body");
        assert!(fs::metadata(&target).unwrap().is_file());
    }

    #[test]
    fn test_write_file_rejects_content_past_the_served_bound() {
        // A build emitting past the read cap would publish a file the
        // runtime cannot serve — refuse at write time with the limit
        // named, never green-build a runtime 404.
        let dir = tempdir().unwrap();
        let owned = dir.path().join("dist");
        let js = owned.join("js");
        fs::create_dir_all(&js).unwrap();
        let target = js.join("out.js");

        let oversized = vec![b'x'; MAX_COMPILED_ASSET_BYTES as usize + 1];
        let err = StorageBackend::local(&owned)
            .write_file(&target, &oversized, &owned)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("compiled-asset limit"),
            "got {err:#}"
        );
        assert!(!target.exists(), "an oversized asset must not publish");

        // Exactly at the bound still writes.
        let at_bound = vec![b'x'; MAX_COMPILED_ASSET_BYTES as usize];
        StorageBackend::local(&owned)
            .write_file(&target, &at_bound, &owned)
            .unwrap();
        assert_eq!(
            fs::metadata(&target).unwrap().len() as u64,
            MAX_COMPILED_ASSET_BYTES
        );
    }

    #[cfg(unix)]
    fn mkfifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo {} failed", path.display());
    }

    /// A regular file that does not match still gets the same answer the
    /// type check gives a FIFO: the destination is replaced. The read
    /// bound proves itself here — an existing file larger than the new
    /// content is different by length alone, without comparing a byte
    /// past what a matching file could hold.
    #[test]
    fn test_write_file_rewrites_an_oversized_destination() {
        let dir = tempdir().unwrap();
        let owned = dir.path().join("dist");
        let js = owned.join("js");
        fs::create_dir_all(&js).unwrap();
        let target = js.join("out.js");
        fs::write(&target, vec![0u8; 1024 * 1024]).unwrap();

        StorageBackend::local(&owned)
            .write_file(&target, b"body", &owned)
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"body");
    }

    /// An unchanged destination keeps its bytes and its mtime — the
    /// compare path returns "identical" only for a verified regular
    /// file holding exactly the content.
    #[test]
    fn test_write_file_unchanged_destination_is_left_alone() {
        let dir = tempdir().unwrap();
        let owned = dir.path().join("dist");
        let js = owned.join("js");
        fs::create_dir_all(&js).unwrap();
        let target = js.join("out.js");
        let storage = StorageBackend::local(&owned);
        storage.write_file(&target, b"body", &owned).unwrap();
        let before = fs::metadata(&target).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        storage.write_file(&target, b"body", &owned).unwrap();

        assert_eq!(
            fs::metadata(&target).unwrap().modified().unwrap(),
            before,
            "an identical destination was rewritten"
        );
    }

    /// A directory occupying the destination can neither be compared
    /// nor renamed over — the build refuses it rather than succeeding
    /// or descending into it.
    #[test]
    fn test_write_file_refuses_a_directory_destination() {
        let dir = tempdir().unwrap();
        let owned = dir.path().join("dist");
        let js = owned.join("js");
        fs::create_dir_all(js.join("out.js")).unwrap();

        let result = StorageBackend::local(&owned).write_file(&js.join("out.js"), b"body", &owned);

        assert!(result.is_err(), "a directory destination was written");
        assert!(js.join("out.js").is_dir(), "the directory was moved");
    }
}
