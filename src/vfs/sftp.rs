//! SFTP `Fs` implementation on `openssh-sftp-client`, over an `Sftp` built
//! from the stdin/stdout of an `ssh -s <host> sftp` child (or a sudo
//! sftp-server child). Maps protocol and background-task failures to
//! `FsError`, with a lost connection reported as `FsError::Disconnected`.
//!
//! # Ownership and teardown
//!
//! An `SftpFs` owns its child for as long as its `Sftp` lives (plan 7.3
//! S10). The `Sftp` field is declared before the child so it drops first;
//! the child then drops and `kill_on_drop` (set by the spawner) kills the
//! ssh process, which ends the session. `Sftp::close()` is never called:
//! after a disconnect it only reports the broken pipe (plan 8.5), and on a
//! healthy session the kill ends the channel just as well. Nothing is ever
//! shown for teardown.
//!
//! [`SftpFs::closed`] resolves when the child exits or when any operation
//! (including a reader or writer handed out by this file system) observed
//! `BackgroundTaskFailure`, whichever comes first.
//!
//! # Server differences
//!
//! Facts from the Wave 1 spike that this module honors: `readdir` returns
//! `.` and `..` (filtered here); OpenSSH 7.9 lacks `limits`, `copy-data`,
//! and `expand-path`, so every extension is feature-detected from the
//! server's hello: `posix_rename` from `posix-rename@openssh.com`,
//! `copy_within` from `copy-data`; the client applies `limits` when present
//! and uses `expand-path` for `canonicalize` when present (plain `realpath`
//! otherwise); `home` is the canonical form of `.` (the server's start
//! directory, normally the login home), which works on every server.
//!
//! Without `posix-rename@openssh.com`, the client sends a plain
//! `SSH_FXP_RENAME`, which refuses an existing target. `rename` then
//! confirms the target with `lstat`, removes it when it is not a directory,
//! and renames again. This is not atomic (plan 7.5 S14): a failure between
//! the two steps leaves the target removed.
//!
//! `open_write` with `WriteMode::Truncate { permissions }` truncates an
//! existing file (which keeps its mode); a missing file is created with
//! `create_new` and restricted to `permissions & 0o777` with `fsetstat` on
//! the new handle before the writer is returned, so no data is written to
//! it before the bits are in place. `openssh-sftp-client` 0.15 always sends
//! empty attributes with `SSH_FXP_OPEN`, so the bits cannot travel with the
//! open itself: between the two requests (one round trip) the empty file
//! has the server's default mode. A user who opens it in that window can
//! read what is written later; closing that window needs open attributes
//! from the client library.
//!
//! Reads and writes are pipelined: a reader keeps up to 2 MiB of 64 KiB
//! read requests in flight and a writer up to 2 MiB of write requests, so
//! throughput does not collapse to one request per round trip. Pipelined write
//! chunks never exceed the negotiated write limit, so each chunk is one
//! request sent in offset order. Interrupted uploads therefore leave a
//! contiguous prefix that can be resumed from the part file's size.
//! Initial chunks run alone until a full 64 KiB chunk is offered to learn a
//! safe request size from the client's actual write count; its remaining
//! bytes finish before the pipeline starts. Short caller buffers do not
//! constrain later pipelined chunks.
//! A writer's `shutdown()` waits for every write acknowledgement and then
//! closes the remote handle, reporting a failed close; only then is the
//! data committed.
//!
//! # Error mapping
//!
//! - `BackgroundTaskFailure` (master or child gone; child stderr is empty)
//!   and transport I/O errors → `FsError::Disconnected` for operations,
//!   `ConnectError::ConnectionLost` at start.
//! - At start only: `SftpServerHelloMsgTooLong` → `ConnectError::ProtocolCorrupted`
//!   (shell startup output); `UnexpectedEof` with stderr line
//!   `Cannot execute command-line and remote command.` →
//!   `ConnectError::ProtocolCorrupted` naming the `RemoteCommand` conflict;
//!   other `UnexpectedEof` → `ConnectError::SftpServerMissing` with the
//!   stderr tail; no hello within 20 s → `ConnectError::UnsupportedServer`;
//!   any other start failure → `ConnectError::UnsupportedServer`. Before
//!   reading the stderr tail, a failed start waits up to 2 s for the child
//!   to exit so the drain task has its last lines.
//! - SFTP status codes: no such file → `NotFound`, permission denied →
//!   `PermissionDenied`, op unsupported → `Unsupported`, failure on
//!   `mkdir`/`rename` of an existing target → `AlreadyExists` after a stat
//!   confirms it, everything else → `FsError::Io` with the server message.
//! - Readers and writers fail with an `io::Error` whose inner error
//!   (`io::Error::get_ref`) is the classified `FsError`, so callers recover
//!   `FsError::Disconnected` with `downcast_ref::<FsError>()`. Its kind
//!   matches the class (`NotFound`, `PermissionDenied`, `ConnectionAborted`
//!   for a lost connection, `Other` otherwise).
//!
//! # Public API (Wave 2)
//!
//! ```text
//! pub struct SftpFs { /* private: endpoint, Sftp, RemoteChild, extensions */ }
//!
//! impl SftpFs {
//!     /// Runs the SFTP handshake on the child's stdin/stdout and queries
//!     /// the extensions. `channel.stderr` is consulted for classification.
//!     pub async fn start(endpoint: Endpoint, channel: RemoteChild) -> Result<SftpFs, ConnectError>;
//!     /// Resolves when the session is unusable (child exited or the
//!     /// client's background task failed); the session awaits it to notice
//!     /// a lost connection.
//!     pub async fn closed(&self);
//! }
//!
//! impl Fs for SftpFs { /* every method */ }
//! ```
//!
//! `capabilities()`: `posix_rename` and `copy_within` as advertised,
//! `symlink: true`, `trash: false`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesOrdered, FuturesUnordered, StreamExt, TryStreamExt};
use futures_util::FutureExt;
use openssh_sftp_client::error::{SftpErrMsg, SftpErrorKind};
use openssh_sftp_client::file::File;
use openssh_sftp_client::metadata::{MetaData, MetaDataBuilder, Permissions};
use openssh_sftp_client::{Error as SftpError, Sftp, SftpOptions, UnixTimeStamp};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use tokio::process::Child;
use tokio::sync::{watch, Mutex};
use tokio_util::bytes::BytesMut;

use crate::connections::{RemoteChild, StderrTail};
use crate::error::ConnectError;
use crate::vfs::{
    Capabilities, Endpoint, Entry, EntryKind, Fs, FsError, FsReader, FsResult, FsWriter,
    SpecialKind, WriteMode,
};

/// How long the server may take to answer the SFTP hello.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a failed start waits for the child to exit before reading its
/// stderr tail.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Read chunk size and upper bound of a write chunk. The client splits reads
/// as needed; writes discover a single-request limit before pipelining.
const CHUNK: usize = 64 * 1024;

/// Bytes a reader keeps requested ahead of the caller.
const READ_AHEAD: usize = 2 * 1024 * 1024;

/// Bytes a writer may have sent but not yet acknowledged.
const WRITE_BEHIND: usize = 2 * 1024 * 1024;

/// Symlinks of one listing resolved concurrently.
const SYMLINK_CONCURRENCY: usize = 64;

/// The stderr line ssh prints when a configured `RemoteCommand` conflicts
/// with the sftp subsystem (spike experiment 2B).
const REMOTE_COMMAND_CONFLICT: &str = "Cannot execute command-line and remote command.";

/// Stderr lines quoted in a `SftpServerMissing` detail.
const STDERR_DETAIL_LINES: usize = 5;

/// SFTP file system on one remote endpoint channel.
pub struct SftpFs {
    endpoint: Endpoint,
    capabilities: Capabilities,
    /// Declared before `child`: fields drop in declaration order, so the
    /// session ends before `kill_on_drop` kills the ssh process.
    sftp: Sftp,
    lost: LostSignal,
    child: Mutex<Child>,
}

impl SftpFs {
    /// Runs the SFTP handshake on the child's stdin/stdout and queries the
    /// extensions. `channel.stderr` is consulted for classification.
    pub async fn start(endpoint: Endpoint, channel: RemoteChild) -> Result<SftpFs, ConnectError> {
        let RemoteChild {
            child,
            stdin,
            stdout,
            stderr,
        } = channel;
        SftpFs::start_on(endpoint, child, stdin, stdout, &stderr).await
    }

