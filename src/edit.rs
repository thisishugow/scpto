//! Editing files with `$VISUAL` / `$EDITOR`.
//!
//! Local files open in the editor directly. Remote files are downloaded to a
//! mode-0600 temp file in a per-process temp directory, the TUI is suspended
//! while the editor runs (the command is split with `shell-words`, fallback
//! `vi`), and the file is uploaded back only if its size or bytes changed.
//! Before uploading, the remote size and mtime are re-checked against the
//! values seen at download; a difference is reported as
//! [`EditOutcome::RemoteChanged`] so the user can decide. An editor that
//! exits in under one second without changes triggers a hint that GUI
//! editors need a wait flag (for example `code --wait`).
//!
//! [`EditOutcome`] is the payload of `BgEvent::EditFinished`.
//!
//! # Contract
//!
//! - The editor command is `$VISUAL`, else `$EDITOR`, else `vi`, split with
//!   `shell-words`, with the file path appended. A variable that is unset,
//!   empty, or only whitespace is skipped. A value that is not UTF-8 or
//!   does not split (an unbalanced quote) is `Error::CommandFailed` with
//!   `ExitStatus::NotStarted`.
//! - The temp directory is created on first use with mode 0700; each
//!   download gets its own mode-0700 subdirectory so the file keeps its real
//!   name (editors pick syntax by name), created with mode 0600 at open
//!   time. The runtime removes the temp directory on exit.
//! - Remote files are downloaded and uploaded through the `Fs` handed in,
//!   which is the Browse channel. A remote symlink is resolved with
//!   `canonicalize` first, so the upload replaces the link target and the
//!   link itself survives. Only regular files can be edited.
//! - The upload writes `<name>.scpto-part` next to the file, created with
//!   the original `mode & 0o777` (`0o600` when the mode is unknown, for
//!   example for a remote file deleted in the meantime) so the edits are
//!   never more widely readable than the original, shuts the writer down
//!   (committing the data), renames the part onto the file (`Fs::rename`
//!   handles servers without `posix-rename`), then sets the exact original
//!   mode, which restores bits the server's umask cleared at creation.
//! - No edit is ever lost: once the editor saved, every failure (reading
//!   the temp file, the remote re-check, the upload) is reported as
//!   [`EditOutcome::UploadFailed`] with the temp file's path, which is kept
//!   until the runtime cleans up on exit; the part file is removed when
//!   possible. The app shows the path and can retry the upload with
//!   `Effect::ResolveEditConflict { overwrite: true }`.
//! - Change detection for remote files compares the size, then the bytes,
//!   of the temp file with the downloaded content. Local files are edited in
//!   place and compared by size and full-precision modification time.
//! - Before uploading, `stat` of the remote file must still show the size
//!   and mtime seen at download (the `Option` values compared as they are),
//!   else the outcome is `RemoteChanged` and the temp file is kept until
//!   `Effect::ResolveEditConflict`. A remote file deleted in the meantime is
//!   also `RemoteChanged`.
//! - On `Unchanged` and `Uploaded` the per-download subdirectory is removed.
//!   [`resolve_conflict`] removes the temp file and its subdirectory after
//!   uploading or discarding; discarding reports `Unchanged` with
//!   `quick_exit: false`. A failed upload there is `UploadFailed` as well,
//!   and the temp file stays.
//!
//! The runtime runs the command from [`editor_command`] with
//! `term::TerminalGuard::run_foreground` between [`prepare`] and
//! [`finish`], timing it for the quick-exit hint. A non-zero editor exit
//! status is reported by the runtime as `Error::CommandFailed`; it does not
//! call [`finish`], so nothing is uploaded.
//!
//! # Public API
//!
//! ```text
//! /// A file being edited.
//! pub struct EditSession {
//!     pub location: Location,
//!     /// The file the editor opens (the local file itself, or the temp copy).
//!     pub file: PathBuf,
//!     /* private: downloaded bytes and remote size/mtime/mode */
//! }
//!
//! /// Downloads a remote file into the temp directory, or wraps a local one.
//! pub async fn prepare(fs: &dyn Fs, location: Location, temp_root: &Path) -> Result<EditSession, Error>;
//!
//! /// The editor command for `file` from the environment values given.
//! pub fn editor_command(visual: Option<&OsStr>, editor: Option<&OsStr>, file: &Path) -> Result<tokio::process::Command, Error>;
//!
//! /// After the editor exited successfully: compares, re-checks the remote
//! /// file, uploads.
//! pub async fn finish(session: EditSession, fs: &dyn Fs, editor_time: Duration) -> Result<EditOutcome, Error>;
//!
//! /// `Effect::ResolveEditConflict`: uploads `temp` over the remote file when
//! /// `overwrite`, then removes `temp` either way.
//! pub async fn resolve_conflict(fs: &dyn Fs, location: &Location, temp: &Path, overwrite: bool) -> Result<EditOutcome, Error>;
//! ```

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Error, ExitStatus};
use crate::vfs::{EntryKind, Fs, FsError, FsResult, Location, WriteMode};

/// Editors returning faster than this without a change trigger the GUI
/// editor hint.
const QUICK_EXIT: Duration = Duration::from_secs(1);

/// The editor used when neither `$VISUAL` nor `$EDITOR` is set.
const FALLBACK_EDITOR: &str = "vi";

