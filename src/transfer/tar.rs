//! tar acceleration between the local machine and a non-sudo remote: the
//! `tar` crate packs or unpacks locally through `tokio_util::io::SyncIoBridge`
//! in `spawn_blocking` (symlinks not followed; absolute and `..` entries
//! rejected), streaming through remote `tar -C <dir> -mxf -` / `-cf -`.
//!
//! # Contract
//!
//! - The remote side is `Connections::spawn_remote(remote, ["tar", "-C",
//!   dir, "-mxf", "-"])` for uploads and `["tar", "-C", dir, "-cf", "-",
//!   "./<name>"...]` for downloads (words are quoted by the session; the
//!   `./` prefix keeps a name such as `-rf` from being read as an option).
//!   Its stderr tail goes into `Error::CommandFailed { program: "tar", .. }`
//!   on a non-zero exit. Paths and names must be valid UTF-8 because remote
//!   argv words are strings; others fail with `FsError::Unsupported`.
//! - Packing: `Builder::follow_symlinks(false)`, entries named relative to
//!   the local source's parent, so each source lands under its own name.
//!   Regular files, directories, and symlinks (as links) are archived; FIFOs,
//!   sockets, and devices are skipped, as in per-file transfers.
//!   Upload extraction skips timestamps with `-m`; after the stream succeeds,
//!   `Fs::set_times` restores regular files, then directories deepest first.
//!   Symlinks are excluded because SFTP SETSTAT follows their targets.
//!   Metadata failures return path-associated warnings without failing data.
//! - Unpacking: every entry is checked first; an absolute path or a `..`
//!   component (in the entry path or a hard link target) fails the job with
//!   `Error::Archive`. Extraction uses `Entry::unpack_in(local_dir)` with
//!   `set_overwrite(true)` and `set_preserve_permissions(false)`, which makes
//!   the tar crate apply exactly `mode & 0o777` (no umask, no setuid bits);
//!   mtimes are kept (the tar crate sets none on directories, so this module
//!   applies those itself). Directory entries are applied last, deepest
//!   first, so their mtimes and modes survive the files written into them.
//!   Directory timestamp failures are returned as non-fatal warnings.
//!   FIFO and device entries are skipped.
//! - Error precedence: a local failure (file system, unsafe entry) wins and
//!   ends the remote child. When the stream itself breaks (the remote stopped
//!   reading, ended early, or sent a malformed archive), the remote exit
//!   status explains it first; only a remote that exited successfully leaves
//!   the local stream error as the reported one.
//! - No `.scpto-part`, no resume: a failure leaves whatever was written.
//! - `progress` is called with the tar stream bytes moved so far.
//! - Cancel is by dropping the future (the remote child is killed on drop).
//!
//! # Public API
//!
//! ```text
//! /// Packs `sources` (absolute local paths) into remote `remote_dir`.
//! pub async fn upload(
//!     connections: &dyn Connections,
//!     sources: &[PathBuf],
//!     remote: &Endpoint,
//!     remote_dir: &Path,
//!     progress: &(dyn Fn(u64) + Send + Sync),
//! ) -> Result<Vec<(PathBuf, Error)>, Error>;
//!
//! /// Unpacks `names` (entries of remote `remote_dir`) into `local_dir`.
//! pub async fn download(
//!     connections: &dyn Connections,
//!     remote: &Endpoint,
//!     remote_dir: &Path,
//!     names: &[OsString],
//!     local_dir: &Path,
//!     progress: &(dyn Fn(u64) + Send + Sync),
//! ) -> Result<Vec<(PathBuf, Error)>, Error>;
//! ```

use std::ffi::{OsStr, OsString};
use std::fs::{self, FileTimes};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use tar::{Archive, Builder, Entry, EntryType};
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::io::SyncIoBridge;

use crate::connections::{Connections, FsChannel, RemoteChild, StderrTail};
use crate::error::{ConnectError, Error, FsError};
use crate::vfs::Endpoint;

/// Buffer between the tar crate's small reads and writes and the pipe.
const PIPE_BUFFER: usize = 256 * 1024;

