//! Local `Fs` implementation on `tokio::fs`. Deletion goes to the system
//! trash through the `trash` crate (`DeleteMethod::NsFileManager` on macOS,
//! so Finder automation is never prompted), run in `spawn_blocking`.
//!
//! Errors are classified with `FsError::from_io` on the path concerned.
//! `list` uses `symlink_metadata` per entry and `metadata` on symlinks only
//! to fill `target_is_dir` (a dangling link is not an error). Modes are
//! `mode & 0o7777`; mtimes are whole seconds. `home` is `$HOME`.
//! `open_write` with `WriteMode::Truncate { permissions }` creates a new
//! file through `open(2)` with `permissions & 0o777` as its mode (minus the
//! umask), so the bits are in place from the start; an existing file keeps
//! its mode. `WriteMode::Resume { offset }` opens the existing file
//! without truncating and seeks to `offset`. `copy_within` is
//! `FsError::Unsupported` (local copies stream like any other); `trash`
//! failures are `FsError::TrashFailed` so the app can offer a permanent
//! delete.
//!
//! `open_read` and `open_write` open with `O_NONBLOCK` so a FIFO never
//! blocks a worker thread, and they accept regular files only (a directory
//! is `FsError::IsADirectory`). A resume offset beyond the end of the file
//! is rejected instead of leaving a hole. `set_times` uses `utimensat` and
//! follows symlinks, like `chmod`. `trash` on a missing path is
//! `FsError::NotFound`, never `TrashFailed`.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! pub struct LocalFs { /* private: endpoint == Endpoint::Local */ }
//!
//! impl LocalFs {
//!     pub fn new() -> LocalFs;
//! }
//!
//! impl Fs for LocalFs { /* every method */ }
//! ```
//!
//! `capabilities()`: `posix_rename: true`, `copy_within: false`,
//! `symlink: true`, `trash: true`.

use std::ffi::{CString, OsString};
use std::fs::{Metadata, Permissions};
use std::io::{self, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncSeekExt;

use super::{
    Capabilities, Endpoint, Entry, EntryKind, Fs, FsError, FsReader, FsResult, FsWriter,
    SpecialKind, WriteMode,
};

/// The local machine's file system.
#[derive(Debug, Clone)]
pub struct LocalFs {
    endpoint: Endpoint,
}

impl LocalFs {
    /// Creates the local file system handle.
    pub fn new() -> LocalFs {
        LocalFs {
            endpoint: Endpoint::Local,
        }
    }
}

impl Default for LocalFs {
    fn default() -> LocalFs {
        LocalFs::new()
    }
}

/// Builds an [`Entry`] from lstat metadata. For symlinks the target is read
/// and probed with a following stat; any probe failure (dangling link, loop,
/// no permission) means the link is not enterable.
async fn describe(path: &Path, name: OsString, meta: &Metadata) -> FsResult<Entry> {
    let file_type = meta.file_type();
    let kind = if file_type.is_symlink() {
        let target = tokio::fs::read_link(path)
            .await
            .map_err(|e| FsError::from_io(&e, path))?;
        let target_is_dir = tokio::fs::metadata(path)
            .await
            .is_ok_and(|target| target.is_dir());
        EntryKind::Symlink {
            target,
            target_is_dir,
        }
    } else {
        kind_of(meta)
    };
    Ok(Entry {
        name,
        kind,
        size: Some(meta.len()),
        mtime: Some(meta.mtime()),
        mode: Some(meta.mode() & 0o7777),
    })
}

/// The kind of a non-symlink object.
fn kind_of(meta: &Metadata) -> EntryKind {
    let file_type = meta.file_type();
    if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_fifo() {
        EntryKind::Special(SpecialKind::Fifo)
    } else if file_type.is_socket() {
        EntryKind::Special(SpecialKind::Socket)
    } else if file_type.is_block_device() {
        EntryKind::Special(SpecialKind::BlockDevice)
    } else if file_type.is_char_device() {
        EntryKind::Special(SpecialKind::CharDevice)
    } else {
        EntryKind::Special(SpecialKind::Unknown)
    }
}

/// The last path component, or the whole path for `/`.
fn name_of(path: &Path) -> OsString {
    path.file_name()
        .map_or_else(|| path.as_os_str().to_owned(), ToOwned::to_owned)
}

/// Opens `path` without blocking on FIFOs and accepts regular files only.
async fn open_regular(path: &Path, options: &mut OpenOptions) -> FsResult<File> {
    let file = options
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .await
        .map_err(|e| FsError::from_io(&e, path))?;
    let meta = file
        .metadata()
        .await
        .map_err(|e| FsError::from_io(&e, path))?;
    if meta.is_dir() {
        return Err(FsError::IsADirectory { path: path.into() });
    }
    if !meta.is_file() {
        return Err(FsError::Io {
            path: path.into(),
            message: "not a regular file".into(),
        });
    }
    Ok(file)
}

/// Seeks an opened file to `offset`.
async fn seek_to(file: &mut File, path: &Path, offset: u64) -> FsResult<()> {
    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(|e| FsError::from_io(&e, path))?;
    Ok(())
}

/// Sets access and modification times with `utimensat`, following symlinks.
fn utimens(path: &Path, atime: i64, mtime: i64) -> FsResult<()> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| FsError::Io {
        path: path.into(),
        message: "the path contains a NUL byte".into(),
    })?;
    let times = [
        libc::timespec {
            tv_sec: atime,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: mtime,
            tv_nsec: 0,
        },
    ];
    // SAFETY: `c_path` is a valid NUL-terminated string and `times` is an
    // array of exactly two initialized `timespec` values, both alive for the
    // duration of the call.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(FsError::from_io(&io::Error::last_os_error(), path))
    }
}

