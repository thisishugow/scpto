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
