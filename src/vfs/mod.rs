//! Virtual file system: one `Fs` trait over local and SFTP endpoints.
//!
//! # Async style
//!
//! Methods return [`BoxFuture`] instead of using native `async fn` in the
//! trait. Native `async fn` makes a trait dyn-incompatible (E0038), and the
//! transfer engine, effect executors, and sessions all hold endpoints as
//! `Arc<dyn Fs>` so that local and remote implementations are interchangeable
//! at run time. Implementations write `Box::pin(async move { ... })`.
//!
//! # Paths
//!
//! All paths are absolute `PathBuf`s in the endpoint's own namespace (remote
//! paths are POSIX paths on the server). File names are `OsString` because
//! Unix names are arbitrary bytes and must round-trip unchanged.

pub mod local;
pub mod sftp;

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

pub use crate::error::FsError;

/// Result type of every `Fs` operation.
pub type FsResult<T> = Result<T, FsError>;

/// Streaming reader returned by [`Fs::open_read`].
///
/// Reader and writer errors carry their class: an implementation that knows
/// the [`FsError`] puts it inside the `io::Error` (its `get_ref()` downcasts
/// to `FsError`), so a caller recovers `FsError::Disconnected` and does not
/// fail the item as a plain I/O error. An `io::Error` without an inner
/// `FsError` is classified with `FsError::from_io`.
pub type FsReader = Pin<Box<dyn AsyncRead + Send>>;

/// Streaming writer returned by [`Fs::open_write`]. Callers must call
/// `shutdown()` and check its result; only then is the data committed.
/// Errors carry their class as described on [`FsReader`].
pub type FsWriter = Pin<Box<dyn AsyncWrite + Send>>;

/// Where a pane or a transfer side points: the local machine, or a remote
/// host alias from `~/.ssh/config`, optionally running sftp-server via sudo.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Endpoint {
    /// The local machine.
    Local,
    /// A remote SSH host.
    Remote {
        /// The ssh alias (as typed in `~/.ssh/config` or on the command line).
        host: String,
        /// Whether sftp-server runs through sudo.
        sudo: bool,
    },
}

impl Endpoint {
    /// Whether this is the local machine.
    pub fn is_local(&self) -> bool {
        matches!(self, Endpoint::Local)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Local => f.write_str("Local"),
            Endpoint::Remote { host, sudo: false } => f.write_str(host),
            Endpoint::Remote { host, sudo: true } => write!(f, "{host} [sudo]"),
        }
    }
}

/// A directory or file on a specific endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    /// The endpoint the path belongs to.
    pub endpoint: Endpoint,
    /// Absolute path on that endpoint.
    pub path: PathBuf,
}

/// Non-regular, non-directory, non-symlink file types. They are listed but
/// never transferred (plan 7.5 S15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SpecialKind {
    /// Named pipe.
    Fifo,
    /// Unix domain socket.
    Socket,
    /// Block device.
    BlockDevice,
    /// Character device.
    CharDevice,
    /// The server did not report a recognizable type.
    Unknown,
}

/// What kind of object an [`Entry`] is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link (never followed by transfers).
    Symlink {
        /// The link target exactly as stored in the link.
        target: PathBuf,
        /// Whether the target resolves to a directory; the listing shows such
        /// links as enterable. `false` for dangling links.
        target_is_dir: bool,
    },
    /// FIFO, socket, device, or unknown.
    Special(SpecialKind),
}

/// One directory entry or stat result.
///
/// Attributes are `Option` because the SFTP protocol allows a server to omit
/// them; implementations never invent values.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry {
    /// File name (last path component).
    pub name: OsString,
    /// Object kind.
    pub kind: EntryKind,
    /// Size in bytes.
    pub size: Option<u64>,
    /// Modification time in whole seconds since the Unix epoch.
    pub mtime: Option<i64>,
    /// Permission bits (`mode & 0o7777`).
    pub mode: Option<u32>,
}

impl Entry {
    /// Whether opening this entry navigates into a directory (directories
    /// and symlinks to directories).
    pub fn is_enterable(&self) -> bool {
        match &self.kind {
            EntryKind::Dir => true,
            EntryKind::Symlink { target_is_dir, .. } => *target_is_dir,
            EntryKind::File | EntryKind::Special(_) => false,
        }
    }
}

/// How [`Fs::open_write`] opens its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Create the file, or truncate it if it exists. A file this call
    /// creates gets at most `permissions & 0o777` from the start (the
    /// endpoint's umask may clear more bits), so no data written through the
    /// writer is ever more widely readable than that; an existing file keeps
    /// its permissions. Callers that need exact bits on the final file
    /// `chmod` it after the data is committed.
    Truncate {
        /// Permission bits of a newly created file.
        permissions: u32,
    },
    /// Open an existing file and write starting at `offset`, used to resume
    /// a `.scpto-part` file. Bytes before `offset` are kept.
    Resume {
        /// Byte offset where writing starts.
        offset: u64,
    },
}

/// Optional features an endpoint supports, queried once per connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// `rename` atomically replaces an existing target
    /// (`posix-rename@openssh.com` on SFTP; always true locally). When false,
    /// `rename` onto an existing target deletes the target first and is not
    /// atomic (plan 7.5 S14).
    pub posix_rename: bool,
    /// `copy_within` is available (`copy-data` extension on SFTP).
    pub copy_within: bool,
    /// `symlink` is available.
    pub symlink: bool,
    /// `trash` is available (local endpoint only).
    pub trash: bool,
}

