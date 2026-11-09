//! File changes below the vault root that never follow symbolic links and never reach an
//! existing entry through another spelling of its name.
//!
//! Each operation opens the vault root, then opens every folder on the path relative to its
//! parent's handle with `O_NOFOLLOW`, and acts on the final name relative to the innermost
//! handle. The kernel refuses a symlink at the moment a name is used, so swapping a folder for a
//! symlink between two calls cannot redirect a change outside the vault. A handle keeps pointing
//! at the folder it opened even if another process moves that folder elsewhere. Doing that needs
//! write access outside the vault and is out of scope.
//!
//! Internal symlinks are refused too. An alias such as `Notes/cfg -> ../.obsidian` would let a
//! path reach folders that the vault's path rules keep out.
//!
//! Changes also require every existing entry on the path to be spelled exactly as its folder
//! lists it. Case- and normalization-insensitive filesystems such as APFS open `.Obsidian`,
//! `ATTACHMENTS`, `attachmentſ`, or a decomposed `é` when the folder stores another spelling.
//! Refusing those spellings keeps the path that changes byte-for-byte equal to the path that the
//! caller's rules checked and that inventory lists. Reads skip this check because they change
//! nothing.

use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
pub(crate) use unix::{create, open_file, remove, rename, replace};
#[cfg(not(unix))]
pub(crate) use unsupported::{create, open_file, remove, rename, replace};

/// Explain a [`Refusal::Misspelled`] for an error message.
///
/// Spellings that differ only in Unicode composition, such as a precomposed `è` and `e` plus a
/// combining accent, look identical, so non-ASCII spellings also show their code points.
pub(crate) fn describe_misspelling(path: &Path, listed: Option<&Path>) -> String {
    match listed {
        Some(listed) if path.as_os_str().is_ascii() && listed.as_os_str().is_ascii() => format!(
            "the vault spells `{}` as `{}`",
            path.display(),
            listed.display()
        ),
        Some(listed) => format!(
            "the vault spells `{}` as `{}` (code points `{}` and `{}`)",
            path.display(),
            listed.display(),
            code_points(path),
            code_points(listed)
        ),
        None => format!(
            "`{}` reaches an existing entry that the vault spells differently",
            path.display()
        ),
    }
}

fn code_points(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii() {
                c.to_string()
            } else {
                c.escape_unicode().to_string()
            }
        })
        .collect()
}