    /// [`SftpFs::start`] over any byte transport belonging to `child`.
    async fn start_on<W, R>(
        endpoint: Endpoint,
        mut child: Child,
        stdin: W,
        stdout: R,
        stderr: &StderrTail,
    ) -> Result<SftpFs, ConnectError>
    where
        W: AsyncWrite + Send + 'static,
        R: AsyncRead + Send + 'static,
    {
        let handshake = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            Sftp::new(stdin, stdout, SftpOptions::new()),
        )
        .await;
        match handshake {
            Ok(Ok(sftp)) => {
                let capabilities = Capabilities {
                    posix_rename: sftp.support_posix_rename(),
                    copy_within: sftp.support_copy(),
                    symlink: true,
                    trash: false,
                };
                Ok(SftpFs {
                    endpoint,
                    capabilities,
                    sftp,
                    lost: LostSignal::new(),
                    child: Mutex::new(child),
                })
            }
            Ok(Err(error)) => {
                // The exit status is not used: this wait only lets the drain
                // task collect ssh's last stderr lines before the snapshot,
                // and a child that does not exit in time is killed on drop.
                let _ = tokio::time::timeout(EXIT_GRACE, child.wait()).await;
                Err(classify_start_error(&error, &stderr_snapshot(stderr)))
            }
            // `child` drops here and `kill_on_drop` ends it.
            Err(_elapsed) => Err(ConnectError::UnsupportedServer {
                detail: format!(
                    "no SFTP handshake reply within {} seconds",
                    HANDSHAKE_TIMEOUT.as_secs()
                ),
            }),
        }
    }

    /// Resolves when the session is unusable (child exited or the client's
    /// background task failed); the session awaits it to notice a lost
    /// connection.
    pub async fn closed(&self) {
        let mut lost = self.lost.subscribe();
        tokio::select! {
            // Any outcome of `wait` means the child is gone or cannot be
            // waited on; either way the session is over.
            _ = async { self.child.lock().await.wait().await } => {}
            // The sender lives in `self`, so this only returns once lost.
            _ = lost.wait_for(|lost| *lost) => {}
        }
    }

    /// Classifies `error` for `path` and records a lost connection.
    fn fail(&self, error: &SftpError, path: &Path, operation: &str) -> FsError {
        self.lost.classify(error, path, operation)
    }

    /// Builds the `Entry` for `path` from its lstat `meta`, resolving a
    /// symlink's target and whether it leads to a directory.
    async fn describe(&self, path: &Path, name: OsString, meta: MetaData) -> FsResult<Entry> {
        let kind = match raw_kind(&meta) {
            RawKind::File => EntryKind::File,
            RawKind::Dir => EntryKind::Dir,
            RawKind::Special(special) => EntryKind::Special(special),
            RawKind::Symlink => {
                let mut fs = self.sftp.fs();
                let target = fs
                    .read_link(path)
                    .await
                    .map_err(|error| self.fail(&error, path, "read link"))?;
                let target_is_dir = match fs.metadata(path).await {
                    Ok(target) => raw_kind(&target) == RawKind::Dir,
                    // Dangling, looping, or unreadable targets are simply
                    // not enterable; only a lost connection is an error.
                    Err(error) => match self.fail(&error, path, "stat") {
                        FsError::Disconnected => return Err(FsError::Disconnected),
                        _ => false,
                    },
                };
                EntryKind::Symlink {
                    target,
                    target_is_dir,
                }
            }
        };
        Ok(entry(name, kind, &meta))
    }

    /// Maps a generic `Failure` from creating `path` to `AlreadyExists`
    /// when `path` exists; otherwise classifies `error` as usual.
    async fn confirm_exists(&self, error: SftpError, path: &Path, operation: &str) -> FsError {
        if !is_failure(&error) {
            return self.fail(&error, path, operation);
        }
        match self.sftp.fs().symlink_metadata(path).await {
            Ok(_) => FsError::AlreadyExists {
                path: path.to_path_buf(),
            },
            Err(stat_error) => match self.fail(&stat_error, path, "stat") {
                FsError::Disconnected => FsError::Disconnected,
                _ => self.fail(&error, path, operation),
            },
        }
    }

    async fn rename_impl(&self, from: &Path, to: &Path) -> FsResult<()> {
        let mut fs = self.sftp.fs();
        let error = match fs.rename(from, to).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if !is_failure(&error) {
            return Err(self.fail(&error, from, "rename"));
        }
        let target = match fs.symlink_metadata(to).await {
            Ok(target) => target,
            Err(stat_error) => {
                return Err(match self.fail(&stat_error, to, "stat") {
                    FsError::Disconnected => FsError::Disconnected,
                    _ => self.fail(&error, from, "rename"),
                })
            }
        };
        if self.capabilities.posix_rename || raw_kind(&target) == RawKind::Dir {
            return Err(FsError::AlreadyExists {
                path: to.to_path_buf(),
            });
        }
        // Plain SSH_FXP_RENAME refuses an existing target: remove it first
        // (not atomic, plan 7.5 S14).
        fs.remove_file(to)
            .await
            .map_err(|error| self.fail(&error, to, "rename"))?;
        fs.rename(from, to)
            .await
            .map_err(|error| self.fail(&error, from, "rename"))
    }

    async fn copy_within_impl(&self, from: &Path, to: &Path) -> FsResult<()> {
        if !self.capabilities.copy_within {
            return Err(FsError::Unsupported {
                operation: "copy_within".into(),
                reason: "the server does not offer the copy-data extension".into(),
            });
        }
        let mut source = self
            .sftp
            .open(from)
            .await
            .map_err(|error| self.fail(&error, from, "copy"))?;
        let mut destination = self
            .sftp
            .create(to)
            .await
            .map_err(|error| self.fail(&error, to, "copy"))?;
        source
            .copy_all_to(&mut destination)
            .await
            .map_err(|error| self.fail(&error, to, "copy"))?;
        destination
            .close()
            .await
            .map_err(|error| self.fail(&error, to, "copy"))?;
        source
            .close()
            .await
            .map_err(|error| self.fail(&error, from, "copy"))
    }
}

impl Fs for SftpFs {
    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn home(&self) -> BoxFuture<'_, FsResult<PathBuf>> {
        Box::pin(async move {
            let here = Path::new(".");
            self.sftp
                .fs()
                .canonicalize(here)
                .await
                .map_err(|error| self.fail(&error, here, "find the home directory"))
        })
    }

    fn canonicalize<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
        Box::pin(async move {
            self.sftp
                .fs()
                .canonicalize(path)
                .await
                .map_err(|error| self.fail(&error, path, "resolve path"))
        })
    }

    fn list<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, FsResult<Vec<Entry>>> {
        Box::pin(async move {
            let raw: Vec<_> = self
                .sftp
                .fs()
                .open_dir(dir)
                .await
                .map_err(|error| self.fail(&error, dir, "list"))?
                .read_dir()
                .try_collect()
                .await
                .map_err(|error| self.fail(&error, dir, "list"))?;
            futures_util::stream::iter(raw.into_iter().filter_map(|item| {
                let name = item.filename().as_os_str();
                (!is_dot_entry(name)).then(|| (name.to_os_string(), item.metadata()))
            }))
            .map(|(name, meta)| async move {
                let path = dir.join(&name);
                self.describe(&path, name, meta).await
            })
            .buffered(SYMLINK_CONCURRENCY)
            .try_collect()
            .await
        })
    }

    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
        Box::pin(async move {
            let meta = self
                .sftp
                .fs()
                .metadata(path)
                .await
                .map_err(|error| self.fail(&error, path, "stat"))?;
            // A followed stat never reports a symlink; `describe` resolves
            // one anyway should a server do so.
            self.describe(path, file_name(path), meta).await
        })
    }

    fn lstat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
        Box::pin(async move {
            let meta = self
                .sftp
                .fs()
                .symlink_metadata(path)
                .await
                .map_err(|error| self.fail(&error, path, "stat"))?;
            self.describe(path, file_name(path), meta).await
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            match self.sftp.fs().create_dir(path).await {
                Ok(()) => Ok(()),
                Err(error) => Err(self.confirm_exists(error, path, "create directory").await),
            }
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(self.rename_impl(from, to))
    }

    fn remove_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            self.sftp
                .fs()
                .remove_file(path)
                .await
                .map_err(|error| self.fail(&error, path, "delete"))
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            self.sftp
                .fs()
                .remove_dir(path)
                .await
                .map_err(|error| self.fail(&error, path, "delete directory"))
        })
    }

    fn trash<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            Err(FsError::Unsupported {
                operation: "trash".into(),
                reason: "remote hosts have no trash; deletion there is permanent".into(),
            })
        })
    }

    fn chmod<'a>(&'a self, path: &'a Path, mode: u32) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            // The mask keeps the value within 12 bits, so it fits in u16.
            let permissions = Permissions::from((mode & 0o7777) as u16);
            self.sftp
                .fs()
                .set_permissions(path, permissions)
                .await
                .map_err(|error| self.fail(&error, path, "change permissions"))
        })
    }

    fn set_times<'a>(
        &'a self,
        path: &'a Path,
        atime: i64,
        mtime: i64,
    ) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            let accessed = sftp_time(atime, path)?;
            let modified = sftp_time(mtime, path)?;
            let meta = MetaDataBuilder::new().time(accessed, modified).create();
            self.sftp
                .fs()
                .set_metadata(path, meta)
                .await
                .map_err(|error| self.fail(&error, path, "set times"))
        })
    }

    fn read_link<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
        Box::pin(async move {
            self.sftp
                .fs()
                .read_link(path)
                .await
                .map_err(|error| self.fail(&error, path, "read link"))
        })
    }

    fn symlink<'a>(&'a self, target: &'a Path, link: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            self.sftp
                .fs()
                .symlink(target, link)
                .await
                .map_err(|error| self.fail(&error, link, "create symlink"))
        })
    }

    fn open_read<'a>(&'a self, path: &'a Path, offset: u64) -> BoxFuture<'a, FsResult<FsReader>> {
        Box::pin(async move {
            let file = self
                .sftp
                .open(path)
                .await
                .map_err(|error| self.fail(&error, path, "open"))?;
            let reader = SftpReader::new(file, offset, path, self.lost.clone());
            Ok(Box::pin(reader) as FsReader)
        })
    }

    fn open_write<'a>(
        &'a self,
        path: &'a Path,
        mode: WriteMode,
    ) -> BoxFuture<'a, FsResult<FsWriter>> {
        Box::pin(async move {
            let open_error = |error: SftpError| self.fail(&error, path, "open for writing");
            let mut options = self.sftp.options();
            options.write(true);
            let (file, offset) = match mode {
                WriteMode::Truncate { permissions } => match self.lstat(path).await {
                    Ok(_) => {
                        options.create(true).truncate(true);
                        (options.open(path).await.map_err(open_error)?, 0)
                    }
                    Err(FsError::NotFound { .. }) => {
                        // `create_new` guarantees that the handle is the file
                        // this call created, so restricting it cannot touch
                        // anything else.
                        options.create_new(true);
                        let mut file = options.open(path).await.map_err(open_error)?;
                        // The mask keeps the value within 9 bits.
                        let bits = Permissions::from((permissions & 0o777) as u16);
                        file.set_permissions(bits)
                            .await
                            .map_err(|error| self.fail(&error, path, "change permissions"))?;
                        (file, 0)
                    }
                    Err(error) => return Err(error),
                },
                WriteMode::Resume { offset } => {
                    (options.open(path).await.map_err(open_error)?, offset)
                }
            };
            let writer = SftpWriter::new(file, offset, path, self.lost.clone());
            Ok(Box::pin(writer) as FsWriter)
        })
    }

    fn copy_within<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(self.copy_within_impl(from, to))
    }
}