/// File system operations on one endpoint. Dyn-compatible: held as
/// `Arc<dyn Fs>`.
pub trait Fs: Send + Sync {
    /// The endpoint this file system serves.
    fn endpoint(&self) -> &Endpoint;

    /// Features this endpoint supports.
    fn capabilities(&self) -> Capabilities;

    /// The user's home directory on this endpoint (the initial directory of a
    /// newly connected pane).
    fn home(&self) -> BoxFuture<'_, FsResult<PathBuf>>;

    /// Resolves `path` to an absolute path without `.`/`..` or symlinks.
    fn canonicalize<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>>;

    /// Lists a directory, excluding `.` and `..`. Entries describe the
    /// objects themselves (lstat); for symlinks the implementation also
    /// resolves `target_is_dir`. Order is unspecified.
    fn list<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, FsResult<Vec<Entry>>>;

    /// Stat that follows symlinks.
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>>;

    /// Stat that does not follow symlinks.
    fn lstat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>>;

    /// Creates one directory; the parent must exist.
    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>>;

    /// Renames `from` to `to`, replacing an existing non-directory `to`.
    /// See [`Capabilities::posix_rename`] for atomicity.
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>>;

    /// Permanently removes a file or symlink.
    fn remove_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>>;

    /// Permanently removes an empty directory.
    fn remove_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>>;

    /// Moves a file or directory tree to the system trash. Fails with
    /// [`FsError::Unsupported`] when [`Capabilities::trash`] is false and with
    /// [`FsError::TrashFailed`] when the trash backend refuses.
    fn trash<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>>;

    /// Sets permission bits.
    fn chmod<'a>(&'a self, path: &'a Path, mode: u32) -> BoxFuture<'a, FsResult<()>>;

    /// Sets access and modification times (seconds since the Unix epoch).
    fn set_times<'a>(
        &'a self,
        path: &'a Path,
        atime: i64,
        mtime: i64,
    ) -> BoxFuture<'a, FsResult<()>>;

    /// Reads a symlink's target without following it.
    fn read_link<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>>;

    /// Creates a symlink at `link` pointing to `target`.
    fn symlink<'a>(&'a self, target: &'a Path, link: &'a Path) -> BoxFuture<'a, FsResult<()>>;

    /// Opens a file for streaming reads starting at `offset`.
    fn open_read<'a>(&'a self, path: &'a Path, offset: u64) -> BoxFuture<'a, FsResult<FsReader>>;

    /// Opens a file for streaming writes.
    fn open_write<'a>(
        &'a self,
        path: &'a Path,
        mode: WriteMode,
    ) -> BoxFuture<'a, FsResult<FsWriter>>;

    /// Copies a file to another path on the same endpoint without moving the
    /// data through scpto. Fails with [`FsError::Unsupported`] when
    /// [`Capabilities::copy_within`] is false.
    fn copy_within<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>>;
}

/// Permanently removes `path` and, when it is a directory, everything
/// beneath it (children before their directory; symlinks are removed, never
/// followed). A path that is already gone counts as removed. Shared by the
/// permanent delete of the file executor and the sync's deletion of
/// extraneous destination entries.
pub async fn remove_tree(fs: &dyn Fs, path: &Path) -> FsResult<()> {
    // (path, listed): a listed directory is removed once its children are.
    let mut pending = vec![(path.to_path_buf(), false)];
    while let Some((path, listed)) = pending.pop() {
        if listed {
            fs.remove_dir(&path).await?;
            continue;
        }
        let entry = match fs.lstat(&path).await {
            Ok(entry) => entry,
            Err(FsError::NotFound { .. }) => continue,
            Err(error) => return Err(error),
        };
        if entry.kind == EntryKind::Dir {
            let children = fs.list(&path).await?;
            pending.push((path.clone(), true));
            pending.extend(
                children
                    .into_iter()
                    .map(|child| (path.join(child.name), false)),
            );
        } else {
            match fs.remove_file(&path).await {
                Ok(()) | Err(FsError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn remove_tree_removes_children_first_and_tolerates_missing_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("a/b")).expect("mkdir");
        std::fs::write(root.join("a/b/file"), b"x").expect("write");
        std::fs::write(root.join("top"), b"y").expect("write");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).expect("mkdir");
        std::fs::write(outside.join("kept"), b"z").expect("write");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");
        let fs = local::LocalFs::new();

        remove_tree(&fs, &root).await.expect("remove tree");
        assert!(!root.exists());
        // The symlink was removed, not followed.
        assert!(outside.join("kept").exists());
        remove_tree(&fs, &root).await.expect("already gone");

        let file = dir.path().join("single");
        std::fs::write(&file, b"x").expect("write");
        remove_tree(&fs, &file).await.expect("remove file");
        assert!(!file.exists());
    }

    #[test]
    fn fs_handle_is_dyn_compatible_and_shareable() {
        // Compiles only while `Fs` stays dyn-compatible and `Arc<dyn Fs>`
        // can cross threads.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn Fs>>();
    }

    #[test]
    fn endpoint_display_marks_sudo() {
        let remote = Endpoint::Remote {
            host: "web".into(),
            sudo: true,
        };
        assert_eq!(remote.to_string(), "web [sudo]");
        assert_eq!(Endpoint::Local.to_string(), "Local");
    }

    #[test]
    fn symlinks_to_directories_are_enterable() {
        let entry = Entry {
            name: "link".into(),
            kind: EntryKind::Symlink {
                target: "/srv".into(),
                target_is_dir: true,
            },
            size: Some(4),
            mtime: Some(0),
            mode: Some(0o777),
        };
        assert!(entry.is_enterable());
    }
}