/// Why a file operation stopped. Paths are relative to the vault root.
#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) enum Refusal {
    /// The entry at this path is a symbolic link.
    Symlink(PathBuf),
    /// An existing entry answers to `path`, but its folder lists it under another spelling.
    /// `listed` is that spelling when the listing shows which entry it is.
    Misspelled {
        path: PathBuf,
        listed: Option<PathBuf>,
    },
    /// The final entry already exists: exclusive creation, or a rename destination.
    Exists,
    /// The final entry, or a folder above it, does not exist.
    Missing,
    /// The filesystem rejected the operation at this path.
    Io(PathBuf, io::Error),
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::{OsStr, OsString},
        fs::{File, Permissions},
        io::{self, Write},
        os::unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt, fchown},
        },
        path::{Component, Path, PathBuf},
    };

    use rustix::{
        fd::OwnedFd,
        fs::{
            AtFlags, Dir, FileType, Mode, OFlags, fstat, mkdirat, open, openat, renameat, statat,
            unlinkat,
        },
        io::Errno,
    };
    use tracing::warn;

    use super::Refusal;

    /// Create a file that must not exist yet, creating missing parent folders.
    pub(crate) fn create(root: &Path, relative: &Path, contents: &[u8]) -> Result<(), Refusal> {
        let (names, name) = split(relative)?;
        let folders = Folders::open(root, &names, Walk::Create)?;
        let parent = folders.innermost();
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | file_flags();
        let file = openat(parent, name, flags, Mode::from_raw_mode(0o666)).map_err(|errno| {
            match symlink_or_io(parent, name, relative.to_path_buf(), errno) {
                Refusal::Io(..) if errno == Errno::EXIST => Refusal::Exists,
                refusal => refusal,
            }
        })?;
        File::from(file)
            .write_all(contents)
            .map_err(|error| Refusal::Io(relative.to_path_buf(), error))
    }

    /// Replace a file's contents, or create the file, creating missing parent folders.
    ///
    /// The contents go to a temporary file in the same folder, which is then renamed over the
    /// name. The previous file's inode is never written, so a hard link to it elsewhere keeps
    /// the old contents, and the old file stays whole if anything fails before the rename. The
    /// replacement keeps what an in-place write would have kept where the OS allows it: the
    /// permission bits, extended attributes, owner and group, and on macOS the creation time.
    pub(crate) fn replace(root: &Path, relative: &Path, contents: &[u8]) -> Result<(), Refusal> {
        let (names, name) = split(relative)?;
        let folders = Folders::open(root, &names, Walk::Create)?;
        let parent = folders.innermost();
        let previous = open_previous(parent, name, relative)?;
        let (temp_name, temp) = create_temp(parent, previous.is_some(), relative)?;
        let replaced = fill(&temp, contents, previous.as_ref(), relative).and_then(|()| {
            renameat(parent, &temp_name, parent, name)
                .map_err(|errno| Refusal::Io(relative.to_path_buf(), errno.into()))
        });
        if replaced.is_err() {
            let _ = unlinkat(parent, &temp_name, AtFlags::empty());
        }
        replaced
    }

    /// Open an existing regular file for reading.
    pub(crate) fn open_file(root: &Path, relative: &Path) -> Result<File, Refusal> {
        let (names, name) = split(relative)?;
        let folders = Folders::open(root, &names, Walk::Read)?;
        let parent = folders.innermost();
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | file_flags();
        let file = openat(parent, name, flags, Mode::empty()).map_err(|errno| match errno {
            Errno::NOENT => Refusal::Missing,
            _ => symlink_or_io(parent, name, relative.to_path_buf(), errno),
        })?;
        regular(File::from(file), relative)
    }

    /// Unlink a file, then remove the folders it leaves empty. The first `keep` folders of the
    /// path and the vault root always stay. Returns the removed folders, deepest first.
    pub(crate) fn remove(
        root: &Path,
        relative: &Path,
        keep: usize,
    ) -> Result<Vec<PathBuf>, Refusal> {
        let (names, name) = split(relative)?;
        let folders = Folders::open(root, &names, Walk::Change)?;
        let parent = folders.innermost();

        // `unlinkat` never follows the final name, so refusing a symlink here only enforces the
        // policy that symlinks are not vault files. Containment does not rely on it.
        let stat = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                return Err(Refusal::Symlink(relative.to_path_buf()));
            }
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Err(Refusal::Missing),
            Err(errno) => return Err(Refusal::Io(relative.to_path_buf(), errno.into())),
        };
        check_spelling(parent, name, stat.st_ino as u64, relative.to_path_buf())?;
        unlinkat(parent, name, AtFlags::empty()).map_err(|errno| match errno {
            Errno::NOENT => Refusal::Missing,
            _ => Refusal::Io(relative.to_path_buf(), errno.into()),
        })?;
        Ok(folders.prune(&names, keep))
    }

    /// Move a regular file to a path that does not exist yet, creating missing destination
    /// folders, then remove the source folders left empty below the first `keep`.
    pub(crate) fn rename(root: &Path, from: &Path, to: &Path, keep: usize) -> Result<(), Refusal> {
        let (from_names, from_name) = split(from)?;
        let (to_names, to_name) = split(to)?;
        let source = Folders::open(root, &from_names, Walk::Change)?;
        let source_parent = source.innermost();
        let stat = match statat(source_parent, from_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Err(Refusal::Missing),
            Err(errno) => return Err(Refusal::Io(from.to_path_buf(), errno.into())),
        };
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink => return Err(Refusal::Symlink(from.to_path_buf())),
            FileType::RegularFile => {}
            _ => return Err(not_a_file(from)),
        }
        check_spelling(
            source_parent,
            from_name,
            stat.st_ino as u64,
            from.to_path_buf(),
        )?;

        let target = Folders::open(root, &to_names, Walk::Create)?;
        let target_parent = target.innermost();
        match statat(target_parent, to_name, AtFlags::SYMLINK_NOFOLLOW) {
            Err(Errno::NOENT) => {}
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                return Err(Refusal::Symlink(to.to_path_buf()));
            }
            Ok(_) => return Err(Refusal::Exists),
            Err(errno) => return Err(Refusal::Io(to.to_path_buf(), errno.into())),
        }
        renameat(source_parent, from_name, target_parent, to_name)
            .map_err(|errno| Refusal::Io(to.to_path_buf(), errno.into()))?;
        source.prune(&from_names, keep);
        Ok(())
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Walk {
        /// Open existing folders. Reads change nothing, so spelling is not checked.
        Read,
        /// Open existing folders, each spelled exactly as its parent lists it.
        Change,
        /// Like `Change`, and create missing folders.
        Create,
    }

    struct Folders {
        root: OwnedFd,
        /// `opened[i]` is the folder at depth `i`, outermost first.
        opened: Vec<OwnedFd>,
    }

    impl Folders {
        /// Open the vault root and each folder below it.
        ///
        /// The root may sit behind symlinks. It was canonicalised when the vault was opened and
        /// is the trust anchor. Every folder below it is opened with `O_NOFOLLOW`.
        fn open(root: &Path, names: &[&OsStr], walk: Walk) -> Result<Self, Refusal> {
            let root_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
            let root = open(root, root_flags, Mode::empty())
                .map_err(|errno| Refusal::Io(PathBuf::new(), errno.into()))?;
            let mut folders = Self {
                root,
                opened: Vec::with_capacity(names.len()),
            };

            for (depth, name) in names.iter().copied().enumerate() {
                let at = path_to(names, depth);
                let parent = folders.innermost();
                let opened = match openat(parent, name, folder_flags(), Mode::empty()) {
                    Err(Errno::NOENT) if walk == Walk::Create => {
                        match mkdirat(parent, name, Mode::from_raw_mode(0o777)) {
                            Ok(()) | Err(Errno::EXIST) => {}
                            Err(errno) => return Err(Refusal::Io(at, errno.into())),
                        }
                        openat(parent, name, folder_flags(), Mode::empty())
                    }
                    opened => opened,
                };
                let handle = opened.map_err(|errno| match errno {
                    Errno::NOENT if walk != Walk::Create => Refusal::Missing,
                    _ => symlink_or_io(parent, name, at.clone(), errno),
                })?;
                if walk != Walk::Read {
                    let ino = fstat(&handle)
                        .map_err(|errno| Refusal::Io(at.clone(), errno.into()))?
                        .st_ino;
                    check_spelling(parent, name, ino as u64, at)?;
                }
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

        /// Remove the folders left empty, deepest first, through the handles already open. Stops
        /// at the first folder that is not empty or cannot be removed. The first `keep` folders
        /// and the vault root are never removed.
        fn prune(&self, names: &[&OsStr], keep: usize) -> Vec<PathBuf> {
            let mut pruned = Vec::new();
            for depth in (keep..names.len()).rev() {
                let folder = path_to(names, depth);
                match unlinkat(self.parent_of(depth), names[depth], AtFlags::REMOVEDIR) {
                    Ok(()) => pruned.push(folder),
                    Err(Errno::NOTEMPTY | Errno::EXIST) => break,
                    Err(errno) => {
                        warn!(
                            folder = %folder.display(),
                            error = %errno,
                            "left an empty folder in place after removing a file"
                        );
                        break;
                    }
                }
            }
            pruned
        }
    }

    /// Refuse `name` unless `parent` lists an entry with exactly these bytes.
    ///
    /// The listing shows each entry's stored bytes. A name that opened an entry but is missing
    /// from the listing reached that entry through a case or Unicode alias. `ino` identifies the
    /// entry, so the error can show the spelling the folder uses.
    fn check_spelling(
        parent: &OwnedFd,
        name: &OsStr,
        ino: u64,
        at: PathBuf,
    ) -> Result<(), Refusal> {
        let mut listed = None;
        let mut entries =
            Dir::read_from(parent).map_err(|errno| Refusal::Io(at.clone(), errno.into()))?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(|errno| Refusal::Io(at.clone(), errno.into()))?;
            let stored = entry.file_name().to_bytes();
            if stored == name.as_bytes() {
                return Ok(());
            }
            if entry.ino() == ino {
                listed = Some(at.with_file_name(OsStr::from_bytes(stored)));
            }
        }
        Err(Refusal::Misspelled { path: at, listed })
    }

    /// Open the file that `replace` is about to supersede, if there is one.
    ///
    /// Renaming over a file needs only write access to its folder. Opening the file for writing
    /// first keeps the check an in-place write made, so a read-only file stays as it is.
    fn open_previous(
        parent: &OwnedFd,
        name: &OsStr,
        relative: &Path,
    ) -> Result<Option<File>, Refusal> {
        let stat = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(None),
            Err(errno) => return Err(Refusal::Io(relative.to_path_buf(), errno.into())),
        };
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink => return Err(Refusal::Symlink(relative.to_path_buf())),
            FileType::RegularFile => {}
            _ => return Err(not_a_file(relative)),
        }
        check_spelling(parent, name, stat.st_ino as u64, relative.to_path_buf())?;
        let flags = OFlags::WRONLY | OFlags::NONBLOCK | file_flags();
        let file = openat(parent, name, flags, Mode::empty())
            .map_err(|errno| symlink_or_io(parent, name, relative.to_path_buf(), errno))?;
        regular(File::from(file), relative).map(Some)
    }

    /// Create a uniquely named temporary file in `parent`.
    ///
    /// A file that will take an existing file's place starts readable only by its owner and
    /// gets that file's permissions once written, so new contents are never exposed more widely
    /// than the old ones. The name is hidden and ends in `.tmp`, so inventory never lists it.
    fn create_temp(
        parent: &OwnedFd,
        replacing: bool,
        relative: &Path,
    ) -> Result<(OsString, File), Refusal> {
        let name = OsString::from(format!(
            ".arrowhead.{}.{:016x}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mode = if replacing { 0o600 } else { 0o666 };
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | file_flags();
        let file = openat(parent, &name, flags, Mode::from_raw_mode(mode))
            .map_err(|errno| Refusal::Io(relative.with_file_name(&name), errno.into()))?;
        Ok((name, File::from(file)))
    }

    fn fill(
        temp: &File,
        contents: &[u8],
        previous: Option<&File>,
        relative: &Path,
    ) -> Result<(), Refusal> {
        let failed = |error| Refusal::Io(relative.to_path_buf(), error);
        (&mut &*temp).write_all(contents).map_err(failed)?;
        match previous {
            Some(previous) => keep_metadata(previous, temp, relative).map_err(failed),
            None => Ok(()),
        }
    }

    /// Give the replacement the metadata that an in-place write would have kept.
    ///
    /// Only the permission bits carry over. Writing a file clears its set-user-ID and
    /// set-group-ID bits, so the replacement does not get them back. Metadata the OS will not
    /// let the caller set is logged and dropped rather than failing the change.
    fn keep_metadata(previous: &File, temp: &File, relative: &Path) -> io::Result<()> {
        let before = previous.metadata()?;
        copy_extended_attributes(previous, temp, relative);
        let after = temp.metadata()?;
        if before.gid() != after.gid() {
            if let Err(error) = fchown(temp, None, Some(before.gid())) {
                warn!(path = %relative.display(), %error, "replacement could not keep the file's group");
            }
        }
        if before.uid() != after.uid() {
            if let Err(error) = fchown(temp, Some(before.uid()), None) {
                warn!(path = %relative.display(), %error, "replacement could not keep the file's owner");
            }
        }
        temp.set_permissions(Permissions::from_mode(before.mode() & 0o777))?;
        #[cfg(target_os = "macos")]
        {
            use std::{fs::FileTimes, os::macos::fs::FileTimesExt};

            let kept = before
                .created()
                .and_then(|created| temp.set_times(FileTimes::new().set_created(created)));
            if let Err(error) = kept {
                warn!(path = %relative.display(), %error, "replacement could not keep the file's creation time");
            }
        }
        if before.nlink() > 1 {
            warn!(
                path = %relative.display(),
                links = before.nlink(),
                "replaced a file that has other hard links; the other links keep the previous contents"
            );
        }
        Ok(())
    }

    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    fn copy_extended_attributes(previous: &File, temp: &File, relative: &Path) {
        use rustix::fs::{XattrFlags, fgetxattr, flistxattr, fsetxattr};

        let names = match read_growing(|buffer| flistxattr(previous, buffer)) {
            Ok(names) => names,
            Err(error) => {
                warn!(path = %relative.display(), %error, "replacement could not list the file's extended attributes");
                return;
            }
        };
        for name in names
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            let copied =
                read_growing(|buffer| fgetxattr(previous, name, buffer)).and_then(|value| {
                    fsetxattr(temp, name, &value, XattrFlags::empty()).map_err(io::Error::from)
                });
            if let Err(error) = copied {
                warn!(
                    path = %relative.display(),
                    attribute = %String::from_utf8_lossy(name),
                    %error,
                    "replacement dropped an extended attribute it could not copy"
                );
            }
        }
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    fn copy_extended_attributes(_previous: &File, _temp: &File, _relative: &Path) {}

    /// Call `fill` with a buffer that doubles until the value fits.
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    fn read_growing(
        mut fill: impl FnMut(&mut Vec<u8>) -> rustix::io::Result<usize>,
    ) -> io::Result<Vec<u8>> {
        const LIMIT: usize = 16 << 20;
        let mut buffer = vec![0; 4096];
        loop {
            match fill(&mut buffer) {
                Ok(len) => {
                    buffer.truncate(len);
                    return Ok(buffer);
                }
                Err(Errno::RANGE) if buffer.len() < LIMIT => buffer.resize(buffer.len() * 2, 0),
                Err(errno) => return Err(errno.into()),
            }
        }
    }

    /// Split a path into folder names and the file name. Anything but plain names is rejected,
    /// so a `..` never reaches the descriptor walk.
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
                    "vault path must be a relative path of plain names",
                ),
            )),
        }
    }

    /// Report a failed open as a symlink refusal when a symlink sits at `name`.
    ///
    /// The errno alone is not enough. macOS reports `ENOTDIR` for a symlinked folder opened with
    /// `O_DIRECTORY | O_NOFOLLOW`, and exclusive creation reports `EEXIST` for any symlink,
    /// including a broken one.
    fn symlink_or_io(parent: &OwnedFd, name: &OsStr, at: PathBuf, errno: Errno) -> Refusal {
        match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                Refusal::Symlink(at)
            }
            _ => Refusal::Io(at, errno.into()),
        }
    }

    fn regular(file: File, relative: &Path) -> Result<File, Refusal> {
        match file.metadata() {
            Ok(metadata) if metadata.is_file() => Ok(file),
            Ok(_) => Err(not_a_file(relative)),
            Err(error) => Err(Refusal::Io(relative.to_path_buf(), error)),
        }
    }

    fn not_a_file(relative: &Path) -> Refusal {
        Refusal::Io(
            relative.to_path_buf(),
            io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
        )
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
}