/// Packs `sources` (absolute local paths) into remote `remote_dir`.
///
/// Each source lands in `remote_dir` under its own file name. Fails with
/// `Error::CommandFailed` when the remote `tar` exits unsuccessfully, and
/// with the local error when a source cannot be read.
/// Returns non-fatal timestamp warnings paired with destination paths.
pub async fn upload(
    connections: &dyn Connections,
    sources: &[PathBuf],
    remote: &Endpoint,
    remote_dir: &Path,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<Vec<(PathBuf, Error)>, Error> {
    let roots = sources
        .iter()
        .map(|source| upload_root(source))
        .collect::<Result<Vec<_>, Error>>()?;
    let argv = vec![
        "tar".to_owned(),
        "-C".to_owned(),
        utf8(remote_dir.as_os_str(), remote_dir)?.to_owned(),
        "-mxf".to_owned(),
        "-".to_owned(),
    ];
    let RemoteChild {
        mut child,
        stdin,
        mut stdout,
        stderr,
    } = connections.spawn_remote(remote, &argv).await?;

    let (moved_tx, moved_rx) = watch::channel(0);
    let ended = Arc::new(AtomicBool::new(false));
    let writer = Metered::new(SyncIoBridge::new(stdin), moved_tx, Arc::clone(&ended));
    let task = tokio::task::spawn_blocking(move || {
        let mut metadata = Vec::new();
        let bridge = pack(
            BufWriter::with_capacity(PIPE_BUFFER, writer),
            &roots,
            &ended,
            &mut metadata,
        )?;
        // Closing stdin is the end-of-input signal for the remote tar.
        let mut bridge = bridge.inner;
        bridge.shutdown().map_err(StreamFailure::Stream)?;
        Ok(metadata)
    });
    // `tar -x` writes nothing to stdout; it is drained so that unexpected
    // output can never block the remote side. The drain ends when ssh closes
    // stdout, which happens once the remote tar has exited.
    let mut sink = tokio::io::sink();
    let (packed, drained) = tokio::join!(
        track(task, moved_rx, progress),
        tokio::io::copy(&mut stdout, &mut sink),
    );
    let metadata = settle(packed?, &mut child, &stderr).await?;
    drained.map_err(|error| {
        Error::Connect(ConnectError::Other {
            detail: format!("reading the output of the remote tar failed: {error}"),
        })
    })?;
    Ok(restore_uploaded_times(connections, remote, remote_dir, metadata).await)
}

/// Restores only files and directories: SFTP SETSTAT follows symlinks.
async fn restore_uploaded_times(
    connections: &dyn Connections,
    remote: &Endpoint,
    remote_dir: &Path,
    mut metadata: Vec<PackedMetadata>,
) -> Vec<(PathBuf, Error)> {
    metadata.sort_by_key(|entry| {
        (
            entry.directory,
            std::cmp::Reverse(entry.name.components().count()),
        )
    });
    let destination = connections.fs(remote, FsChannel::Transfer).await;
    let mut warnings = Vec::new();
    for entry in metadata {
        let path = remote_dir.join(entry.name);
        let result = match &destination {
            Ok(fs) => fs
                .set_times(&path, entry.mtime, entry.mtime)
                .await
                .map_err(Error::from),
            Err(error) => Err(error.clone()),
        };
        if let Err(error) = result {
            warnings.push((path, error));
        }
    }
    warnings
}

/// Unpacks `names` (entries of remote `remote_dir`) into `local_dir`.
///
/// Fails with `Error::Archive` when the archive holds an absolute or `..`
/// entry, with the local error when writing fails, and with
/// `Error::CommandFailed` when the remote `tar` exits unsuccessfully (for
/// example because one of the names does not exist).
/// Returns directory timestamp warnings paired with their destination paths.
pub async fn download(
    connections: &dyn Connections,
    remote: &Endpoint,
    remote_dir: &Path,
    names: &[OsString],
    local_dir: &Path,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<Vec<(PathBuf, Error)>, Error> {
    let mut argv = vec![
        "tar".to_owned(),
        "-C".to_owned(),
        utf8(remote_dir.as_os_str(), remote_dir)?.to_owned(),
        "-cf".to_owned(),
        "-".to_owned(),
    ];
    for name in names {
        argv.push(format!("./{}", download_name(name, remote_dir)?));
    }
    let RemoteChild {
        mut child,
        stdin,
        stdout,
        stderr,
    } = connections.spawn_remote(remote, &argv).await?;
    // `tar -c` reads no input; closing stdin right away is the documented
    // end-of-input signal.
    drop(stdin);

    let (moved_tx, moved_rx) = watch::channel(0);
    let ended = Arc::new(AtomicBool::new(false));
    let reader = Metered::new(SyncIoBridge::new(stdout), moved_tx, Arc::clone(&ended));
    let local_dir = local_dir.to_path_buf();
    let task = tokio::task::spawn_blocking(move || {
        unpack(
            BufReader::with_capacity(PIPE_BUFFER, reader),
            &local_dir,
            &ended,
            |path, times| fs::set_times_nofollow(path, times),
        )
    });
    let unpacked = track(task, moved_rx, progress).await?;
    settle(unpacked, &mut child, &stderr).await
}

/// Why the local half of a tar stream stopped.
#[derive(Debug)]
enum StreamFailure {
    /// The pipe to or from the remote tar broke or ran dry, or the archive
    /// it carried was malformed. The remote exit status explains this first.
    Stream(io::Error),
    /// A failure on the local side that the remote cannot explain.
    Local(Error),
}

/// A source path and the archive name it is packed under.
struct UploadRoot {
    path: PathBuf,
    name: PathBuf,
}

/// Timestamp captured while packing a regular file or directory.
struct PackedMetadata {
    name: PathBuf,
    mtime: i64,
    directory: bool,
}

fn upload_root(source: &Path) -> Result<UploadRoot, Error> {
    if !source.is_absolute() {
        return Err(unsupported(format!(
            "{} is not an absolute path",
            source.display()
        )));
    }
    let name = source
        .file_name()
        .ok_or_else(|| unsupported(format!("{} has no file name to pack", source.display())))?;
    Ok(UploadRoot {
        path: source.to_path_buf(),
        name: PathBuf::from(name),
    })
}

/// Checks that `name` is one plain entry of `dir` and returns it as a string.
fn download_name<'a>(name: &'a OsStr, dir: &Path) -> Result<&'a str, Error> {
    let mut components = Path::new(name).components();
    let single = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    );
    if !single || name.as_encoded_bytes().contains(&b'/') {
        return Err(unsupported(format!(
            "{} is not a plain entry name of {}",
            Path::new(name).display(),
            dir.display()
        )));
    }
    utf8(name, &dir.join(name))
}

fn utf8<'a>(word: &'a OsStr, path: &Path) -> Result<&'a str, Error> {
    word.to_str()
        .ok_or_else(|| unsupported(format!("{} is not valid UTF-8", path.display())))
}

fn unsupported(reason: String) -> Error {
    Error::Fs(FsError::Unsupported {
        operation: "tar transfer".to_owned(),
        reason,
    })
}