/// Latch shared by an `SftpFs` and its readers and writers: set once any of
/// them classifies an error as a lost connection.
#[derive(Clone)]
struct LostSignal(Arc<watch::Sender<bool>>);

impl LostSignal {
    fn new() -> LostSignal {
        LostSignal(Arc::new(watch::Sender::new(false)))
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }

    /// Classifies `error` for `path` and latches a lost connection.
    fn classify(&self, error: &SftpError, path: &Path, operation: &str) -> FsError {
        let classified = classify_error(error, path, operation);
        self.observe(&classified);
        classified
    }

    fn observe(&self, error: &FsError) {
        if *error == FsError::Disconnected {
            self.0.send_replace(true);
        }
    }

    /// Classifies an `io::Error` produced inside a pipelined request and
    /// wraps the result for an `AsyncRead`/`AsyncWrite` caller.
    fn io_error(&self, error: FsError) -> io::Error {
        self.observe(&error);
        fs_io_error(error)
    }
}

/// Classifies an SFTP client error raised by an operation on `path`.
fn classify_error(error: &SftpError, path: &Path, operation: &str) -> FsError {
    let path = path.to_path_buf();
    match error {
        SftpError::SftpError(SftpErrorKind::NoSuchFile, _) => FsError::NotFound { path },
        SftpError::SftpError(SftpErrorKind::PermDenied, _) => FsError::PermissionDenied { path },
        SftpError::SftpError(SftpErrorKind::OpUnsupported, message) => FsError::Unsupported {
            operation: operation.to_owned(),
            reason: server_message(SftpErrorKind::OpUnsupported, message),
        },
        SftpError::SftpError(kind, message) => FsError::Io {
            path,
            message: server_message(*kind, message),
        },
        // The read/flush tasks died (master or child gone), the transport
        // failed, or a response can no longer arrive: all mean the channel
        // is over.
        SftpError::BackgroundTaskFailure(_)
        | SftpError::IOError(_)
        | SftpError::AwaitableError(_) => FsError::Disconnected,
        SftpError::UnsupportedExtension(name) => FsError::Unsupported {
            operation: operation.to_owned(),
            reason: format!("the server does not offer the {name} extension"),
        },
        SftpError::RecursiveErrors(errors) => {
            classify_error(&errors.original_error, &path, operation)
        }
        SftpError::RecursiveErrors3(errors) => classify_error(&errors.err1, &path, operation),
        other => FsError::Io {
            path,
            message: other.to_string(),
        },
    }
}

/// The server's error text, or the status name when the server sent none.
fn server_message(kind: SftpErrorKind, message: &SftpErrMsg) -> String {
    let (text, _language) = message.get();
    let text = text.trim();
    if text.is_empty() {
        format!("the server reported {kind:?}")
    } else {
        text.to_owned()
    }
}

/// Whether `error` is the generic `SSH_FX_FAILURE` status.
fn is_failure(error: &SftpError) -> bool {
    matches!(error, SftpError::SftpError(SftpErrorKind::Failure, _))
}

/// Wraps a classified error for an `AsyncRead`/`AsyncWrite` caller; the
/// `FsError` is the inner error (see the module doc).
fn fs_io_error(error: FsError) -> io::Error {
    let kind = match &error {
        FsError::NotFound { .. } => io::ErrorKind::NotFound,
        FsError::PermissionDenied { .. } => io::ErrorKind::PermissionDenied,
        FsError::AlreadyExists { .. } => io::ErrorKind::AlreadyExists,
        FsError::Unsupported { .. } => io::ErrorKind::Unsupported,
        FsError::Disconnected => io::ErrorKind::ConnectionAborted,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}

/// Classifies a failed `Sftp::new` using the last stderr lines of the child.
fn classify_start_error(error: &SftpError, stderr: &[String]) -> ConnectError {
    match error {
        SftpError::SftpServerHelloMsgTooLong { .. } => ConnectError::ProtocolCorrupted {
            detail: error.to_string(),
        },
        SftpError::BackgroundTaskFailure(_) => ConnectError::ConnectionLost,
        SftpError::IOError(io_error)
            if matches!(
                io_error.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
            ) =>
        {
            if stderr
                .iter()
                .any(|line| line.trim() == REMOTE_COMMAND_CONFLICT)
            {
                ConnectError::ProtocolCorrupted {
                    detail: format!(
                        "ssh: {REMOTE_COMMAND_CONFLICT} A RemoteCommand in the ssh \
                         configuration conflicts with SFTP"
                    ),
                }
            } else {
                ConnectError::SftpServerMissing {
                    detail: stderr_detail(stderr),
                }
            }
        }
        SftpError::RecursiveErrors(errors) => classify_start_error(&errors.original_error, stderr),
        SftpError::RecursiveErrors3(errors) => classify_start_error(&errors.err1, stderr),
        other => ConnectError::UnsupportedServer {
            detail: other.to_string(),
        },
    }
}

/// The last non-empty stderr lines, joined for a one-line message.
fn stderr_detail(stderr: &[String]) -> String {
    let lines: Vec<&str> = stderr
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return "the server closed the SFTP channel without a reply".into();
    }
    lines[lines.len().saturating_sub(STDERR_DETAIL_LINES)..].join(" / ")
}

/// A copy of the current stderr tail. A poisoned lock still holds valid
/// lines (they are only ever pushed and popped whole), so it is read anyway.
fn stderr_snapshot(stderr: &StderrTail) -> Vec<String> {
    let tail = match stderr.lock() {
        Ok(tail) => tail,
        Err(poisoned) => poisoned.into_inner(),
    };
    tail.iter().cloned().collect()
}

/// Object kinds as reported in SFTP attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawKind {
    File,
    Dir,
    Symlink,
    Special(SpecialKind),
}

fn raw_kind(meta: &MetaData) -> RawKind {
    let Some(file_type) = meta.file_type() else {
        return RawKind::Special(SpecialKind::Unknown);
    };
    if file_type.is_file() {
        RawKind::File
    } else if file_type.is_dir() {
        RawKind::Dir
    } else if file_type.is_symlink() {
        RawKind::Symlink
    } else if file_type.is_fifo() {
        RawKind::Special(SpecialKind::Fifo)
    } else if file_type.is_socket() {
        RawKind::Special(SpecialKind::Socket)
    } else if file_type.is_block_device() {
        RawKind::Special(SpecialKind::BlockDevice)
    } else if file_type.is_char_device() {
        RawKind::Special(SpecialKind::CharDevice)
    } else {
        RawKind::Special(SpecialKind::Unknown)
    }
}

/// An `Entry` from SFTP attributes; attributes the server omitted stay
/// `None`.
fn entry(name: OsString, kind: EntryKind, meta: &MetaData) -> Entry {
    Entry {
        name,
        kind,
        size: meta.len(),
        mtime: meta.modified().map(|time| i64::from(time.into_raw())),
        mode: meta
            .permissions()
            .map(|permissions| permissions.as_raw().bits() & 0o7777),
    }
}

/// The last component of `path`, or the whole path for `/`.
fn file_name(path: &Path) -> OsString {
    path.file_name().unwrap_or(path.as_os_str()).to_os_string()
}

/// SFTP version 3 stores times as unsigned 32-bit seconds; anything else is
/// refused rather than clamped.
fn sftp_time(seconds: i64, path: &Path) -> FsResult<UnixTimeStamp> {
    u32::try_from(seconds)
        .ok()
        .and_then(UnixTimeStamp::from_raw)
        .ok_or_else(|| FsError::Io {
            path: path.to_path_buf(),
            message: format!(
                "the time {seconds} is outside the range SFTP can store (1970 to 2106)"
            ),
        })
}

/// `file` positioned at `offset` (seeking an SFTP file only sets the offset
/// sent with the next request).
fn file_at(file: &File, offset: u64, path: &Path) -> Result<File, FsError> {
    let mut file = file.clone();
    Pin::new(&mut file)
        .start_seek(io::SeekFrom::Start(offset))
        .map_err(|error| FsError::from_io(&error, path))?;
    Ok(file)
}

/// One pipelined read: up to `CHUNK` bytes from one offset. A result shorter
/// than `CHUNK` means end of file.
type ReadChunk = BoxFuture<'static, FsResult<BytesMut>>;

