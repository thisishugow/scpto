//! Transfers in the app: building job requests from copy, move, drag and
//! drop, and Finder drops; the queue mirror built from `TransferEvent`s;
//! queue panel controls; conflict prompts; and the confirmation before
//! quitting while transfers run.
//!
//! # Behavior
//!
//! - `Copy` / `Move`: the focused pane's targets (`Pane::targets`) into the
//!   other pane's current directory as one `JobTargets::IntoDirectory` job
//!   (`Effect::Transfer(TransferCommand::Submit)`, id from
//!   `CoreState::next_job`). Same endpoint and same directory on both sides
//!   is refused with a status message. The job is pushed onto
//!   [`TransferState::jobs`] at once (mode `None`, no items) so the queue
//!   shows it while the engine expands it.
//! - Drag and drop: `DragStart` records [`Drag`] with
//!   `Pane::drag_sources`; `DragMove` updates its target and pointer;
//!   `Drop` on the other pane (`DropTarget::Pane` → its directory,
//!   `DropTarget::Folder` → that folder) copies; on a folder row of the
//!   source pane it moves (same endpoint); `DropTarget::Invalid` and
//!   `DragCancel` end the drag without effect. A drag over a quick-view pane
//!   is `Invalid` (hit map), so nothing is dropped there.
//! - Finder drop: `Paste` in browse or queue mode splits the text with
//!   `ui::input::split_dropped_paths`; `Some(paths)` →
//!   `Effect::CheckDroppedPaths`. `DroppedPathsChecked { all_exist: true }`
//!   pushes a [`DialogPurpose::FinderDrop`] dialog ("Upload N items to
//!   `<dir>`", buttons Upload / Cancel) whose destination is the focused
//!   pane's directory when it is remote, else the other pane's when that one
//!   is remote, else the focused pane's (plan 2.4). `all_exist: false` and
//!   `None` ignore the paste.
//! - Queue mirror: `TransferEvent`s update [`TransferState::jobs`] in place.
//!   `JobCreated` for an id already in the list (a submitted job, or a sync
//!   job pushed by the sync feature) fills in its mode and endpoints instead
//!   of adding a second entry. `JobFinished` / `JobFailed` set
//!   [`QueueJob::outcome`], put a summary in the status bar, and refresh
//!   every pane showing the job's destination endpoint (and the source
//!   endpoint for a move) with `App::refresh`.
//! - Conflicts: `ConflictRaised` pushes a [`DialogPurpose::Conflict`] dialog
//!   (buttons Overwrite, Skip, Rename, OverwriteIfNewer, and Resume when
//!   `Conflict::partial` is present; checkbox "Apply to all in this job");
//!   `TarConflictRaised` pushes [`DialogPurpose::TarConflict`] (buttons
//!   OverwriteAll / Cancel; the message lists the existing names and says
//!   tar mode has no `.scpto-part` protection and no resume). Answers come
//!   back through `App::transfer_dialog_answer` and become
//!   `TransferCommand::ResolveConflict` / `ResolveTarConflict`; Cancel on a
//!   per-item conflict is `Skip` for that item.
//! - Queue controls (queue mode keys, row buttons from any mode, clicks on
//!   queue rows): `QueueControl` on the cursor row or the clicked target →
//!   `TransferCommand::{Pause, Resume, Cancel, Retry, DiscardPartial,
//!   RetryAllFailed, ClearFinished}`; `TogglePause` resumes paused targets
//!   and pauses others. `Cancel` on a job with [`QueueJob::sync`] set is
//!   `Effect::CancelSync`.
//! - Quit: `App::transfer_quit_requested` answers `[Effect::Quit]` when no
//!   job is unfinished, else pushes a [`DialogPurpose::QuitWhileRunning`]
//!   dialog (Yes / No; "N transfers are running; partial files are kept for
//!   resume") and answers no effects. Core routes its `Yes` to
//!   `Effect::Quit`.
//!
//! Rows of the queue panel are the jobs in [`TransferState::jobs`] order,
//! each followed by its items; [`TransferState::cursor`] and
//! [`TransferState::first_visible`] index that flattened row list.
//!
//! # Public API (Wave 3, ui/app/transfer)
//!
//! ```text
//! impl App {
//!     /// Claims `Copy`, `Move`, `DragStart`, `DragMove`, `Drop`,
//!     /// `DragCancel`, `Paste` (browse and queue modes), `Queue { .. }`,
//!     /// and in queue mode `MoveCursor`, `Click`/`DoubleClick` on a
//!     /// `ClickTarget::QueueRow`, `Scroll { region: ScrollRegion::Queue }`;
//!     /// hands every other action back unchanged.
//!     pub fn transfer_action(&mut self, action: Action) -> Result<Vec<Effect>, Action>;
//!     /// `BgEvent::Transfer`.
//!     pub fn transfer_event(&mut self, event: TransferEvent) -> Vec<Effect>;
//!     /// `BgEvent::DroppedPathsChecked`.
//!     pub fn transfer_dropped_paths_checked(&mut self, text: String, paths: Vec<PathBuf>, all_exist: bool) -> Vec<Effect>;
//!     /// Answers of `DialogPurpose::{Conflict, TarConflict, FinderDrop}`.
//!     pub fn transfer_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect>;
//!     /// `Action::Quit` in any mode that quits: see Behavior.
//!     pub fn transfer_quit_requested(&mut self) -> Vec<Effect>;
//!
//!     /// View accessors for render. `texts` and `rows` are per-frame
//!     /// scratch buffers owned by render: the accessor first pushes every
//!     /// row's label and state text (item labels come from the
//!     /// destination's file name, state texts such as `running 45%` or
//!     /// `failed: <error>` are formatted here) into `texts`, then reborrows
//!     /// it shared and fills `rows` with views borrowing those strings.
//!     pub fn queue_view<'a>(&'a self, texts: &'a mut Vec<String>, rows: &'a mut Vec<QueueRowView<'a>>) -> QueueView<'a>;
//!     /// The floating label while dragging.
//!     pub fn drag_label_view(&self) -> Option<DragLabelView>;
//!     /// The drop target to highlight in `side`'s pane.
//!     pub fn drop_target(&self, side: PaneSide) -> Option<DropTarget>;
//!     /// Status bar summary, for example `2 running, 1 failed`; empty when
//!     /// the queue is idle.
//!     pub fn transfer_summary(&self) -> String;
//! }
//! ```
//!
//! Other features read [`TransferState::jobs`] directly: a job whose
//! `outcome` is `None` is unfinished and keeps its endpoints connected
//! (`App::connection_release_unused`). The sync feature pushes its own
//! [`QueueJob`] (with `sync: true`) when it emits `Effect::Sync`.
//!
//! [`DialogPurpose::FinderDrop`]: super::dialog::DialogPurpose::FinderDrop
//! [`DialogPurpose::Conflict`]: super::dialog::DialogPurpose::Conflict
//! [`DialogPurpose::TarConflict`]: super::dialog::DialogPurpose::TarConflict
//! [`DialogPurpose::QuitWhileRunning`]: super::dialog::DialogPurpose::QuitWhileRunning

