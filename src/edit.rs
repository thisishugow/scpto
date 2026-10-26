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
//!   `shell-words` (a value that does not split is `Error::CommandFailed`
//!   with `ExitStatus::NotStarted`), with the file path appended.
//! - The temp directory is created once per process with mode 0700; each
//!   download gets its own subdirectory so the file keeps its real name
//!   (editors pick syntax by name), written with mode 0600. The runtime
//!   removes the directory on exit.
//! - Remote files are downloaded and uploaded on the Browse channel. The
//!   upload reuses `transfer::stream::copy_file` (part file, then rename),
//!   then re-applies the original mode. A non-zero editor exit status is
//!   reported as `Error::CommandFailed` and nothing is uploaded.
//! - Change detection compares size, then bytes, of the temp file with the
//!   downloaded content. Before uploading, `stat` of the remote file must
//!   still show the size and mtime seen at download, else the outcome is
//!   `RemoteChanged` and the temp file is kept until
//!   `Effect::ResolveEditConflict`.
//!
//! The runtime runs the editor with `term::TerminalGuard::run_foreground`
//! between `prepare` and `finish`, timing it for the quick-exit hint.
//!
//! # Public API (Wave 3)
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

use std::path::PathBuf;

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
}