/// Streaming reader with read-ahead: chunks are requested concurrently and
/// handed out in file order.
struct SftpReader {
    /// Template for the per-request clones; holds the remote handle open.
    file: File,
    path: PathBuf,
    lost: LostSignal,
    /// Offset of the next chunk to request.
    next_offset: u64,
    pending: FuturesOrdered<ReadChunk>,
    /// Bytes received and not yet handed to the caller.
    buffered: BytesMut,
    /// The end of the file was seen; nothing more is requested.
    at_eof: bool,
}

impl SftpReader {
    fn new(file: File, offset: u64, path: &Path, lost: LostSignal) -> SftpReader {
        SftpReader {
            file,
            path: path.to_path_buf(),
            lost,
            next_offset: offset,
            pending: FuturesOrdered::new(),
            buffered: BytesMut::new(),
            at_eof: false,
        }
    }

    /// Requests chunks until `READ_AHEAD` bytes are outstanding.
    fn fill_pipeline(&mut self) {
        while !self.at_eof && self.pending.len() * CHUNK < READ_AHEAD {
            let offset = self.next_offset;
            self.next_offset += CHUNK as u64;
            let file = self.file.clone();
            let path = self.path.clone();
            let lost = self.lost.clone();
            self.pending
                .push_back(read_chunk(file, offset, path, lost).boxed());
        }
    }
}

/// Reads `CHUNK` bytes at `offset`, repeating short reads until the chunk is
/// full or the file ends.
async fn read_chunk(
    file: File,
    offset: u64,
    path: PathBuf,
    lost: LostSignal,
) -> FsResult<BytesMut> {
    let mut data = BytesMut::with_capacity(CHUNK);
    while data.len() < CHUNK {
        let mut file = file_at(&file, offset + data.len() as u64, &path)?;
        // At most CHUNK (64 KiB), so it fits in u32.
        let wanted = (CHUNK - data.len()) as u32;
        let tail = data.split_off(data.len());
        match file.read(wanted, tail).await {
            Ok(Some(received)) if received.is_empty() => {
                return Err(FsError::Io {
                    path,
                    message: "the server answered a read with no data".into(),
                })
            }
            Ok(Some(received)) => data.unsplit(received),
            Ok(None) => break,
            Err(error) => return Err(lost.classify(&error, &path, "read")),
        }
    }
    Ok(data)
}

impl AsyncRead for SftpReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.buffered.is_empty() {
                let n = this.buffered.len().min(buf.remaining());
                buf.put_slice(&this.buffered.split_to(n));
                return Poll::Ready(Ok(()));
            }
            this.fill_pipeline();
            let Some(chunk) = ready!(this.pending.poll_next_unpin(cx)) else {
                // Nothing pending and nothing requested: end of file.
                return Poll::Ready(Ok(()));
            };
            match chunk {
                Ok(data) => {
                    if data.len() < CHUNK {
                        // Later chunks lie past the end; drop them.
                        this.at_eof = true;
                        this.pending = FuturesOrdered::new();
                    }
                    this.buffered = data;
                }
                Err(error) => return Poll::Ready(Err(this.lost.io_error(error))),
            }
        }
    }
}

/// One pipelined write; yields the number of bytes it carried.
type WriteChunk = BoxFuture<'static, (usize, FsResult<()>)>;

/// A chunk that finishes alone, yielding a safe request size if offered `CHUNK` bytes.
type WriteProbe = BoxFuture<'static, FsResult<Option<usize>>>;

/// Streaming writer with write-behind. The first full chunk learns a
/// safe request size before later writes are pipelined, one request each,
/// with at most `WRITE_BEHIND` bytes awaiting acknowledgement. `shutdown`
/// waits for all writes and closes the handle.
struct SftpWriter {
    /// The remote handle; `None` once closing started.
    file: Option<File>,
    path: PathBuf,
    lost: LostSignal,
    /// Offset of the next write.
    offset: u64,
    /// A payload size already acknowledged as one request.
    chunk_size: usize,
    probe: Option<WriteProbe>,
    pending: FuturesUnordered<WriteChunk>,
    /// Bytes sent and not yet acknowledged.
    in_flight: usize,
    closing: Option<BoxFuture<'static, FsResult<()>>>,
}

impl SftpWriter {
    fn new(file: File, offset: u64, path: &Path, lost: LostSignal) -> SftpWriter {
        SftpWriter {
            file: Some(file),
            path: path.to_path_buf(),
            lost,
            offset,
            chunk_size: 0,
            probe: None,
            pending: FuturesUnordered::new(),
            in_flight: 0,
            closing: None,
        }
    }