/// Writes a complete archive of `roots` to `writer` and returns the writer
/// with every byte flushed. `ended` is the flag shared with the pipe end of
/// `writer`, set once a write to the pipe failed.
fn pack<W: Write>(
    writer: BufWriter<W>,
    roots: &[UploadRoot],
    ended: &AtomicBool,
    metadata: &mut Vec<PackedMetadata>,
) -> Result<W, StreamFailure> {
    let mut builder = Builder::new(writer);
    builder.follow_symlinks(false);
    for root in roots {
        append_tree(&mut builder, &root.path, &root.name, ended, metadata)?;
    }
    // Finishing only writes the end marker and flushes, so any failure here
    // is the pipe's.
    let writer = builder.into_inner().map_err(StreamFailure::Stream)?;
    writer
        .into_inner()
        .map_err(|error| StreamFailure::Stream(error.into_error()))
}

fn append_tree<W: Write>(
    builder: &mut Builder<W>,
    path: &Path,
    name: &Path,
    ended: &AtomicBool,
    metadata: &mut Vec<PackedMetadata>,
) -> Result<(), StreamFailure> {
    let local = |error: io::Error| StreamFailure::Local(FsError::from_io(&error, path).into());
    let attributes = fs::symlink_metadata(path).map_err(local)?;
    let file_type = attributes.file_type();
    if !(file_type.is_dir() || file_type.is_file() || file_type.is_symlink()) {
        return Ok(());
    }
    builder
        .append_path_with_name(path, name)
        .map_err(|error| stream_or_local(error, path, ended))?;
    if !file_type.is_symlink() {
        metadata.push(PackedMetadata {
            name: name.to_path_buf(),
            mtime: attributes.mtime(),
            directory: file_type.is_dir(),
        });
    }
    if file_type.is_dir() {
        let mut children = fs::read_dir(path)
            .and_then(|entries| entries.collect::<io::Result<Vec<_>>>())
            .map_err(local)?;
        children.sort_by_key(|child| child.file_name());
        for child in children {
            let child_name = child.file_name();
            append_tree(
                builder,
                &child.path(),
                &name.join(&child_name),
                ended,
                metadata,
            )?;
        }
    }
    Ok(())
}

/// Reads a complete archive from `reader` and extracts it into `local_dir`,
/// then consumes the rest of the stream (tar pads its output past the end
/// marker). `ended` is the flag shared with the pipe end of `reader`, set
/// once the pipe ran dry or failed.
fn unpack<R: Read>(
    reader: R,
    local_dir: &Path,
    ended: &AtomicBool,
    mut set_directory_time: impl FnMut(&Path, FileTimes) -> io::Result<()>,
) -> Result<Vec<(PathBuf, Error)>, StreamFailure> {
    let mut archive = Archive::new(reader);
    let mut warnings = Vec::new();
    archive.set_overwrite(true);
    archive.set_preserve_permissions(false);
    archive.set_preserve_mtime(true);
    {
        let mut directories = Vec::new();
        for entry in archive.entries().map_err(StreamFailure::Stream)? {
            let mut entry = entry.map_err(StreamFailure::Stream)?;
            let path = entry.path().map_err(StreamFailure::Stream)?.into_owned();
            check_entry(&entry, &path)?;
            match entry.header().entry_type() {
                EntryType::Directory => directories.push((entry, path)),
                EntryType::Fifo | EntryType::Char | EntryType::Block => {}
                _ => extract(&mut entry, &path, local_dir, ended)?,
            }
        }
        // Deepest first, as the tar crate's own `Archive::unpack` does, so a
        // parent's mtime and mode are set after its children were written.
        directories.sort_by_key(|(_, path)| std::cmp::Reverse(path.components().count()));
        for (mut entry, path) in directories {
            extract(&mut entry, &path, local_dir, ended)?;
            let seconds = entry.header().mtime().map_err(StreamFailure::Stream)?;
            let mtime = UNIX_EPOCH
                .checked_add(Duration::from_secs(seconds))
                .ok_or_else(|| {
                    StreamFailure::Local(Error::Archive {
                        message: format!(
                            "{} has an out-of-range directory timestamp",
                            path.display()
                        ),
                    })
                })?;
            let destination = local_dir.join(&path);
            if let Err(error) =
                set_directory_time(&destination, FileTimes::new().set_modified(mtime))
            {
                warnings.push((
                    destination.clone(),
                    FsError::from_io(&error, &destination).into(),
                ));
            }
        }
    }
    io::copy(&mut archive.into_inner(), &mut io::sink()).map_err(StreamFailure::Stream)?;
    Ok(warnings)
}

/// Rejects entries that would land outside the destination directory.
fn check_entry<R: Read>(entry: &Entry<'_, R>, path: &Path) -> Result<(), StreamFailure> {
    check_relative(path, "entry")?;
    if entry.header().entry_type() == EntryType::Link {
        if let Some(target) = entry.link_name().map_err(StreamFailure::Stream)? {
            check_relative(&target, "hard link target")?;
        }
    }
    Ok(())
}

fn check_relative(path: &Path, what: &str) -> Result<(), StreamFailure> {
    let unsafe_reason = path.components().find_map(|component| match component {
        Component::RootDir | Component::Prefix(_) => Some("is absolute"),
        Component::ParentDir => Some("contains `..`"),
        Component::CurDir | Component::Normal(_) => None,
    });
    match unsafe_reason {
        Some(reason) => Err(StreamFailure::Local(Error::Archive {
            message: format!("the archive {what} {} {reason}", path.display()),
        })),
        None => Ok(()),
    }
}

