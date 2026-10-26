//! Transfer queue data model shared by the engine and the UI.
//!
//! The engine owns the authoritative queue. The UI keeps a mirror built only
//! from [`TransferEvent`]s and controls the engine only through
//! [`TransferCommand`]s, so the app state machine never touches I/O.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::Error;
use crate::vfs::{Endpoint, Entry, SpecialKind};

/// Identifies a job. Allocated by the app when it submits a request, so the
/// app can correlate later events with its own intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(pub u64);

/// Identifies an item. Allocated by the engine, unique across all jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemId(pub u64);

/// What a job does with its sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// Copy sources to the destination.
    Copy,
    /// Move sources: a rename within one endpoint, otherwise copy and then
    /// delete each source after its item succeeds (plan 7.5).
    Move,
}

/// How a job moves data. The engine picks the mode and reports it in
/// [`TransferEvent::JobCreated`]; the UI keeps it on its job mirror so the
/// queue panel labels tar jobs "tar (no resume)".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    /// One SFTP stream per file, with `.scpto-part` and resume.
    PerFile,
    /// One tar stream for the whole job (local ↔ non-sudo remote, file count
    /// at least `tar-threshold`). No `.scpto-part` protection and no resume.
    /// There is no separate confirmation before a tar job starts: the
    /// "dialog says so" of plan 7.5 S8 is the dialog shown for
    /// [`TransferEvent::TarConflictRaised`], which states that existing files
    /// are overwritten without resume protection.
    Tar,
}

/// What a job transfers, as submitted by the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobTargets {
    /// Copy or move each source path (file or directory tree) into
    /// `directory`, keeping its name. Directories are expanded into items.
    /// This is the only form eligible for tar mode.
    IntoDirectory {
        /// Absolute source paths on the source endpoint.
        sources: Vec<PathBuf>,
        /// Absolute destination directory on the destination endpoint.
        directory: PathBuf,
    },
    /// Explicit file pairs (sync plans, edit upload-back). Each pair is
    /// (source path, destination path); parent directories are created.
    Pairs(Vec<(PathBuf, PathBuf)>),
}

/// A request to create a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRequest {
    /// Id chosen by the app.
    pub id: JobId,
    /// Copy or move.
    pub kind: JobKind,
    /// Endpoint the sources are read from.
    pub source: Endpoint,
    /// Endpoint the destinations are written to.
    pub destination: Endpoint,
    /// What to transfer.
    pub targets: JobTargets,
    /// A conflict decision applied to every item without asking, for example
    /// `Overwrite` for edit upload-back.
    pub preset: Option<ConflictChoice>,
}

/// What an item is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    /// A regular file.
    File,
    /// A directory to create (and, for a move, to remove once everything
    /// beneath it succeeded).
    Dir,
    /// A symlink recreated with the same target.
    Symlink {
        /// The link target.
        target: PathBuf,
    },
    /// A FIFO, socket, or device found while expanding a directory. Such
    /// items are created already `Skipped { reason: SpecialFile }` and are
    /// never transferred (plan 7.5 S15).
    Special(SpecialKind),
}

/// One unit of work in a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Item id.
    pub id: ItemId,
    /// Owning job.
    pub job: JobId,
    /// What the item is.
    pub kind: ItemKind,
    /// Absolute source path.
    pub source: PathBuf,
    /// Absolute final destination path (changes when the user picks Rename).
    pub destination: PathBuf,
    /// Source size in bytes (0 for directories and symlinks).
    pub size: u64,
    /// Source modification time in whole seconds, when known.
    pub mtime: Option<i64>,
    /// Source permission bits; applied as `mode & 0o777` on completion.
    pub mode: Option<u32>,
    /// Current state.
    pub state: ItemState,
    /// A non-fatal metadata warning; transferred data remains successful.
    pub warning: Option<Error>,
}

/// Why an item was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The user chose Skip in a conflict prompt.
    UserSkipped,
    /// "Overwrite if newer" found the destination as new or newer.
    NotNewer,
    /// The source is a FIFO, socket, or device; the item's kind is
    /// [`ItemKind::Special`] (plan 7.5 S15).
    SpecialFile,
    /// The user canceled the item or its job. Any `.scpto-part` is kept and
    /// the item can be retried or its partial discarded.
    Canceled,
}

/// Lifecycle state of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemState {
    /// Waiting for a concurrency slot.
    Queued,
    /// Transferring; `bytes` written to the destination so far, including a
    /// resumed prefix.
    Running {
        /// Bytes written so far.
        bytes: u64,
    },
    /// Paused by the user.
    Paused,
    /// Interrupted by a lost connection; resumes after reconnect.
    WaitingForReconnect,
    /// The destination exists; waiting for a [`ConflictDecision`].
    WaitingForConflict,
    /// Finished successfully.
    Done,
    /// Not transferred.
    Skipped {
        /// Why.
        reason: SkipReason,
    },
    /// Failed; the error is kept and the item can be retried.
    Failed {
        /// What went wrong.
        error: Error,
    },
}

impl ItemState {
    /// Whether the item reached a final state (Done, Skipped, Failed).
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            ItemState::Done | ItemState::Skipped { .. } | ItemState::Failed { .. }
        )
    }
}

/// Answer options of the per-item conflict prompt (plan 2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    /// Replace the destination.
    Overwrite,
    /// Leave the destination untouched.
    Skip,
    /// Write to `name (1).ext` (first free number).
    Rename,
    /// Replace only when the source mtime is newer, by whole seconds beyond
    /// `mtime-tolerance`.
    OverwriteIfNewer,
    /// Continue an existing matching `.scpto-part`. Offered only when
    /// [`Conflict::partial`] is present.
    Resume,
}