/// Suffix of the file an upload writes before renaming it into place.
const PART_SUFFIX: &str = ".scpto-part";

/// Name prefix of the per-download subdirectories in the temp root.
const SESSION_DIR_PREFIX: &str = "edit-";

/// Numbers the per-download subdirectories of this process.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How an edit session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// The file was not changed. `quick_exit` is true when the editor
    /// returned within one second.
    Unchanged {
        /// Whether the editor exited in under one second.
        quick_exit: bool,
    },
    /// A local file was edited in place.
    LocalEdited,
    /// The changed remote file was uploaded back.
    Uploaded,
    /// The remote file changed since it was downloaded; the edited copy is
    /// kept at `temp` until the user overwrites or discards.
    RemoteChanged {
        /// The local temp file holding the user's edits.
        temp: PathBuf,
    },
    /// The edits could not be uploaded. They are kept at `temp` (until scpto
    /// exits), so the user can retry or copy them elsewhere.
    UploadFailed {
        /// The local temp file holding the user's edits.
        temp: PathBuf,
        /// Why the upload failed.
        error: Error,
    },
}

/// A file being edited.
pub struct EditSession {
    /// The file as the user selected it.
    pub location: Location,
    /// The file the editor opens (the local file itself, or the temp copy).
    pub file: PathBuf,
    origin: Origin,
}

/// What `finish` compares the edited file against.
enum Origin {
    /// A local file edited in place, with its metadata before editing.
    Local { size: u64, modified: SystemTime },
    /// A downloaded remote file.
    Remote(RemoteCopy),
}

/// A remote file as it was when downloaded.
struct RemoteCopy {
    /// The resolved remote path (symlinks followed) that uploads replace.
    path: PathBuf,
    /// The per-download temp subdirectory holding the temp file.
    dir: PathBuf,
    content: Vec<u8>,
    size: Option<u64>,
    mtime: Option<i64>,
    mode: Option<u32>,
}

/// Downloads a remote file into the temp directory, or wraps a local one.
///
/// `fs` serves `location.endpoint` (the Browse channel for remote files).
/// `temp_root` is the per-process temp directory; it is created with mode
/// 0700 when missing.
pub async fn prepare(
    fs: &dyn Fs,
    location: Location,
    temp_root: &Path,
) -> Result<EditSession, Error> {
    if location.endpoint.is_local() {
        let metadata = tokio::fs::metadata(&location.path)
            .await
            .map_err(|error| FsError::from_io(&error, &location.path))?;
        if metadata.is_dir() {
            return Err(FsError::IsADirectory {
                path: location.path,
            }
            .into());
        }
        if !metadata.is_file() {
            return Err(not_a_regular_file(&location.path).into());
        }
        let modified = metadata
            .modified()
            .map_err(|error| FsError::from_io(&error, &location.path))?;
        return Ok(EditSession {
            file: location.path.clone(),
            location,
            origin: Origin::Local {
                size: metadata.len(),
                modified,
            },
        });
    }

    let name = location
        .path
        .file_name()
        .ok_or_else(|| FsError::IsADirectory {
            path: location.path.clone(),
        })?
        .to_owned();
    let path = fs.canonicalize(&location.path).await?;
    let entry = fs.stat(&path).await?;
    match entry.kind {
        EntryKind::File => {}
        EntryKind::Dir => {
            return Err(FsError::IsADirectory {
                path: location.path,
            }
            .into())
        }
        EntryKind::Symlink { .. } | EntryKind::Special(_) => {
            return Err(not_a_regular_file(&location.path).into())
        }
    }
    let content = download(fs, &path).await?;

    let dir = create_session_dir(temp_root).await?;
    let file = dir.join(&name);
    if let Err(error) = write_temp_file(&file, &content).await {
        // The write error is what the user needs to see; a leftover
        // subdirectory is removed with the temp root when scpto exits.
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err(error.into());
    }

    Ok(EditSession {
        location,
        file,
        origin: Origin::Remote(RemoteCopy {
            path,
            dir,
            content,
            size: entry.size,
            mtime: entry.mtime,
            mode: entry.mode,
        }),
    })
}

/// The editor command for `file` from the environment values given
/// (`$VISUAL` and `$EDITOR`).
pub fn editor_command(
    visual: Option<&OsStr>,
    editor: Option<&OsStr>,
    file: &Path,
) -> Result<tokio::process::Command, Error> {
    let chosen = [("$VISUAL", visual), ("$EDITOR", editor)]
        .into_iter()
        .find_map(|(variable, value)| {
            value
                .filter(|value| !value.to_string_lossy().trim().is_empty())
                .map(|value| (variable, value))
        });
    let words = match chosen {
        None => vec![FALLBACK_EDITOR.to_owned()],
        Some((variable, value)) => {
            let text = value.to_str().ok_or_else(|| {
                editor_not_started(variable, format!("{variable} is not valid UTF-8"))
            })?;
            shell_words::split(text).map_err(|error| {
                editor_not_started(variable, format!("cannot split {text:?}: {error}"))
            })?
        }
    };
    let Some((program, args)) = words.split_first() else {
        return Err(editor_not_started("editor", "the command is empty".into()));
    };
    if program.is_empty() {
        return Err(editor_not_started(
            "editor",
            "the program name is empty".into(),
        ));
    }
    let mut command = tokio::process::Command::new(program);
    command.args(args).arg(file);
    Ok(command)
}

