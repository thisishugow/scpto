//! Single-file stream copy from any `Fs` to any `Fs`. Remote-to-remote
//! copies relay through local memory without touching disk; same-endpoint
//! copies use `copy_within` when the endpoint supports it. Writes to
//! `<name>.scpto-part`, renames to the final name on completion, then sets
//! mtime and permissions (`mode & 0o777`). The part file is restricted
//! before any data reaches it, so a secret is never readable by more users
//! than its source grants, even while a part waits to be resumed.
//!
//! # Contract
//!
//! - The part file is the final destination with `.scpto-part` appended to
//!   its file name, in the same directory.
//! - When the source mode is known, the part file is chmodded to
//!   `part_mode` (the source's `mode & 0o777` plus owner read and write,
//!   so a read-only source can still be resumed) right after it is opened
//!   and before the first byte is written, for both a fresh and a resumed
//!   part. Group and other bits never exceed the source's.
//! - `resume_from > 0` opens the part file with `WriteMode::Resume` and the
//!   source with `open_read(source, resume_from)`; `0` truncates. Deciding
//!   whether resuming is allowed (the `partials.json` record) is the
//!   engine's job. A `resume_from` larger than `size` is rejected before
//!   anything is opened, because no valid part can be longer than its
//!   source.
//! - Data is copied in chunks with a bounded buffer; `progress` is called
//!   with the total bytes now in the part file (including the resumed
//!   prefix) at most every 100 ms and once at the end.
//! - On success the writer is shut down (committing the data), the part is
//!   renamed onto the destination (`Fs::rename` handles servers without
//!   `posix-rename`), then `set_times(mtime, mtime)` and `chmod(mode &
//!   0o777)` are applied when the source values are known.
//! - Pause and cancel are done by the engine dropping the future: the part
//!   file stays and can be resumed. No other cancellation mechanism exists.
//! - `copy_within` path: when both sides are the same endpoint and
//!   `capabilities().copy_within`, the server copies the whole file to the
//!   part path (`resume_from` does not apply), which is then renamed as
//!   above; progress jumps to `size` at the end. When the source mode is
//!   known, the part is first created empty and chmodded to `part_mode`:
//!   the server's copy truncates the existing file, which keeps its mode.
//! - Errors are returned unchanged; `FsError::Disconnected` lets the engine
//!   park the item as `WaitingForReconnect`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

use crate::error::FsError;
use crate::vfs::{Fs, WriteMode};

/// Suffix appended to the destination file name while a copy is running.
pub const PART_SUFFIX: &str = ".scpto-part";

/// Size of the copy buffer; one chunk is in flight per copy.
const CHUNK_SIZE: usize = 256 * 1024;

/// Minimum interval between two progress reports of one copy.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// `<destination>.scpto-part`: the destination with [`PART_SUFFIX`]
/// appended to its file name, in the same directory.
pub fn part_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .map(OsString::from)
        .unwrap_or_default();
    name.push(PART_SUFFIX);
    destination.with_file_name(name)
}

/// Permission bits of a part file whose source has permission bits
/// `source_mode`: the source's `mode & 0o777` with owner read and write
/// added, so the part can be reopened for resume even when the source is
/// read-only.
fn part_mode(source_mode: u32) -> u32 {
    (source_mode & 0o777) | 0o600
}

/// What [`copy_file`] copies.
pub struct CopySpec<'a> {
    /// File system the source is read from.
    pub source: &'a dyn Fs,
    /// Absolute source path.
    pub source_path: &'a Path,
    /// File system the destination is written to.
    pub destination: &'a dyn Fs,
    /// The final path (not the part path).
    pub destination_path: &'a Path,
    /// Source size, for the copy_within progress and sanity checks.
    pub size: u64,
    /// Source modification time in whole seconds, applied on completion.
    pub mtime: Option<i64>,
    /// Source permission bits, applied as `mode & 0o777` on completion.
    pub mode: Option<u32>,
    /// Bytes already in a matching part file; 0 starts over.
    pub resume_from: u64,
}

/// Copies one regular file as described in the module documentation.
pub async fn copy_file(
    spec: CopySpec<'_>,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<(), FsError> {
    let part = part_path(spec.destination_path);
    if spec.resume_from > spec.size {
        return Err(FsError::Io {
            path: part,
            message: format!(
                "cannot resume at byte {} of a {}-byte file",
                spec.resume_from, spec.size
            ),
        });
    }

    let same_endpoint = spec.source.endpoint() == spec.destination.endpoint();
    if same_endpoint && spec.destination.capabilities().copy_within {
        if let Some(mode) = spec.mode {
            create_restricted_part(spec.destination, &part, mode).await?;
        }
        spec.destination
            .copy_within(spec.source_path, &part)
            .await?;
        progress(spec.size);
    } else {
        stream_into_part(&spec, &part, progress).await?;
    }

    spec.destination
        .rename(&part, spec.destination_path)
        .await?;
    if let Some(mtime) = spec.mtime {
        spec.destination
            .set_times(spec.destination_path, mtime, mtime)
            .await?;
    }
    if let Some(mode) = spec.mode {
        spec.destination
            .chmod(spec.destination_path, mode & 0o777)
            .await?;
    }
    Ok(())
}