/// The user's answer to a conflict prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictDecision {
    /// The chosen option.
    pub choice: ConflictChoice,
    /// Apply the choice to every later conflict in the same job.
    pub apply_to_all: bool,
}

/// Answer options of the job-level prompt used in tar mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TarConflictChoice {
    /// Overwrite every existing destination.
    OverwriteAll,
    /// Cancel the job.
    Cancel,
}

/// A resumable partial file left at the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialInfo {
    /// Bytes already present in the `.scpto-part` file.
    pub size: u64,
}

/// Source attributes recorded for a `.scpto-part` in `partials.json`
/// (plan 7.5 S12). Resume is allowed only when both still match the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialRecord {
    /// Source size when the partial was started.
    pub source_size: u64,
    /// Source mtime (whole seconds) when the partial was started.
    pub source_mtime: i64,
}

/// A conflict raised for one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The waiting item.
    pub item: ItemId,
    /// Its job.
    pub job: JobId,
    /// Source attributes.
    pub source: Entry,
    /// Attributes of the existing destination (actual stat, plan 7.5).
    pub destination: Entry,
    /// A matching resumable partial, when one exists.
    pub partial: Option<PartialInfo>,
}

/// Item counts of a finished job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JobSummary {
    /// Items done.
    pub done: usize,
    /// Items skipped.
    pub skipped: usize,
    /// Items failed.
    pub failed: usize,
}

/// What a queue control applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueTarget {
    /// One item.
    Item(ItemId),
    /// Every item of a job.
    Job(JobId),
}

/// Control messages from the app to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferCommand {
    /// Create a job.
    Submit(JobRequest),
    /// Pause running or queued items.
    Pause(QueueTarget),
    /// Resume paused items.
    Resume(QueueTarget),
    /// Cancel items; they become `Skipped { Canceled }`, partials are kept.
    Cancel(QueueTarget),
    /// Re-queue failed or canceled items.
    Retry(QueueTarget),
    /// Re-queue every failed item of every job.
    RetryAllFailed,
    /// Delete the `.scpto-part` file and its record for canceled or failed
    /// items.
    DiscardPartial(QueueTarget),
    /// Answer a per-item conflict prompt.
    ResolveConflict {
        /// The waiting item.
        item: ItemId,
        /// The answer.
        decision: ConflictDecision,
    },
    /// Answer a tar-mode job-level conflict prompt.
    ResolveTarConflict {
        /// The waiting job.
        job: JobId,
        /// The answer.
        choice: TarConflictChoice,
    },
    /// An endpoint is connected again; items waiting for it resume. This is
    /// only a notification: the engine fetches fresh handles through
    /// `Connections::fs`.
    EndpointReconnected(Endpoint),
    /// Drop finished jobs from the queue.
    ClearFinished,
}

/// Events from the engine to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEvent {
    /// A job was accepted; items follow once its sources are expanded.
    JobCreated {
        /// The job.
        job: JobId,
        /// Copy or move.
        kind: JobKind,
        /// The chosen mode; shown by the queue panel for the job's lifetime.
        mode: TransferMode,
        /// Source endpoint.
        source: Endpoint,
        /// Destination endpoint.
        destination: Endpoint,
    },
    /// Items discovered while expanding the job's sources.
    ItemsAdded {
        /// The job.
        job: JobId,
        /// The new items in their initial state.
        items: Vec<Item>,
    },
    /// An item changed state (including progress while running; the engine
    /// coalesces progress to a few updates per second).
    ItemChanged {
        /// The item.
        item: ItemId,
        /// The new state.
        state: ItemState,
    },
    /// An item's non-fatal metadata warning changed independently of its state.
    ItemWarning {
        /// The item.
        item: ItemId,
        /// The warning, or `None` after a retry clears it.
        warning: Option<Error>,
    },
    /// An item's destination path changed (Rename choice).
    ItemRenamed {
        /// The item.
        item: ItemId,
        /// The new final destination.
        destination: PathBuf,
    },
    /// An item waits for a conflict decision.
    ConflictRaised(Box<Conflict>),
    /// A tar-mode job found existing destinations and waits for a
    /// [`TarConflictChoice`]. Its dialog is the one that tells the user tar
    /// mode has no `.scpto-part` protection and no resume.
    TarConflictRaised {
        /// The job.
        job: JobId,
        /// Destination paths that already exist.
        existing: Vec<PathBuf>,
    },
    /// The job could not run at all (source expansion or tar stream setup
    /// failed).
    JobFailed {
        /// The job.
        job: JobId,
        /// What went wrong.
        error: Error,
    },
    /// Every item of the job is finished.
    JobFinished {
        /// The job.
        job: JobId,
        /// Item counts.
        summary: JobSummary,
    },
    /// Finished jobs were removed from the queue.
    JobsCleared(Vec<JobId>),
}

/// Sending half of the engine event channel.
pub type TransferEventTx = mpsc::Sender<TransferEvent>;

/// Receiving half of the engine event channel.
pub type TransferEventRx = mpsc::Receiver<TransferEvent>;

/// Sending half of the engine command channel.
pub type TransferCommandTx = mpsc::UnboundedSender<TransferCommand>;

/// Receiving half of the engine command channel.
pub type TransferCommandRx = mpsc::UnboundedReceiver<TransferCommand>;
