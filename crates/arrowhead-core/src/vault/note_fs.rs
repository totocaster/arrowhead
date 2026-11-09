//! Note file operations that never follow symbolic links.
//!
//! Each operation opens the vault root, then opens every folder on the note
//! path relative to its parent's handle with `O_NOFOLLOW`, and finally creates,
//! truncates, or unlinks the note relative to the last handle. The kernel
//! refuses a symlink at the moment a name is used, so swapping a folder for a
//! symlink between two calls cannot redirect a write, unlink, or rmdir outside
//! the vault. A handle keeps pointing at the folder it opened even if another
//! process moves that folder elsewhere. Doing that needs write access outside
//! the vault and is out of scope.
//!
//! Internal symlinks are refused too. An alias such as `Notes/cfg -> ../.obsidian`
//! would let a note id reach folders the lexical id checks keep out.

use std::{io, path::PathBuf};

#[cfg(unix)]
pub(super) use unix::{create, remove, replace};
#[cfg(not(unix))]
pub(super) use unsupported::{create, remove, replace};

/// Why a note file operation stopped. Paths are relative to the vault root.
#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) enum Refusal {
    /// The entry at this path is a symbolic link.
    Symlink(PathBuf),
    /// Exclusive creation found an existing entry at the note path.
    Exists,
    /// Removal found no note at the note path.
    Missing,
    /// The filesystem rejected the operation at this path.
    Io(PathBuf, io::Error),
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::OsStr,
        fs::File,
        io::{self, Write},
        path::{Component, Path, PathBuf},
    };

    use rustix::{
        fd::OwnedFd,
        fs::{AtFlags, FileType, Mode, OFlags, mkdirat, open, openat, statat, unlinkat},
        io::Errno,
    };
    use tracing::warn;

    use super::Refusal;

    pub(in crate::vault) fn create(
        root: &Path,
        relative: &Path,
        contents: &[u8],
    ) -> Result<(), Refusal> {
        let (names, name) = split(relative)?;
        let folders = Folders::open(root, &names, true)?;
        let parent = folders.innermost();
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | file_flags();
        let file =
            openat(parent, name, flags, file_mode()).map_err(|errno| {
                match symlink_or_io(parent, name, relative.to_path_buf(), errno) {
                    Refusal::Io(..) if errno == Errno::EXIST => Refusal::Exists,
                    refusal => refusal,
                }
            })?;
        write_contents(file, relative, contents)
    }

    pub(in crate::vault) fn replace(
        root: &Path,
        relative: &Path,
        contents: &[u8],
    ) -> Result<(), Refusal> {
        let (names, name) = split(relative)?;
        let folders = Folders::open(root, &names, true)?;
        let parent = folders.innermost();
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | file_flags();
        let file = openat(parent, name, flags, file_mode())
            .map_err(|errno| symlink_or_io(parent, name, relative.to_path_buf(), errno))?;
        write_contents(file, relative, contents)
    }

    pub(in crate::vault) fn remove(root: &Path, relative: &Path) -> Result<Vec<PathBuf>, Refusal> {
        let (names, name) = split(relative)?;
        let folders = Folders::open(root, &names, false)?;
        let parent = folders.innermost();

        // `unlinkat` never follows the final name, so this check only enforces
        // the policy that symlinks are not notes. Containment does not rely on it.
        match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                return Err(Refusal::Symlink(relative.to_path_buf()));
            }
            Ok(_) => {}
            Err(Errno::NOENT) => return Err(Refusal::Missing),
            Err(errno) => return Err(Refusal::Io(relative.to_path_buf(), errno.into())),
        }
        unlinkat(parent, name, AtFlags::empty()).map_err(|errno| match errno {
            Errno::NOENT => Refusal::Missing,
            _ => Refusal::Io(relative.to_path_buf(), errno.into()),
        })?;

        let mut pruned = Vec::new();
        for depth in (0..names.len()).rev() {
            let folder = path_to(&names, depth);
            match unlinkat(folders.parent_of(depth), names[depth], AtFlags::REMOVEDIR) {
                Ok(()) => pruned.push(folder),
                Err(Errno::NOTEMPTY | Errno::EXIST) => break,
                Err(errno) => {
                    warn!(
                        folder = %folder.display(),
                        error = %errno,
                        "left empty note folder in place after deleting note"
                    );
                    break;
                }
            }
        }
        Ok(pruned)
    }

    struct Folders {
        root: OwnedFd,
        /// `opened[i]` is the folder at depth `i`, outermost first.
        opened: Vec<OwnedFd>,
    }

    impl Folders {
        /// Open the vault root and each folder below it.
        ///
        /// The root may sit behind symlinks. It was canonicalised when the
        /// vault was opened and is the trust anchor. Every folder below it is
        /// opened with `O_NOFOLLOW`. Missing folders are created when `create`
        /// is set and reported as [`Refusal::Missing`] otherwise.
        fn open(root: &Path, names: &[&OsStr], create: bool) -> Result<Self, Refusal> {
            let root_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
            let root = open(root, root_flags, Mode::empty())
                .map_err(|errno| Refusal::Io(PathBuf::new(), errno.into()))?;
            let mut folders = Self {
                root,
                opened: Vec::with_capacity(names.len()),
            };

            for (depth, name) in names.iter().copied().enumerate() {
                let parent = folders.innermost();
                let opened = match openat(parent, name, folder_flags(), Mode::empty()) {
                    Err(Errno::NOENT) if create => {
                        match mkdirat(parent, name, Mode::from_raw_mode(0o777)) {
                            Ok(()) | Err(Errno::EXIST) => {}
                            Err(errno) => {
                                return Err(Refusal::Io(path_to(names, depth), errno.into()));
                            }
                        }
                        openat(parent, name, folder_flags(), Mode::empty())
                    }
                    opened => opened,
                };
                let handle = opened.map_err(|errno| match errno {
                    Errno::NOENT if !create => Refusal::Missing,
                    _ => symlink_or_io(parent, name, path_to(names, depth), errno),
                })?;
                folders.opened.push(handle);
            }
            Ok(folders)
        }

        fn innermost(&self) -> &OwnedFd {
            self.opened.last().unwrap_or(&self.root)
        }

        fn parent_of(&self, depth: usize) -> &OwnedFd {
            match depth {
                0 => &self.root,
                _ => &self.opened[depth - 1],
            }
        }
    }

    /// Split a note path into folder names and the file name. Anything but
    /// plain names is rejected, so a `..` never reaches the descriptor walk.
    fn split(relative: &Path) -> Result<(Vec<&OsStr>, &OsStr), Refusal> {
        let names = relative
            .components()
            .map(|component| match component {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
        match names.as_deref() {
            Some([folders @ .., file]) => Ok((folders.to_vec(), file)),
            _ => Err(Refusal::Io(
                relative.to_path_buf(),
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "note path must be a relative path of plain names",
                ),
            )),
        }
    }

    /// Report a failed open as a symlink refusal when a symlink sits at `name`.
    ///
    /// The errno alone is not enough. macOS reports `ENOTDIR` for a symlinked
    /// folder opened with `O_DIRECTORY | O_NOFOLLOW`, and exclusive creation
    /// reports `EEXIST` for any symlink, including a broken one.
    fn symlink_or_io(parent: &OwnedFd, name: &OsStr, at: PathBuf, errno: Errno) -> Refusal {
        match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                Refusal::Symlink(at)
            }
            _ => Refusal::Io(at, errno.into()),
        }
    }

    fn write_contents(file: OwnedFd, relative: &Path, contents: &[u8]) -> Result<(), Refusal> {
        File::from(file)
            .write_all(contents)
            .map_err(|error| Refusal::Io(relative.to_path_buf(), error))
    }

    fn path_to(names: &[&OsStr], depth: usize) -> PathBuf {
        names[..=depth].iter().collect()
    }

    fn folder_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    fn file_flags() -> OFlags {
        OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    /// Same default as `std::fs::File::create`; the process umask still applies.
    fn file_mode() -> Mode {
        Mode::from_raw_mode(0o666)
    }
}

/// Note changes fail closed where descriptor-relative, no-follow operations
/// are not implemented, instead of falling back to path-based calls that
/// follow symlinks.
#[cfg(not(unix))]
mod unsupported {
    use std::{
        io,
        path::{Path, PathBuf},
    };

    use super::Refusal;

    pub(in crate::vault) fn create(
        _root: &Path,
        relative: &Path,
        _contents: &[u8],
    ) -> Result<(), Refusal> {
        Err(unsupported(relative))
    }

    pub(in crate::vault) fn replace(
        _root: &Path,
        relative: &Path,
        _contents: &[u8],
    ) -> Result<(), Refusal> {
        Err(unsupported(relative))
    }

    pub(in crate::vault) fn remove(_root: &Path, relative: &Path) -> Result<Vec<PathBuf>, Refusal> {
        Err(unsupported(relative))
    }

    fn unsupported(relative: &Path) -> Refusal {
        Refusal::Io(
            relative.to_path_buf(),
            io::Error::new(
                io::ErrorKind::Unsupported,
                "note changes need no-follow file operations that Arrowhead implements only on Unix",
            ),
        )
    }
}