    /// Collects finished writes; `Ready` once `in_flight` is at most `limit`.
    fn poll_acknowledged(&mut self, cx: &mut Context<'_>, limit: usize) -> Poll<io::Result<()>> {
        loop {
            match self.pending.poll_next_unpin(cx) {
                Poll::Ready(Some((len, result))) => {
                    self.in_flight -= len;
                    if let Err(error) = result {
                        return Poll::Ready(Err(self.lost.io_error(error)));
                    }
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending if self.in_flight <= limit => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn shut_down_error(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("{} was already closed for writing", self.path.display()),
        )
    }

    /// Finishes a probe chunk before any later offsets are sent.
    fn poll_probe(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(probe) = &mut self.probe {
            let result = ready!(probe.poll_unpin(cx));
            self.probe = None;
            if let Some(size) = result.map_err(|error| self.lost.io_error(error))? {
                self.chunk_size = size;
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SftpWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        ready!(this.poll_probe(cx))?;
        if this.chunk_size == 0 {
            let Some(file) = &this.file else {
                return Poll::Ready(Err(this.shut_down_error()));
            };
            let mut file = file_at(file, this.offset, &this.path)
                .map_err(|error| this.lost.io_error(error))?;
            let len = buf.len().min(CHUNK);
            let data = buf[..len].to_vec();
            let path = this.path.clone();
            let lost = this.lost.clone();
            // Accept the owned chunk now so cancelling a pending caller
            // never consumes unreported bytes. Only probe chunks may span
            // multiple requests, and each finishes before later chunks.
            this.probe = Some(
                async move {
                    let written = file
                        .write(&data)
                        .await
                        .map_err(|error| lost.classify(&error, &path, "write"))?;
                    file.write_all(&data[written..])
                        .await
                        .map_err(|error| lost.classify(&error, &path, "write"))?;
                    Ok((len == CHUNK).then_some(written))
                }
                .boxed(),
            );
            this.offset += len as u64;
            return Poll::Ready(Ok(len));
        }
        // Wait for room before sending, so at most WRITE_BEHIND is unacked.
        ready!(this.poll_acknowledged(cx, WRITE_BEHIND - CHUNK))?;
        let Some(file) = &this.file else {
            return Poll::Ready(Err(this.shut_down_error()));
        };
        let mut file = match file_at(file, this.offset, &this.path) {
            Ok(file) => file,
            Err(error) => return Poll::Ready(Err(this.lost.io_error(error))),
        };
        // Each future must send one request, so later offsets cannot reach
        // the server before an earlier chunk's remaining split requests.
        let len = buf.len().min(this.chunk_size);
        let data = buf[..len].to_vec();
        let path = this.path.clone();
        let lost = this.lost.clone();
        this.pending.push(
            async move {
                let result = file
                    .write_all(&data)
                    .await
                    .map_err(|error| lost.classify(&error, &path, "write"));
                (len, result)
            }
            .boxed(),
        );
        this.offset += len as u64;
        this.in_flight += len;
        // Poll the new request once so it is queued for sending now; its
        // acknowledgement, or an earlier failure, is collected later.
        match this.poll_acknowledged(cx, usize::MAX) {
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) | Poll::Pending => Poll::Ready(Ok(len)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_probe(cx))?;
        this.poll_acknowledged(cx, 0)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut this = self;
        ready!(this.as_mut().poll_flush(cx))?;
        let this = this.get_mut();
        if this.closing.is_none() {
            let Some(file) = this.file.take() else {
                return Poll::Ready(Ok(()));
            };
            let path = this.path.clone();
            let lost = this.lost.clone();
            // Every request clone has finished, so this is the last
            // reference and `close` sends SSH_FXP_CLOSE and awaits it.
            this.closing = Some(
                async move {
                    file.close()
                        .await
                        .map_err(|error| lost.classify(&error, &path, "close"))
                }
                .boxed(),
            );
        }
        let closing = this.closing.as_mut().expect("closing was just set");
        let result = ready!(closing.poll_unpin(cx));
        this.closing = None;
        Poll::Ready(result.map_err(fs_io_error))
    }
}

/// Whether a `readdir` name is `.` or `..`, which listings omit.
fn is_dot_entry(name: &OsStr) -> bool {
    name == "." || name == ".."
}

#[cfg(test)]
mod tests {
    //! Tests run against a real OpenSSH `sftp-server` started locally on
    //! pipes (no network, no ssh), so every `Fs` method makes real SFTP
    //! round trips over a temporary directory. Tests that need the docker
    //! sshd fixtures are `#[ignore]`d; run `tests/it/fixtures/up.sh` and
    //! then `cargo test -- --ignored`.

    use super::*;

    use std::collections::{BTreeMap, VecDeque};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::Stdio;
    use std::sync::Mutex as StdMutex;

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;
    use tokio::sync::oneshot;

    /// Where OpenSSH installs `sftp-server` on macOS, Debian/Ubuntu,
    /// Fedora/RHEL, and Alpine/Arch.
    const SFTP_SERVER_CANDIDATES: [&str; 4] = [
        "/usr/libexec/sftp-server",
        "/usr/lib/openssh/sftp-server",
        "/usr/libexec/openssh/sftp-server",
        "/usr/lib/ssh/sftp-server",
    ];

    fn sftp_server() -> &'static str {
        SFTP_SERVER_CANDIDATES
            .into_iter()
            .find(|candidate| Path::new(candidate).is_file())
            .unwrap_or_else(|| {
                panic!(
                    "these tests need OpenSSH's sftp-server at one of {SFTP_SERVER_CANDIDATES:?} \
                     (install openssh-sftp-server)"
                )
            })
    }

    fn endpoint() -> Endpoint {
        Endpoint::Remote {
            host: "local-sftp-server".into(),
            sudo: false,
        }
    }

    /// Spawns `program` the way the session spawns channels: piped stdio,
    /// `kill_on_drop`, stderr drained into a bounded tail without line
    /// terminators.
    fn spawn(program: &str, args: &[&str]) -> RemoteChild {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn test child");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let stderr: StderrTail = Arc::new(StdMutex::new(VecDeque::new()));
        let tail = Arc::clone(&stderr);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr_pipe).lines();
            while let Some(line) = lines.next_line().await.expect("read child stderr") {
                let mut tail = tail.lock().expect("stderr tail lock");
                if tail.len() == 64 {
                    tail.pop_front();
                }
                tail.push_back(line.trim_end_matches('\r').to_owned());
            }
        });
        RemoteChild {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    async fn local_fs() -> SftpFs {
        SftpFs::start(endpoint(), spawn(sftp_server(), &[]))
            .await
            .expect("start local sftp-server")
    }

    /// Reads one length-prefixed SSH string from the front of `rest`.
    fn take_string(rest: &mut &[u8]) -> Vec<u8> {
        let len = u32::from_be_bytes(rest[..4].try_into().expect("length")) as usize;
        let value = rest[4..4 + len].to_vec();
        *rest = &rest[4 + len..];
        value
    }

    /// Splits an `SSH_FXP_VERSION` body into its extension pairs, dropping
    /// those named in `drop`. Returns the rewritten body and every
    /// advertised name.
    fn rewrite_hello(body: &[u8], drop: &[&str]) -> (Vec<u8>, Vec<String>) {
        assert_eq!(body[0], 2, "first packet is SSH_FXP_VERSION");
        let mut kept = body[..5].to_vec();
        let mut names = Vec::new();
        let mut rest = &body[5..];
        while !rest.is_empty() {
            let name = take_string(&mut rest);
            let data = take_string(&mut rest);
            let name_text = String::from_utf8(name.clone()).expect("ASCII extension name");
            if !drop.contains(&name_text.as_str()) {
                for field in [&name, &data] {
                    kept.extend_from_slice(&(field.len() as u32).to_be_bytes());
                    kept.extend_from_slice(field);
                }
            }
            names.push(name_text);
        }
        (kept, names)
    }

    /// A real local sftp-server whose hello reaches the client without the
    /// extensions in `drop`, as an older server would send it. Also returns
    /// every extension the server actually advertised.
    async fn filtered_fs(drop: &'static [&'static str]) -> (SftpFs, Vec<String>) {
        let RemoteChild {
            child,
            stdin,
            mut stdout,
            stderr,
        } = spawn(sftp_server(), &[]);
        let (mut near, far) = tokio::io::duplex(256 * 1024);
        let (names_tx, names_rx) = oneshot::channel();
        // The forwarding task's outcome is not inspected: it ends with an
        // error once the client side drops, and tests observe everything
        // through the `SftpFs`.
        tokio::spawn(async move {
            let len = stdout.read_u32().await? as usize;
            let mut body = vec![0; len];
            stdout.read_exact(&mut body).await?;
            let (kept, names) = rewrite_hello(&body, drop);
            names_tx.send(names).expect("test awaits the names");
            near.write_u32(kept.len() as u32).await?;
            near.write_all(&kept).await?;
            tokio::io::copy(&mut stdout, &mut near).await
        });
        let fs = SftpFs::start_on(endpoint(), child, stdin, far, &stderr)
            .await
            .expect("start filtered sftp-server");
        (fs, names_rx.await.expect("hello names"))
    }

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        names.sort();
        names
    }

    fn sorted_names(entries: &[Entry]) -> Vec<String> {
        let mut names: Vec<String> = entries
            .iter()
            .map(|entry| entry.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Deterministic non-repeating test data.
    fn pattern(len: usize) -> Vec<u8> {
        let mut state: u32 = 0x9e37_79b9;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    async fn write_file(fs: &SftpFs, path: &Path, mode: WriteMode, data: &[u8]) {
        let mut writer = fs.open_write(path, mode).await.expect("open_write");
        writer.write_all(data).await.expect("write");
        writer.shutdown().await.expect("shutdown commits");
    }

    async fn read_file(fs: &SftpFs, path: &Path, offset: u64) -> Vec<u8> {
        let mut reader = fs.open_read(path, offset).await.expect("open_read");
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.expect("read");
        data
    }

    #[tokio::test]
    async fn capabilities_follow_the_advertised_extensions() {
        let (fs, names) = filtered_fs(&[]).await;
        let capabilities = fs.capabilities();
        assert_eq!(
            capabilities.posix_rename,
            names.iter().any(|name| name == "posix-rename@openssh.com")
        );
        assert_eq!(
            capabilities.copy_within,
            names.iter().any(|name| name == "copy-data")
        );
        assert!(capabilities.symlink);
        assert!(!capabilities.trash);
        assert_eq!(fs.endpoint(), &endpoint());

        let (fs, _) = filtered_fs(&["posix-rename@openssh.com", "copy-data"]).await;
        assert!(!fs.capabilities().posix_rename);
        assert!(!fs.capabilities().copy_within);
    }

    #[tokio::test]
    async fn home_is_the_server_working_directory() {
        let fs = local_fs().await;
        let expected = std::env::current_dir()
            .and_then(|dir| dir.canonicalize())
            .expect("cwd");
        assert_eq!(fs.home().await.expect("home"), expected);
    }

    #[tokio::test]
    async fn canonicalize_resolves_dots_and_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().canonicalize().expect("canonical tempdir");
        std::fs::create_dir(real.join("sub")).expect("mkdir");
        std::os::unix::fs::symlink(real.join("sub"), real.join("link")).expect("symlink");
        let fs = local_fs().await;
        let resolved = fs
            .canonicalize(&dir.path().join("link/../sub/."))
            .await
            .expect("canonicalize");
        assert_eq!(resolved, real.join("sub"));
        let missing = real.join("missing/deeper");
        assert_eq!(
            fs.canonicalize(&missing).await,
            Err(FsError::NotFound { path: missing })
        );
    }

    #[tokio::test]
    async fn list_omits_dots_and_describes_every_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("file.txt"), b"hello").expect("write");
        std::fs::set_permissions(
            root.join("file.txt"),
            std::fs::Permissions::from_mode(0o640),
        )
        .expect("chmod");
        std::fs::create_dir(root.join("dir")).expect("mkdir");
        std::os::unix::fs::symlink("dir", root.join("to-dir")).expect("symlink");
        std::os::unix::fs::symlink("file.txt", root.join("to-file")).expect("symlink");
        std::os::unix::fs::symlink("nowhere", root.join("dangling")).expect("symlink");
        let fifo = std::ffi::CString::new(root.join("pipe").into_os_string().into_encoded_bytes())
            .expect("path without NUL");
        // SAFETY: `fifo` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "mkfifo");
        // A non-ASCII name with a space round-trips byte for byte.
        let odd_name = "\u{7a7a}\u{767d} \u{6a94}\u{6848}.txt";
        std::fs::write(root.join(odd_name), b"x").expect("write");

        let fs = local_fs().await;
        let entries = fs.list(root).await.expect("list");
        assert_eq!(
            sorted_names(&entries),
            sorted(&["dangling", "dir", "file.txt", "pipe", "to-dir", "to-file", odd_name])
        );
        let by_name: BTreeMap<String, Entry> = entries
            .into_iter()
            .map(|entry| (entry.name.to_string_lossy().into_owned(), entry))
            .collect();

        let file = &by_name["file.txt"];
        let local = std::fs::symlink_metadata(root.join("file.txt")).expect("stat");
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.size, Some(5));
        assert_eq!(file.mode, Some(0o640));
        assert_eq!(file.mtime, Some(local.mtime()));

        assert_eq!(by_name["dir"].kind, EntryKind::Dir);
        assert_eq!(
            by_name["to-dir"].kind,
            EntryKind::Symlink {
                target: "dir".into(),
                target_is_dir: true
            }
        );
        assert!(by_name["to-dir"].is_enterable());
        assert_eq!(
            by_name["to-file"].kind,
            EntryKind::Symlink {
                target: "file.txt".into(),
                target_is_dir: false
            }
        );
        assert_eq!(
            by_name["dangling"].kind,
            EntryKind::Symlink {
                target: "nowhere".into(),
                target_is_dir: false
            }
        );
        assert_eq!(by_name["pipe"].kind, EntryKind::Special(SpecialKind::Fifo));
    }