/// Moves `path` to the system trash.
fn trash_blocking(path: &Path) -> FsResult<()> {
    // A missing path is not a trash failure: offering a permanent delete for
    // it would make no sense.
    std::fs::symlink_metadata(path).map_err(|e| FsError::from_io(&e, path))?;
    trash_context()
        .delete(path)
        .map_err(|error| FsError::TrashFailed {
            path: path.into(),
            message: trash_message(&error),
        })
}

/// The trash backend: `NSFileManager` on macOS, which needs no Finder
/// automation permission.
#[cfg(target_os = "macos")]
fn trash_context() -> trash::TrashContext {
    use trash::macos::{DeleteMethod, TrashContextExtMacos};
    let mut context = trash::TrashContext::default();
    context.set_delete_method(DeleteMethod::NsFileManager);
    context
}

/// The trash backend: the freedesktop.org trash on Linux.
#[cfg(not(target_os = "macos"))]
fn trash_context() -> trash::TrashContext {
    trash::TrashContext::default()
}

/// A user-readable text for a trash backend error. The crate's `Display`
/// embeds its `Debug` output, so known variants are unpacked here.
fn trash_message(error: &trash::Error) -> String {
    match error {
        trash::Error::Unknown { description } | trash::Error::Os { description, .. } => {
            description.clone()
        }
        trash::Error::TargetedRoot => "the file system root cannot be trashed".into(),
        trash::Error::CouldNotAccess { target } => format!("cannot access {target}"),
        other => other.to_string(),
    }
}

/// Runs blocking work on the blocking pool. A join failure (the closure
/// panicked) is reported on `path`.
async fn blocking<T, F>(path: &Path, work: F) -> FsResult<T>
where
    T: Send + 'static,
    F: FnOnce(PathBuf) -> FsResult<T> + Send + 'static,
{
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || work(owned))
        .await
        .map_err(|e| FsError::Io {
            path: path.into(),
            message: e.to_string(),
        })?
}