/// Creates `part` empty with `part_mode(mode)` so that the data a
/// server-side copy writes into it is never exposed under the server's
/// default mode.
async fn create_restricted_part(
    destination: &dyn Fs,
    part: &Path,
    mode: u32,
) -> Result<(), FsError> {
    let mut writer = destination.open_write(part, WriteMode::Truncate).await?;
    destination.chmod(part, part_mode(mode)).await?;
    writer
        .shutdown()
        .await
        .map_err(|error| io_error(&error, part))
}

/// Streams the source (from `resume_from`) into the part file and commits
/// it with `shutdown`. The part is restricted before the first write.
async fn stream_into_part(
    spec: &CopySpec<'_>,
    part: &Path,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<(), FsError> {
    let write_mode = if spec.resume_from > 0 {
        WriteMode::Resume {
            offset: spec.resume_from,
        }
    } else {
        WriteMode::Truncate
    };
    let mut reader = spec
        .source
        .open_read(spec.source_path, spec.resume_from)
        .await?;
    let mut writer = spec.destination.open_write(part, write_mode).await?;
    if let Some(mode) = spec.mode {
        spec.destination.chmod(part, part_mode(mode)).await?;
    }

    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut total = spec.resume_from;
    let mut last_report = Instant::now();
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| io_error(&error, spec.source_path))?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(|error| io_error(&error, part))?;
        total += read as u64;
        if last_report.elapsed() >= PROGRESS_INTERVAL {
            progress(total);
            last_report = Instant::now();
        }
    }
    writer
        .shutdown()
        .await
        .map_err(|error| io_error(&error, part))?;
    progress(total);
    Ok(())
}

/// Maps an I/O error from an `FsReader` / `FsWriter` back to an `FsError`.
/// Implementations wrap their classified `FsError` in the `io::Error`
/// (`io::Error::other(fs_error)`), which is recovered unchanged here so a
/// lost connection stays `FsError::Disconnected`; any other I/O error is
/// classified on `path`.
fn io_error(error: &std::io::Error, path: &Path) -> FsError {
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<FsError>())
    {
        Some(fs_error) => fs_error.clone(),
        None => FsError::from_io(error, path),
    }
}