    #[tokio::test]
    async fn list_errors_are_classified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = local_fs().await;
        let missing = dir.path().join("missing");
        assert_eq!(
            fs.list(&missing).await,
            Err(FsError::NotFound { path: missing })
        );
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let result = fs.list(&locked).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert_eq!(result, Err(FsError::PermissionDenied { path: locked }));
    }

    #[tokio::test]
    async fn stat_follows_symlinks_and_lstat_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir(root.join("dir")).expect("mkdir");
        std::os::unix::fs::symlink("dir", root.join("link")).expect("symlink");
        let fs = local_fs().await;

        let followed = fs.stat(&root.join("link")).await.expect("stat");
        assert_eq!(followed.name, OsString::from("link"));
        assert_eq!(followed.kind, EntryKind::Dir);

        let link = fs.lstat(&root.join("link")).await.expect("lstat");
        assert_eq!(
            link.kind,
            EntryKind::Symlink {
                target: "dir".into(),
                target_is_dir: true
            }
        );
        let missing = root.join("nope");
        assert_eq!(
            fs.stat(&missing).await,
            Err(FsError::NotFound { path: missing })
        );
        assert_eq!(fs.lstat(Path::new("/")).await.expect("root").name, "/");
    }

    #[tokio::test]
    async fn large_files_round_trip_through_pipelined_streams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.bin");
        // Not a multiple of CHUNK, and larger than READ_AHEAD/WRITE_BEHIND.
        let data = pattern(5 * 1024 * 1024 + 12_345);
        let fs = local_fs().await;

        write_file(
            &fs,
            &path,
            WriteMode::Truncate { permissions: 0o644 },
            &data,
        )
        .await;
        assert_eq!(std::fs::read(&path).expect("local read"), data);
        assert_eq!(read_file(&fs, &path, 0).await, data);
        let offset = 3 * CHUNK + 7;
        assert_eq!(read_file(&fs, &path, offset as u64).await, &data[offset..]);
        assert!(read_file(&fs, &path, data.len() as u64 + 10)
            .await
            .is_empty());

        // Truncate replaces longer content entirely.
        write_file(
            &fs,
            &path,
            WriteMode::Truncate { permissions: 0o644 },
            b"short",
        )
        .await;
        assert_eq!(std::fs::read(&path).expect("local read"), b"short");
    }

    #[tokio::test]
    async fn initial_chunk_survives_cancelled_next_write_and_flushes_before_larger_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("short-first.bin");
        let fs = local_fs().await;
        let data = pattern(3 * CHUNK + 7);
        let mut writer = fs
            .open_write(&path, WriteMode::Truncate { permissions: 0o600 })
            .await
            .expect("open_write");
        writer.write_all(&data[..4093]).await.expect("short write");
        // This runtime has not driven the client's response task since the
        // initial chunk was accepted. Cancel the next write while it waits
        // for that chunk; none of this second buffer may reach the file.
        assert!(writer.write_all(b"cancelled").now_or_never().is_none());
        writer.flush().await.expect("flush initial chunk");
        assert_eq!(std::fs::read(&path).expect("flushed prefix"), data[..4093]);
        writer.write_all(&data[4093..]).await.expect("larger write");
        writer.shutdown().await.expect("shutdown commits");
        assert_eq!(std::fs::read(&path).expect("complete file"), data);
    }

    #[tokio::test]
    async fn short_first_writes_do_not_limit_later_pipelined_chunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("short-then-large.bin");
        let fs = local_fs().await;
        let data = pattern(WRITE_BEHIND + 3 * CHUNK + 7);
        let mut writer = fs
            .open_write(&path, WriteMode::Truncate { permissions: 0o600 })
            .await
            .expect("open_write");
        let mut offset = 0;
        for len in [4093, 17] {
            writer
                .write_all(&data[offset..offset + len])
                .await
                .expect("short write");
            offset += len;
            writer.flush().await.expect("flush short write");
            assert_eq!(
                std::fs::read(&path).expect("flushed prefix"),
                data[..offset]
            );
        }
        let accepted = writer.write(&data[offset..]).await.expect("full probe");
        assert_eq!(accepted, CHUNK);
        offset += accepted;
        let accepted = writer
            .write(&data[offset..])
            .await
            .expect("pipelined write");
        assert!(accepted > 4093 && accepted <= CHUNK);
        offset += accepted;
        writer
            .write_all(&data[offset..])
            .await
            .expect("remaining writes");
        writer.shutdown().await.expect("shutdown commits");
        assert_eq!(std::fs::read(&path).expect("complete file"), data);
    }

    #[tokio::test]
    async fn truncate_creates_restricted_files_and_keeps_existing_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("id_rsa.scpto-part");
        let fs = local_fs().await;
        let mut writer = fs
            .open_write(&path, WriteMode::Truncate { permissions: 0o600 })
            .await
            .expect("open_write");
        // The bits are in place before any data is written.
        let mode =
            |path: &Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o7777;
        assert_eq!(mode(&path), 0o600);
        writer.write_all(b"key").await.expect("write");
        writer.shutdown().await.expect("shutdown");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        write_file(
            &fs,
            &path,
            WriteMode::Truncate {
                permissions: 0o4600,
            },
            b"new",
        )
        .await;
        assert_eq!(mode(&path), 0o640, "an existing file keeps its mode");
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
    }

    #[tokio::test]
    async fn resume_keeps_the_prefix_and_writes_from_the_offset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("part");
        let data = pattern(300_000);
        std::fs::write(&path, &data[..100_000]).expect("seed prefix");
        let fs = local_fs().await;
        write_file(
            &fs,
            &path,
            WriteMode::Resume { offset: 100_000 },
            &data[100_000..],
        )
        .await;
        assert_eq!(std::fs::read(&path).expect("local read"), data);

        // Resume never creates a file.
        let missing = dir.path().join("missing");
        assert_eq!(
            fs.open_write(&missing, WriteMode::Resume { offset: 0 })
                .await
                .err(),
            Some(FsError::NotFound { path: missing })
        );
    }

    #[tokio::test]
    async fn writer_after_shutdown_refuses_more_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f");
        let fs = local_fs().await;
        let mut writer = fs
            .open_write(&path, WriteMode::Truncate { permissions: 0o644 })
            .await
            .expect("open_write");
        writer.write_all(b"abc").await.expect("write");
        writer.shutdown().await.expect("shutdown");
        writer.shutdown().await.expect("second shutdown is a no-op");
        let error = writer.write_all(b"more").await.expect_err("closed");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(std::fs::read(&path).expect("read"), b"abc");
    }

    #[tokio::test]
    async fn open_read_of_a_missing_file_is_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = local_fs().await;
        let missing = dir.path().join("missing");
        assert_eq!(
            fs.open_read(&missing, 0).await.err(),
            Some(FsError::NotFound { path: missing })
        );
    }

    #[tokio::test]
    async fn mkdir_classifies_existing_and_missing_parents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = local_fs().await;
        let new = dir.path().join("new");
        fs.mkdir(&new).await.expect("mkdir");
        assert!(new.is_dir());
        assert_eq!(
            fs.mkdir(&new).await,
            Err(FsError::AlreadyExists { path: new.clone() })
        );
        let orphan = dir.path().join("no/parent");
        assert_eq!(
            fs.mkdir(&orphan).await,
            Err(FsError::NotFound { path: orphan })
        );
    }

    #[tokio::test]
    async fn rename_replaces_an_existing_file_atomically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("a"), b"new").expect("write");
        std::fs::write(root.join("b"), b"old").expect("write");
        let fs = local_fs().await;
        assert!(fs.capabilities().posix_rename);
        fs.rename(&root.join("a"), &root.join("b"))
            .await
            .expect("rename");
        assert!(!root.join("a").exists());
        assert_eq!(std::fs::read(root.join("b")).expect("read"), b"new");

        // A non-empty directory is never replaced.
        std::fs::create_dir_all(root.join("d1")).expect("mkdir");
        std::fs::create_dir_all(root.join("d2/inner")).expect("mkdir");
        assert_eq!(
            fs.rename(&root.join("d1"), &root.join("d2")).await,
            Err(FsError::AlreadyExists {
                path: root.join("d2")
            })
        );
        let missing = root.join("missing");
        assert_eq!(
            fs.rename(&missing, &root.join("x")).await,
            Err(FsError::NotFound { path: missing })
        );
    }

    #[tokio::test]
    async fn rename_without_posix_rename_removes_the_target_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (fs, advertised) = filtered_fs(&["posix-rename@openssh.com"]).await;
        assert!(advertised
            .iter()
            .any(|name| name == "posix-rename@openssh.com"));
        assert!(!fs.capabilities().posix_rename);

        std::fs::write(root.join("a.scpto-part"), b"new").expect("write");
        std::fs::write(root.join("a"), b"old").expect("write");
        fs.rename(&root.join("a.scpto-part"), &root.join("a"))
            .await
            .expect("rename onto an existing file");
        assert!(!root.join("a.scpto-part").exists());
        assert_eq!(std::fs::read(root.join("a")).expect("read"), b"new");

        // Renaming to a free name needs no removal.
        fs.rename(&root.join("a"), &root.join("c"))
            .await
            .expect("plain rename");
        assert_eq!(std::fs::read(root.join("c")).expect("read"), b"new");

        // A directory target is never removed.
        std::fs::create_dir(root.join("dir")).expect("mkdir");
        assert_eq!(
            fs.rename(&root.join("c"), &root.join("dir")).await,
            Err(FsError::AlreadyExists {
                path: root.join("dir")
            })
        );
        assert!(root.join("dir").is_dir());
        assert!(root.join("c").exists());
    }

    #[tokio::test]
    async fn removal_and_attribute_changes_reach_the_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let file = root.join("f");
        std::fs::write(&file, b"x").expect("write");
        std::fs::create_dir_all(root.join("full/inner")).expect("mkdir");
        std::fs::create_dir(root.join("empty")).expect("mkdir");
        let fs = local_fs().await;

        fs.chmod(&file, 0o100_604).await.expect("chmod");
        let mode = std::fs::metadata(&file).expect("stat").mode();
        assert_eq!(mode & 0o7777, 0o604);

        fs.set_times(&file, 1_000_000_000, 1_234_567_890)
            .await
            .expect("set_times");
        let meta = std::fs::metadata(&file).expect("stat");
        assert_eq!(meta.mtime(), 1_234_567_890);
        assert_eq!(meta.atime(), 1_000_000_000);
        assert!(matches!(
            fs.set_times(&file, 0, -1).await,
            Err(FsError::Io { .. })
        ));
        assert!(matches!(
            fs.set_times(&file, 0, i64::from(u32::MAX) + 1).await,
            Err(FsError::Io { .. })
        ));

        fs.remove_file(&file).await.expect("remove_file");
        assert!(!file.exists());
        assert_eq!(
            fs.remove_file(&file).await,
            Err(FsError::NotFound { path: file.clone() })
        );
        fs.remove_dir(&root.join("empty")).await.expect("rmdir");
        assert!(!root.join("empty").exists());
        assert!(matches!(
            fs.remove_dir(&root.join("full")).await,
            Err(FsError::Io { .. })
        ));
        assert!(root.join("full").is_dir());
    }

    #[tokio::test]
    async fn symlink_points_link_at_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let fs = local_fs().await;
        fs.symlink(Path::new("target.txt"), &root.join("link"))
            .await
            .expect("symlink");
        assert_eq!(
            std::fs::read_link(root.join("link")).expect("local readlink"),
            PathBuf::from("target.txt")
        );
        assert_eq!(
            fs.read_link(&root.join("link")).await.expect("read_link"),
            PathBuf::from("target.txt")
        );
        let absent = root.join("absent");
        assert_eq!(
            fs.read_link(&absent).await,
            Err(FsError::NotFound { path: absent })
        );
    }

    #[tokio::test]
    async fn trash_is_unsupported_remotely() {
        let fs = local_fs().await;
        assert!(matches!(
            fs.trash(Path::new("/tmp")).await,
            Err(FsError::Unsupported { .. })
        ));
    }

    #[tokio::test]
    async fn copy_within_needs_copy_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let data = pattern(200_000);
        std::fs::write(root.join("src"), &data).expect("write");

        let (fs, advertised) = filtered_fs(&[]).await;
        // Servers before OpenSSH 9.0 lack copy-data; the docker legacy
        // fixture covers that case, this one the server copy itself.
        if advertised.iter().any(|name| name == "copy-data") {
            fs.copy_within(&root.join("src"), &root.join("dst"))
                .await
                .expect("copy_within");
            assert_eq!(std::fs::read(root.join("dst")).expect("read"), data);
        }

        let (fs, _) = filtered_fs(&["copy-data"]).await;
        assert!(matches!(
            fs.copy_within(&root.join("src"), &root.join("other")).await,
            Err(FsError::Unsupported { .. })
        ));
        assert!(!root.join("other").exists());
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn copy_file_restricts_the_part_while_copying_and_after_cancel() {
        use crate::transfer::stream::test_support::MemFs;
        use crate::transfer::stream::{copy_file, CopySpec};

        let dir = tempfile::tempdir().expect("tempdir");
        let destination = dir.path().join("id_rsa");
        let part = dir.path().join("id_rsa.scpto-part");
        let source = MemFs::new(Endpoint::Local);
        source.add_file("/id_rsa", b"0123456789", 5, 0o600);
        let _gate = source.gate_reads("/id_rsa", 4);
        let fs = local_fs().await;
        let copy = copy_file(
            CopySpec {
                source: &*source,
                source_path: Path::new("/id_rsa"),
                destination: &fs,
                destination_path: &destination,
                size: 10,
                mtime: Some(1_700_000_000),
                mode: Some(0o600),
                resume_from: 0,
            },
            &|_| {},
        );
        let mut copy = Box::pin(copy);
        tokio::select! {
            result = copy.as_mut() => panic!("copy finished unexpectedly: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        assert_eq!(mode_of(&part), 0o600, "while copying");
        drop(copy);
        assert_eq!(mode_of(&part), 0o600, "after cancel");
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn copy_file_restricts_the_part_before_a_server_side_copy() {
        use crate::transfer::stream::{copy_file, CopySpec};

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("secret"), b"0123").expect("write");
        std::fs::set_permissions(root.join("secret"), std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
        // A directory at the destination makes the final rename fail, so
        // the part the server filled stays behind for inspection.
        std::fs::create_dir(root.join("copy")).expect("mkdir");
        let fs = local_fs().await;
        if !fs.capabilities().copy_within {
            let reason = "local sftp-server does not advertise copy-data; server-side part-mode test cannot run";
            eprintln!("{reason}");
            assert!(
                std::env::var_os("SCPTO_EXPECT_COPY_DATA").as_deref()
                    != Some(std::ffi::OsStr::new("1")),
                "{reason}; SCPTO_EXPECT_COPY_DATA=1 requires this capability"
            );
            return;
        }
        let error = copy_file(
            CopySpec {
                source: &fs,
                source_path: &root.join("secret"),
                destination: &fs,
                destination_path: &root.join("copy"),
                size: 4,
                mtime: None,
                mode: Some(0o600),
                resume_from: 0,
            },
            &|_| {},
        )
        .await
        .expect_err("rename onto a directory fails");
        assert!(!matches!(error, FsError::Disconnected), "{error:?}");
        let part = root.join("copy.scpto-part");
        assert_eq!(std::fs::read(&part).expect("part kept"), b"0123");
        assert_eq!(mode_of(&part), 0o600);
    }

    #[tokio::test]
    async fn a_killed_channel_is_disconnected_everywhere() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f");
        std::fs::write(&path, pattern(5 * 1024 * 1024)).expect("write");
        let channel = spawn(sftp_server(), &[]);
        let pid = libc::pid_t::try_from(channel.child.id().expect("running child"))
            .expect("pid fits pid_t");
        let fs = SftpFs::start(endpoint(), channel).await.expect("start");
        let mut reader = fs.open_read(&path, 0).await.expect("open_read");

        // SAFETY: plain kill(2) on the child this test spawned.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        tokio::time::timeout(Duration::from_secs(5), fs.closed())
            .await
            .expect("closed resolves once the child is gone");

        assert_eq!(fs.list(dir.path()).await, Err(FsError::Disconnected));
        let mut sink = Vec::new();
        let error = reader
            .read_to_end(&mut sink)
            .await
            .expect_err("reader fails");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<FsError>()),
            Some(&FsError::Disconnected)
        );
    }

    #[tokio::test]
    async fn closed_resolves_on_a_failed_operation_while_the_child_lives() {
        let RemoteChild {
            child,
            stdin,
            mut stdout,
            stderr,
        } = spawn(sftp_server(), &[]);
        let (mut near, far) = tokio::io::duplex(64 * 1024);
        let forward = tokio::spawn(async move { tokio::io::copy(&mut stdout, &mut near).await });
        let fs = SftpFs::start_on(endpoint(), child, stdin, far, &stderr)
            .await
            .expect("start");
        // Cutting the response stream fails the client's read task while
        // the sftp-server child itself keeps running.
        forward.abort();
        assert_eq!(fs.list(Path::new("/")).await, Err(FsError::Disconnected));
        tokio::time::timeout(Duration::from_secs(5), fs.closed())
            .await
            .expect("closed resolves after BackgroundTaskFailure");
    }

    #[tokio::test]
    async fn start_reports_shell_output_as_protocol_corruption() {
        let script = format!("echo NOISY-STARTUP-OUTPUT; exec {}", sftp_server());
        let error = SftpFs::start(endpoint(), spawn("sh", &["-c", &script]))
            .await
            .err()
            .expect("corrupted hello");
        assert!(
            matches!(error, ConnectError::ProtocolCorrupted { ref detail } if detail.contains("hello")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn start_names_the_remote_command_conflict() {
        let script = format!("echo '{REMOTE_COMMAND_CONFLICT}' >&2; exit 255");
        let error = SftpFs::start(endpoint(), spawn("sh", &["-c", &script]))
            .await
            .err()
            .expect("conflict");
        assert!(
            matches!(error, ConnectError::ProtocolCorrupted { ref detail } if detail.contains("RemoteCommand")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn start_reports_a_missing_subsystem_with_stderr() {
        let script = "echo 'subsystem request failed on channel 0' >&2; exit 255";
        let error = SftpFs::start(endpoint(), spawn("sh", &["-c", script]))
            .await
            .err()
            .expect("no sftp");
        assert_eq!(
            error,
            ConnectError::SftpServerMissing {
                detail: "subsystem request failed on channel 0".into()
            }
        );
    }

    #[test]
    fn transport_failures_classify_as_disconnected() {
        let path = Path::new("/x");
        for error in [
            SftpError::BackgroundTaskFailure(&"read/flush task failed"),
            SftpError::IOError(io::Error::from(io::ErrorKind::BrokenPipe)),
        ] {
            assert_eq!(classify_error(&error, path, "list"), FsError::Disconnected);
        }
        assert_eq!(
            classify_error(&SftpError::UnsupportedExtension(&"copy-data"), path, "copy"),
            FsError::Unsupported {
                operation: "copy".into(),
                reason: "the server does not offer the copy-data extension".into()
            }
        );
        assert!(matches!(
            classify_error(&SftpError::HandleTooLong, path, "open"),
            FsError::Io { .. }
        ));
    }

    #[test]
    fn lost_signal_latches_on_disconnect_only() {
        let lost = LostSignal::new();
        let receiver = lost.subscribe();
        lost.classify(&SftpError::HandleTooLong, Path::new("/x"), "open");
        assert!(!*receiver.borrow());
        lost.classify(
            &SftpError::BackgroundTaskFailure(&"read/flush task failed"),
            Path::new("/x"),
            "open",
        );
        assert!(*receiver.borrow());
    }

    #[test]
    fn start_errors_follow_the_classification_table() {
        let eof = || SftpError::IOError(io::Error::from(io::ErrorKind::UnexpectedEof));
        assert_eq!(
            classify_start_error(&SftpError::BackgroundTaskFailure(&"gone"), &[]),
            ConnectError::ConnectionLost
        );
        assert!(matches!(
            classify_start_error(&SftpError::SftpServerHelloMsgTooLong { len: 1 }, &[]),
            ConnectError::ProtocolCorrupted { .. }
        ));
        assert_eq!(
            classify_start_error(&eof(), &[]),
            ConnectError::SftpServerMissing {
                detail: "the server closed the SFTP channel without a reply".into()
            }
        );
        let recursive =
            SftpError::RecursiveErrors(Box::new(openssh_sftp_client::error::RecursiveError {
                original_error: eof(),
                occuring_error: SftpError::IOError(io::Error::from(io::ErrorKind::BrokenPipe)),
            }));
        assert!(matches!(
            classify_start_error(&recursive, &[format!("{REMOTE_COMMAND_CONFLICT}  ")]),
            ConnectError::ProtocolCorrupted { .. }
        ));
        assert!(matches!(
            classify_start_error(&SftpError::UnsupportedSftpProtocol { version: 6 }, &[]),
            ConnectError::UnsupportedServer { .. }
        ));
    }

    #[test]
    fn stderr_detail_keeps_the_last_non_empty_lines() {
        let mut lines: Vec<String> = (1..=8).map(|n| format!("line {n}")).collect();
        lines.push("   ".into());
        assert_eq!(
            stderr_detail(&lines),
            "line 4 / line 5 / line 6 / line 7 / line 8"
        );
    }

    #[test]
    fn sftp_times_are_unsigned_32_bit() {
        let path = Path::new("/x");
        assert_eq!(sftp_time(0, path).expect("epoch").into_raw(), 0);
        assert_eq!(
            sftp_time(i64::from(u32::MAX), path)
                .expect("2106")
                .into_raw(),
            u32::MAX
        );
        assert!(sftp_time(-1, path).is_err());
        assert!(sftp_time(i64::from(u32::MAX) + 1, path).is_err());
    }

    #[test]
    fn io_errors_carry_the_classified_error() {
        let error = fs_io_error(FsError::NotFound { path: "/a".into() });
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<FsError>()),
            Some(&FsError::NotFound { path: "/a".into() })
        );
    }

    /// Against the docker sshd fixtures (`tests/it/fixtures/up.sh`).
    mod fixtures {
        use super::*;

        fn ssh_channel(host: &str, common_options: bool) -> RemoteChild {
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/it/ssh_config");
            assert!(
                config.is_file(),
                "run tests/it/fixtures/up.sh first ({} is missing)",
                config.display()
            );
            let config = config.to_string_lossy().into_owned();
            let mut args = vec!["-F", config.as_str(), "-o", "BatchMode=yes"];
            if common_options {
                args.extend([
                    "-o",
                    "RemoteCommand=none",
                    "-o",
                    "RequestTTY=no",
                    "-o",
                    "ClearAllForwardings=yes",
                ]);
            }
            args.extend(["-s", host, "sftp"]);
            spawn("ssh", &args)
        }

        async fn fixture_fs(host: &str) -> SftpFs {
            SftpFs::start(endpoint(), ssh_channel(host, true))
                .await
                .expect("start fixture channel")
        }

        async fn check_seed(fs: &SftpFs) -> PathBuf {
            let seed = fs.home().await.expect("home").join("seed");
            let entries = fs.list(&seed).await.expect("list seed");
            let cjk = "\u{7a7a}\u{767d} \u{6a94}\u{6848} \u{6e2c}\u{8a66}.txt";
            assert_eq!(
                sorted_names(&entries),
                sorted(&[
                    "hello.txt",
                    "link-to-dir",
                    "link-to-file",
                    "linkdir-target",
                    "nested",
                    "pixel.png",
                    cjk,
                ])
            );
            let link = entries
                .iter()
                .find(|entry| entry.name == "link-to-dir")
                .expect("link-to-dir");
            assert_eq!(
                link.kind,
                EntryKind::Symlink {
                    target: "linkdir-target".into(),
                    target_is_dir: true
                }
            );
            assert_eq!(
                read_file(fs, &seed.join("hello.txt"), 0).await,
                b"hello scpto\n"
            );
            seed
        }

        async fn check_round_trip(fs: &SftpFs) {
            let work = fs.home().await.expect("home").join("vfs-sftp-it");
            if fs.lstat(&work).await.is_ok() {
                for entry in fs.list(&work).await.expect("list work") {
                    fs.remove_file(&work.join(&entry.name))
                        .await
                        .expect("clean work");
                }
                fs.remove_dir(&work).await.expect("remove stale work");
            }
            fs.mkdir(&work).await.expect("mkdir work");
            let data = pattern(3 * 1024 * 1024 + 99);
            let part = work.join("big.scpto-part");
            write_file(
                fs,
                &part,
                WriteMode::Truncate { permissions: 0o600 },
                &data[..1_000_000],
            )
            .await;
            // The part was created with the requested bits.
            assert_eq!(fs.lstat(&part).await.expect("part").mode, Some(0o600));
            write_file(
                fs,
                &part,
                WriteMode::Resume { offset: 1_000_000 },
                &data[1_000_000..],
            )
            .await;
            write_file(
                fs,
                &work.join("big"),
                WriteMode::Truncate { permissions: 0o644 },
                b"old",
            )
            .await;
            fs.rename(&part, &work.join("big")).await.expect("rename");
            assert_eq!(read_file(fs, &work.join("big"), 0).await, data);
            fs.remove_file(&work.join("big")).await.expect("remove");
            fs.remove_dir(&work).await.expect("rmdir");
        }

        #[tokio::test]
        #[ignore = "needs the docker sshd fixtures (tests/it/fixtures/up.sh)"]
        async fn modern_server_offers_every_extension() {
            let fs = fixture_fs("it-modern-plain").await;
            assert!(fs.capabilities().posix_rename);
            assert!(fs.capabilities().copy_within);
            let seed = check_seed(&fs).await;
            check_round_trip(&fs).await;
            let copy = seed.join("hello-copy.txt");
            fs.copy_within(&seed.join("hello.txt"), &copy)
                .await
                .expect("copy-data");
            assert_eq!(read_file(&fs, &copy, 0).await, b"hello scpto\n");
            fs.remove_file(&copy).await.expect("remove copy");
        }

        #[tokio::test]
        #[ignore = "needs the docker sshd fixtures (tests/it/fixtures/up.sh)"]
        async fn legacy_server_works_without_copy_data() {
            let fs = fixture_fs("it-legacy-plain").await;
            assert!(fs.capabilities().posix_rename);
            assert!(!fs.capabilities().copy_within);
            let seed = check_seed(&fs).await;
            check_round_trip(&fs).await;
            assert!(matches!(
                fs.copy_within(&seed.join("hello.txt"), &seed.join("copy"))
                    .await,
                Err(FsError::Unsupported { .. })
            ));
        }

        #[tokio::test]
        #[ignore = "needs the docker sshd fixtures (tests/it/fixtures/up.sh)"]
        async fn noisy_shell_startup_is_protocol_corruption() {
            for host in ["it-modern-noisy", "it-legacy-noisy"] {
                let error = SftpFs::start(endpoint(), ssh_channel(host, true))
                    .await
                    .err()
                    .expect("noisy hello");
                assert!(
                    matches!(error, ConnectError::ProtocolCorrupted { .. }),
                    "{host}: {error:?}"
                );
            }
        }

        #[tokio::test]
        #[ignore = "needs the docker sshd fixtures (tests/it/fixtures/up.sh)"]
        async fn configured_remote_command_is_named() {
            let error = SftpFs::start(endpoint(), ssh_channel("it-modern-sideeffects", false))
                .await
                .err()
                .expect("RemoteCommand conflict");
            assert!(
                matches!(error, ConnectError::ProtocolCorrupted { ref detail } if detail.contains("RemoteCommand")),
                "{error:?}"
            );
        }

        #[tokio::test]
        #[ignore = "needs the docker sshd fixtures (tests/it/fixtures/up.sh)"]
        async fn stopping_the_session_disconnects() {
            let channel = ssh_channel("it-modern-plain", true);
            let pid = libc::pid_t::try_from(channel.child.id().expect("running ssh"))
                .expect("pid fits pid_t");
            let fs = SftpFs::start(endpoint(), channel).await.expect("start");
            // SAFETY: plain kill(2) on the ssh child this test spawned.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
            tokio::time::timeout(Duration::from_secs(5), fs.closed())
                .await
                .expect("closed resolves");
            assert_eq!(fs.home().await, Err(FsError::Disconnected));
        }
    }
}