impl Fs for LocalFs {
    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            posix_rename: true,
            copy_within: false,
            symlink: true,
            trash: true,
        }
    }

    fn home(&self) -> BoxFuture<'_, FsResult<PathBuf>> {
        Box::pin(async move {
            match std::env::var_os("HOME").map(PathBuf::from) {
                Some(home) if home.is_absolute() => Ok(home),
                _ => Err(FsError::Io {
                    path: "~".into(),
                    message: "the HOME environment variable is not set to an absolute path".into(),
                }),
            }
        })
    }

    fn canonicalize<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
        Box::pin(async move {
            tokio::fs::canonicalize(path)
                .await
                .map_err(|e| FsError::from_io(&e, path))
        })
    }

    fn list<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, FsResult<Vec<Entry>>> {
        Box::pin(async move {
            let mut reader = tokio::fs::read_dir(dir)
                .await
                .map_err(|e| FsError::from_io(&e, dir))?;
            let mut entries = Vec::new();
            while let Some(dir_entry) = reader
                .next_entry()
                .await
                .map_err(|e| FsError::from_io(&e, dir))?
            {
                let path = dir_entry.path();
                // `DirEntry::metadata` does not follow symlinks (lstat).
                let meta = match dir_entry.metadata().await {
                    Ok(meta) => meta,
                    // Removed between readdir and lstat: it is no longer
                    // part of the directory.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(FsError::from_io(&e, &path)),
                };
                match describe(&path, dir_entry.file_name(), &meta).await {
                    Ok(entry) => entries.push(entry),
                    // A symlink removed between lstat and readlink.
                    Err(FsError::NotFound { .. }) => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(entries)
        })
    }

    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
        Box::pin(async move {
            let meta = tokio::fs::metadata(path)
                .await
                .map_err(|e| FsError::from_io(&e, path))?;
            Ok(Entry {
                name: name_of(path),
                kind: kind_of(&meta),
                size: Some(meta.len()),
                mtime: Some(meta.mtime()),
                mode: Some(meta.mode() & 0o7777),
            })
        })
    }

    fn lstat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
        Box::pin(async move {
            let meta = tokio::fs::symlink_metadata(path)
                .await
                .map_err(|e| FsError::from_io(&e, path))?;
            describe(path, name_of(path), &meta).await
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            tokio::fs::create_dir(path)
                .await
                .map_err(|e| FsError::from_io(&e, path))
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            tokio::fs::rename(from, to).await.map_err(|e| {
                // A missing source is the common cause of `NotFound`; every
                // other failure concerns the target.
                let concerned = if e.kind() == io::ErrorKind::NotFound {
                    from
                } else {
                    to
                };
                FsError::from_io(&e, concerned)
            })
        })
    }

    fn remove_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| FsError::from_io(&e, path))
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            tokio::fs::remove_dir(path)
                .await
                .map_err(|e| FsError::from_io(&e, path))
        })
    }

    fn trash<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(blocking(path, |owned| trash_blocking(&owned)))
    }

    fn chmod<'a>(&'a self, path: &'a Path, mode: u32) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            tokio::fs::set_permissions(path, Permissions::from_mode(mode))
                .await
                .map_err(|e| FsError::from_io(&e, path))
        })
    }

    fn set_times<'a>(
        &'a self,
        path: &'a Path,
        atime: i64,
        mtime: i64,
    ) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(blocking(path, move |owned| utimens(&owned, atime, mtime)))
    }

    fn read_link<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
        Box::pin(async move {
            tokio::fs::read_link(path)
                .await
                .map_err(|e| FsError::from_io(&e, path))
        })
    }

    fn symlink<'a>(&'a self, target: &'a Path, link: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            tokio::fs::symlink(target, link)
                .await
                .map_err(|e| FsError::from_io(&e, link))
        })
    }

    fn open_read<'a>(&'a self, path: &'a Path, offset: u64) -> BoxFuture<'a, FsResult<FsReader>> {
        Box::pin(async move {
            let mut file = open_regular(path, OpenOptions::new().read(true)).await?;
            if offset > 0 {
                seek_to(&mut file, path, offset).await?;
            }
            Ok(Box::pin(file) as FsReader)
        })
    }

    fn open_write<'a>(
        &'a self,
        path: &'a Path,
        mode: WriteMode,
    ) -> BoxFuture<'a, FsResult<FsWriter>> {
        Box::pin(async move {
            let file = match mode {
                WriteMode::Truncate { permissions } => {
                    open_regular(
                        path,
                        OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(permissions & 0o777),
                    )
                    .await?
                }
                WriteMode::Resume { offset } => {
                    let mut file = open_regular(path, OpenOptions::new().write(true)).await?;
                    let len = file
                        .metadata()
                        .await
                        .map_err(|e| FsError::from_io(&e, path))?
                        .len();
                    if offset > len {
                        return Err(FsError::Io {
                            path: path.into(),
                            message: format!(
                                "cannot resume at byte {offset}: the file has only {len} bytes"
                            ),
                        });
                    }
                    seek_to(&mut file, path, offset).await?;
                    file
                }
            };
            Ok(Box::pin(file) as FsWriter)
        })
    }

    fn copy_within<'a>(&'a self, _from: &'a Path, _to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            Err(FsError::Unsupported {
                operation: "copy_within".into(),
                reason: "local copies are streamed like any other transfer".into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        // macOS tempdirs live behind the /var -> /private/var symlink.
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize tempdir");
        (dir, root)
    }

    fn mkfifo(path: &Path) {
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("path without NUL");
        // SAFETY: `c_path` is a valid NUL-terminated string.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed: {}", io::Error::last_os_error());
    }

    fn find<'e>(entries: &'e [Entry], name: &str) -> &'e Entry {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} not listed"))
    }

    async fn read_all(fs: &LocalFs, path: &Path, offset: u64) -> Vec<u8> {
        let mut reader = fs.open_read(path, offset).await.expect("open_read");
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.expect("read");
        data
    }

    async fn write_all(fs: &LocalFs, path: &Path, mode: WriteMode, data: &[u8]) {
        let mut writer = fs.open_write(path, mode).await.expect("open_write");
        writer.write_all(data).await.expect("write");
        writer.shutdown().await.expect("shutdown");
    }

    #[test]
    fn capabilities_and_endpoint_describe_the_local_machine() {
        let fs = LocalFs::new();
        assert_eq!(fs.endpoint(), &Endpoint::Local);
        assert_eq!(
            fs.capabilities(),
            Capabilities {
                posix_rename: true,
                copy_within: false,
                symlink: true,
                trash: true,
            }
        );
    }

    #[tokio::test]
    async fn home_is_the_home_variable() {
        let home = LocalFs::new().home().await.expect("home");
        assert_eq!(Some(home.into_os_string()), std::env::var_os("HOME"));
    }

    #[tokio::test]
    async fn list_describes_every_kind_without_dot_entries() {
        let (_guard, root) = tempdir();
        std::fs::write(root.join("file"), b"hello").unwrap();
        std::fs::set_permissions(root.join("file"), Permissions::from_mode(0o640)).unwrap();
        std::fs::create_dir(root.join("dir")).unwrap();
        std::os::unix::fs::symlink("dir", root.join("dir-link")).unwrap();
        std::os::unix::fs::symlink("missing", root.join("dangling")).unwrap();
        mkfifo(&root.join("pipe"));

        let entries = LocalFs::new().list(&root).await.expect("list");
        assert_eq!(entries.len(), 5);
        assert!(entries.iter().all(|e| e.name != "." && e.name != ".."));

        let file = find(&entries, "file");
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.size, Some(5));
        assert_eq!(file.mode, Some(0o640));
        let expected_mtime = std::fs::metadata(root.join("file")).unwrap().mtime();
        assert_eq!(file.mtime, Some(expected_mtime));

        assert_eq!(find(&entries, "dir").kind, EntryKind::Dir);
        let dir_link = find(&entries, "dir-link");
        assert_eq!(
            dir_link.kind,
            EntryKind::Symlink {
                target: "dir".into(),
                target_is_dir: true,
            }
        );
        assert!(dir_link.is_enterable());
        assert_eq!(
            find(&entries, "dangling").kind,
            EntryKind::Symlink {
                target: "missing".into(),
                target_is_dir: false,
            }
        );
        assert_eq!(
            find(&entries, "pipe").kind,
            EntryKind::Special(SpecialKind::Fifo)
        );
    }

    #[tokio::test]
    async fn list_of_a_missing_directory_is_not_found() {
        let (_guard, root) = tempdir();
        let missing = root.join("missing");
        assert_eq!(
            LocalFs::new().list(&missing).await,
            Err(FsError::NotFound { path: missing })
        );
    }

    #[tokio::test]
    async fn stat_follows_symlinks_and_lstat_does_not() {
        let (_guard, root) = tempdir();
        std::fs::create_dir(root.join("dir")).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(root.join("dir"), &link).unwrap();
        let fs = LocalFs::new();

        let followed = fs.stat(&link).await.expect("stat");
        assert_eq!(followed.name, "link");
        assert_eq!(followed.kind, EntryKind::Dir);

        let own = fs.lstat(&link).await.expect("lstat");
        assert_eq!(
            own.kind,
            EntryKind::Symlink {
                target: root.join("dir"),
                target_is_dir: true,
            }
        );
        assert_eq!(
            fs.read_link(&link).await.expect("read_link"),
            root.join("dir")
        );
        assert_eq!(fs.stat(Path::new("/")).await.expect("stat /").name, "/");
    }

    #[tokio::test]
    async fn canonicalize_resolves_symlinks_and_dots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let fs = LocalFs::new();
        let resolved = fs
            .canonicalize(&dir.path().join("a/../a/."))
            .await
            .expect("canonicalize");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path().join("a")).unwrap()
        );
    }

    #[tokio::test]
    async fn mkdir_and_removals_classify_errors() {
        let (_guard, root) = tempdir();
        let fs = LocalFs::new();
        let dir = root.join("dir");
        fs.mkdir(&dir).await.expect("mkdir");
        assert_eq!(
            fs.mkdir(&dir).await,
            Err(FsError::AlreadyExists { path: dir.clone() })
        );
        let orphan = root.join("missing/child");
        assert_eq!(
            fs.mkdir(&orphan).await,
            Err(FsError::NotFound { path: orphan })
        );

        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            fs.remove_dir(&dir).await,
            Err(FsError::DirectoryNotEmpty { path: dir.clone() })
        );
        fs.remove_file(&file).await.expect("remove_file");
        fs.remove_dir(&dir).await.expect("remove_dir");
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn rename_replaces_an_existing_target() {
        let (_guard, root) = tempdir();
        let fs = LocalFs::new();
        let from = root.join("new.scpto-part");
        let to = root.join("final");
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"old").unwrap();
        fs.rename(&from, &to).await.expect("rename");
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
        assert_eq!(
            fs.rename(&from, &to).await,
            Err(FsError::NotFound { path: from })
        );
    }

    #[tokio::test]
    async fn open_read_starts_at_the_offset() {
        let (_guard, root) = tempdir();
        let fs = LocalFs::new();
        let file = root.join("data");
        std::fs::write(&file, b"0123456789").unwrap();
        assert_eq!(read_all(&fs, &file, 0).await, b"0123456789");
        assert_eq!(read_all(&fs, &file, 7).await, b"789");
    }

    #[tokio::test]
    async fn open_rejects_directories_and_fifos_without_blocking() {
        let (_guard, root) = tempdir();
        let fs = LocalFs::new();
        assert!(matches!(
            fs.open_read(&root, 0).await,
            Err(FsError::IsADirectory { path }) if path == root
        ));
        let pipe = root.join("pipe");
        mkfifo(&pipe);
        assert!(matches!(
            fs.open_read(&pipe, 0).await,
            Err(FsError::Io { path, .. }) if path == pipe
        ));
        assert!(fs
            .open_write(&pipe, WriteMode::Truncate { permissions: 0o644 })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn open_write_truncates_and_resumes() {
        let (_guard, root) = tempdir();
        let fs = LocalFs::new();
        let file = root.join("out.scpto-part");
        write_all(
            &fs,
            &file,
            WriteMode::Truncate { permissions: 0o644 },
            b"stale contents",
        )
        .await;
        write_all(
            &fs,
            &file,
            WriteMode::Truncate { permissions: 0o644 },
            b"0123",
        )
        .await;
        assert_eq!(std::fs::read(&file).unwrap(), b"0123");

        write_all(&fs, &file, WriteMode::Resume { offset: 4 }, b"4567").await;
        assert_eq!(std::fs::read(&file).unwrap(), b"01234567");
        write_all(&fs, &file, WriteMode::Resume { offset: 6 }, b"67").await;
        assert_eq!(std::fs::read(&file).unwrap(), b"01234567");

        assert!(matches!(
            fs.open_write(&file, WriteMode::Resume { offset: 9 }).await,
            Err(FsError::Io { path, .. }) if path == file
        ));
        let missing = root.join("missing");
        assert!(matches!(
            fs.open_write(&missing, WriteMode::Resume { offset: 0 }).await,
            Err(FsError::NotFound { path }) if path == missing
        ));
    }

    #[tokio::test]
    async fn truncate_creates_with_the_given_permissions_and_keeps_existing_ones() {
        let (_guard, root) = tempdir();
        let fs = LocalFs::new();
        let secret = root.join("id_rsa.scpto-part");
        let mut writer = fs
            .open_write(&secret, WriteMode::Truncate { permissions: 0o600 })
            .await
            .expect("open_write");
        // The bits are in place before any data is written.
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        writer.write_all(b"key").await.expect("write");
        writer.shutdown().await.expect("shutdown");

        // Bits beyond 0o777 are never applied, and an existing file keeps
        // its mode when truncated.
        std::fs::set_permissions(&secret, Permissions::from_mode(0o640)).unwrap();
        write_all(
            &fs,
            &secret,
            WriteMode::Truncate {
                permissions: 0o4600,
            },
            b"new",
        )
        .await;
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o640);
        assert_eq!(std::fs::read(&secret).unwrap(), b"new");
    }

    #[tokio::test]
    async fn chmod_and_set_times_are_visible_in_lstat() {
        let (_guard, root) = tempdir();
        let fs = LocalFs::new();
        let file = root.join("file");
        std::fs::write(&file, b"x").unwrap();
        fs.chmod(&file, 0o604).await.expect("chmod");
        fs.set_times(&file, 1_000_000_000, 1_234_567_890)
            .await
            .expect("set_times");
        let entry = fs.lstat(&file).await.expect("lstat");
        assert_eq!(entry.mode, Some(0o604));
        assert_eq!(entry.mtime, Some(1_234_567_890));
        assert_eq!(std::fs::metadata(&file).unwrap().atime(), 1_000_000_000);

        let missing = root.join("missing");
        assert_eq!(
            fs.set_times(&missing, 0, 0).await,
            Err(FsError::NotFound { path: missing })
        );
    }

    #[tokio::test]
    async fn symlink_creates_a_link_with_the_given_target() {
        let (_guard, root) = tempdir();
        let fs = LocalFs::new();
        let link = root.join("link");
        fs.symlink(Path::new("../elsewhere"), &link)
            .await
            .expect("symlink");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../elsewhere")
        );
        assert_eq!(
            fs.symlink(Path::new("x"), &link).await,
            Err(FsError::AlreadyExists { path: link })
        );
    }

    #[tokio::test]
    async fn copy_within_is_unsupported() {
        let (_guard, root) = tempdir();
        assert!(matches!(
            LocalFs::new()
                .copy_within(&root.join("a"), &root.join("b"))
                .await,
            Err(FsError::Unsupported { .. })
        ));
    }

    #[tokio::test]
    async fn trash_of_a_missing_path_is_not_found() {
        let (_guard, root) = tempdir();
        let missing = root.join("missing");
        assert_eq!(
            LocalFs::new().trash(&missing).await,
            Err(FsError::NotFound { path: missing })
        );
    }

    #[test]
    fn trash_messages_are_unpacked() {
        let error = trash::Error::Unknown {
            description: "volume has no trash".into(),
        };
        assert_eq!(trash_message(&error), "volume has no trash");
        assert_eq!(
            trash_message(&trash::Error::TargetedRoot),
            "the file system root cannot be trashed"
        );
    }
}