fn extract<R: Read>(
    entry: &mut Entry<'_, R>,
    path: &Path,
    local_dir: &Path,
    ended: &AtomicBool,
) -> Result<(), StreamFailure> {
    match entry.unpack_in(local_dir) {
        Ok(true) => Ok(()),
        // `unpack_in` declines only paths that `check_entry` rejects first.
        Ok(false) => Err(StreamFailure::Local(Error::Archive {
            message: format!("the archive entry {} cannot be extracted", path.display()),
        })),
        Err(error) => Err(stream_or_local(error, &local_dir.join(path), ended)),
    }
}

/// Attributes an I/O error raised while the tar crate used both the pipe and
/// the local file system: it is a stream failure once the pipe end reported
/// trouble, and a local failure on `path` otherwise.
fn stream_or_local(error: io::Error, path: &Path, ended: &AtomicBool) -> StreamFailure {
    if ended.load(Ordering::Acquire) {
        StreamFailure::Stream(error)
    } else {
        StreamFailure::Local(FsError::from_io(&error, path).into())
    }
}

/// Awaits the blocking tar task while forwarding the bytes it moved to
/// `progress`.
async fn track<T>(
    mut task: JoinHandle<T>,
    mut moved: watch::Receiver<u64>,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<T, Error> {
    let mut reported = 0;
    let joined = loop {
        tokio::select! {
            joined = &mut task => break joined,
            changed = moved.changed() => match changed {
                Ok(()) => {
                    reported = *moved.borrow_and_update();
                    progress(reported);
                }
                // The task dropped its counter, so it is about to finish.
                Err(_) => break (&mut task).await,
            },
        }
    };
    let total = *moved.borrow();
    if total != reported {
        progress(total);
    }
    match joined {
        Ok(value) => Ok(value),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(Error::Archive {
            message: format!("the tar worker stopped: {error}"),
        }),
    }
}

/// Combines the local outcome with the remote tar's exit status, following
/// the precedence in the module documentation. A local failure returns at
/// once, and dropping the child afterwards kills it.
async fn settle<T>(
    local: Result<T, StreamFailure>,
    child: &mut Child,
    stderr: &StderrTail,
) -> Result<T, Error> {
    let local = match local {
        Ok(value) => Ok(value),
        Err(StreamFailure::Local(error)) => return Err(error),
        Err(StreamFailure::Stream(error)) => Err(error),
    };
    let status = child.wait().await.map_err(|error| {
        Error::Connect(ConnectError::Other {
            detail: format!("waiting for the remote tar failed: {error}"),
        })
    })?;
    if !status.success() {
        return Err(Error::CommandFailed {
            program: "tar".to_owned(),
            status: status.into(),
            stderr: stderr_text(stderr),
        });
    }
    local.map_err(|error| Error::Archive {
        message: error.to_string(),
    })
}

fn stderr_text(stderr: &StderrTail) -> String {
    let lines = stderr.lock().unwrap_or_else(PoisonError::into_inner);
    lines
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The pipe end of a tar stream: counts the bytes moved for progress and
/// records when the pipe ran dry or failed, so errors can be attributed.
struct Metered<T> {
    inner: T,
    moved: u64,
    counter: watch::Sender<u64>,
    ended: Arc<AtomicBool>,
}

impl<T> Metered<T> {
    fn new(inner: T, counter: watch::Sender<u64>, ended: Arc<AtomicBool>) -> Self {
        Metered {
            inner,
            moved: 0,
            counter,
            ended,
        }
    }

    fn count(&mut self, bytes: usize) {
        self.moved += bytes as u64;
        self.counter.send_replace(self.moved);
    }

    fn mark_ended<V>(&self, result: io::Result<V>) -> io::Result<V> {
        if result.is_err() {
            self.ended.store(true, Ordering::Release);
        }
        result
    }
}

impl<T: Write> Write for Metered<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf);
        let written = self.mark_ended(written)?;
        self.count(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let flushed = self.inner.flush();
        self.mark_ended(flushed)
    }
}

