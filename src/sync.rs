//! Directory comparison and one-way sync.
//!
//! Compares two directory trees by NFC-normalized name, size, and mtime in
//! whole seconds (honoring `mtime-tolerance`), producing a [`Comparison`].
//! A sync copies the selected entries in one direction. It runs via rsync
//! when one side is local, the other is a non-sudo remote, and both report
//! rsync 3.1 or newer; otherwise it runs as a transfer engine job of file
//! pairs.
//!
//! The types below are the payload of `BgEvent::Compared` and of the sync
//! effect.
//!
//! # Contract
//!
//! - Comparison walks both trees with `list` on the Browse channel
//!   (symlinks are compared as entries, never followed). Names are matched
//!   after NFC normalization; `relative` uses the left name when both exist.
//!   A directory present on both sides is descended into and also listed
//!   as `Same` (directories carry no size or mtime status).
//! - Status: one side missing → `LeftOnly` / `RightOnly`; directory vs.
//!   non-directory → `KindDiffers`; mtimes differing by more than
//!   `mtime-tolerance` whole seconds → `LeftNewer` / `RightNewer`; else
//!   different sizes → `SizeDiffers`; else `Same`. Unknown attributes on
//!   either side count as different (`SizeDiffers`).
//! - rsync path: eligible when exactly one side is `Local`, the other is a
//!   non-sudo remote with `RemoteTools::rsync_ok`, and the local rsync
//!   passes `connections::rsync_version_ok` (probed by the sync executor).
//!   It runs `rsync -a -s --files-from=- -e <ssh> <src>/ <dst>/`, where
//!   `<ssh>` is `Connections::ssh_command(remote, false)` joined with
//!   `session::join_command`, the remote side is written `host:dir`, and
//!   the selected relative paths are fed on stdin (newline-separated; a
//!   path containing a newline fails with `Error::NewlineInPath`). The child
//!   runs with `process_group(0)` and `kill_on_drop`; cancel drops it.
//! - Engine path: a `JobRequest` with `JobTargets::Pairs` of
//!   `(source_dir/rel, destination_dir/rel)` for every selected path that is
//!   a file or symlink on the source side, `JobKind::Copy`, preset
//!   `Overwrite`, under the given job id.
//!
//! # Public API (Wave 3)
//!
//! ```text
//! pub async fn compare(
//!     left_fs: &dyn Fs,
//!     right_fs: &dyn Fs,
//!     left: Location,
//!     right: Location,
//!     mtime_tolerance: u64,
//! ) -> Result<Comparison, Error>;
//!
//! /// Pure: the status of one path from both sides' attributes.
//! pub fn diff_status(left: Option<&Entry>, right: Option<&Entry>, mtime_tolerance: u64) -> DiffStatus;
//!
//! pub fn rsync_eligible(left: &Endpoint, right: &Endpoint, remote: RemoteTools, local_rsync_ok: bool) -> bool;
//!
//! /// Runs rsync to completion as described above.
//! pub async fn run_rsync(
//!     ssh: &[String],
//!     direction: SyncDirection,
//!     left: &Location,
//!     right: &Location,
//!     paths: &[PathBuf],
//! ) -> Result<(), Error>;
//!
//! /// The engine job for a sync. `comparison` supplies the source kinds.
//! pub fn engine_job(job: JobId, direction: SyncDirection, comparison: &Comparison, paths: &[PathBuf]) -> JobRequest;
//! ```

use std::path::PathBuf;

use crate::vfs::{Entry, Location};

/// Which way a one-way sync copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    /// From the left pane's directory to the right pane's.
    LeftToRight,
    /// From the right pane's directory to the left pane's.
    RightToLeft,
}

/// How one path differs between the two trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    /// Exists only on the left.
    LeftOnly,
    /// Exists only on the right.
    RightOnly,
    /// Both exist; the left mtime is newer.
    LeftNewer,
    /// Both exist; the right mtime is newer.
    RightNewer,
    /// Same mtime but different size.
    SizeDiffers,
    /// One side is a directory and the other is not.
    KindDiffers,
    /// Same size and mtime.
    Same,
}

/// One compared path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparisonEntry {
    /// Path relative to both compared directories.
    pub relative: PathBuf,
    /// How the two sides differ.
    pub status: DiffStatus,
    /// The left side's attributes, when it exists.
    pub left: Option<Entry>,
    /// The right side's attributes, when it exists.
    pub right: Option<Entry>,
}

/// The result of comparing two directory trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// The left directory.
    pub left: Location,
    /// The right directory.
    pub right: Location,
    /// Every path present on either side, sorted by relative path.
    pub entries: Vec<ComparisonEntry>,
}