/// After the editor exited successfully: compares, re-checks the remote
/// file, uploads.
///
/// `fs` serves `session.location.endpoint`; `editor_time` is how long the
/// editor ran.
pub async fn finish(
    session: EditSession,
    fs: &dyn Fs,
    editor_time: Duration,
) -> Result<EditOutcome, Error> {
    let quick_exit = editor_time < QUICK_EXIT;
    let copy = match session.origin {
        Origin::Local { size, modified } => {
            let metadata = tokio::fs::metadata(&session.file)
                .await
                .map_err(|error| FsError::from_io(&error, &session.file))?;
            let now_modified = metadata
                .modified()
                .map_err(|error| FsError::from_io(&error, &session.file))?;
            return Ok(if metadata.len() == size && now_modified == modified {
                EditOutcome::Unchanged { quick_exit }
            } else {
                EditOutcome::LocalEdited
            });
        }
        Origin::Remote(copy) => copy,
    };

    let edited = match tokio::fs::read(&session.file).await {
        Ok(edited) => edited,
        Err(error) => {
            let error = FsError::from_io(&error, &session.file).into();
            return Ok(upload_failed(session.file, error));
        }
    };
    if edited.len() == copy.content.len() && edited == copy.content {
        remove_session_dir(&copy.dir).await?;
        return Ok(EditOutcome::Unchanged { quick_exit });
    }

    let unchanged_remotely = match fs.stat(&copy.path).await {
        Ok(entry) => entry.size == copy.size && entry.mtime == copy.mtime,
        Err(FsError::NotFound { .. }) => false,
        Err(error) => return Ok(upload_failed(session.file, error.into())),
    };
    if !unchanged_remotely {
        return Ok(EditOutcome::RemoteChanged { temp: session.file });
    }

    if let Err(error) = upload(fs, &copy.path, &edited, copy.mode).await {
        return Ok(upload_failed(session.file, error.into()));
    }
    remove_session_dir(&copy.dir).await?;
    Ok(EditOutcome::Uploaded)
}

/// The outcome of a failed upload: the edits stay at `temp`.
fn upload_failed(temp: PathBuf, error: Error) -> EditOutcome {
    EditOutcome::UploadFailed { temp, error }
}

/// `Effect::ResolveEditConflict`: uploads `temp` over the remote file when
/// `overwrite`, then removes `temp` either way.
///
/// `temp` is the path from [`EditOutcome::RemoteChanged`] or
/// [`EditOutcome::UploadFailed`]. The upload keeps the mode the remote file
/// has now; a remote file deleted in the meantime is recreated with mode
/// `0o600`. When the upload fails, the outcome is `UploadFailed` and `temp`
/// is kept.
pub async fn resolve_conflict(
    fs: &dyn Fs,
    location: &Location,
    temp: &Path,
    overwrite: bool,
) -> Result<EditOutcome, Error> {
    let outcome = if overwrite {
        if let Err(error) = overwrite_with(fs, location, temp).await {
            return Ok(upload_failed(temp.to_path_buf(), error.into()));
        }
        EditOutcome::Uploaded
    } else {
        EditOutcome::Unchanged { quick_exit: false }
    };
    remove_temp_file(temp).await?;
    Ok(outcome)
}

/// Uploads the edits in `temp` over `location`, keeping the mode the remote
/// file has now.
async fn overwrite_with(fs: &dyn Fs, location: &Location, temp: &Path) -> FsResult<()> {
    let edited = tokio::fs::read(temp)
        .await
        .map_err(|error| FsError::from_io(&error, temp))?;
    let path = match fs.canonicalize(&location.path).await {
        Ok(path) => path,
        Err(FsError::NotFound { .. }) => location.path.clone(),
        Err(error) => return Err(error),
    };
    let mode = match fs.stat(&path).await {
        Ok(entry) => entry.mode,
        Err(FsError::NotFound { .. }) => None,
        Err(error) => return Err(error),
    };
    upload(fs, &path, &edited, mode).await
}

/// Reads a whole remote file.
async fn download(fs: &dyn Fs, path: &Path) -> FsResult<Vec<u8>> {
    let mut reader = fs.open_read(path, 0).await?;
    let mut content = Vec::new();
    reader
        .read_to_end(&mut content)
        .await
        .map_err(|error| FsError::from_io(&error, path))?;
    Ok(content)
}

/// Writes `content` to `path` through its `.scpto-part` file, as described
/// in the module contract.
async fn upload(fs: &dyn Fs, path: &Path, content: &[u8], mode: Option<u32>) -> FsResult<()> {
    let part = part_path(path);
    let result = write_part_and_rename(fs, &part, path, content, mode).await;
    if result.is_err() {
        // The upload error is what the user needs to see; removing the
        // partial file is best effort (it fails when the connection is gone).
        let _ = fs.remove_file(&part).await;
    }
    result
}