/// Vault file changes fail closed where descriptor-relative, no-follow operations are not
/// implemented, instead of falling back to path-based calls that follow symlinks.
#[cfg(not(unix))]
mod unsupported {
    use std::{
        fs::File,
        io,
        path::{Path, PathBuf},
    };

    use super::Refusal;

    pub(crate) fn create(_root: &Path, relative: &Path, _contents: &[u8]) -> Result<(), Refusal> {
        Err(unsupported(relative))
    }

    pub(crate) fn replace(_root: &Path, relative: &Path, _contents: &[u8]) -> Result<(), Refusal> {
        Err(unsupported(relative))
    }

    pub(crate) fn open_file(_root: &Path, relative: &Path) -> Result<File, Refusal> {
        Err(unsupported(relative))
    }

    pub(crate) fn remove(
        _root: &Path,
        relative: &Path,
        _keep: usize,
    ) -> Result<Vec<PathBuf>, Refusal> {
        Err(unsupported(relative))
    }

    pub(crate) fn rename(
        _root: &Path,
        from: &Path,
        _to: &Path,
        _keep: usize,
    ) -> Result<(), Refusal> {
        Err(unsupported(from))
    }

    fn unsupported(relative: &Path) -> Refusal {
        Refusal::Io(
            relative.to_path_buf(),
            io::Error::new(
                io::ErrorKind::Unsupported,
                "vault file changes need no-follow file operations that Arrowhead implements only on Unix",
            ),
        )
    }
}
