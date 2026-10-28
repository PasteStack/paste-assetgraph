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

//! Directory operations anchored to an opened handle.
//!
//! Validating a directory chain and then acting on its pathname leaves a
//! window: the checked directory — or one of its components — can be
//! replaced by a symlink between the check and the operation, redirecting
//! a write, rename, or unlink into a tree this build never owned. On
//! Unix every operation below runs through `openat`/`renameat`/`unlinkat`
//! against an `O_DIRECTORY | O_NOFOLLOW` descriptor acquired at
//! validation time, so the checked inode itself carries the work — a
//! later swap changes the name, never the pinned directory. Other
//! platforms retain the pathname walk: each component is checked as it is
//! descended, and the pinned directory itself (not the chain above it) is
//! re-checked at the moment each mutation runs — the window narrows but
//! does not close.
//!
//! The trust boundary is the anchor itself: only descent *beneath* it
//! refuses links. The anchor's own ancestors may legitimately resolve
//! through symlinks — a configured output root reached through a version
//! alias is a normal deployment shape, and `OwnedDir::open` pins whatever
//! real directory the configured path names rather than policing how it
//! was reached.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::Path;
#[cfg(not(unix))]
use std::path::PathBuf;

#[cfg(unix)]
use std::os::fd::OwnedFd;

/// What a single path component is, looked up without following a
/// trailing symlink — the anchored equivalent of `symlink_metadata`'s
/// file-type read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Symlink,
    Directory,
    Regular,
    Other,
}

/// The outcome of descending beneath an anchor — an owned directory
/// handle to operate on, or the reason descent refused.
pub(crate) enum Descend {
    Anchored(OwnedDir),
    /// A component resolved to a symlink or a non-directory — the path
    /// redirects outside the owned tree.
    Unsafe,
    /// A component does not exist — there is nothing beneath to act on.
    Missing,
}

/// The payload a non-Unix branch wraps into its symlink-refusal errors.
/// Stable `ErrorKind` carries no dedicated is-a-link variant (`FilesystemLoop`
/// is unstable), so the refusal is `Other` + this marker — classifiers
/// recognize it by downcast rather than a kind that cannot compile.
#[cfg(not(unix))]
#[derive(Debug)]
pub(crate) struct SymlinkRefused(pub(crate) String);

#[cfg(not(unix))]
impl std::fmt::Display for SymlinkRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(not(unix))]
impl std::error::Error for SymlinkRefused {}

/// Whether the error carries a [`SymlinkRefused`] marker.
#[cfg(not(unix))]
pub(crate) fn is_symlink_refusal(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|r| r.is::<SymlinkRefused>())
}

/// The directory component a path's parent really is. `Path::parent`
/// answers `Some("")` for a bare filename — an empty path opens nothing,
/// so the fallback to `"."` must filter empties, not `None`.
pub(crate) fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// An opened directory every relative operation stays pinned to.
///
/// On Unix this is an `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` descriptor;
/// on other platforms it is the already-validated root path the
/// operations join — the same pathname behavior the tree has today.
pub(crate) struct OwnedDir {
    #[cfg(unix)]
    fd: OwnedFd,
    #[cfg(not(unix))]
    root: PathBuf,
}