async fn write_part_and_rename(
    fs: &dyn Fs,
    part: &Path,
    path: &Path,
    content: &[u8],
    mode: Option<u32>,
) -> FsResult<()> {
    // Created restricted, so no edited data is ever more widely readable
    // than the original.
    let permissions = mode.map_or(0o600, |mode| mode & 0o777);
    let mut writer = fs
        .open_write(part, WriteMode::Truncate { permissions })
        .await?;
    writer
        .write_all(content)
        .await
        .map_err(|error| FsError::from_io(&error, part))?;
    writer
        .shutdown()
        .await
        .map_err(|error| FsError::from_io(&error, part))?;
    drop(writer);
    fs.rename(part, path).await?;
    match mode {
        Some(mode) => fs.chmod(path, mode).await,
        None => Ok(()),
    }
}

/// `<path>.scpto-part`, in the same directory.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(PART_SUFFIX);
    path.with_file_name(name)
}

/// Creates `temp_root` when missing and a fresh subdirectory in it, both
/// with mode 0700.
async fn create_session_dir(temp_root: &Path) -> FsResult<PathBuf> {
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(temp_root)
        .await
        .map_err(|error| FsError::from_io(&error, temp_root))?;
    loop {
        let number = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = temp_root.join(format!("{SESSION_DIR_PREFIX}{number}"));
        match tokio::fs::DirBuilder::new().mode(0o700).create(&dir).await {
            Ok(()) => {
                // DirBuilder's mode is reduced by the umask; set it exactly.
                tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    .await
                    .map_err(|error| FsError::from_io(&error, &dir))?;
                return Ok(dir);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(FsError::from_io(&error, &dir)),
        }
    }
}

/// Creates `file` with mode 0600 and writes `content` to it.
async fn write_temp_file(file: &Path, content: &[u8]) -> FsResult<()> {
    let to_fs_error = |error: io::Error| FsError::from_io(&error, file);
    let mut handle = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)
        .await
        .map_err(to_fs_error)?;
    handle.write_all(content).await.map_err(to_fs_error)?;
    handle.flush().await.map_err(to_fs_error)
}

/// Removes a per-download subdirectory and everything the editor left in it.
async fn remove_session_dir(dir: &Path) -> FsResult<()> {
    tokio::fs::remove_dir_all(dir)
        .await
        .map_err(|error| FsError::from_io(&error, dir))
}

/// Removes a temp file and its per-download subdirectory. The parent is
/// removed only when it is named like a per-download subdirectory and is
/// empty, so a wrong `temp` never deletes other directories or files.
async fn remove_temp_file(temp: &Path) -> FsResult<()> {
    tokio::fs::remove_file(temp)
        .await
        .map_err(|error| FsError::from_io(&error, temp))?;
    let Some(dir) = temp.parent().filter(|dir| {
        dir.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with(SESSION_DIR_PREFIX))
    }) else {
        return Ok(());
    };
    match tokio::fs::remove_dir(dir).await {
        Ok(()) => Ok(()),
        // Files the editor left behind (backups) stay until the runtime
        // removes the temp root on exit.
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(FsError::from_io(&error, dir)),
    }
}

fn not_a_regular_file(path: &Path) -> FsError {
    FsError::Unsupported {
        operation: "Editing".into(),
        reason: format!("{} is not a regular file", path.display()),
    }
}