impl<T: Read> Read for Metered<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf);
        let read = self.mark_ended(read)?;
        if read == 0 && !buf.is_empty() {
            self.ended.store(true, Ordering::Release);
        }
        self.count(read);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Stdio;
    use std::sync::Mutex;

    use futures_util::future::BoxFuture;
    use tar::Header;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    use tokio::process::Command;

    use super::*;
    use crate::connections::{FsChannel, RemoteTools};
    use crate::error::ExitStatus;
    use crate::vfs::Fs;

    /// One node of a directory tree as compared by the tests.
    #[derive(Debug, PartialEq, Eq)]
    enum Node {
        File {
            content: Content,
            mode: u32,
            mtime: u64,
        },
        Dir {
            mode: u32,
            mtime: u64,
        },
        Link(PathBuf),
    }

    /// File bytes, shown by length and a checksum so a failed comparison of
    /// large files stays readable.
    #[derive(PartialEq, Eq)]
    struct Content(Vec<u8>);

    impl std::fmt::Debug for Content {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let sum = self
                .0
                .iter()
                .fold(0u32, |sum, byte| sum.rotate_left(5) ^ u32::from(*byte));
            write!(f, "{} bytes, checksum {sum:08x}", self.0.len())
        }
    }

    const FILE_MTIME: u64 = 1_600_000_000;
    const DIR_MTIME: u64 = 1_500_000_000;
    const BIG_FILE_LEN: u32 = 300_000;

    fn set_mtime(path: &Path, seconds: u64) {
        let time = UNIX_EPOCH + Duration::from_secs(seconds);
        File::open(path)
            .expect("open for mtime")
            .set_times(FileTimes::new().set_modified(time).set_accessed(time))
            .expect("set mtime");
    }

    fn write_file(path: &Path, content: &[u8], mode: u32) {
        fs::write(path, content).expect("write file");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod file");
        set_mtime(path, FILE_MTIME);
    }

    /// Builds `<root>/proj` with files, a name starting with `-`, a non-ASCII
    /// name with spaces, nested directories, and symlinks to a file and to a
    /// directory. Directory mtimes are set last.
    fn sample_tree(root: &Path) -> PathBuf {
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("nested/deeper")).expect("mkdir nested");
        fs::create_dir(proj.join("-rf")).expect("mkdir dash");
        write_file(&proj.join("a.txt"), b"alpha\n", 0o640);
        write_file(&proj.join("-rf/inner.txt"), b"dash dir\n", 0o600);
        let big: Vec<u8> = (0..BIG_FILE_LEN).map(|i| (i % 251) as u8).collect();
        write_file(&proj.join("nested/deeper/big.bin"), &big, 0o751);
        write_file(
            &proj.join("sp ace \u{7a7a}\u{767d}.txt"),
            b"non-ascii\n",
            0o644,
        );
        symlink("a.txt", proj.join("link-to-file")).expect("file symlink");
        symlink("nested", proj.join("link-to-dir")).expect("dir symlink");
        fs::set_permissions(proj.join("nested"), fs::Permissions::from_mode(0o750))
            .expect("chmod dir");
        for dir in ["nested/deeper", "nested", "-rf", ""] {
            set_mtime(&proj.join(dir), DIR_MTIME);
        }
        proj
    }

    fn mtime_of(metadata: &fs::Metadata) -> u64 {
        metadata
            .modified()
            .expect("mtime")
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_secs()
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Node> {
        fn walk(root: &Path, relative: &Path, nodes: &mut BTreeMap<PathBuf, Node>) {
            let path = root.join(relative);
            let metadata = fs::symlink_metadata(&path).expect("lstat");
            let mode = metadata.permissions().mode() & 0o7777;
            let node = if metadata.file_type().is_symlink() {
                Node::Link(fs::read_link(&path).expect("readlink"))
            } else if metadata.is_dir() {
                let mut children: Vec<_> = fs::read_dir(&path)
                    .expect("read_dir")
                    .map(|entry| entry.expect("dir entry").file_name())
                    .collect();
                children.sort();
                for child in children {
                    walk(root, &relative.join(child), nodes);
                }
                Node::Dir {
                    mode,
                    mtime: mtime_of(&metadata),
                }
            } else {
                Node::File {
                    content: Content(fs::read(&path).expect("read")),
                    mode,
                    mtime: mtime_of(&metadata),
                }
            };
            nodes.insert(relative.to_path_buf(), node);
        }
        let mut nodes = BTreeMap::new();
        walk(root, Path::new(""), &mut nodes);
        nodes
    }

    fn root_of(path: &Path) -> UploadRoot {
        upload_root(path).expect("upload root")
    }

    fn pack_to_vec(roots: &[UploadRoot]) -> Vec<u8> {
        let ended = AtomicBool::new(false);
        pack(BufWriter::new(Vec::new()), roots, &ended, &mut Vec::new()).expect("pack")
    }

    fn unpack_bytes(bytes: Vec<u8>, dst: &Path) -> Result<Vec<(PathBuf, Error)>, StreamFailure> {
        let (counter, _moved) = watch::channel(0);
        let ended = Arc::new(AtomicBool::new(false));
        let reader = Metered::new(Cursor::new(bytes), counter, Arc::clone(&ended));
        unpack(reader, dst, &ended, |path, times| {
            fs::set_times_nofollow(path, times)
        })
    }

    fn entry_names(bytes: &[u8]) -> Vec<String> {
        let mut archive = Archive::new(bytes);
        archive
            .entries()
            .expect("entries")
            .map(|entry| {
                let entry = entry.expect("entry");
                String::from_utf8_lossy(&entry.path_bytes()).into_owned()
            })
            .collect()
    }

    /// An archive whose single entry has the raw name `name` (bypassing the
    /// tar crate's own path checks) and, for links, the raw target `link`.
    fn raw_archive(name: &[u8], kind: EntryType, link: Option<&[u8]>) -> Vec<u8> {
        let data: &[u8] = if kind == EntryType::Regular {
            b"evil"
        } else {
            b""
        };
        let mut header = Header::new_gnu();
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        if let Some(link) = link {
            header.as_old_mut().linkname[..link.len()].copy_from_slice(link);
        }
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        header.set_cksum();
        let mut builder = Builder::new(Vec::new());
        builder.append(&header, data).expect("append raw");
        builder.into_inner().expect("finish raw")
    }

    fn assert_unsafe(result: Result<Vec<(PathBuf, Error)>, StreamFailure>) {
        assert!(
            matches!(result, Err(StreamFailure::Local(Error::Archive { .. }))),
            "expected an unsafe-entry archive error, got {result:?}"
        );
    }

    #[test]
    fn directory_timestamp_failure_keeps_extracted_data_and_restores_other_directories() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        let proj = sample_tree(src.path());
        let bytes = pack_to_vec(&[root_of(&proj)]);
        let failed = dst.path().join("proj/nested");
        let mut restored = Vec::new();
        let warnings = unpack(
            Cursor::new(bytes),
            dst.path(),
            &AtomicBool::new(false),
            |path, times| {
                assert!(dst.path().join("proj/a.txt").is_file());
                restored.push(path.to_path_buf());
                if path == failed {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    fs::set_times_nofollow(path, times)
                }
            },
        )
        .expect("extraction succeeds despite metadata warning");
        assert_eq!(
            warnings,
            vec![(
                failed.clone(),
                FsError::PermissionDenied {
                    path: failed.clone()
                }
                .into()
            )]
        );
        assert_eq!(restored.last(), Some(&dst.path().join("proj")));
        assert_eq!(
            mtime_of(&fs::metadata(dst.path().join("proj")).expect("root metadata")),
            DIR_MTIME
        );
        assert_eq!(
            fs::read(dst.path().join("proj/a.txt")).expect("copied data"),
            fs::read(proj.join("a.txt")).expect("source data")
        );
    }

    #[test]
    fn pack_and_unpack_round_trip_the_tree() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        let proj = sample_tree(src.path());

        unpack_bytes(pack_to_vec(&[root_of(&proj)]), dst.path()).expect("unpack");

        let copy = snapshot(&dst.path().join("proj"));
        assert_eq!(copy, snapshot(&proj));
        assert_eq!(
            copy[Path::new("link-to-dir")],
            Node::Link(PathBuf::from("nested"))
        );
    }

    #[test]
    fn pack_names_each_source_after_itself() {
        let src = tempfile::tempdir().expect("tempdir");
        let proj = sample_tree(src.path());
        let bytes = pack_to_vec(&[root_of(&proj.join("a.txt")), root_of(&proj.join("-rf"))]);
        assert_eq!(
            entry_names(&bytes),
            ["a.txt", "-rf", "-rf/inner.txt"].map(String::from)
        );
    }

    #[test]
    fn pack_skips_special_files_and_never_follows_symlinks() {
        let src = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        fs::write(outside.path().join("secret.txt"), b"secret").expect("write secret");
        let dir = src.path().join("dir");
        fs::create_dir(&dir).expect("mkdir");
        let fifo = CString::new(dir.join("pipe").as_os_str().as_bytes()).expect("cstring");
        // SAFETY: `fifo` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        symlink(outside.path(), dir.join("out")).expect("symlink");

        let bytes = pack_to_vec(&[root_of(&dir)]);
        assert_eq!(entry_names(&bytes), ["dir", "dir/out"].map(String::from));
    }

    #[test]
    fn unpack_overwrites_existing_entries() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        let proj = sample_tree(src.path());
        let existing = dst.path().join("proj");
        fs::create_dir(&existing).expect("mkdir");
        write_file(&existing.join("a.txt"), b"old and longer content", 0o600);
        symlink("elsewhere", existing.join("link-to-file")).expect("symlink");

        unpack_bytes(pack_to_vec(&[root_of(&proj)]), dst.path()).expect("unpack");
        assert_eq!(snapshot(&existing), snapshot(&proj));
    }

    #[test]
    fn unpack_applies_only_permission_bits() {
        let dst = tempfile::tempdir().expect("tempdir");
        let mut header = Header::new_gnu();
        header.set_path("setuid.sh").expect("path");
        header.set_mode(0o4755);
        header.set_size(0);
        header.set_cksum();
        let mut builder = Builder::new(Vec::new());
        builder.append(&header, io::empty()).expect("append");
        unpack_bytes(builder.into_inner().expect("finish"), dst.path()).expect("unpack");
        let mode = fs::metadata(dst.path().join("setuid.sh"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[test]
    fn unpack_rejects_absolute_and_parent_entries() {
        let dst = tempfile::tempdir().expect("tempdir");
        assert_unsafe(unpack_bytes(
            raw_archive(b"/tmp/scpto-evil", EntryType::Regular, None),
            dst.path(),
        ));
        assert_unsafe(unpack_bytes(
            raw_archive(b"a/../../evil", EntryType::Regular, None),
            dst.path(),
        ));
        assert_unsafe(unpack_bytes(
            raw_archive(b"hard", EntryType::Link, Some(b"../evil")),
            dst.path(),
        ));
        assert_unsafe(unpack_bytes(
            raw_archive(b"hard", EntryType::Link, Some(b"/etc/passwd")),
            dst.path(),
        ));
        assert_eq!(fs::read_dir(dst.path()).expect("read_dir").count(), 0);
    }

    #[test]
    fn unpack_refuses_to_write_through_a_symlink() {
        let dst = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let mut builder = Builder::new(Vec::new());
        let mut link = Header::new_gnu();
        link.set_entry_type(EntryType::Symlink);
        link.set_path("esc").expect("path");
        link.set_link_name(outside.path()).expect("link name");
        link.set_size(0);
        link.set_cksum();
        builder.append(&link, io::empty()).expect("append link");
        let mut file = Header::new_gnu();
        file.set_path("esc/pwned").expect("path");
        file.set_size(4);
        file.set_mode(0o644);
        file.set_cksum();
        builder.append(&file, &b"evil"[..]).expect("append file");

        let result = unpack_bytes(builder.into_inner().expect("finish"), dst.path());
        assert!(
            matches!(result, Err(StreamFailure::Local(Error::Fs(_)))),
            "{result:?}"
        );
        assert!(!outside.path().join("pwned").exists());
    }

    #[test]
    fn a_truncated_stream_is_a_stream_failure() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        let proj = sample_tree(src.path());
        let mut bytes = pack_to_vec(&[root_of(&proj)]);
        bytes.truncate(bytes.len() / 2);
        let result = unpack_bytes(bytes, dst.path());
        assert!(
            matches!(result, Err(StreamFailure::Stream(_))),
            "{result:?}"
        );
    }

    #[test]
    fn a_local_write_failure_is_local() {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        let proj = sample_tree(src.path());
        let locked = dst.path().join("locked");
        fs::create_dir(&locked).expect("mkdir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");
        let result = unpack_bytes(pack_to_vec(&[root_of(&proj)]), &locked);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod back");
        assert!(
            matches!(
                result,
                Err(StreamFailure::Local(Error::Fs(
                    FsError::PermissionDenied { .. }
                )))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn inputs_that_cannot_be_named_remotely_are_rejected() {
        let dir = Path::new("/srv/data");
        assert_eq!(download_name(OsStr::new("-rf"), dir).expect("name"), "-rf");
        assert_eq!(
            download_name(OsStr::new("sp ace"), dir).expect("name"),
            "sp ace"
        );
        for bad in ["", ".", "..", "a/b", "/abs", "dir/"] {
            assert!(download_name(OsStr::new(bad), dir).is_err(), "{bad:?}");
        }
        assert!(download_name(OsStr::from_bytes(b"caf\xe9"), dir).is_err());
        assert!(upload_root(Path::new("relative/path")).is_err());
        assert!(upload_root(Path::new("/")).is_err());
    }

    #[test]
    fn exit_statuses_and_stderr_are_reported() {
        let tail: StderrTail = Arc::new(Mutex::new(VecDeque::from([
            "tar: x: Cannot stat".to_owned(),
            "tar: Exiting with failure status".to_owned(),
        ])));
        assert_eq!(
            stderr_text(&tail),
            "tar: x: Cannot stat\ntar: Exiting with failure status"
        );
    }

    /// Connections using local files and the system tar for hermetic streams.
    struct ProcessConnections;

    fn not_needed<T: Send + 'static>() -> BoxFuture<'static, Result<T, Error>> {
        Box::pin(async { Err(unsupported("not used by the tar tests".to_owned())) })
    }

    impl Connections for ProcessConnections {
        fn fs<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _channel: FsChannel,
        ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
            Box::pin(async { Ok(Arc::new(crate::vfs::local::LocalFs::new()) as Arc<dyn Fs>) })
        }

        fn spawn_remote<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            argv: &'a [String],
        ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).env("COPYFILE_DISABLE", "1");
            Box::pin(async move {
                let mut child = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .expect("spawn");
                let stdin = child.stdin.take().expect("stdin");
                let stdout = child.stdout.take().expect("stdout");
                let mut lines =
                    tokio::io::BufReader::new(child.stderr.take().expect("stderr")).lines();
                let stderr: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
                let tail = Arc::clone(&stderr);
                tokio::spawn(async move {
                    while let Some(line) = lines.next_line().await.expect("read stderr") {
                        tail.lock().expect("tail lock").push_back(line);
                    }
                });
                Ok(RemoteChild {
                    child,
                    stdin,
                    stdout,
                    stderr,
                })
            })
        }

        fn ssh_command<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _interactive: bool,
        ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
            not_needed()
        }

        fn tools<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
            not_needed()
        }
    }

    fn remote(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.to_owned(),
            sudo: false,
        }
    }

    /// Records every progress report and checks that they never go back.
    #[derive(Default)]
    struct ProgressLog(Mutex<Vec<u64>>);

    impl ProgressLog {
        fn report(&self, moved: u64) {
            let mut seen = self.0.lock().expect("progress lock");
            assert!(seen.last().is_none_or(|last| *last <= moved));
            seen.push(moved);
        }

        fn last(&self) -> u64 {
            *self
                .0
                .lock()
                .expect("progress lock")
                .last()
                .expect("progress reported")
        }
    }

    /// Uploads a sample tree into `remote_dir`, downloads it again into a
    /// new local directory, and checks that it survived both ways.
    async fn round_trip(connections: &dyn Connections, endpoint: &Endpoint, remote_dir: &Path) {
        let src = tempfile::tempdir().expect("tempdir");
        let back = tempfile::tempdir().expect("tempdir");
        let proj = sample_tree(src.path());

        let sent = ProgressLog::default();
        upload(
            connections,
            std::slice::from_ref(&proj),
            endpoint,
            remote_dir,
            &|moved| sent.report(moved),
        )
        .await
        .expect("upload");
        assert!(sent.last() > u64::from(BIG_FILE_LEN));

        let received = ProgressLog::default();
        download(
            connections,
            endpoint,
            remote_dir,
            &[OsString::from("proj")],
            back.path(),
            &|moved| received.report(moved),
        )
        .await
        .expect("download");
        assert!(received.last() > u64::from(BIG_FILE_LEN));
        assert_eq!(snapshot(&back.path().join("proj")), snapshot(&proj));
    }

    #[tokio::test]
    async fn upload_timestamp_warning_keeps_data_and_never_sets_symlink_targets() {
        use crate::transfer::stream::test_support::{MemFs, Op};
        use crate::ui::effects::transfer::test_support::{TestConnections, Tools};

        let src = tempfile::tempdir().expect("source");
        let dst = tempfile::tempdir().expect("destination");
        let outside = tempfile::tempdir().expect("outside");
        let tree = src.path().join("tree");
        fs::create_dir(&tree).expect("source directory");
        write_file(&tree.join("file"), b"data", 0o600);
        let sentinel = outside.path().join("sentinel");
        write_file(&sentinel, b"untouched", 0o600);
        set_mtime(&sentinel, 123);
        symlink(&sentinel, tree.join("link")).expect("source link");
        set_mtime(&tree, DIR_MTIME);

        let endpoint = remote("fake");
        let metadata = MemFs::new(endpoint.clone());
        let destination = dst.path().join("tree");
        let path = destination.to_str().expect("utf8 path");
        metadata.add_dir(path);
        metadata.add_file(
            destination.join("file").to_str().expect("utf8 path"),
            b"data",
            0,
            0o600,
        );
        metadata.add_symlink(
            destination.join("link").to_str().expect("utf8 path"),
            sentinel.to_str().expect("utf8 path"),
        );
        let error = FsError::PermissionDenied {
            path: destination.clone(),
        };
        metadata.fail(Op::SetTimes, path, error.clone(), false);
        let connections = TestConnections::new(
            vec![(endpoint.clone(), metadata.clone())],
            Tools::Ready(Ok(RemoteTools {
                tar: true,
                rsync_ok: false,
            })),
        );
        let warnings = upload(&connections, &[tree], &endpoint, dst.path(), &|_| {})
            .await
            .expect("data succeeds");
        assert_eq!(warnings, vec![(destination.clone(), error.into())]);
        assert_eq!(
            fs::read(destination.join("file")).expect("copied data"),
            b"data"
        );
        assert_eq!(
            fs::read_link(destination.join("link")).expect("copied link"),
            sentinel
        );
        assert_eq!(
            mtime_of(&fs::metadata(&sentinel).expect("sentinel metadata")),
            123
        );
        let restored: Vec<_> = metadata
            .calls()
            .into_iter()
            .filter(|(op, _)| *op == Op::SetTimes)
            .map(|(_, path)| path)
            .collect();
        assert_eq!(restored, [destination.join("file"), destination]);
    }

    #[tokio::test]
    async fn upload_and_download_through_the_system_tar() {
        let connections = ProcessConnections;
        let remote_dir = tempfile::tempdir().expect("tempdir");
        round_trip(&connections, &remote("fake"), remote_dir.path()).await;
    }

    #[tokio::test]
    async fn remote_tar_failures_are_command_failures() {
        let connections = ProcessConnections;
        let endpoint = remote("fake");
        let src = tempfile::tempdir().expect("tempdir");
        let dir = tempfile::tempdir().expect("tempdir");
        let proj = sample_tree(src.path());

        let missing_dir = dir.path().join("missing");
        let result = upload(&connections, &[proj], &endpoint, &missing_dir, &|_| {}).await;
        assert!(
            matches!(
                &result,
                Err(Error::CommandFailed { program, status: ExitStatus::Code(code), .. })
                    if program == "tar" && *code != 0
            ),
            "{result:?}"
        );

        let result = download(
            &connections,
            &endpoint,
            dir.path(),
            &[OsString::from("missing")],
            dir.path(),
            &|_| {},
        )
        .await;
        assert!(
            matches!(&result, Err(Error::CommandFailed { program, .. }) if program == "tar"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_local_source_failure_wins_over_the_remote_outcome() {
        let connections = ProcessConnections;
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("gone");
        let result = upload(
            &connections,
            std::slice::from_ref(&missing),
            &remote("fake"),
            dir.path(),
            &|_| {},
        )
        .await;
        assert_eq!(result, Err(Error::Fs(FsError::NotFound { path: missing })));
    }

    async fn run_remote(
        connections: &dyn Connections,
        endpoint: &Endpoint,
        argv: &[&str],
    ) -> String {
        let argv: Vec<String> = argv.iter().map(|word| (*word).to_owned()).collect();
        let RemoteChild {
            mut child,
            stdin,
            mut stdout,
            ..
        } = connections
            .spawn_remote(endpoint, &argv)
            .await
            .expect("spawn");
        drop(stdin);
        let mut output = String::new();
        stdout
            .read_to_string(&mut output)
            .await
            .expect("read output");
        assert!(child.wait().await.expect("wait").success(), "{argv:?}");
        output.trim_end().to_owned()
    }

    /// Round trips against the docker sshd fixtures (`tests/it/fixtures/up.sh`
    /// writes `target/it/ssh_config`); runs only with `SCPTO_IT=1`.
    #[tokio::test]
    async fn docker_round_trip() {
        if std::env::var_os("SCPTO_IT").as_deref() != Some(OsStr::new("1")) {
            eprintln!("skipped: set SCPTO_IT=1 to run against the docker fixtures");
            return;
        }
        let connections = crate::ui::effects::transfer::test_support::FixtureConnections::new();
        for host in ["it-modern-plain", "it-legacy-plain"] {
            let endpoint = remote(host);
            let remote_dir =
                PathBuf::from(run_remote(&connections, &endpoint, &["mktemp", "-d"]).await);
            round_trip(&connections, &endpoint, &remote_dir).await;

            let home = PathBuf::from(run_remote(&connections, &endpoint, &["pwd"]).await);
            let seed = tempfile::tempdir().expect("tempdir");
            download(
                &connections,
                &endpoint,
                &home,
                &[OsString::from("seed")],
                seed.path(),
                &|_| {},
            )
            .await
            .expect("download seed");
            let seed_dir = seed.path().join("seed");
            assert_eq!(
                fs::read(seed_dir.join("hello.txt")).expect("hello"),
                b"hello scpto\n"
            );
            assert!(seed_dir
                .join("\u{7a7a}\u{767d} \u{6a94}\u{6848} \u{6e2c}\u{8a66}.txt")
                .is_file());
            assert!(fs::symlink_metadata(seed_dir.join("link-to-dir"))
                .expect("lstat")
                .file_type()
                .is_symlink());

            let result = download(
                &connections,
                &endpoint,
                &home,
                &[OsString::from("no-such-entry")],
                seed.path(),
                &|_| {},
            )
            .await;
            assert!(
                matches!(&result, Err(Error::CommandFailed { program, .. }) if program == "tar"),
                "{result:?}"
            );

            let remote_dir = remote_dir.to_str().expect("utf8");
            run_remote(&connections, &endpoint, &["rm", "-rf", remote_dir]).await;
        }
    }
}