// The cfg(unix)/cfg(not(unix)) sibling blocks each need `return` to keep
// the other's block from being tail position — the lint cannot see that
// only one block ever compiles.
#[allow(clippy::needless_return)]
impl OwnedDir {
    /// Open `path` itself as the anchor — a symlinked leaf is refused,
    /// so the pinned descriptor can never be a redirect.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let fd = rustix::fs::openat(
                rustix::fs::CWD,
                path,
                rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            return Ok(Self { fd });
        }
        #[cfg(not(unix))]
        {
            let meta = std::fs::symlink_metadata(path)?;
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    SymlinkRefused(format!("{} is a symlink", path.display())),
                ));
            }
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} is not a directory", path.display()),
                ));
            }
            Ok(Self {
                root: path.to_path_buf(),
            })
        }
    }

    /// Open a configured trust anchor — the project root or an established
    /// output root — following a symlink at the anchor itself: how the
    /// operator reached the directory (a deploy-style `current` link,
    /// macOS `/tmp`) is configuration, not a swap. Everything beneath the
    /// anchor is still descended with no-follow semantics.
    pub(crate) fn open_anchor(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let fd = rustix::fs::openat(
                rustix::fs::CWD,
                path,
                rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            return Ok(Self { fd });
        }
        #[cfg(not(unix))]
        {
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} is not a directory", path.display()),
                ));
            }
            Ok(Self {
                root: path.to_path_buf(),
            })
        }
    }

    /// Descend `rel` one component at a time, refusing a symlinked or
    /// non-directory component at the moment it is used — a component
    /// swapped to a link after the anchor was pinned cannot redirect the
    /// result.
    ///
    /// `rel` must contain ordinary components only (the callers' lexical
    /// containment check already guarantees it). A missing component
    /// answers [`Descend::Missing`]; a link or non-directory answers
    /// [`Descend::Unsafe`].
    pub(crate) fn descend(&self, rel: &Path) -> io::Result<Descend> {
        let mut dir = self.clone_dir()?;
        for component in rel.components() {
            let name = match component {
                std::path::Component::Normal(name) => name,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is not an ordinary component spelling", rel.display()),
                    ))
                }
            };
            match dir.open_dir(name) {
                Ok(child) => dir = child,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Descend::Missing),
                Err(e) if is_unsafe_component(&e) => return Ok(Descend::Unsafe),
                Err(e) => return Err(e),
            }
        }
        Ok(Descend::Anchored(dir))
    }

    /// The kind of `name`, without following a trailing link.
    pub(crate) fn entry_kind(&self, name: &OsStr) -> io::Result<EntryKind> {
        #[cfg(unix)]
        {
            let stat = rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            return Ok(match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
                rustix::fs::FileType::Symlink => EntryKind::Symlink,
                rustix::fs::FileType::Directory => EntryKind::Directory,
                rustix::fs::FileType::RegularFile => EntryKind::Regular,
                _ => EntryKind::Other,
            });
        }
        #[cfg(not(unix))]
        {
            let meta = std::fs::symlink_metadata(self.root.join(name))?;
            Ok(if meta.file_type().is_symlink() {
                EntryKind::Symlink
            } else if meta.is_dir() {
                EntryKind::Directory
            } else if meta.is_file() {
                EntryKind::Regular
            } else {
                EntryKind::Other
            })
        }
    }

    /// Create `name` exclusively — a planted same-name file or link is a
    /// collision to retry, never something to open, truncate, or follow.
    pub(crate) fn create_new(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            let fd = rustix::fs::openat(
                &self.fd,
                name,
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o666),
            )
            .map_err(std::io::Error::from)?;
            return Ok(File::from(fd));
        }
        #[cfg(not(unix))]
        {
            self.revalidate()?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join(name))
        }
    }

    /// Open `name` for reading without following a trailing link — reads
    /// stay inside the pinned tree too.
    ///
    /// `O_NONBLOCK` rides along on every open: an entry swapped to a FIFO
    /// between the caller's kind check and this call returns immediately
    /// instead of waiting on a writer that never comes. The flag is
    /// inert for the regular files these opens ever legitimately reach.
    pub(crate) fn open_file(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            let fd = rustix::fs::openat(
                &self.fd,
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            return Ok(File::from(fd));
        }
        #[cfg(not(unix))]
        {
            // No O_NOFOLLOW here: refuse a trailing link by inspecting the
            // entry first (the pathname fallback's documented window).
            let path = self.root.join(name);
            if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    SymlinkRefused(format!("{} is a symlink", path.display())),
                ));
            }
            File::open(path)
        }
    }

    /// Open `name` for reading, following a trailing link — the read
    /// path's counterpart to [`Self::open_file`] for entries a deployment
    /// legitimately symlinks (a shipped output tree assembled by link is
    /// fine; the bytes still carry their recorded hash check). The same
    /// `O_NONBLOCK` guarantee keeps a link-to-FIFO from stalling, and the
    /// caller re-verifies the opened handle's type before reading.
    pub(crate) fn open_file_follow(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            let fd = rustix::fs::openat(
                &self.fd,
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            return Ok(File::from(fd));
        }
        #[cfg(not(unix))]
        {
            File::open(self.root.join(name))
        }
    }

    /// Open `name` for reading and writing, creating it when absent —
    /// never truncating and never following a trailing link. A symlinked
    /// leaf refuses at the same `openat` that would have followed it, so
    /// there is no check/open window for a lock file to be swapped
    /// through. The caller verifies the opened handle is a regular file.
    pub(crate) fn open_or_create(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            let fd = rustix::fs::openat(
                &self.fd,
                name,
                rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o666),
            )
            .map_err(std::io::Error::from)?;
            return Ok(File::from(fd));
        }
        #[cfg(not(unix))]
        {
            self.revalidate()?;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.root.join(name))
        }
    }

    /// The opened handle answers what it actually is — the post-open
    /// check that closes the window between a pathname's type read and
    /// its open. Anything but a regular file is not safe to read to a
    /// completion bound.
    pub(crate) fn opened_file_is_regular(file: &File) -> io::Result<bool> {
        Ok(file.metadata()?.file_type().is_file())
    }

    /// Flush this directory's entries to stable storage, so a rename just
    /// made survives a crash. Best effort where the platform has no
    /// directory fsync.
    pub(crate) fn sync(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            return rustix::fs::fsync(&self.fd).map_err(std::io::Error::from);
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }

    /// Atomically replace `to` with `from` inside this directory — both
    /// names resolve against the pinned descriptor, never a swapped path.
    pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            return rustix::fs::renameat(&self.fd, from, &self.fd, to)
                .map_err(std::io::Error::from);
        }
        #[cfg(not(unix))]
        {
            self.revalidate()?;
            std::fs::rename(self.root.join(from), self.root.join(to))
        }
    }

    /// Unlink `name` — the entry itself, never a target a link points at.
    pub(crate) fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            return rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())
                .map_err(std::io::Error::from);
        }
        #[cfg(not(unix))]
        {
            self.revalidate()?;
            std::fs::remove_file(self.root.join(name))
        }
    }

    /// The directory's entry names — the anchored scan prune and cleanup
    /// use in place of `fs::read_dir`.
    pub(crate) fn entries(&self) -> io::Result<Vec<OsString>> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut found = Vec::new();
            for entry in rustix::fs::Dir::read_from(&self.fd).map_err(std::io::Error::from)? {
                let entry = entry.map_err(std::io::Error::from)?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                found.push(OsStr::from_bytes(name).to_os_string());
            }
            return Ok(found);
        }
        #[cfg(not(unix))]
        {
            let mut found = Vec::new();
            for entry in std::fs::read_dir(&self.root)? {
                found.push(entry?.file_name());
            }
            Ok(found)
        }
    }

    /// The pathname fallback's enforcement limit: without descriptor
    /// anchoring, a mutation resolves its path twice — once at
    /// validation, once at the syscall. Re-validating the pinned root
    /// immediately before each operation narrows that window to the
    /// final check; the residual is documented, not hidden.
    #[cfg(not(unix))]
    fn revalidate(&self) -> io::Result<()> {
        let meta = std::fs::symlink_metadata(&self.root)?;
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                SymlinkRefused(format!("{} is a symlink", self.root.display())),
            ));
        }
        if !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", self.root.display()),
            ));
        }
        Ok(())
    }

    fn clone_dir(&self) -> io::Result<Self> {
        #[cfg(unix)]
        {
            return Ok(Self {
                fd: self.fd.try_clone()?,
            });
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                root: self.root.clone(),
            })
        }
    }

    /// Open one child component as an `OwnedDir` — `O_NOFOLLOW` refuses a
    /// link and `O_DIRECTORY` a non-directory, at use time.
    #[cfg(unix)]
    pub(crate) fn open_dir(&self, name: &OsStr) -> io::Result<OwnedDir> {
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        Ok(OwnedDir { fd })
    }

    /// Create one child component beneath the pinned parent and return it
    /// as an `OwnedDir`.
    #[cfg(unix)]
    pub(crate) fn mkdir(&self, name: &OsStr) -> io::Result<OwnedDir> {
        rustix::fs::mkdirat(&self.fd, name, rustix::fs::Mode::from_raw_mode(0o777))
            .map_err(std::io::Error::from)?;
        self.open_dir(name)
    }

    #[cfg(not(unix))]
    pub(crate) fn open_dir(&self, name: &OsStr) -> io::Result<OwnedDir> {
        let path = self.root.join(name);
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                SymlinkRefused(format!("{} is a symlink", path.display())),
            ));
        }
        if !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", path.display()),
            ));
        }
        Ok(OwnedDir { root: path })
    }

    #[cfg(not(unix))]
    pub(crate) fn mkdir(&self, name: &OsStr) -> io::Result<OwnedDir> {
        let path = self.root.join(name);
        std::fs::create_dir(&path)?;
        Ok(OwnedDir { root: path })
    }
}

/// Whether an open/descend error means the component cannot be trusted
/// as a directory — a symlink refusal or a non-directory.
#[cfg(unix)]
pub(crate) fn is_unsafe_component(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotADirectory
        || e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
}

#[cfg(not(unix))]
pub(crate) fn is_unsafe_component(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotADirectory || is_symlink_refusal(e)
}