/// In-memory `Fs` shared by the stream and engine tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::{BTreeMap, HashMap};
    use std::io;
    use std::path::{Component, Path, PathBuf};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use tokio::io::AsyncWrite;
    use tokio::sync::Semaphore;
    use tokio_util::io::StreamReader;

    use crate::error::FsError;
    use crate::vfs::{
        Capabilities, Endpoint, Entry, EntryKind, Fs, FsReader, FsResult, FsWriter, SpecialKind,
        WriteMode,
    };

    /// Chunk size of the mock reader, small so copies take several reads.
    pub const READ_CHUNK: usize = 4;

    /// One node of the in-memory tree.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Node {
        File(Vec<u8>),
        Dir,
        Symlink(PathBuf),
        Special(SpecialKind),
    }

    /// A node with its attributes.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Meta {
        pub node: Node,
        pub mtime: i64,
        pub mode: u32,
    }

    /// Operations that can be made to fail.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Op {
        Lstat,
        List,
        Mkdir,
        Rename,
        RemoveFile,
        RemoveDir,
        OpenRead,
        OpenWrite,
        Shutdown,
        SetTimes,
        Chmod,
        Symlink,
        CopyWithin,
    }

    #[derive(Default)]
    struct State {
        nodes: BTreeMap<PathBuf, Meta>,
        failures: HashMap<(Op, PathBuf), (FsError, bool)>,
        read_failures: HashMap<PathBuf, (u64, FsError)>,
        calls: Vec<(Op, PathBuf)>,
        read_gate: Option<(PathBuf, u64, Arc<Semaphore>)>,
        read_delay: Option<Duration>,
        read_offsets: Vec<(PathBuf, u64)>,
    }

    /// In-memory file system with failure injection.
    pub struct MemFs {
        endpoint: Endpoint,
        capabilities: Capabilities,
        state: Arc<Mutex<State>>,
    }

    fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
        state.lock().expect("mock fs lock")
    }

    impl MemFs {
        pub fn new(endpoint: Endpoint) -> Arc<MemFs> {
            MemFs::with_capabilities(
                endpoint,
                Capabilities {
                    posix_rename: true,
                    copy_within: false,
                    symlink: true,
                    trash: false,
                },
            )
        }

        pub fn with_capabilities(endpoint: Endpoint, capabilities: Capabilities) -> Arc<MemFs> {
            let fs = MemFs {
                endpoint,
                capabilities,
                state: Arc::default(),
            };
            fs.add_dir("/");
            Arc::new(fs)
        }

        fn insert(&self, path: &str, node: Node, mtime: i64, mode: u32) {
            lock(&self.state)
                .nodes
                .insert(PathBuf::from(path), Meta { node, mtime, mode });
        }

        /// Adds a directory; parents must have been added.
        pub fn add_dir(&self, path: &str) {
            self.insert(path, Node::Dir, 0, 0o755);
        }

        pub fn add_file(&self, path: &str, data: &[u8], mtime: i64, mode: u32) {
            self.insert(path, Node::File(data.to_vec()), mtime, mode);
        }

        pub fn add_symlink(&self, path: &str, target: &str) {
            self.insert(path, Node::Symlink(PathBuf::from(target)), 0, 0o777);
        }

        pub fn add_special(&self, path: &str, kind: SpecialKind) {
            self.insert(path, Node::Special(kind), 0, 0o644);
        }

        pub fn remove_node(&self, path: &str) {
            lock(&self.state).nodes.remove(Path::new(path));
        }

        pub fn meta(&self, path: &str) -> Option<Meta> {
            lock(&self.state).nodes.get(Path::new(path)).cloned()
        }

        pub fn data(&self, path: &str) -> Option<Vec<u8>> {
            match self.meta(path)?.node {
                Node::File(data) => Some(data),
                _ => None,
            }
        }

        pub fn exists(&self, path: &str) -> bool {
            self.meta(path).is_some()
        }

        /// Makes `op` on `path` fail with `error`; once only when `once`.
        pub fn fail(&self, op: Op, path: &str, error: FsError, once: bool) {
            lock(&self.state)
                .failures
                .insert((op, PathBuf::from(path)), (error, once));
        }

        pub fn clear_failure(&self, op: Op, path: &str) {
            lock(&self.state)
                .failures
                .remove(&(op, PathBuf::from(path)));
        }

        /// Makes the next read of `path` fail once `after` bytes were
        /// delivered.
        pub fn fail_read_after(&self, path: &str, after: u64, error: FsError) {
            lock(&self.state)
                .read_failures
                .insert(PathBuf::from(path), (after, error));
        }

        /// Blocks reads of `path` beyond `after` bytes until the returned
        /// semaphore gets permits (one permit per chunk).
        pub fn gate_reads(&self, path: &str, after: u64) -> Arc<Semaphore> {
            let gate = Arc::new(Semaphore::new(0));
            lock(&self.state).read_gate = Some((PathBuf::from(path), after, gate.clone()));
            gate
        }

        /// Sleeps this long before every chunk read.
        pub fn set_read_delay(&self, delay: Duration) {
            lock(&self.state).read_delay = Some(delay);
        }

        /// Offsets of every `open_read` of `path`, in call order.
        pub fn read_offsets(&self, path: &str) -> Vec<u64> {
            lock(&self.state)
                .read_offsets
                .iter()
                .filter(|(read, _)| read == Path::new(path))
                .map(|(_, offset)| *offset)
                .collect()
        }

        pub fn calls(&self) -> Vec<(Op, PathBuf)> {
            lock(&self.state).calls.clone()
        }

        pub fn called(&self, op: Op, path: &str) -> bool {
            self.calls()
                .iter()
                .any(|(called, at)| *called == op && at == Path::new(path))
        }

        fn begin(&self, op: Op, path: &Path) -> FsResult<()> {
            let mut state = lock(&self.state);
            state.calls.push((op, path.to_path_buf()));
            let key = (op, path.to_path_buf());
            match state.failures.get(&key).cloned() {
                Some((error, once)) => {
                    if once {
                        state.failures.remove(&key);
                    }
                    Err(error)
                }
                None => Ok(()),
            }
        }

        fn entry(path: &Path, meta: &Meta, nodes: &BTreeMap<PathBuf, Meta>) -> Entry {
            let kind = match &meta.node {
                Node::File(_) => EntryKind::File,
                Node::Dir => EntryKind::Dir,
                Node::Symlink(target) => EntryKind::Symlink {
                    target: target.clone(),
                    target_is_dir: matches!(
                        nodes.get(target).map(|meta| &meta.node),
                        Some(Node::Dir)
                    ),
                },
                Node::Special(kind) => EntryKind::Special(*kind),
            };
            let size = match &meta.node {
                Node::File(data) => data.len() as u64,
                _ => 0,
            };
            Entry {
                name: path.file_name().map(Into::into).unwrap_or_default(),
                kind,
                size: Some(size),
                mtime: Some(meta.mtime),
                mode: Some(meta.mode),
            }
        }

        fn parent_is_dir(nodes: &BTreeMap<PathBuf, Meta>, path: &Path) -> FsResult<()> {
            let parent = path.parent().unwrap_or(Path::new("/"));
            match nodes.get(parent).map(|meta| &meta.node) {
                Some(Node::Dir) => Ok(()),
                Some(_) => Err(FsError::NotADirectory {
                    path: parent.to_path_buf(),
                }),
                None => Err(FsError::NotFound {
                    path: parent.to_path_buf(),
                }),
            }
        }

        fn is_direct_child(dir: &Path, path: &Path) -> bool {
            path.parent() == Some(dir) && path != dir
        }
    }

    struct MemWriter {
        state: Arc<Mutex<State>>,
        path: PathBuf,
        position: usize,
        shutdown_failure: Option<FsError>,
    }

    impl AsyncWrite for MemWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let position = self.position;
            let mut state = lock(&self.state);
            let Some(Meta {
                node: Node::File(data),
                ..
            }) = state.nodes.get_mut(&self.path)
            else {
                return Poll::Ready(Err(io::Error::other(FsError::NotFound {
                    path: self.path.clone(),
                })));
            };
            let end = position + buf.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[position..end].copy_from_slice(buf);
            drop(state);
            self.position = end;
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.shutdown_failure.take() {
                Some(error) => Poll::Ready(Err(io::Error::other(error))),
                None => Poll::Ready(Ok(())),
            }
        }
    }

    fn normalize(path: &Path) -> PathBuf {
        let mut out = PathBuf::from("/");
        for component in path.components() {
            if let Component::Normal(name) = component {
                out.push(name);
            }
        }
        out
    }

    impl Fs for MemFs {
        fn endpoint(&self) -> &Endpoint {
            &self.endpoint
        }

        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }

        fn home(&self) -> BoxFuture<'_, FsResult<PathBuf>> {
            Box::pin(async { Ok(PathBuf::from("/")) })
        }

        fn canonicalize<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
            Box::pin(async move { Ok(normalize(path)) })
        }

        fn list<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, FsResult<Vec<Entry>>> {
            Box::pin(async move {
                self.begin(Op::List, dir)?;
                let state = lock(&self.state);
                match state.nodes.get(dir).map(|meta| &meta.node) {
                    Some(Node::Dir) => {}
                    Some(_) => {
                        return Err(FsError::NotADirectory {
                            path: dir.to_path_buf(),
                        })
                    }
                    None => {
                        return Err(FsError::NotFound {
                            path: dir.to_path_buf(),
                        })
                    }
                }
                Ok(state
                    .nodes
                    .iter()
                    .filter(|(path, _)| MemFs::is_direct_child(dir, path))
                    .map(|(path, meta)| MemFs::entry(path, meta, &state.nodes))
                    .collect())
            })
        }

        fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
            Box::pin(async move {
                let state = lock(&self.state);
                let mut current = path.to_path_buf();
                loop {
                    let meta = state.nodes.get(&current).ok_or_else(|| FsError::NotFound {
                        path: path.to_path_buf(),
                    })?;
                    match &meta.node {
                        Node::Symlink(target) => current = target.clone(),
                        _ => {
                            let mut entry = MemFs::entry(&current, meta, &state.nodes);
                            entry.name = path.file_name().map(Into::into).unwrap_or_default();
                            return Ok(entry);
                        }
                    }
                }
            })
        }

        fn lstat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
            Box::pin(async move {
                self.begin(Op::Lstat, path)?;
                let state = lock(&self.state);
                let meta = state.nodes.get(path).ok_or_else(|| FsError::NotFound {
                    path: path.to_path_buf(),
                })?;
                Ok(MemFs::entry(path, meta, &state.nodes))
            })
        }

        fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                self.begin(Op::Mkdir, path)?;
                let mut state = lock(&self.state);
                if state.nodes.contains_key(path) {
                    return Err(FsError::AlreadyExists {
                        path: path.to_path_buf(),
                    });
                }
                MemFs::parent_is_dir(&state.nodes, path)?;
                state.nodes.insert(
                    path.to_path_buf(),
                    Meta {
                        node: Node::Dir,
                        mtime: 0,
                        mode: 0o755,
                    },
                );
                Ok(())
            })
        }

        fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                self.begin(Op::Rename, from)?;
                let mut state = lock(&self.state);
                MemFs::parent_is_dir(&state.nodes, to)?;
                if let Some(Meta {
                    node: Node::Dir, ..
                }) = state.nodes.get(to)
                {
                    return Err(FsError::IsADirectory {
                        path: to.to_path_buf(),
                    });
                }
                let moved: Vec<PathBuf> = state
                    .nodes
                    .keys()
                    .filter(|path| path.starts_with(from))
                    .cloned()
                    .collect();
                if moved.is_empty() {
                    return Err(FsError::NotFound {
                        path: from.to_path_buf(),
                    });
                }
                for old in moved {
                    let meta = state.nodes.remove(&old).expect("listed above");
                    let suffix = old.strip_prefix(from).expect("prefix checked");
                    let new = if suffix.as_os_str().is_empty() {
                        to.to_path_buf()
                    } else {
                        to.join(suffix)
                    };
                    state.nodes.insert(new, meta);
                }
                Ok(())
            })
        }

        fn remove_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                self.begin(Op::RemoveFile, path)?;
                let mut state = lock(&self.state);
                match state.nodes.get(path).map(|meta| &meta.node) {
                    None => Err(FsError::NotFound {
                        path: path.to_path_buf(),
                    }),
                    Some(Node::Dir) => Err(FsError::IsADirectory {
                        path: path.to_path_buf(),
                    }),
                    Some(_) => {
                        state.nodes.remove(path);
                        Ok(())
                    }
                }
            })
        }

        fn remove_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                self.begin(Op::RemoveDir, path)?;
                let mut state = lock(&self.state);
                match state.nodes.get(path).map(|meta| &meta.node) {
                    None => Err(FsError::NotFound {
                        path: path.to_path_buf(),
                    }),
                    Some(Node::Dir) => {
                        if state
                            .nodes
                            .keys()
                            .any(|child| MemFs::is_direct_child(path, child))
                        {
                            return Err(FsError::DirectoryNotEmpty {
                                path: path.to_path_buf(),
                            });
                        }
                        state.nodes.remove(path);
                        Ok(())
                    }
                    Some(_) => Err(FsError::NotADirectory {
                        path: path.to_path_buf(),
                    }),
                }
            })
        }

        fn trash<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async {
                Err(FsError::Unsupported {
                    operation: "trash".into(),
                    reason: "mock".into(),
                })
            })
        }

        fn chmod<'a>(&'a self, path: &'a Path, mode: u32) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                self.begin(Op::Chmod, path)?;
                let mut state = lock(&self.state);
                let meta = state.nodes.get_mut(path).ok_or_else(|| FsError::NotFound {
                    path: path.to_path_buf(),
                })?;
                meta.mode = mode;
                Ok(())
            })
        }

        fn set_times<'a>(
            &'a self,
            path: &'a Path,
            _atime: i64,
            mtime: i64,
        ) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                self.begin(Op::SetTimes, path)?;
                let mut state = lock(&self.state);
                let meta = state.nodes.get_mut(path).ok_or_else(|| FsError::NotFound {
                    path: path.to_path_buf(),
                })?;
                meta.mtime = mtime;
                Ok(())
            })
        }

        fn read_link<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
            Box::pin(async move {
                match self.meta(&path.to_string_lossy()).map(|meta| meta.node) {
                    Some(Node::Symlink(target)) => Ok(target),
                    _ => Err(FsError::NotFound {
                        path: path.to_path_buf(),
                    }),
                }
            })
        }

        fn symlink<'a>(&'a self, target: &'a Path, link: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                self.begin(Op::Symlink, link)?;
                if !self.capabilities.symlink {
                    return Err(FsError::Unsupported {
                        operation: "symlink".into(),
                        reason: "mock".into(),
                    });
                }
                let mut state = lock(&self.state);
                if state.nodes.contains_key(link) {
                    return Err(FsError::AlreadyExists {
                        path: link.to_path_buf(),
                    });
                }
                MemFs::parent_is_dir(&state.nodes, link)?;
                state.nodes.insert(
                    link.to_path_buf(),
                    Meta {
                        node: Node::Symlink(target.to_path_buf()),
                        mtime: 0,
                        mode: 0o777,
                    },
                );
                Ok(())
            })
        }

        fn open_read<'a>(
            &'a self,
            path: &'a Path,
            offset: u64,
        ) -> BoxFuture<'a, FsResult<FsReader>> {
            Box::pin(async move {
                self.begin(Op::OpenRead, path)?;
                let mut state = lock(&self.state);
                let data = match state.nodes.get(path).map(|meta| &meta.node) {
                    Some(Node::File(data)) => data.clone(),
                    Some(_) => {
                        return Err(FsError::IsADirectory {
                            path: path.to_path_buf(),
                        })
                    }
                    None => {
                        return Err(FsError::NotFound {
                            path: path.to_path_buf(),
                        })
                    }
                };
                state.read_offsets.push((path.to_path_buf(), offset));
                let failure = state.read_failures.remove(path);
                let gate = state
                    .read_gate
                    .clone()
                    .filter(|(gated, _, _)| gated == path)
                    .map(|(_, after, gate)| (after, gate));
                let delay = state.read_delay;
                drop(state);
                let start = (offset as usize).min(data.len());
                let chunks = futures_util::stream::unfold(start, move |position| {
                    let data = data.clone();
                    let failure = failure.clone();
                    let gate = gate.clone();
                    async move {
                        if let Some(delay) = delay {
                            tokio::time::sleep(delay).await;
                        }
                        if let Some((after, error)) = &failure {
                            if position as u64 >= *after {
                                return Some((Err(io::Error::other(error.clone())), data.len()));
                            }
                        }
                        if position >= data.len() {
                            return None;
                        }
                        if let Some((after, gate)) = &gate {
                            if position as u64 >= *after {
                                gate.acquire().await.expect("gate open").forget();
                            }
                        }
                        let end = (position + READ_CHUNK).min(data.len());
                        let chunk = io::Cursor::new(data[position..end].to_vec());
                        Some((Ok(chunk), end))
                    }
                });
                let reader: FsReader = Box::pin(StreamReader::new(chunks));
                Ok(reader)
            })
        }

        fn open_write<'a>(
            &'a self,
            path: &'a Path,
            mode: WriteMode,
        ) -> BoxFuture<'a, FsResult<FsWriter>> {
            Box::pin(async move {
                self.begin(Op::OpenWrite, path)?;
                let mut state = lock(&self.state);
                MemFs::parent_is_dir(&state.nodes, path)?;
                let position = match mode {
                    WriteMode::Truncate => {
                        state.nodes.insert(
                            path.to_path_buf(),
                            Meta {
                                node: Node::File(Vec::new()),
                                mtime: 0,
                                mode: 0o644,
                            },
                        );
                        0
                    }
                    WriteMode::Resume { offset } => match state.nodes.get_mut(path) {
                        Some(Meta {
                            node: Node::File(data),
                            ..
                        }) => {
                            data.truncate(offset as usize);
                            offset as usize
                        }
                        _ => {
                            return Err(FsError::NotFound {
                                path: path.to_path_buf(),
                            })
                        }
                    },
                };
                let shutdown_failure = match state
                    .failures
                    .get(&(Op::Shutdown, path.to_path_buf()))
                    .cloned()
                {
                    Some((error, once)) => {
                        if once {
                            state.failures.remove(&(Op::Shutdown, path.to_path_buf()));
                        }
                        Some(error)
                    }
                    None => None,
                };
                drop(state);
                let writer: FsWriter = Box::pin(MemWriter {
                    state: self.state.clone(),
                    path: path.to_path_buf(),
                    position,
                    shutdown_failure,
                });
                Ok(writer)
            })
        }

        fn copy_within<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                self.begin(Op::CopyWithin, from)?;
                if !self.capabilities.copy_within {
                    return Err(FsError::Unsupported {
                        operation: "copy".into(),
                        reason: "mock".into(),
                    });
                }
                let mut state = lock(&self.state);
                let meta = state
                    .nodes
                    .get(from)
                    .cloned()
                    .ok_or_else(|| FsError::NotFound {
                        path: from.to_path_buf(),
                    })?;
                MemFs::parent_is_dir(&state.nodes, to)?;
                // Like the server's truncating open: an existing file keeps
                // its mode, a new one gets the default.
                let mode = state.nodes.get(to).map_or(0o644, |existing| existing.mode);
                state.nodes.insert(
                    to.to_path_buf(),
                    Meta {
                        node: meta.node,
                        mtime: 0,
                        mode,
                    },
                );
                Ok(())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::test_support::{MemFs, Node, Op};
    use super::*;
    use crate::vfs::local::LocalFs;
    use crate::vfs::{Capabilities, Endpoint};

    fn remote() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        }
    }

    fn spec<'a>(
        source: &'a MemFs,
        destination: &'a MemFs,
        source_path: &'a Path,
        destination_path: &'a Path,
        size: u64,
    ) -> CopySpec<'a> {
        CopySpec {
            source,
            source_path,
            destination,
            destination_path,
            size,
            mtime: Some(1_700_000_000),
            mode: Some(0o4755),
            resume_from: 0,
        }
    }

    #[test]
    fn part_path_appends_suffix_to_the_whole_file_name() {
        assert_eq!(
            part_path(Path::new("/srv/a.tar.gz")),
            PathBuf::from("/srv/a.tar.gz.scpto-part")
        );
        assert_eq!(
            part_path(Path::new("/srv/.bashrc")),
            PathBuf::from("/srv/.bashrc.scpto-part")
        );
    }

    #[tokio::test]
    async fn copies_across_endpoints_and_applies_attributes() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/a.txt", b"hello world", 5, 0o644);
        let reports = Mutex::new(Vec::new());
        copy_file(
            spec(&local, &web, Path::new("/a.txt"), Path::new("/a.txt"), 11),
            &|bytes| reports.lock().expect("lock").push(bytes),
        )
        .await
        .expect("copy");

        let meta = web.meta("/a.txt").expect("destination");
        assert_eq!(web.data("/a.txt").as_deref(), Some(&b"hello world"[..]));
        assert_eq!(meta.mtime, 1_700_000_000);
        assert_eq!(meta.mode, 0o755, "setuid bit is masked off");
        assert!(!web.exists("/a.txt.scpto-part"));
        assert!(web.called(Op::OpenWrite, "/a.txt.scpto-part"));
        assert_eq!(reports.into_inner().expect("lock").last(), Some(&11));
    }

    #[tokio::test]
    async fn resume_appends_to_the_existing_part() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/a.txt", b"0123456789", 5, 0o644);
        web.add_file("/a.txt.scpto-part", b"0123", 0, 0o644);
        let mut copy = spec(&local, &web, Path::new("/a.txt"), Path::new("/a.txt"), 10);
        copy.resume_from = 4;
        let reports = Mutex::new(Vec::new());
        copy_file(copy, &|bytes| reports.lock().expect("lock").push(bytes))
            .await
            .expect("copy");
        assert_eq!(web.data("/a.txt").as_deref(), Some(&b"0123456789"[..]));
        assert_eq!(reports.into_inner().expect("lock"), vec![10]);
    }

    #[tokio::test]
    async fn resume_beyond_the_source_size_is_rejected() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/a.txt", b"0123", 5, 0o644);
        let mut copy = spec(&local, &web, Path::new("/a.txt"), Path::new("/a.txt"), 4);
        copy.resume_from = 9;
        let error = copy_file(copy, &|_| {}).await.expect_err("rejected");
        assert!(matches!(error, FsError::Io { .. }));
        assert!(local.calls().is_empty(), "nothing was opened");
    }

    #[tokio::test]
    async fn a_lost_connection_keeps_the_part_and_stays_disconnected() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/a.txt", b"0123456789", 5, 0o644);
        local.fail_read_after("/a.txt", 4, FsError::Disconnected);
        let error = copy_file(
            spec(&local, &web, Path::new("/a.txt"), Path::new("/a.txt"), 10),
            &|_| {},
        )
        .await
        .expect_err("disconnected");
        assert_eq!(error, FsError::Disconnected);
        assert_eq!(web.data("/a.txt.scpto-part").as_deref(), Some(&b"0123"[..]));
        assert!(!web.exists("/a.txt"));
    }

    #[tokio::test]
    async fn a_failed_commit_does_not_rename() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/a.txt", b"0123", 5, 0o644);
        web.fail(
            Op::Shutdown,
            "/a.txt.scpto-part",
            FsError::Disconnected,
            true,
        );
        let error = copy_file(
            spec(&local, &web, Path::new("/a.txt"), Path::new("/a.txt"), 4),
            &|_| {},
        )
        .await
        .expect_err("commit failed");
        assert_eq!(error, FsError::Disconnected);
        assert!(!web.called(Op::Rename, "/a.txt.scpto-part"));
    }

    #[tokio::test]
    async fn dropping_the_future_keeps_the_part() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/a.txt", b"0123456789", 5, 0o644);
        let gate = local.gate_reads("/a.txt", 4);
        let source = Path::new("/a.txt");
        let copy = copy_file(spec(&local, &web, source, source, 10), &|_| {});
        tokio::select! {
            result = copy => panic!("copy finished unexpectedly: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        drop(gate);
        assert_eq!(web.data("/a.txt.scpto-part").as_deref(), Some(&b"0123"[..]));
        assert!(!web.exists("/a.txt"));
    }

    /// Polls `copy` until it blocks (a gated read), so its part file can be
    /// inspected mid-transfer; the caller drops it afterwards (cancel).
    async fn run_until_blocked<F: std::future::Future>(copy: std::pin::Pin<&mut F>)
    where
        F::Output: std::fmt::Debug,
    {
        tokio::select! {
            result = copy => panic!("copy finished unexpectedly: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }

    #[tokio::test]
    async fn the_part_is_restricted_while_copying_and_after_cancel() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/id_rsa", b"0123456789", 5, 0o600);
        let _gate = local.gate_reads("/id_rsa", 4);
        let source = Path::new("/id_rsa");
        let mut copy = spec(&local, &web, source, source, 10);
        copy.mode = Some(0o600);
        let copy = copy_file(copy, &|_| {});
        let mut copy = Box::pin(copy);
        run_until_blocked(copy.as_mut()).await;
        let part = web.meta("/id_rsa.scpto-part").expect("part while copying");
        assert_eq!(part.node, Node::File(b"0123".to_vec()));
        assert_eq!(part.mode, 0o600);
        let calls = web.calls();
        let opened = calls
            .iter()
            .position(|call| *call == (Op::OpenWrite, PathBuf::from("/id_rsa.scpto-part")));
        let restricted = calls
            .iter()
            .position(|call| *call == (Op::Chmod, PathBuf::from("/id_rsa.scpto-part")));
        assert!(
            opened.is_some() && opened < restricted,
            "chmod follows the open: {calls:?}"
        );

        drop(copy);
        let part = web
            .meta("/id_rsa.scpto-part")
            .expect("part kept for resume");
        assert_eq!(part.mode, 0o600);
        assert!(!web.exists("/id_rsa"));
    }

    #[tokio::test]
    async fn a_resumed_part_is_restricted_before_new_data() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/.env", b"0123456789", 5, 0o600);
        // Left behind world-readable, e.g. by an older version.
        web.add_file("/.env.scpto-part", b"0123", 0, 0o644);
        let _gate = local.gate_reads("/.env", 8);
        let source = Path::new("/.env");
        let mut copy = spec(&local, &web, source, source, 10);
        copy.mode = Some(0o600);
        copy.resume_from = 4;
        let copy = copy_file(copy, &|_| {});
        let mut copy = Box::pin(copy);
        run_until_blocked(copy.as_mut()).await;
        let part = web.meta("/.env.scpto-part").expect("part while copying");
        assert_eq!(part.node, Node::File(b"01234567".to_vec()));
        assert_eq!(part.mode, 0o600);
    }

    #[tokio::test]
    async fn a_read_only_source_keeps_an_owner_writable_part() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/key.pem", b"0123456789", 5, 0o400);
        let gate = local.gate_reads("/key.pem", 4);
        let source = Path::new("/key.pem");
        let mut copy = spec(&local, &web, source, source, 10);
        copy.mode = Some(0o400);
        let copy = copy_file(copy, &|_| {});
        let mut copy = Box::pin(copy);
        run_until_blocked(copy.as_mut()).await;
        assert_eq!(web.meta("/key.pem.scpto-part").expect("part").mode, 0o600);
        gate.add_permits(16);
        copy.await.expect("copy");
        assert_eq!(web.meta("/key.pem").expect("destination").mode, 0o400);
    }

    #[test]
    fn part_mode_never_widens_group_or_other() {
        assert_eq!(part_mode(0o600), 0o600);
        assert_eq!(part_mode(0o400), 0o600);
        assert_eq!(part_mode(0o640), 0o640);
        assert_eq!(part_mode(0o4755), 0o755);
        assert_eq!(part_mode(0o444), 0o644);
    }

    #[tokio::test]
    async fn copy_within_fills_a_part_restricted_beforehand() {
        let web = MemFs::with_capabilities(
            remote(),
            Capabilities {
                posix_rename: true,
                copy_within: true,
                symlink: true,
                trash: false,
            },
        );
        web.add_file("/secret", b"0123", 5, 0o600);
        web.fail(Op::Rename, "/copy.scpto-part", FsError::Disconnected, true);
        let mut copy = spec(&web, &web, Path::new("/secret"), Path::new("/copy"), 4);
        copy.mode = Some(0o600);
        let error = copy_file(copy, &|_| {}).await.expect_err("rename failed");
        assert_eq!(error, FsError::Disconnected);
        let part = web.meta("/copy.scpto-part").expect("part kept");
        assert_eq!(part.node, Node::File(b"0123".to_vec()));
        assert_eq!(part.mode, 0o600);
        let calls: Vec<Op> = web.calls().into_iter().map(|(op, _)| op).collect();
        let order: Vec<Op> = calls
            .into_iter()
            .filter(|op| matches!(op, Op::OpenWrite | Op::Chmod | Op::CopyWithin))
            .collect();
        assert_eq!(order, vec![Op::OpenWrite, Op::Chmod, Op::CopyWithin]);
    }

    #[tokio::test]
    async fn a_local_part_is_restricted_while_copying_and_after_cancel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let web = MemFs::new(remote());
        web.add_file("/id_rsa", b"0123456789", 5, 0o600);
        let _gate = web.gate_reads("/id_rsa", 4);
        let local = LocalFs::new();
        let destination = root.join("id_rsa");
        let part = root.join("id_rsa.scpto-part");
        let copy = copy_file(
            CopySpec {
                source: &*web,
                source_path: Path::new("/id_rsa"),
                destination: &local,
                destination_path: &destination,
                size: 10,
                mtime: Some(1_700_000_000),
                mode: Some(0o600),
                resume_from: 0,
            },
            &|_| {},
        );
        let mut copy = Box::pin(copy);
        run_until_blocked(copy.as_mut()).await;
        let mode_of =
            |path: &Path| std::fs::metadata(path).expect("part").permissions().mode() & 0o777;
        assert_eq!(mode_of(&part), 0o600, "while copying");
        drop(copy);
        assert_eq!(std::fs::read(&part).expect("part kept"), b"0123");
        assert_eq!(mode_of(&part), 0o600, "after cancel");
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn same_endpoint_uses_copy_within_when_supported() {
        let web = MemFs::with_capabilities(
            remote(),
            Capabilities {
                posix_rename: true,
                copy_within: true,
                symlink: true,
                trash: false,
            },
        );
        web.add_file("/a.txt", b"0123", 5, 0o644);
        let reports = Mutex::new(Vec::new());
        copy_file(
            spec(&web, &web, Path::new("/a.txt"), Path::new("/b.txt"), 4),
            &|bytes| reports.lock().expect("lock").push(bytes),
        )
        .await
        .expect("copy");
        assert!(web.called(Op::CopyWithin, "/a.txt"));
        assert!(!web.called(Op::OpenRead, "/a.txt"));
        assert_eq!(web.data("/b.txt").as_deref(), Some(&b"0123"[..]));
        assert_eq!(web.meta("/b.txt").expect("copied").mtime, 1_700_000_000);
        assert_eq!(reports.into_inner().expect("lock"), vec![4]);
    }

    #[tokio::test]
    async fn same_endpoint_streams_without_copy_within() {
        let web = MemFs::new(remote());
        web.add_file("/a.txt", b"0123", 5, 0o644);
        copy_file(
            spec(&web, &web, Path::new("/a.txt"), Path::new("/b.txt"), 4),
            &|_| {},
        )
        .await
        .expect("copy");
        assert!(!web.called(Op::CopyWithin, "/a.txt"));
        assert_eq!(web.data("/b.txt").as_deref(), Some(&b"0123"[..]));
    }

    #[tokio::test]
    async fn unknown_attributes_are_not_applied() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/a.txt", b"0123", 5, 0o644);
        let mut copy = spec(&local, &web, Path::new("/a.txt"), Path::new("/a.txt"), 4);
        copy.mtime = None;
        copy.mode = None;
        copy_file(copy, &|_| {}).await.expect("copy");
        assert!(!web.called(Op::SetTimes, "/a.txt"));
        assert!(!web.called(Op::Chmod, "/a.txt"));
    }

    #[tokio::test]
    async fn progress_is_reported_at_most_every_100_ms_and_at_the_end() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        // 10 chunks of 4 bytes, 30 ms apart: about one report per 4 chunks.
        local.add_file("/a.bin", &[7u8; 40], 5, 0o644);
        local.set_read_delay(Duration::from_millis(30));
        let reports = Mutex::new(Vec::new());
        copy_file(
            spec(&local, &web, Path::new("/a.bin"), Path::new("/a.bin"), 40),
            &|bytes| reports.lock().expect("lock").push(bytes),
        )
        .await
        .expect("copy");
        let reports = reports.into_inner().expect("lock");
        assert_eq!(reports.last(), Some(&40));
        assert!(
            (2..=4).contains(&reports.len()),
            "throttled reports: {reports:?}"
        );
        assert!(reports.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[tokio::test]
    async fn a_fast_copy_reports_progress_once() {
        let local = MemFs::new(Endpoint::Local);
        let web = MemFs::new(remote());
        local.add_file("/a.bin", &[7u8; 40], 5, 0o644);
        let reports = Mutex::new(Vec::new());
        copy_file(
            spec(&local, &web, Path::new("/a.bin"), Path::new("/a.bin"), 40),
            &|bytes| reports.lock().expect("lock").push(bytes),
        )
        .await
        .expect("copy");
        assert_eq!(reports.into_inner().expect("lock"), vec![40]);
    }
}