fn editor_not_started(program: &str, stderr: String) -> Error {
    Error::CommandFailed {
        program: program.into(),
        status: ExitStatus::NotStarted,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::os::unix::ffi::OsStrExt;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use futures_util::future::BoxFuture;
    use tokio::io::AsyncWrite;

    use crate::vfs::{Capabilities, Endpoint, Entry, FsReader, FsWriter};

    /// One regular file in the mock file system.
    #[derive(Clone)]
    struct MockFile {
        data: Vec<u8>,
        mtime: i64,
        mode: u32,
    }

    #[derive(Default)]
    struct MockState {
        files: HashMap<PathBuf, MockFile>,
        dirs: Vec<PathBuf>,
        /// Symlink path to target path.
        links: HashMap<PathBuf, PathBuf>,
        /// Mutating operations in call order, for protocol assertions.
        log: Vec<String>,
        /// Seconds clock used as the mtime of committed writes.
        clock: i64,
        fail_writes: bool,
        /// `stat` fails as if the connection were lost.
        fail_stat: bool,
    }

    /// In-memory remote file system with the operations `edit` uses.
    struct MockFs {
        endpoint: Endpoint,
        state: Arc<Mutex<MockState>>,
    }

    impl MockFs {
        fn new() -> MockFs {
            let state = MockState {
                dirs: vec!["/".into(), "/srv".into()],
                clock: 1_000,
                ..MockState::default()
            };
            MockFs {
                endpoint: Endpoint::Remote {
                    host: "web".into(),
                    sudo: false,
                },
                state: Arc::new(Mutex::new(state)),
            }
        }

        fn add_file(&self, path: &str, data: &[u8], mode: u32) {
            let mut state = self.state.lock().unwrap();
            let mtime = state.clock;
            state.files.insert(
                path.into(),
                MockFile {
                    data: data.to_vec(),
                    mtime,
                    mode,
                },
            );
        }

        fn file(&self, path: &str) -> Option<MockFile> {
            self.state
                .lock()
                .unwrap()
                .files
                .get(Path::new(path))
                .cloned()
        }

        /// Simulates another user saving the file on the server.
        fn touch(&self, path: &str) {
            let mut state = self.state.lock().unwrap();
            state.clock += 5;
            let clock = state.clock;
            state.files.get_mut(Path::new(path)).unwrap().mtime = clock;
        }

        fn log(&self) -> Vec<String> {
            self.state.lock().unwrap().log.clone()
        }

        fn location(&self, path: &str) -> Location {
            Location {
                endpoint: self.endpoint.clone(),
                path: path.into(),
            }
        }

        fn unsupported<T: Send + 'static>(operation: &str) -> BoxFuture<'static, FsResult<T>> {
            let error = FsError::Unsupported {
                operation: operation.into(),
                reason: "not used by edit".into(),
            };
            Box::pin(async move { Err(error) })
        }
    }

    fn resolve(state: &MockState, path: &Path) -> PathBuf {
        state
            .links
            .get(path)
            .cloned()
            .unwrap_or_else(|| path.to_path_buf())
    }

    fn not_found(path: &Path) -> FsError {
        FsError::NotFound {
            path: path.to_path_buf(),
        }
    }

    impl Fs for MockFs {
        fn endpoint(&self) -> &Endpoint {
            &self.endpoint
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                posix_rename: true,
                copy_within: false,
                symlink: false,
                trash: false,
            }
        }

        fn home(&self) -> BoxFuture<'_, FsResult<PathBuf>> {
            Box::pin(async { Ok(PathBuf::from("/srv")) })
        }

        fn canonicalize<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
            Box::pin(async move {
                let state = self.state.lock().unwrap();
                let resolved = resolve(&state, path);
                if state.files.contains_key(&resolved) || state.dirs.contains(&resolved) {
                    Ok(resolved)
                } else {
                    Err(not_found(path))
                }
            })
        }

        fn list<'a>(&'a self, _dir: &'a Path) -> BoxFuture<'a, FsResult<Vec<Entry>>> {
            MockFs::unsupported("list")
        }

        fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
            Box::pin(async move {
                let state = self.state.lock().unwrap();
                if state.fail_stat {
                    return Err(FsError::Disconnected);
                }
                let resolved = resolve(&state, path);
                let name = path.file_name().unwrap_or_default().to_owned();
                if let Some(file) = state.files.get(&resolved) {
                    Ok(Entry {
                        name,
                        kind: EntryKind::File,
                        size: Some(file.data.len() as u64),
                        mtime: Some(file.mtime),
                        mode: Some(file.mode),
                    })
                } else if state.dirs.contains(&resolved) {
                    Ok(Entry {
                        name,
                        kind: EntryKind::Dir,
                        size: Some(4096),
                        mtime: Some(0),
                        mode: Some(0o755),
                    })
                } else {
                    Err(not_found(path))
                }
            })
        }

        fn lstat<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<Entry>> {
            MockFs::unsupported("lstat")
        }

        fn mkdir<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            MockFs::unsupported("mkdir")
        }

        fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state
                    .log
                    .push(format!("rename {} {}", from.display(), to.display()));
                let file = state.files.remove(from).ok_or_else(|| not_found(from))?;
                state.files.insert(to.to_path_buf(), file);
                Ok(())
            })
        }

        fn remove_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state.log.push(format!("remove {}", path.display()));
                state
                    .files
                    .remove(path)
                    .map(drop)
                    .ok_or_else(|| not_found(path))
            })
        }

        fn remove_dir<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            MockFs::unsupported("remove_dir")
        }

        fn trash<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
            MockFs::unsupported("trash")
        }

        fn chmod<'a>(&'a self, path: &'a Path, mode: u32) -> BoxFuture<'a, FsResult<()>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state.log.push(format!("chmod {} {mode:o}", path.display()));
                let file = state.files.get_mut(path).ok_or_else(|| not_found(path))?;
                file.mode = mode;
                Ok(())
            })
        }

        fn set_times<'a>(
            &'a self,
            _path: &'a Path,
            _atime: i64,
            _mtime: i64,
        ) -> BoxFuture<'a, FsResult<()>> {
            MockFs::unsupported("set_times")
        }

        fn read_link<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, FsResult<PathBuf>> {
            MockFs::unsupported("read_link")
        }

        fn symlink<'a>(
            &'a self,
            _target: &'a Path,
            _link: &'a Path,
        ) -> BoxFuture<'a, FsResult<()>> {
            MockFs::unsupported("symlink")
        }

        fn open_read<'a>(
            &'a self,
            path: &'a Path,
            offset: u64,
        ) -> BoxFuture<'a, FsResult<FsReader>> {
            Box::pin(async move {
                assert_eq!(offset, 0, "edit downloads whole files");
                let state = self.state.lock().unwrap();
                let file = state.files.get(path).ok_or_else(|| not_found(path))?;
                let reader: FsReader = Box::pin(io::Cursor::new(file.data.clone()));
                Ok(reader)
            })
        }

        fn open_write<'a>(
            &'a self,
            path: &'a Path,
            mode: WriteMode,
        ) -> BoxFuture<'a, FsResult<FsWriter>> {
            Box::pin(async move {
                let WriteMode::Truncate { permissions } = mode else {
                    panic!("edit never resumes");
                };
                let mut state = self.state.lock().unwrap();
                state
                    .log
                    .push(format!("open_write {} {permissions:o}", path.display()));
                // Creating the file makes it visible (as on a real server)
                // with the requested mode, minus a 022 umask; data is
                // committed on shutdown.
                let mtime = state.clock;
                state.files.insert(
                    path.to_path_buf(),
                    MockFile {
                        data: Vec::new(),
                        mtime,
                        mode: permissions & 0o755,
                    },
                );
                let writer: FsWriter = Box::pin(MockWriter {
                    state: Arc::clone(&self.state),
                    path: path.to_path_buf(),
                    buffer: Vec::new(),
                });
                Ok(writer)
            })
        }

        fn copy_within<'a>(
            &'a self,
            _from: &'a Path,
            _to: &'a Path,
        ) -> BoxFuture<'a, FsResult<()>> {
            MockFs::unsupported("copy_within")
        }
    }

    /// Buffers writes and commits them to the mock file on shutdown.
    struct MockWriter {
        state: Arc<Mutex<MockState>>,
        path: PathBuf,
        buffer: Vec<u8>,
    }

    impl AsyncWrite for MockWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.state.lock().unwrap().fail_writes {
                return Poll::Ready(Err(io::Error::other("disk full")));
            }
            self.buffer.extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let mut state = self.state.lock().unwrap();
            state.clock += 1;
            let clock = state.clock;
            let file = state
                .files
                .get_mut(&self.path)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            file.data = self.buffer.clone();
            file.mtime = clock;
            Poll::Ready(Ok(()))
        }
    }

    /// Stands in for the editor the runtime runs between `prepare` and
    /// `finish`: saves `content` into the file it was given.
    fn stub_editor_saves(file: &Path, content: &[u8]) {
        std::fs::write(file, content).unwrap();
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    const SLOW: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn prepare_downloads_into_private_temp_file_with_real_name() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("scpto-edit");
        let fs = MockFs::new();
        fs.add_file("/srv/app.toml", b"port = 80\n", 0o640);

        let session = prepare(&fs, fs.location("/srv/app.toml"), &root)
            .await
            .unwrap();

        assert_eq!(session.file.file_name().unwrap(), "app.toml");
        assert!(session.file.starts_with(&root));
        assert_eq!(std::fs::read(&session.file).unwrap(), b"port = 80\n");
        assert_eq!(mode_of(&session.file), 0o600);
        assert_eq!(mode_of(session.file.parent().unwrap()), 0o700);
        assert_eq!(mode_of(&root), 0o700);
    }

    #[tokio::test]
    async fn two_downloads_of_the_same_name_get_separate_directories() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"a", 0o644);

        let first = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();
        let second = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();

        assert_ne!(first.file, second.file);
        assert!(first.file.exists() && second.file.exists());
    }

    #[tokio::test]
    async fn unchanged_quick_exit_uploads_nothing_and_cleans_up() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"hello", 0o644);
        let session = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();
        let dir = session.file.parent().unwrap().to_path_buf();

        let outcome = finish(session, &fs, Duration::from_millis(200))
            .await
            .unwrap();

        assert_eq!(outcome, EditOutcome::Unchanged { quick_exit: true });
        assert!(fs.log().is_empty());
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn unchanged_after_a_long_edit_is_not_a_quick_exit() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"hello", 0o644);
        let session = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();
        // Saving identical bytes is not a change.
        stub_editor_saves(&session.file, b"hello");

        let outcome = finish(session, &fs, SLOW).await.unwrap();

        assert_eq!(outcome, EditOutcome::Unchanged { quick_exit: false });
    }

    #[tokio::test]
    async fn changed_file_is_uploaded_through_part_file_with_original_mode() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/secret.env", b"KEY=1\n", 0o600);
        let session = prepare(&fs, fs.location("/srv/secret.env"), temp.path())
            .await
            .unwrap();
        let dir = session.file.parent().unwrap().to_path_buf();
        stub_editor_saves(&session.file, b"KEY=2\nOTHER=3\n");

        let outcome = finish(session, &fs, Duration::from_millis(100))
            .await
            .unwrap();

        assert_eq!(outcome, EditOutcome::Uploaded);
        let remote = fs.file("/srv/secret.env").unwrap();
        assert_eq!(remote.data, b"KEY=2\nOTHER=3\n");
        assert_eq!(remote.mode, 0o600);
        assert!(fs.file("/srv/secret.env.scpto-part").is_none());
        assert_eq!(
            fs.log(),
            [
                "open_write /srv/secret.env.scpto-part 600",
                "rename /srv/secret.env.scpto-part /srv/secret.env",
                "chmod /srv/secret.env 600",
            ]
        );
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn same_size_edit_is_detected_by_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"abc", 0o644);
        let session = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();
        stub_editor_saves(&session.file, b"abd");

        let outcome = finish(session, &fs, SLOW).await.unwrap();

        assert_eq!(outcome, EditOutcome::Uploaded);
        assert_eq!(fs.file("/srv/a.txt").unwrap().data, b"abd");
    }

    #[tokio::test]
    async fn remote_change_during_edit_keeps_the_edits_and_uploads_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"v1", 0o644);
        let session = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();
        let file = session.file.clone();
        stub_editor_saves(&file, b"mine");
        fs.touch("/srv/a.txt");

        let outcome = finish(session, &fs, SLOW).await.unwrap();

        assert_eq!(outcome, EditOutcome::RemoteChanged { temp: file.clone() });
        assert_eq!(std::fs::read(&file).unwrap(), b"mine");
        assert_eq!(fs.file("/srv/a.txt").unwrap().data, b"v1");
        assert!(fs.log().is_empty());
    }

    #[tokio::test]
    async fn remote_file_deleted_during_edit_is_a_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"v1", 0o644);
        let session = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();
        let file = session.file.clone();
        stub_editor_saves(&file, b"mine");
        fs.state
            .lock()
            .unwrap()
            .files
            .remove(Path::new("/srv/a.txt"));

        let outcome = finish(session, &fs, SLOW).await.unwrap();

        assert_eq!(outcome, EditOutcome::RemoteChanged { temp: file });
    }

    #[tokio::test]
    async fn resolve_conflict_overwrite_uploads_and_removes_temp() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"v1", 0o640);
        let location = fs.location("/srv/a.txt");
        let session = prepare(&fs, location.clone(), temp.path()).await.unwrap();
        let file = session.file.clone();
        stub_editor_saves(&file, b"mine");
        fs.touch("/srv/a.txt");
        finish(session, &fs, SLOW).await.unwrap();

        let outcome = resolve_conflict(&fs, &location, &file, true).await.unwrap();

        assert_eq!(outcome, EditOutcome::Uploaded);
        let remote = fs.file("/srv/a.txt").unwrap();
        assert_eq!(remote.data, b"mine");
        assert_eq!(remote.mode, 0o640);
        assert!(!file.exists());
        assert!(!file.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn resolve_conflict_discard_removes_temp_and_leaves_remote_alone() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"v1", 0o644);
        let location = fs.location("/srv/a.txt");
        let session = prepare(&fs, location.clone(), temp.path()).await.unwrap();
        let file = session.file.clone();
        stub_editor_saves(&file, b"mine");
        fs.touch("/srv/a.txt");
        finish(session, &fs, SLOW).await.unwrap();

        let outcome = resolve_conflict(&fs, &location, &file, false)
            .await
            .unwrap();

        assert_eq!(outcome, EditOutcome::Unchanged { quick_exit: false });
        assert_eq!(fs.file("/srv/a.txt").unwrap().data, b"v1");
        assert!(fs.log().is_empty());
        assert!(!file.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn resolve_conflict_recreates_a_deleted_remote_file() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        let location = fs.location("/srv/gone.txt");
        let edits = temp.path().join("gone.txt");
        std::fs::write(&edits, b"mine").unwrap();

        let outcome = resolve_conflict(&fs, &location, &edits, true)
            .await
            .unwrap();

        assert_eq!(outcome, EditOutcome::Uploaded);
        assert_eq!(fs.file("/srv/gone.txt").unwrap().data, b"mine");
        assert!(!edits.exists());
        // The parent is not a per-download directory; it stays although empty.
        assert!(temp.path().exists());
    }

    #[tokio::test]
    async fn symlinked_remote_file_is_uploaded_to_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/real.conf", b"a", 0o644);
        fs.state
            .lock()
            .unwrap()
            .links
            .insert("/srv/link.conf".into(), "/srv/real.conf".into());
        let session = prepare(&fs, fs.location("/srv/link.conf"), temp.path())
            .await
            .unwrap();
        assert_eq!(session.file.file_name().unwrap(), "link.conf");
        stub_editor_saves(&session.file, b"b");

        let outcome = finish(session, &fs, SLOW).await.unwrap();

        assert_eq!(outcome, EditOutcome::Uploaded);
        assert_eq!(fs.file("/srv/real.conf").unwrap().data, b"b");
        assert!(fs.file("/srv/link.conf").is_none());
    }

    #[tokio::test]
    async fn failed_upload_removes_part_file_and_keeps_the_edits() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"v1", 0o644);
        let session = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();
        let file = session.file.clone();
        stub_editor_saves(&file, b"mine");
        fs.state.lock().unwrap().fail_writes = true;

        let outcome = finish(session, &fs, SLOW).await.unwrap();

        assert_eq!(
            outcome,
            EditOutcome::UploadFailed {
                temp: file.clone(),
                error: Error::Fs(FsError::Io {
                    path: "/srv/a.txt.scpto-part".into(),
                    message: "disk full".into(),
                }),
            }
        );
        assert!(fs.file("/srv/a.txt.scpto-part").is_none());
        assert_eq!(fs.file("/srv/a.txt").unwrap().data, b"v1");
        assert_eq!(std::fs::read(&file).unwrap(), b"mine");

        // The kept edits can be uploaded once the problem is gone.
        fs.state.lock().unwrap().fail_writes = false;
        let location = fs.location("/srv/a.txt");
        let outcome = resolve_conflict(&fs, &location, &file, true).await.unwrap();
        assert_eq!(outcome, EditOutcome::Uploaded);
        assert_eq!(fs.file("/srv/a.txt").unwrap().data, b"mine");
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn a_failed_remote_recheck_keeps_the_edits() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"v1", 0o644);
        let session = prepare(&fs, fs.location("/srv/a.txt"), temp.path())
            .await
            .unwrap();
        let file = session.file.clone();
        stub_editor_saves(&file, b"mine");
        fs.state.lock().unwrap().fail_stat = true;

        let outcome = finish(session, &fs, SLOW).await.unwrap();

        assert_eq!(
            outcome,
            EditOutcome::UploadFailed {
                temp: file.clone(),
                error: Error::Fs(FsError::Disconnected),
            }
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"mine");
    }

    #[tokio::test]
    async fn a_failed_overwrite_keeps_the_temp_file() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/a.txt", b"v1", 0o644);
        let edits = temp.path().join("a.txt");
        std::fs::write(&edits, b"mine").unwrap();
        fs.state.lock().unwrap().fail_writes = true;

        let outcome = resolve_conflict(&fs, &fs.location("/srv/a.txt"), &edits, true)
            .await
            .unwrap();

        assert!(
            matches!(&outcome, EditOutcome::UploadFailed { temp, .. } if *temp == edits),
            "{outcome:?}"
        );
        assert_eq!(std::fs::read(&edits).unwrap(), b"mine");
        assert_eq!(fs.file("/srv/a.txt").unwrap().data, b"v1");
    }

    #[tokio::test]
    async fn a_recreated_file_is_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        let edits = temp.path().join("gone.txt");
        std::fs::write(&edits, b"mine").unwrap();

        resolve_conflict(&fs, &fs.location("/srv/gone.txt"), &edits, true)
            .await
            .unwrap();

        assert_eq!(fs.file("/srv/gone.txt").unwrap().mode, 0o600);
    }

    #[tokio::test]
    async fn the_exact_original_mode_survives_the_umask() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();
        fs.add_file("/srv/shared.txt", b"a", 0o664);
        let session = prepare(&fs, fs.location("/srv/shared.txt"), temp.path())
            .await
            .unwrap();
        stub_editor_saves(&session.file, b"b");

        finish(session, &fs, SLOW).await.unwrap();

        // The mock's umask cleared group write at creation; the final chmod
        // puts it back.
        assert_eq!(fs.file("/srv/shared.txt").unwrap().mode, 0o664);
    }

    #[tokio::test]
    async fn directories_cannot_be_edited() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MockFs::new();

        let result = prepare(&fs, fs.location("/srv"), temp.path()).await;

        assert_eq!(
            result.err(),
            Some(Error::Fs(FsError::IsADirectory {
                path: "/srv".into()
            }))
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn local_file_is_edited_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("notes.md");
        std::fs::write(&path, b"one").unwrap();
        let fs = MockFs::new();
        let location = Location {
            endpoint: Endpoint::Local,
            path: path.clone(),
        };

        let session = prepare(&fs, location.clone(), temp.path()).await.unwrap();
        assert_eq!(session.file, path);
        let outcome = finish(session, &fs, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(outcome, EditOutcome::Unchanged { quick_exit: true });

        let session = prepare(&fs, location, temp.path()).await.unwrap();
        stub_editor_saves(&session.file, b"one two");
        let outcome = finish(session, &fs, SLOW).await.unwrap();
        assert_eq!(outcome, EditOutcome::LocalEdited);
        assert!(fs.log().is_empty());
    }

    fn argv(command: &tokio::process::Command) -> Vec<OsString> {
        let std = command.as_std();
        std::iter::once(std.get_program().to_owned())
            .chain(std.get_args().map(OsStr::to_owned))
            .collect()
    }

    #[test]
    fn visual_wins_over_editor_and_is_split_with_shell_words() {
        let file = Path::new("/tmp/x/my file.txt");
        let command = editor_command(
            Some(OsStr::new("code --wait 'a b'")),
            Some(OsStr::new("nano")),
            file,
        )
        .unwrap();
        assert_eq!(
            argv(&command),
            ["code", "--wait", "a b", "/tmp/x/my file.txt"]
        );
    }

    #[test]
    fn blank_visual_falls_through_to_editor_then_vi() {
        let file = Path::new("/f");
        let command =
            editor_command(Some(OsStr::new("  ")), Some(OsStr::new("nano")), file).unwrap();
        assert_eq!(argv(&command), ["nano", "/f"]);
        let command = editor_command(None, Some(OsStr::new("")), file).unwrap();
        assert_eq!(argv(&command), ["vi", "/f"]);
        let command = editor_command(None, None, file).unwrap();
        assert_eq!(argv(&command), ["vi", "/f"]);
    }

    #[test]
    fn unsplittable_editor_value_is_a_not_started_failure() {
        let error = editor_command(None, Some(OsStr::new("vim '")), Path::new("/f")).unwrap_err();
        let Error::CommandFailed {
            program, status, ..
        } = &error
        else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(program, "$EDITOR");
        assert_eq!(*status, ExitStatus::NotStarted);
    }

    #[test]
    fn non_utf8_and_empty_program_are_rejected() {
        let invalid = OsStr::from_bytes(b"vi\xff");
        let error = editor_command(Some(invalid), None, Path::new("/f")).unwrap_err();
        assert!(matches!(
            error,
            Error::CommandFailed {
                status: ExitStatus::NotStarted,
                ..
            }
        ));
        let error = editor_command(Some(OsStr::new("''")), None, Path::new("/f")).unwrap_err();
        assert!(matches!(
            error,
            Error::CommandFailed {
                status: ExitStatus::NotStarted,
                ..
            }
        ));
    }

    #[test]
    fn part_path_appends_suffix_in_same_directory() {
        assert_eq!(
            part_path(Path::new("/srv/a.txt")),
            Path::new("/srv/a.txt.scpto-part")
        );
    }
}