use std::path::PathBuf;

use crate::error::Error;
use crate::transfer::model::{Item, JobId, JobKind, JobSummary, TransferMode};
use crate::ui::action::{DropTarget, PaneSide};
use crate::vfs::Endpoint;

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct TransferState {
    /// The queue mirror in submission order.
    pub jobs: Vec<QueueJob>,
    /// Queue panel cursor: index into the flattened rows (each job followed
    /// by its items).
    pub cursor: usize,
    /// First flattened row shown in the queue panel.
    pub first_visible: usize,
    /// The drag in progress.
    pub drag: Option<Drag>,
}

/// One job as the queue panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueJob {
    /// The job id (allocated by the app).
    pub id: JobId,
    /// Copy or move.
    pub kind: JobKind,
    /// The engine's mode once `JobCreated` arrived; `None` while the job is
    /// being expanded and for the whole life of an rsync sync (which shows
    /// indeterminate progress).
    pub mode: Option<TransferMode>,
    /// Source endpoint.
    pub source: Endpoint,
    /// Destination endpoint.
    pub destination: Endpoint,
    /// Row label, for example `copy 3 items -> web:/srv` or `sync -> web:/srv`.
    pub label: String,
    /// Started by `Effect::Sync`: its `Cancel` is `Effect::CancelSync`.
    pub sync: bool,
    /// Items as last reported by the engine.
    pub items: Vec<Item>,
    /// `None` while the job is unfinished.
    pub outcome: Option<JobOutcome>,
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// Every item reached a final state (`TransferEvent::JobFinished`, or
    /// a successful `BgEvent::SyncFinished` with a default summary).
    Finished(JobSummary),
    /// The job could not run (`TransferEvent::JobFailed`, or a failed
    /// `BgEvent::SyncFinished`).
    Failed(Error),
}

/// A mouse drag of pane rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drag {
    /// The pane the drag started in.
    pub side: PaneSide,
    /// The dragged paths (`Pane::drag_sources`).
    pub sources: Vec<PathBuf>,
    /// What is under the pointer.
    pub target: DropTarget,
    /// Pointer column, for the floating label.
    pub column: u16,
    /// Pointer row, for the floating label.
    pub row: u16,
}
