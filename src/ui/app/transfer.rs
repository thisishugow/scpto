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
//!   is refused with a status message, and so is copying or moving a
//!   directory into itself or below itself on the same endpoint. The job is
//!   pushed onto [`TransferState::jobs`] at once (mode `None`, no items) so
//!   the queue shows it while the engine expands it.
//! - Drag and drop: `DragStart` records [`Drag`] with
//!   `Pane::drag_sources`; `DragMove` updates its target and pointer. In
//!   single-pane mode a `DragMove` onto the hidden pane's tab
//!   (`DropTarget::Pane` of a pane `CoreState::pane_drawn` says is not on
//!   screen) schedules a [`TAB_HOVER`] timer; when it fires with the drag
//!   still there, that pane is focused (and so drawn) and the drag goes on;
//!   `Drop` on the other pane (`DropTarget::Pane` → its directory,
//!   `DropTarget::Folder` → that folder) copies; on a folder row of the
//!   source pane it moves (same endpoint); `DropTarget::Invalid`, the
//!   source pane outside a folder row, a dragged folder dropped on itself,
//!   and `DragCancel` end the drag without effect. A drag over a quick-view
//!   pane is `Invalid` (hit map), so nothing is dropped there.
//! - Finder drop: `Paste` in browse or queue mode splits the text with
//!   `ui::input::split_dropped_paths`; `Some(paths)` →
//!   `Effect::CheckDroppedPaths`; `None` (text that is not a path list) is
//!   handed back unchanged. `DroppedPathsChecked { all_exist: true }`
//!   pushes a [`DialogPurpose::FinderDrop`] dialog ("Upload N items to
//!   `<dir>`", buttons Upload / Cancel) whose destination is the focused
//!   pane's directory when it is remote, else the other pane's when that one
//!   is remote, else the focused pane's (plan 2.4). `all_exist: false` only
//!   shows a status message.
//! - Queue mirror: `TransferEvent`s update [`TransferState::jobs`] in place.
//!   `JobCreated` for an id already in the list (a submitted job, or a sync
//!   job pushed by the sync feature) fills in its mode and endpoints instead
//!   of adding a second entry. An item that leaves its final state (a retry)
//!   clears its job's outcome. `JobFinished` / `JobFailed` set
//!   [`QueueJob::outcome`], put a summary in the status bar, and refresh
//!   every pane showing the job's destination endpoint (and the source
//!   endpoint for a move) with `App::refresh`. The queue cursor stays on the
//!   same job or item while rows come and go.
//! - Conflicts: `ConflictRaised` pushes a [`DialogPurpose::Conflict`] dialog
//!   (buttons Overwrite, Skip, Rename, OverwriteIfNewer, and Resume when
//!   `Conflict::partial` is present; checkbox "Apply to all in this job");
//!   `TarConflictRaised` pushes [`DialogPurpose::TarConflict`] (buttons
//!   OverwriteAll / Cancel; the message lists the existing names and says
//!   tar mode has no `.scpto-part` protection and no resume). Answers come
//!   back through `App::transfer_dialog_answer` and become
//!   `TransferCommand::ResolveConflict` / `ResolveTarConflict`; Cancel on a
//!   per-item conflict is `Skip` for that item. A conflict dialog whose item
//!   left `WaitingForConflict` (answered by "apply to all", canceled) and a
//!   tar dialog whose job ended are removed from the dialog stack.
//! - Queue controls (queue mode keys, row buttons from any mode, clicks on
//!   queue rows): `QueueControl` on the cursor row or the clicked target →
//!   `TransferCommand::{Pause, Resume, Cancel, Retry, DiscardPartial,
//!   RetryAllFailed, ClearFinished}`; `TogglePause` resumes paused targets
//!   and pauses others. `Cancel` on a job with [`QueueJob::sync`] set is
//!   `Effect::CancelSync`. A row control is sent only when the row offers
//!   it (the buttons of [`App::queue_view`]); otherwise the status bar says
//!   why (tar jobs cannot pause, an rsync sync can only be canceled, ...).
//!   `ClearFinished` also drops finished syncs the engine never knew (rsync
//!   syncs and syncs that failed before their job was submitted).
//! - Quit: `App::transfer_quit_requested` answers `[Effect::Quit]` when no
//!   job is unfinished, else pushes a [`DialogPurpose::QuitWhileRunning`]
//!   dialog (Yes / No, focus on No; "N transfers are running; partial files
//!   are kept for resume", plus a note when a tar job cannot resume) and
//!   answers no effects. Core routes its `Yes` to `Effect::Quit`.
//!
//! Rows of the queue panel are the jobs in [`TransferState::jobs`] order,
//! each followed by its items; [`TransferState::cursor`] and
//! [`TransferState::first_visible`] index that flattened row list.
//!
//! Row buttons: an unfinished per-file job (or one still being expanded)
//! offers pause/resume and cancel; an unfinished tar job or sync without a
//! mode offers cancel; a finished job offers retry (when an item failed or
//! was canceled), discard (per-file jobs), and clear done. Item rows offer
//! pause/resume (per-file) and cancel while unfinished, and retry plus
//! discard (per-file files) once failed or canceled.
//!
//! # Public API (Wave 3, ui/app/transfer)
//!
//! ```text
//! impl App {
//!     /// Claims `Copy`, `Move`, `DragStart`, `DragMove`, `Drop`,
//!     /// `DragCancel`, `Paste` of a path list (browse and queue modes),
//!     /// `Queue { .. }`, `Scroll { region: ScrollRegion::Queue }`, and in
//!     /// queue mode `MoveCursor` and `Click`/`DoubleClick` on a
//!     /// `ClickTarget::QueueRow`; hands every other action back unchanged.
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
//!     /// The collapsed queue panel's row.
//!     pub fn queue_summary(&self) -> String;
//!     /// `BgEvent::TimerFired` of a drag's tab hover; `None` for other
//!     /// timers.
//!     pub fn transfer_timer(&mut self, id: TimerId) -> Option<Vec<Effect>>;
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::core::StatusMessage;
use super::dialog::{Checkbox, Dialog, DialogAnswer, DialogFocus, DialogPurpose};
use super::App;
use crate::error::Error;
use crate::transfer::model::{
    Conflict, ConflictChoice, ConflictDecision, Item, ItemId, ItemKind, ItemState, JobId, JobKind,
    JobRequest, JobSummary, JobTargets, QueueTarget, SkipReason, TarConflictChoice,
    TransferCommand, TransferEvent, TransferMode,
};
use crate::ui::action::{
    Action, ClickTarget, CursorMove, DialogButton, DropTarget, Effect, PaneSide, QueueControl,
    ScrollRegion, TimerId,
};
use crate::ui::input::split_dropped_paths;
use crate::ui::widgets::drag_label::DragLabelView;
use crate::ui::widgets::format::{format_local_mtime, format_size};
use crate::ui::widgets::queue::{Progress, QueueRowView, QueueView};
use crate::vfs::{Endpoint, Entry, Location};

/// Rows of the queue panel taken by its border.
const QUEUE_CHROME_ROWS: u16 = 2;

/// Existing names listed in the tar conflict dialog before "and N more".
const TAR_CONFLICT_NAMES: usize = 5;

const PAUSE_CANCEL: &[QueueControl] = &[QueueControl::TogglePause, QueueControl::Cancel];
const CANCEL: &[QueueControl] = &[QueueControl::Cancel];
const RETRY: &[QueueControl] = &[QueueControl::Retry];
const RETRY_DISCARD: &[QueueControl] = &[QueueControl::Retry, QueueControl::DiscardPartial];
const RETRY_CLEAR: &[QueueControl] = &[QueueControl::Retry, QueueControl::ClearFinished];
const RETRY_DISCARD_CLEAR: &[QueueControl] = &[
    QueueControl::Retry,
    QueueControl::DiscardPartial,
    QueueControl::ClearFinished,
];
const CLEAR: &[QueueControl] = &[QueueControl::ClearFinished];
const NO_CONTROLS: &[QueueControl] = &[];

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
    /// The hidden pane whose tab the pointer rests on (single-pane mode),
    /// with the timer that switches to it after [`TAB_HOVER`].
    pub hover: Option<(PaneSide, TimerId)>,
}

/// How long a drag rests on the hidden pane's tab before that pane is shown
/// (plan 8.8).
pub const TAB_HOVER: Duration = Duration::from_millis(400);

impl QueueJob {
    /// An rsync sync (or a sync whose engine job has not been created): no
    /// items, no pause, no retry.
    fn is_modeless_sync(&self) -> bool {
        self.sync && self.mode.is_none()
    }

    fn item(&self, id: ItemId) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }

    /// Whether an item failed or was canceled, so the job can be retried.
    fn has_retryable_items(&self) -> bool {
        self.items.iter().any(|item| is_retryable(&item.state))
    }

    /// A job counts as paused when something is paused and nothing is
    /// queued or running.
    fn is_paused(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.state == ItemState::Paused)
            && !self
                .items
                .iter()
                .any(|item| matches!(item.state, ItemState::Queued | ItemState::Running { .. }))
    }
}

impl TransferState {
    fn job(&self, id: JobId) -> Option<&QueueJob> {
        self.jobs.iter().find(|job| job.id == id)
    }

    fn job_mut(&mut self, id: JobId) -> Option<&mut QueueJob> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    /// The job owning item `id`.
    fn job_of_item_mut(&mut self, id: ItemId) -> Option<&mut QueueJob> {
        self.jobs
            .iter_mut()
            .find(|job| job.items.iter().any(|item| item.id == id))
    }

    /// The job and (for an item target) the item a queue target names.
    fn resolve(&self, target: QueueTarget) -> Option<(&QueueJob, Option<&Item>)> {
        match target {
            QueueTarget::Job(id) => self.job(id).map(|job| (job, None)),
            QueueTarget::Item(id) => self
                .jobs
                .iter()
                .find_map(|job| job.item(id).map(|item| (job, Some(item)))),
        }
    }

    /// The flattened queue rows: each job followed by its items.
    fn rows(&self) -> Vec<QueueTarget> {
        let mut rows = Vec::new();
        for job in &self.jobs {
            rows.push(QueueTarget::Job(job.id));
            rows.extend(job.items.iter().map(|item| QueueTarget::Item(item.id)));
        }
        rows
    }

    fn row_count(&self) -> usize {
        self.jobs.iter().map(|job| 1 + job.items.len()).sum()
    }

    fn cursor_target(&self) -> Option<QueueTarget> {
        self.rows().get(self.cursor).copied()
    }

    /// Keeps `first_visible` such that the cursor row is on a page of
    /// `page` rows and no page ends past the last row.
    fn keep_cursor_visible(&mut self, page: usize) {
        let len = self.row_count();
        self.cursor = self.cursor.min(len.saturating_sub(1));
        if self.cursor < self.first_visible {
            self.first_visible = self.cursor;
        } else if self.cursor >= self.first_visible + page {
            self.first_visible = self.cursor + 1 - page;
        }
        self.first_visible = self.first_visible.min(len.saturating_sub(page));
    }

    /// Puts the cursor back on `target` when it still has a row, else keeps
    /// the index (clamped).
    fn place_cursor(&mut self, target: Option<QueueTarget>, page: usize) {
        if let Some(index) =
            target.and_then(|target| self.rows().iter().position(|row| *row == target))
        {
            self.cursor = index;
        }
        self.keep_cursor_visible(page);
    }

    fn move_cursor(&mut self, movement: CursorMove, page: usize) {
        let last = self.row_count().saturating_sub(1);
        self.cursor = match movement {
            CursorMove::Up => self.cursor.saturating_sub(1),
            CursorMove::Down => (self.cursor + 1).min(last),
            CursorMove::PageUp => self.cursor.saturating_sub(page),
            CursorMove::PageDown => (self.cursor + page).min(last),
            CursorMove::Home => 0,
            CursorMove::End => last,
        };
        self.keep_cursor_visible(page);
    }

    /// Mouse wheel: moves the visible window, dragging the cursor along
    /// only when it would leave the page.
    fn scroll(&mut self, lines: i32, page: usize) {
        let max_first = self.row_count().saturating_sub(page);
        let delta = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
        self.first_visible = if lines < 0 {
            self.first_visible.saturating_sub(delta)
        } else {
            self.first_visible.saturating_add(delta).min(max_first)
        };
        let last_shown = self.first_visible + page - 1;
        self.cursor = self.cursor.clamp(self.first_visible, last_shown);
        self.keep_cursor_visible(page);
    }
}

impl App {
    /// Claims `Copy`, `Move`, `DragStart`, `DragMove`, `Drop`, `DragCancel`,
    /// `Paste` of a path list (browse and queue modes), `Queue { .. }`,
    /// `Scroll { region: ScrollRegion::Queue }`, and in queue mode
    /// `MoveCursor` and `Click`/`DoubleClick` on a `ClickTarget::QueueRow`;
    /// hands every other action back unchanged.
    pub fn transfer_action(&mut self, action: Action) -> Result<Vec<Effect>, Action> {
        let queue_mode = self.core.queue_focused();
        let page = self.queue_page();
        match action {
            Action::Copy => Ok(self.copy_or_move(JobKind::Copy)),
            Action::Move => Ok(self.copy_or_move(JobKind::Move)),
            Action::DragStart { side, index, .. } => {
                let sources = self.core.pane(side).drag_sources(index);
                self.transfer.drag = (!sources.is_empty()).then_some(Drag {
                    side,
                    sources,
                    target: DropTarget::Invalid,
                    column: 0,
                    row: 0,
                    hover: None,
                });
                Ok(Vec::new())
            }
            Action::DragMove {
                target,
                column,
                row,
            } => Ok(self.drag_moved(target, column, row)),
            Action::Drop { target, .. } => Ok(self.drop_drag(target)),
            Action::DragCancel => {
                self.transfer.drag = None;
                Ok(Vec::new())
            }
            Action::Paste(text) => match split_dropped_paths(&text) {
                Some(paths) => Ok(vec![Effect::CheckDroppedPaths { text, paths }]),
                None => Err(Action::Paste(text)),
            },
            Action::Queue { control, target } => Ok(self.queue_control(control, target)),
            Action::Scroll {
                region: ScrollRegion::Queue,
                lines,
            } => {
                self.transfer.scroll(lines, page);
                Ok(Vec::new())
            }
            Action::MoveCursor(movement) if queue_mode => {
                self.transfer.move_cursor(movement, page);
                Ok(Vec::new())
            }
            Action::Click {
                target: ClickTarget::QueueRow(target),
                ..
            }
            | Action::DoubleClick {
                target: ClickTarget::QueueRow(target),
                ..
            } if queue_mode => {
                self.transfer.place_cursor(Some(target), page);
                Ok(Vec::new())
            }
            other => Err(other),
        }
    }

    /// `BgEvent::Transfer`.
    pub fn transfer_event(&mut self, event: TransferEvent) -> Vec<Effect> {
        let keep = self.transfer.cursor_target();
        let effects = match event {
            TransferEvent::JobCreated {
                job,
                kind,
                mode,
                source,
                destination,
            } => {
                match self.transfer.job_mut(job) {
                    Some(entry) => {
                        entry.kind = kind;
                        entry.mode = Some(mode);
                        entry.source = source;
                        entry.destination = destination;
                    }
                    None => {
                        let label = format!("{} {source} -> {destination}", verb(kind));
                        self.transfer.jobs.push(QueueJob {
                            id: job,
                            kind,
                            mode: Some(mode),
                            source,
                            destination,
                            label,
                            sync: false,
                            items: Vec::new(),
                            outcome: None,
                        });
                    }
                }
                Vec::new()
            }
            TransferEvent::ItemsAdded { job, items } => {
                if let Some(entry) = self.transfer.job_mut(job) {
                    entry.items.extend(items);
                }
                Vec::new()
            }
            TransferEvent::ItemChanged { item, state } => {
                self.item_changed(item, state);
                Vec::new()
            }
            TransferEvent::ItemRenamed { item, destination } => {
                if let Some(entry) = self
                    .transfer
                    .job_of_item_mut(item)
                    .and_then(|job| job.items.iter_mut().find(|entry| entry.id == item))
                {
                    entry.destination = destination;
                }
                Vec::new()
            }
            TransferEvent::ConflictRaised(conflict) => {
                self.push_conflict_dialog(&conflict);
                Vec::new()
            }
            TransferEvent::TarConflictRaised { job, existing } => {
                self.push_tar_conflict_dialog(job, &existing);
                Vec::new()
            }
            TransferEvent::JobFailed { job, error } => {
                self.job_ended(job, JobOutcome::Failed(error))
            }
            TransferEvent::JobFinished { job, summary } => {
                self.job_ended(job, JobOutcome::Finished(summary))
            }
            TransferEvent::JobsCleared(cleared) => {
                self.transfer.jobs.retain(|job| !cleared.contains(&job.id));
                Vec::new()
            }
        };
        let page = self.queue_page();
        self.transfer.place_cursor(keep, page);
        effects
    }

    /// `BgEvent::DroppedPathsChecked`. The pasted text itself is not needed:
    /// text that is not a path list never reaches the check.
    pub fn transfer_dropped_paths_checked(
        &mut self,
        _text: String,
        paths: Vec<PathBuf>,
        all_exist: bool,
    ) -> Vec<Effect> {
        if !all_exist {
            self.core.set_status(StatusMessage::info(format!(
                "Ignored dropped {}: not every path exists locally",
                if paths.len() == 1 { "path" } else { "paths" }
            )));
            return Vec::new();
        }
        let Some(destination) = self.finder_drop_destination() else {
            self.core.set_status(StatusMessage::error(
                "Cannot upload dropped files: no directory is listed yet",
            ));
            return Vec::new();
        };
        let message = format!(
            "Upload {} to {}?",
            count_noun(paths.len(), "item"),
            describe(&destination.endpoint, &destination.path)
        );
        self.dialog.stack.push(Dialog {
            purpose: DialogPurpose::FinderDrop { paths, destination },
            title: "Upload".into(),
            message,
            field: None,
            checkbox: None,
            buttons: vec![DialogButton::Upload, DialogButton::Cancel],
            focus: DialogFocus::Button(0),
        });
        Vec::new()
    }

    /// Answers of `DialogPurpose::{Conflict, TarConflict, FinderDrop}`;
    /// answers of other purposes are not this feature's and are ignored.
    pub fn transfer_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect> {
        match answer.purpose {
            DialogPurpose::Conflict { item, partial, .. } => {
                let choice = match answer.button {
                    DialogButton::Overwrite => Some(ConflictChoice::Overwrite),
                    DialogButton::Skip => Some(ConflictChoice::Skip),
                    DialogButton::Rename => Some(ConflictChoice::Rename),
                    DialogButton::OverwriteIfNewer => Some(ConflictChoice::OverwriteIfNewer),
                    DialogButton::Resume if partial => Some(ConflictChoice::Resume),
                    // Esc (and any button the dialog does not show) skips
                    // this one item only.
                    _ => None,
                };
                let decision = match choice {
                    Some(choice) => ConflictDecision {
                        choice,
                        apply_to_all: answer.checked,
                    },
                    None => ConflictDecision {
                        choice: ConflictChoice::Skip,
                        apply_to_all: false,
                    },
                };
                vec![Effect::Transfer(TransferCommand::ResolveConflict {
                    item,
                    decision,
                })]
            }
            DialogPurpose::TarConflict { job } => {
                let choice = match answer.button {
                    DialogButton::OverwriteAll => TarConflictChoice::OverwriteAll,
                    _ => TarConflictChoice::Cancel,
                };
                vec![Effect::Transfer(TransferCommand::ResolveTarConflict {
                    job,
                    choice,
                })]
            }
            DialogPurpose::FinderDrop { paths, destination } => match answer.button {
                DialogButton::Upload => {
                    self.submit(JobKind::Copy, Endpoint::Local, paths, destination)
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// `Action::Quit` in any mode that quits: `[Effect::Quit]` when no job is
    /// unfinished, else the confirmation dialog and no effects.
    pub fn transfer_quit_requested(&mut self) -> Vec<Effect> {
        let unfinished: Vec<&QueueJob> = self
            .transfer
            .jobs
            .iter()
            .filter(|job| job.outcome.is_none())
            .collect();
        if unfinished.is_empty() {
            return vec![Effect::Quit];
        }
        let running = match unfinished.len() {
            1 => "1 transfer is running".to_string(),
            n => format!("{n} transfers are running"),
        };
        let tar_note = if unfinished
            .iter()
            .any(|job| job.mode == Some(TransferMode::Tar))
        {
            " Tar transfers cannot resume and start over."
        } else {
            ""
        };
        let message =
            format!("{running}; partial files are kept for resume.{tar_note}\n\nQuit anyway?");
        self.dialog.stack.push(Dialog {
            purpose: DialogPurpose::QuitWhileRunning,
            title: "Quit".into(),
            message,
            field: None,
            checkbox: None,
            buttons: vec![DialogButton::Yes, DialogButton::No],
            // Enter right after `q` must not throw transfers away.
            focus: DialogFocus::Button(1),
        });
        Vec::new()
    }

    /// The queue panel view. `texts` and `rows` are per-frame scratch
    /// buffers owned by render: every row's label and state text is pushed
    /// into `texts` first, then `rows` is filled with views borrowing them.
    pub fn queue_view<'a>(
        &'a self,
        texts: &'a mut Vec<String>,
        rows: &'a mut Vec<QueueRowView<'a>>,
    ) -> QueueView<'a> {
        texts.clear();
        rows.clear();
        let mut shapes = Vec::with_capacity(self.transfer.row_count());
        for job in &self.transfer.jobs {
            texts.push(job_label(job));
            texts.push(self.job_state_text(job));
            shapes.push((
                QueueTarget::Job(job.id),
                false,
                job_progress(job),
                job_controls(job),
            ));
            for item in &job.items {
                texts.push(item_label(item));
                texts.push(item_state_text(&item.state));
                shapes.push((
                    QueueTarget::Item(item.id),
                    true,
                    item_progress(item),
                    item_controls(job, item),
                ));
            }
        }
        let texts: &'a Vec<String> = texts;
        for (index, (target, indent, progress, controls)) in shapes.into_iter().enumerate() {
            rows.push(QueueRowView {
                target,
                indent,
                label: &texts[2 * index],
                state: &texts[2 * index + 1],
                progress,
                controls,
            });
        }
        let rows: &'a Vec<QueueRowView<'a>> = rows;
        QueueView {
            rows,
            cursor: (!rows.is_empty()).then_some(self.transfer.cursor),
            first_visible: self.transfer.first_visible,
            focused: self.core.queue_focused(),
        }
    }

    /// The floating label while dragging.
    pub fn drag_label_view(&self) -> Option<DragLabelView> {
        self.transfer.drag.as_ref().map(|drag| DragLabelView {
            count: drag.sources.len(),
            column: drag.column,
            row: drag.row,
        })
    }

    /// The drop target to highlight in `side`'s pane: the drag's target when
    /// it lies in that pane and releasing there would transfer something.
    pub fn drop_target(&self, side: PaneSide) -> Option<DropTarget> {
        let drag = self.transfer.drag.as_ref()?;
        let in_side = match drag.target {
            DropTarget::Pane(target) | DropTarget::Folder { side: target, .. } => target == side,
            DropTarget::Invalid => false,
        };
        (in_side && self.drop_destination(drag, drag.target).is_some()).then_some(drag.target)
    }

    /// `BgEvent::TimerFired` of a drag's tab hover: shows the hovered pane
    /// when the drag still rests on its tab. `None` for other timers.
    pub fn transfer_timer(&mut self, id: TimerId) -> Option<Vec<Effect>> {
        let drag = self.transfer.drag.as_mut()?;
        let (side, timer) = drag.hover?;
        if timer != id {
            return None;
        }
        drag.hover = None;
        if drag.target == DropTarget::Pane(side) {
            self.core.focus(side);
        }
        Some(Vec::new())
    }

    /// The collapsed queue panel's row, for example
    /// `Queue: 2 running, 45% of 1.2 GiB`.
    pub fn queue_summary(&self) -> String {
        let summary = self.transfer_summary();
        if summary.is_empty() {
            return "Queue: no transfers".to_owned();
        }
        let (done, total) = self
            .transfer
            .jobs
            .iter()
            .filter(|job| job.outcome.is_none())
            .map(job_bytes)
            .fold((0, 0), |(done, total), (d, t)| (done + d, total + t));
        if total == 0 {
            return format!("Queue: {summary}");
        }
        format!(
            "Queue: {summary}, {}% of {}",
            done.saturating_mul(100) / total,
            format_size(total)
        )
    }

    /// Status bar summary, for example `2 running, 1 failed`; empty when the
    /// queue is idle.
    pub fn transfer_summary(&self) -> String {
        let jobs = &self.transfer.jobs;
        let unfinished = jobs.iter().filter(|job| job.outcome.is_none());
        let paused = unfinished.clone().filter(|job| job.is_paused()).count();
        let running = unfinished.count() - paused;
        let failed = jobs
            .iter()
            .filter(|job| match &job.outcome {
                Some(JobOutcome::Failed(_)) => true,
                Some(JobOutcome::Finished(summary)) => summary.failed > 0,
                None => false,
            })
            .count();
        [(running, "running"), (paused, "paused"), (failed, "failed")]
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, word)| format!("{count} {word}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    // Drag and drop.

    /// `DragMove`: records the target and pointer; resting on the hidden
    /// pane's tab (the only place a `DropTarget::Pane` of an undrawn pane
    /// comes from) schedules the switch to it.
    fn drag_moved(&mut self, target: DropTarget, column: u16, row: u16) -> Vec<Effect> {
        let hidden_tab = match target {
            DropTarget::Pane(side) if !self.core.pane_drawn(side) => Some(side),
            _ => None,
        };
        let Some(drag) = &self.transfer.drag else {
            return Vec::new();
        };
        let keep_hover = drag.hover.filter(|&(side, _)| Some(side) == hidden_tab);
        let (hover, effects) = match (keep_hover, hidden_tab) {
            (Some(hover), _) => (Some(hover), Vec::new()),
            (None, Some(side)) => {
                let id = self.core.next_timer();
                (
                    Some((side, id)),
                    vec![Effect::ScheduleTimer {
                        id,
                        after: TAB_HOVER,
                    }],
                )
            }
            (None, None) => (None, Vec::new()),
        };
        if let Some(drag) = &mut self.transfer.drag {
            drag.target = target;
            drag.column = column;
            drag.row = row;
            drag.hover = hover;
        }
        effects
    }

    // Building jobs.

    fn copy_or_move(&mut self, kind: JobKind) -> Vec<Effect> {
        let side = self.core.focused();
        let sources = self.core.pane(side).targets();
        let Some(destination) = self.core.pane(side.other()).location() else {
            self.core.set_status(StatusMessage::error(format!(
                "Cannot {}: the other pane has no directory yet",
                verb(kind)
            )));
            return Vec::new();
        };
        let source = self.core.pane(side).endpoint.clone();
        self.submit(kind, source, sources, destination)
    }

    /// Submits a job transferring `sources` (absolute paths on `source`)
    /// into `destination`, after refusing no-ops and self-nesting.
    fn submit(
        &mut self,
        kind: JobKind,
        source: Endpoint,
        sources: Vec<PathBuf>,
        destination: Location,
    ) -> Vec<Effect> {
        if sources.is_empty() {
            self.core
                .set_status(StatusMessage::info(format!("Nothing to {}", verb(kind))));
            return Vec::new();
        }
        if source == destination.endpoint {
            if sources
                .iter()
                .any(|path| path.parent() == Some(destination.path.as_path()))
            {
                self.core.set_status(StatusMessage::error(
                    "Source and destination are the same directory",
                ));
                return Vec::new();
            }
            if let Some(path) = sources
                .iter()
                .find(|path| destination.path.starts_with(path))
            {
                self.core.set_status(StatusMessage::error(format!(
                    "Cannot {} {} into itself",
                    verb(kind),
                    file_name(path)
                )));
                return Vec::new();
            }
        }
        let what = match sources.as_slice() {
            [single] => file_name(single),
            many => count_noun(many.len(), "item"),
        };
        let label = format!(
            "{} {what} -> {}",
            verb(kind),
            describe(&destination.endpoint, &destination.path)
        );
        let id = self.core.next_job();
        self.core
            .set_status(StatusMessage::info(format!("Queued {label}")));
        self.transfer.jobs.push(QueueJob {
            id,
            kind,
            mode: None,
            source: source.clone(),
            destination: destination.endpoint.clone(),
            label,
            sync: false,
            items: Vec::new(),
            outcome: None,
        });
        vec![Effect::Transfer(TransferCommand::Submit(JobRequest {
            id,
            kind,
            source,
            destination: destination.endpoint,
            targets: JobTargets::IntoDirectory {
                sources,
                directory: destination.path,
            },
            preset: None,
        }))]
    }

    fn drop_drag(&mut self, target: DropTarget) -> Vec<Effect> {
        let Some(drag) = self.transfer.drag.take() else {
            return Vec::new();
        };
        let Some((kind, destination)) = self.drop_destination(&drag, target) else {
            return Vec::new();
        };
        let source = self.core.pane(drag.side).endpoint.clone();
        self.submit(kind, source, drag.sources, destination)
    }

    /// What releasing `drag` on `target` does: copy into the other pane's
    /// directory or folder, move into a folder of the source pane; `None`
    /// when it does nothing.
    fn drop_destination(&self, drag: &Drag, target: DropTarget) -> Option<(JobKind, Location)> {
        match target {
            DropTarget::Invalid => None,
            DropTarget::Pane(side) if side == drag.side => None,
            DropTarget::Pane(side) => self
                .core
                .pane(side)
                .location()
                .map(|location| (JobKind::Copy, location)),
            DropTarget::Folder { side, index } => {
                let pane = self.core.pane(side);
                if !pane.visible_entry(index)?.is_enterable() {
                    return None;
                }
                let path = pane.path_of(index)?;
                let kind = if side == drag.side {
                    if drag.sources.contains(&path) {
                        return None;
                    }
                    JobKind::Move
                } else {
                    JobKind::Copy
                };
                Some((
                    kind,
                    Location {
                        endpoint: pane.endpoint.clone(),
                        path,
                    },
                ))
            }
        }
    }

    /// Plan 2.4: the focused pane's directory when it is remote, else the
    /// other pane's when that one is remote, else the focused pane's.
    fn finder_drop_destination(&self) -> Option<Location> {
        let focused = self.core.focused();
        let side = if !self.core.pane(focused).endpoint.is_local() {
            focused
        } else if !self.core.pane(focused.other()).endpoint.is_local() {
            focused.other()
        } else {
            focused
        };
        self.core.pane(side).location()
    }

    // Queue mirror.

    fn item_changed(&mut self, id: ItemId, state: ItemState) {
        let waiting = state == ItemState::WaitingForConflict;
        if let Some(job) = self.transfer.job_of_item_mut(id) {
            if !state.is_finished() {
                // A retry re-queues items of a finished job; the engine
                // reports `JobFinished` again later.
                job.outcome = None;
            }
            if let Some(item) = job.items.iter_mut().find(|item| item.id == id) {
                item.state = state;
            }
        }
        if !waiting {
            self.dialog.stack.retain(
                |dialog| !matches!(dialog.purpose, DialogPurpose::Conflict { item, .. } if item == id),
            );
        }
    }

    /// Records a job's outcome, reports it, and refreshes the panes whose
    /// endpoint it changed.
    fn job_ended(&mut self, id: JobId, outcome: JobOutcome) -> Vec<Effect> {
        self.dialog.stack.retain(
            |dialog| !matches!(dialog.purpose, DialogPurpose::TarConflict { job } if job == id),
        );
        let Some(job) = self.transfer.job_mut(id) else {
            return Vec::new();
        };
        let status = match &outcome {
            JobOutcome::Failed(error) => StatusMessage::error(format!("{}: {error}", job.label)),
            JobOutcome::Finished(summary) => {
                let text = format!("{}: {}", job.label, summary_text(summary));
                if summary.failed > 0 {
                    StatusMessage::error(text)
                } else {
                    StatusMessage::info(text)
                }
            }
        };
        job.outcome = Some(outcome);
        let mut changed = vec![job.destination.clone()];
        if job.kind == JobKind::Move {
            changed.push(job.source.clone());
        }
        self.core.set_status(status);
        let mut effects = Vec::new();
        for side in [PaneSide::Left, PaneSide::Right] {
            if changed.contains(&self.core.pane(side).endpoint) {
                effects.extend(self.refresh(side));
            }
        }
        effects
    }

    fn push_conflict_dialog(&mut self, conflict: &Conflict) {
        let item = conflict.item;
        let path = self
            .transfer
            .resolve(QueueTarget::Item(item))
            .and_then(|(job, entry)| {
                entry.map(|entry| describe(&job.destination, &entry.destination))
            })
            .unwrap_or_else(|| conflict.destination.name.to_string_lossy().into_owned());
        let mut message = format!(
            "{path} already exists.\n\nSource:   {}\nExisting: {}",
            entry_attributes(&conflict.source),
            entry_attributes(&conflict.destination)
        );
        let mut buttons = vec![
            DialogButton::Overwrite,
            DialogButton::Skip,
            DialogButton::Rename,
            DialogButton::OverwriteIfNewer,
        ];
        if let Some(partial) = conflict.partial {
            message.push_str(&format!(
                "\n\nA partial file of {} can be resumed.",
                format_size(partial.size)
            ));
            buttons.push(DialogButton::Resume);
        }
        self.dialog.stack.retain(
            |dialog| !matches!(dialog.purpose, DialogPurpose::Conflict { item: open, .. } if open == item),
        );
        self.dialog.stack.push(Dialog {
            purpose: DialogPurpose::Conflict {
                item,
                job: conflict.job,
                partial: conflict.partial.is_some(),
            },
            title: "File exists".into(),
            message,
            field: None,
            checkbox: Some(Checkbox {
                label: "Apply to all in this job".into(),
                checked: false,
            }),
            buttons,
            focus: DialogFocus::Button(0),
        });
    }

    fn push_tar_conflict_dialog(&mut self, job: JobId, existing: &[PathBuf]) {
        let destination = self
            .transfer
            .job(job)
            .map(|entry| entry.destination.clone());
        let mut lines: Vec<String> = existing
            .iter()
            .take(TAR_CONFLICT_NAMES)
            .map(|path| match &destination {
                Some(endpoint) => describe(endpoint, path),
                None => path.display().to_string(),
            })
            .collect();
        if existing.len() > TAR_CONFLICT_NAMES {
            lines.push(format!("and {} more", existing.len() - TAR_CONFLICT_NAMES));
        }
        let message = format!(
            "{} already {}:\n{}\n\nThis job runs as one tar stream: existing files are overwritten \
             without .scpto-part protection, and an interrupted transfer cannot resume.",
            count_noun(existing.len(), "item"),
            if existing.len() == 1 { "exists" } else { "exist" },
            lines.join("\n")
        );
        self.dialog.stack.push(Dialog {
            purpose: DialogPurpose::TarConflict { job },
            title: "Overwrite existing files?".into(),
            message,
            field: None,
            checkbox: None,
            buttons: vec![DialogButton::OverwriteAll, DialogButton::Cancel],
            focus: DialogFocus::Button(0),
        });
    }

    // Queue panel.

    /// Rows of the queue panel's list (at least 1, like a pane page).
    fn queue_page(&self) -> usize {
        self.core
            .layout()
            .queue
            .map_or(1, |area| {
                usize::from(area.height.saturating_sub(QUEUE_CHROME_ROWS))
            })
            .max(1)
    }

    fn queue_control(&mut self, control: QueueControl, target: Option<QueueTarget>) -> Vec<Effect> {
        match control {
            QueueControl::RetryAllFailed => {
                return vec![Effect::Transfer(TransferCommand::RetryAllFailed)];
            }
            QueueControl::ClearFinished => {
                // The engine never knew rsync syncs (and syncs that failed
                // before their job was submitted), so they are dropped here.
                self.transfer
                    .jobs
                    .retain(|job| !(job.is_modeless_sync() && job.outcome.is_some()));
                let page = self.queue_page();
                let keep = self.transfer.cursor_target();
                self.transfer.place_cursor(keep, page);
                return vec![Effect::Transfer(TransferCommand::ClearFinished)];
            }
            QueueControl::TogglePause
            | QueueControl::Cancel
            | QueueControl::Retry
            | QueueControl::DiscardPartial => {}
        }
        let Some(target) = target.or_else(|| self.transfer.cursor_target()) else {
            return Vec::new();
        };
        let Some((job, item)) = self.transfer.resolve(target) else {
            return Vec::new();
        };
        let offered = match item {
            Some(item) => item_controls(job, item),
            None => job_controls(job),
        };
        if !offered.contains(&control) {
            let reason = refusal(control, job);
            self.core.set_status(StatusMessage::info(reason));
            return Vec::new();
        }
        let paused = match item {
            Some(item) => item.state == ItemState::Paused,
            None => job.is_paused(),
        };
        let effect = match control {
            QueueControl::TogglePause if paused => {
                Effect::Transfer(TransferCommand::Resume(target))
            }
            QueueControl::TogglePause => Effect::Transfer(TransferCommand::Pause(target)),
            QueueControl::Cancel => match target {
                QueueTarget::Job(id) if job.sync => Effect::CancelSync(id),
                _ => Effect::Transfer(TransferCommand::Cancel(target)),
            },
            QueueControl::Retry => Effect::Transfer(TransferCommand::Retry(target)),
            QueueControl::DiscardPartial => {
                Effect::Transfer(TransferCommand::DiscardPartial(target))
            }
            QueueControl::RetryAllFailed => Effect::Transfer(TransferCommand::RetryAllFailed),
            QueueControl::ClearFinished => Effect::Transfer(TransferCommand::ClearFinished),
        };
        vec![effect]
    }

    fn job_state_text(&self, job: &QueueJob) -> String {
        match &job.outcome {
            Some(JobOutcome::Failed(error)) => format!("failed: {error}"),
            Some(JobOutcome::Finished(summary)) => finished_state_text(summary),
            None if job.is_modeless_sync() => "syncing".into(),
            None if job.items.is_empty() => "preparing".into(),
            None => {
                let states = || job.items.iter().map(|item| &item.state);
                let tar_waiting = self.dialog.stack.iter().any(|dialog| {
                    matches!(dialog.purpose, DialogPurpose::TarConflict { job: open } if open == job.id)
                });
                if tar_waiting || states().any(|state| *state == ItemState::WaitingForConflict) {
                    "conflict".into()
                } else if states().any(|state| matches!(state, ItemState::Running { .. })) {
                    let (done, total) = job_bytes(job);
                    format!("running {}%", percent(done, total))
                } else if states().any(|state| *state == ItemState::WaitingForReconnect) {
                    "reconnecting".into()
                } else if states().any(|state| *state == ItemState::Queued) {
                    "queued".into()
                } else if states().any(|state| *state == ItemState::Paused) {
                    "paused".into()
                } else {
                    // Every item is final; `JobFinished` is on its way.
                    "finishing".into()
                }
            }
        }
    }
}

/// Why a row does not offer `control`.
fn refusal(control: QueueControl, job: &QueueJob) -> &'static str {
    let tar = job.mode == Some(TransferMode::Tar);
    match control {
        QueueControl::TogglePause if tar => "Tar transfers cannot be paused",
        QueueControl::DiscardPartial if tar => "Tar transfers leave no partial files",
        _ if job.is_modeless_sync() && job.outcome.is_none() => "This sync can only be canceled",
        QueueControl::Retry if job.is_modeless_sync() => {
            "Syncs cannot be retried; compare the directories again"
        }
        QueueControl::Retry if matches!(job.outcome, Some(JobOutcome::Failed(_))) => {
            "This job could not start; start it again"
        }
        QueueControl::TogglePause | QueueControl::Cancel => "This transfer has finished",
        QueueControl::Retry => "Only failed or canceled transfers can be retried",
        QueueControl::DiscardPartial => {
            "Only failed or canceled files have a partial file to discard"
        }
        QueueControl::RetryAllFailed | QueueControl::ClearFinished => {
            "This control applies to the whole queue"
        }
    }
}

/// The buttons of a job row.
fn job_controls(job: &QueueJob) -> &'static [QueueControl] {
    match &job.outcome {
        None => match job.mode {
            Some(TransferMode::PerFile) => PAUSE_CANCEL,
            None if !job.sync => PAUSE_CANCEL,
            Some(TransferMode::Tar) | None => CANCEL,
        },
        Some(JobOutcome::Finished(_)) if job.has_retryable_items() => match job.mode {
            Some(TransferMode::PerFile) => RETRY_DISCARD_CLEAR,
            Some(TransferMode::Tar) | None => RETRY_CLEAR,
        },
        Some(_) => CLEAR,
    }
}

/// The buttons of an item row.
fn item_controls(job: &QueueJob, item: &Item) -> &'static [QueueControl] {
    let per_file = job.mode == Some(TransferMode::PerFile);
    match &item.state {
        ItemState::Queued | ItemState::Running { .. } | ItemState::Paused if per_file => {
            PAUSE_CANCEL
        }
        ItemState::Queued
        | ItemState::Running { .. }
        | ItemState::Paused
        | ItemState::WaitingForReconnect
        | ItemState::WaitingForConflict => CANCEL,
        state if is_retryable(state) && per_file && item.kind == ItemKind::File => RETRY_DISCARD,
        state if is_retryable(state) => RETRY,
        ItemState::Done | ItemState::Skipped { .. } | ItemState::Failed { .. } => NO_CONTROLS,
    }
}

/// Failed or canceled: the engine can re-queue it.
fn is_retryable(state: &ItemState) -> bool {
    matches!(
        state,
        ItemState::Failed { .. }
            | ItemState::Skipped {
                reason: SkipReason::Canceled
            }
    )
}

fn job_label(job: &QueueJob) -> String {
    match job.mode {
        Some(TransferMode::Tar) => format!("{} [tar, no resume]", job.label),
        Some(TransferMode::PerFile) | None => job.label.clone(),
    }
}

/// The destination's file name; directories end in `/`.
fn item_label(item: &Item) -> String {
    let name = file_name(&item.destination);
    match &item.kind {
        ItemKind::Dir => format!("{name}/"),
        ItemKind::File | ItemKind::Symlink { .. } | ItemKind::Special(_) => name,
    }
}

fn item_state_text(state: &ItemState) -> String {
    match state {
        ItemState::Queued => "queued".into(),
        ItemState::Running { .. } => "running".into(),
        ItemState::Paused => "paused".into(),
        ItemState::WaitingForReconnect => "reconnecting".into(),
        ItemState::WaitingForConflict => "conflict".into(),
        ItemState::Done => "done".into(),
        ItemState::Skipped { reason } => match reason {
            SkipReason::UserSkipped => "skipped".into(),
            SkipReason::NotNewer => "not newer".into(),
            SkipReason::SpecialFile => "special file".into(),
            SkipReason::Canceled => "canceled".into(),
        },
        ItemState::Failed { error } => format!("failed: {error}"),
    }
}

fn finished_state_text(summary: &JobSummary) -> String {
    if summary.failed > 0 {
        format!("{} failed", summary.failed)
    } else if summary.skipped > 0 && summary.done == 0 {
        "skipped".into()
    } else if summary.skipped > 0 {
        format!("{} skipped", summary.skipped)
    } else {
        "done".into()
    }
}

/// `3 done, 1 skipped, 2 failed`, leaving out zero counts (`done` for an
/// empty job).
fn summary_text(summary: &JobSummary) -> String {
    let parts: Vec<String> = [
        (summary.done, "done"),
        (summary.skipped, "skipped"),
        (summary.failed, "failed"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, word)| format!("{count} {word}"))
    .collect();
    if parts.is_empty() {
        "done".into()
    } else {
        parts.join(", ")
    }
}

fn item_progress(item: &Item) -> Progress {
    match item.state {
        ItemState::Running { bytes } => Progress::Bytes {
            done: bytes,
            total: item.size,
        },
        _ => Progress::None,
    }
}

fn job_progress(job: &QueueJob) -> Progress {
    match &job.outcome {
        Some(_) => Progress::None,
        None if job.is_modeless_sync() => Progress::Indeterminate,
        None if job.items.is_empty() => Progress::None,
        None => {
            let (done, total) = job_bytes(job);
            Progress::Bytes { done, total }
        }
    }
}

/// Bytes written and bytes to write over the job's items that are not
/// skipped.
fn job_bytes(job: &QueueJob) -> (u64, u64) {
    job.items
        .iter()
        .filter(|item| !matches!(item.state, ItemState::Skipped { .. }))
        .fold((0, 0), |(done, total), item| {
            let written = match item.state {
                ItemState::Done => item.size,
                ItemState::Running { bytes } => bytes.min(item.size),
                _ => 0,
            };
            (done + written, total + item.size)
        })
}

/// Whole percent of `done` out of `total`; nothing to do is 100%.
fn percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 100;
    }
    let ratio = u128::from(done.min(total)) * 100 / u128::from(total);
    u64::try_from(ratio).unwrap_or(100)
}

/// `1.2 MiB, modified 2026-10-01 12:00`.
fn entry_attributes(entry: &Entry) -> String {
    let size = entry
        .size
        .map_or_else(|| "size unknown".into(), format_size);
    match entry.mtime.and_then(format_local_mtime) {
        Some(mtime) => format!("{size}, modified {mtime}"),
        None => size,
    }
}

fn verb(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Copy => "copy",
        JobKind::Move => "move",
    }
}

/// `1 item`, `3 items`.
fn count_noun(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// The last path component, or the whole path for `/`.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// A path as shown to the user: `/path` locally, `host:/path` remotely.
fn describe(endpoint: &Endpoint, path: &Path) -> String {
    let path = path.display();
    match endpoint {
        Endpoint::Local => path.to_string(),
        Endpoint::Remote { host, sudo: false } => format!("{host}:{path}"),
        Endpoint::Remote { host, sudo: true } => format!("{host}:{path} [sudo]"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::error::FsError;
    use crate::groups::Groups;
    use crate::hosts::HostList;
    use crate::preview::Background;
    use crate::settings::Settings;
    use crate::state::PersistedState;
    use crate::transfer::model::PartialInfo;
    use crate::ui::action::{BgEvent, ClickModifiers};
    use crate::vfs::EntryKind;

    fn web() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        }
    }

    fn db() -> Endpoint {
        Endpoint::Remote {
            host: "db".into(),
            sudo: false,
        }
    }

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(size),
            mtime: None,
            mode: Some(0o644),
        }
    }

    fn dir(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::Dir,
            size: Some(0),
            mtime: None,
            mode: Some(0o755),
        }
    }

    /// Directory contents of the fixture tree. Visible order puts
    /// directories first: `/l` is `sub`, `a.txt`, `b.txt`; `/srv` is `www`,
    /// `x`.
    fn entries(path: &Path) -> Vec<Entry> {
        match path.to_str() {
            Some("/l") => vec![file("a.txt", 10), dir("sub"), file("b.txt", 20)],
            Some("/srv") => vec![file("x", 5), dir("www")],
            _ => Vec::new(),
        }
    }

    fn unlisted() -> App {
        App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (Endpoint::Local, Some("/l".into())),
            Some((web(), Some("/srv".into()))),
        )
        .0
    }

    /// An 80x24 app with both panes listed; remote endpoints connect with
    /// home `/home/u`.
    fn build(left: (Endpoint, &str), right: (Endpoint, &str)) -> App {
        let (mut app, mut pending) = App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (left.0, Some(left.1.into())),
            Some((right.0, Some(right.1.into()))),
        );
        app.handle_action(Action::Resize {
            width: 80,
            height: 24,
        });
        while let Some(effect) = pending.pop() {
            match effect {
                Effect::ListDir { request, location } => {
                    let result = Ok(entries(&location.path));
                    pending.extend(app.handle_event(BgEvent::Listed {
                        request,
                        location,
                        result,
                    }));
                }
                Effect::Connect(endpoint) => pending.extend(app.handle_event(BgEvent::Connected {
                    endpoint,
                    home: "/home/u".into(),
                })),
                _ => {}
            }
        }
        app
    }

    /// Left local `/l`, right `web:/srv`.
    fn app() -> App {
        build((Endpoint::Local, "/l"), (web(), "/srv"))
    }

    fn act(app: &mut App, action: Action) -> Vec<Effect> {
        match app.transfer_action(action) {
            Ok(effects) => effects,
            Err(action) => panic!("{action:?} was not claimed"),
        }
    }

    fn submitted(effects: &[Effect]) -> &JobRequest {
        match effects {
            [Effect::Transfer(TransferCommand::Submit(request))] => request,
            other => panic!("expected one Submit, got {other:?}"),
        }
    }

    fn status(app: &App) -> StatusMessage {
        app.core.status().cloned().expect("a status message")
    }

    fn select_cursor(app: &mut App, side: PaneSide, index: usize) {
        app.core.pane_mut(side).set_cursor(index, 10);
    }

    fn item(id: u64, job: u64, name: &str, size: u64, state: ItemState) -> Item {
        Item {
            id: ItemId(id),
            job: JobId(job),
            kind: ItemKind::File,
            source: PathBuf::from("/l").join(name),
            destination: PathBuf::from("/srv").join(name),
            size,
            mtime: None,
            mode: Some(0o644),
            state,
        }
    }

    fn queue_job(id: u64, mode: Option<TransferMode>, items: Vec<Item>) -> QueueJob {
        QueueJob {
            id: JobId(id),
            kind: JobKind::Copy,
            mode,
            source: Endpoint::Local,
            destination: web(),
            label: format!("job {id}"),
            sync: false,
            items,
            outcome: None,
        }
    }

    fn sync_job(id: u64) -> QueueJob {
        QueueJob {
            sync: true,
            label: "sync -> web:/srv".into(),
            ..queue_job(id, None, Vec::new())
        }
    }

    /// Copies `a.txt` (job 1) and lets the engine create it with `items`.
    fn copied(app: &mut App, mode: TransferMode, items: Vec<Item>) {
        select_cursor(app, PaneSide::Left, 1);
        act(app, Action::Copy);
        app.transfer_event(TransferEvent::JobCreated {
            job: JobId(1),
            kind: JobKind::Copy,
            mode,
            source: Endpoint::Local,
            destination: web(),
        });
        app.transfer_event(TransferEvent::ItemsAdded {
            job: JobId(1),
            items,
        });
    }

    fn answer(purpose: DialogPurpose, button: DialogButton, checked: bool) -> DialogAnswer {
        DialogAnswer {
            purpose,
            button,
            field: None,
            checked,
        }
    }

    fn top_dialog(app: &App) -> &Dialog {
        app.dialog.stack.last().expect("an open dialog")
    }

    fn focus_queue(app: &mut App) {
        app.handle_action(Action::ToggleQueue);
        app.handle_action(Action::SwitchFocus);
        app.handle_action(Action::SwitchFocus);
        assert!(app.core.queue_focused());
    }

    fn queue(control: QueueControl, target: Option<QueueTarget>) -> Action {
        Action::Queue { control, target }
    }

    fn not_found() -> Error {
        Error::Fs(FsError::NotFound { path: "/x".into() })
    }

    // Copy and move.

    #[test]
    fn copy_submits_the_cursor_entry_into_the_other_pane_directory() {
        let mut app = app();
        select_cursor(&mut app, PaneSide::Left, 1);
        let effects = act(&mut app, Action::Copy);
        assert_eq!(
            *submitted(&effects),
            JobRequest {
                id: JobId(1),
                kind: JobKind::Copy,
                source: Endpoint::Local,
                destination: web(),
                targets: JobTargets::IntoDirectory {
                    sources: vec!["/l/a.txt".into()],
                    directory: "/srv".into(),
                },
                preset: None,
            }
        );
        let job = &app.transfer.jobs[0];
        assert_eq!(job.id, JobId(1));
        assert_eq!(job.label, "copy a.txt -> web:/srv");
        assert_eq!(
            (job.mode, job.sync, job.outcome.is_none()),
            (None, false, true)
        );
        assert_eq!((&job.source, &job.destination), (&Endpoint::Local, &web()));
        assert!(job.items.is_empty());
        assert_eq!(
            status(&app),
            StatusMessage::info("Queued copy a.txt -> web:/srv")
        );
    }

    #[test]
    fn move_uses_the_selection_and_job_ids_are_fresh() {
        let mut app = app();
        app.core.pane_mut(PaneSide::Left).toggle(1);
        app.core.pane_mut(PaneSide::Left).toggle(2);
        let first = act(&mut app, Action::Move);
        let request = submitted(&first);
        assert_eq!(request.kind, JobKind::Move);
        assert_eq!(
            request.targets,
            JobTargets::IntoDirectory {
                sources: vec!["/l/a.txt".into(), "/l/b.txt".into()],
                directory: "/srv".into(),
            }
        );
        assert_eq!(app.transfer.jobs[0].label, "move 2 items -> web:/srv");
        let second = act(&mut app, Action::Copy);
        assert_eq!(submitted(&second).id, JobId(2));
        assert_eq!(app.transfer.jobs.len(), 2);
    }

    #[test]
    fn copy_from_the_right_pane_goes_to_the_left_directory() {
        let mut app = app();
        app.handle_action(Action::SwitchFocus);
        select_cursor(&mut app, PaneSide::Right, 1);
        let effects = act(&mut app, Action::Copy);
        let request = submitted(&effects);
        assert_eq!(
            (&request.source, &request.destination),
            (&web(), &Endpoint::Local)
        );
        assert_eq!(
            request.targets,
            JobTargets::IntoDirectory {
                sources: vec!["/srv/x".into()],
                directory: "/l".into(),
            }
        );
    }

    #[test]
    fn copy_into_the_same_directory_is_refused() {
        let mut app = build((Endpoint::Local, "/l"), (Endpoint::Local, "/l"));
        select_cursor(&mut app, PaneSide::Left, 1);
        assert!(act(&mut app, Action::Copy).is_empty());
        assert_eq!(
            status(&app),
            StatusMessage::error("Source and destination are the same directory")
        );
        assert!(app.transfer.jobs.is_empty());
    }

    #[test]
    fn copy_of_a_directory_into_itself_is_refused() {
        let mut app = build((Endpoint::Local, "/l"), (Endpoint::Local, "/l/sub/deep"));
        select_cursor(&mut app, PaneSide::Left, 0);
        assert!(act(&mut app, Action::Move).is_empty());
        assert_eq!(
            status(&app),
            StatusMessage::error("Cannot move sub into itself")
        );
        assert!(app.transfer.jobs.is_empty());
    }

    #[test]
    fn copy_between_endpoints_with_equal_paths_is_allowed() {
        let mut app = build((Endpoint::Local, "/srv"), (web(), "/srv"));
        select_cursor(&mut app, PaneSide::Left, 1);
        let effects = act(&mut app, Action::Copy);
        assert_eq!(submitted(&effects).destination, web());
    }

    #[test]
    fn copy_needs_targets_and_a_listed_destination() {
        let mut app = build((Endpoint::Local, "/empty"), (web(), "/srv"));
        assert!(act(&mut app, Action::Copy).is_empty());
        assert_eq!(status(&app), StatusMessage::info("Nothing to copy"));

        let mut app = unlisted();
        assert!(act(&mut app, Action::Copy).is_empty());
        assert_eq!(
            status(&app),
            StatusMessage::error("Cannot copy: the other pane has no directory yet")
        );
        assert!(app.transfer.jobs.is_empty());
    }

    // Drag and drop.

    fn drag_start(app: &mut App, side: PaneSide, index: usize) {
        let at = Instant::now();
        act(app, Action::DragStart { side, index, at });
    }

    fn drag_move(app: &mut App, target: DropTarget) {
        act(
            app,
            Action::DragMove {
                target,
                column: 50,
                row: 4,
            },
        );
    }

    fn drop_on(app: &mut App, target: DropTarget) -> Vec<Effect> {
        let at = Instant::now();
        act(app, Action::Drop { target, at })
    }

    fn hover_timer(effects: &[Effect]) -> TimerId {
        match effects {
            [Effect::ScheduleTimer { id, after }] if *after == TAB_HOVER => *id,
            other => panic!("expected the tab hover timer, got {other:?}"),
        }
    }

    #[test]
    fn resting_on_the_hidden_tab_shows_that_pane_for_the_drop() {
        let mut app = app();
        app.handle_action(Action::Resize {
            width: 50,
            height: 20,
        });
        drag_start(&mut app, PaneSide::Left, 1);
        let moved = |target| Action::DragMove {
            target,
            column: 30,
            row: 0,
        };
        let id = hover_timer(&act(&mut app, moved(DropTarget::Pane(PaneSide::Right))));
        // Moving within the same tab keeps the pending switch.
        assert!(act(&mut app, moved(DropTarget::Pane(PaneSide::Right))).is_empty());
        // The tab of a drop-able hidden pane is highlighted.
        assert_eq!(
            app.drop_target(PaneSide::Right),
            Some(DropTarget::Pane(PaneSide::Right))
        );
        app.handle_event(BgEvent::TimerFired {
            id,
            at: Instant::now(),
        });
        assert_eq!(app.core.focused(), PaneSide::Right);
        assert!(app.core.pane_drawn(PaneSide::Right));
        // The drag goes on in the pane now shown: drop on its folder row.
        assert!(act(
            &mut app,
            moved(DropTarget::Folder {
                side: PaneSide::Right,
                index: 0
            })
        )
        .is_empty());
        let effects = drop_on(
            &mut app,
            DropTarget::Folder {
                side: PaneSide::Right,
                index: 0,
            },
        );
        let request = submitted(&effects);
        assert_eq!(request.kind, JobKind::Copy);
        assert_eq!(
            request.targets,
            JobTargets::IntoDirectory {
                sources: vec![PathBuf::from("/l/a.txt")],
                directory: "/srv/www".into(),
            }
        );
    }

    #[test]
    fn leaving_the_hidden_tab_cancels_the_switch() {
        let mut app = app();
        app.handle_action(Action::Resize {
            width: 50,
            height: 20,
        });
        drag_start(&mut app, PaneSide::Left, 1);
        let at = |target| Action::DragMove {
            target,
            column: 30,
            row: 0,
        };
        let id = hover_timer(&act(&mut app, at(DropTarget::Pane(PaneSide::Right))));
        assert!(act(&mut app, at(DropTarget::Pane(PaneSide::Left))).is_empty());
        app.handle_event(BgEvent::TimerFired {
            id,
            at: Instant::now(),
        });
        assert_eq!(app.core.focused(), PaneSide::Left);
        // Releasing on the hidden tab drops into that pane's directory.
        let effects = drop_on(&mut app, DropTarget::Pane(PaneSide::Right));
        assert_eq!(
            submitted(&effects).targets,
            JobTargets::IntoDirectory {
                sources: vec![PathBuf::from("/l/a.txt")],
                directory: "/srv".into(),
            }
        );
    }

    #[test]
    fn two_panes_side_by_side_need_no_tab_hover() {
        let mut app = app();
        drag_start(&mut app, PaneSide::Left, 1);
        drag_move(&mut app, DropTarget::Pane(PaneSide::Right));
        assert_eq!(app.transfer.drag.as_ref().and_then(|d| d.hover), None);
    }

    #[test]
    fn drop_on_the_other_pane_copies_into_its_directory() {
        let mut app = app();
        drag_start(&mut app, PaneSide::Left, 1);
        assert_eq!(
            app.drag_label_view(),
            Some(DragLabelView {
                count: 1,
                column: 0,
                row: 0
            })
        );
        drag_move(&mut app, DropTarget::Pane(PaneSide::Right));
        assert_eq!(
            app.drag_label_view(),
            Some(DragLabelView {
                count: 1,
                column: 50,
                row: 4
            })
        );
        assert_eq!(
            app.drop_target(PaneSide::Right),
            Some(DropTarget::Pane(PaneSide::Right))
        );
        assert_eq!(app.drop_target(PaneSide::Left), None);

        let effects = drop_on(&mut app, DropTarget::Pane(PaneSide::Right));
        let request = submitted(&effects);
        assert_eq!(request.kind, JobKind::Copy);
        assert_eq!(
            request.targets,
            JobTargets::IntoDirectory {
                sources: vec!["/l/a.txt".into()],
                directory: "/srv".into(),
            }
        );
        assert_eq!(app.transfer.drag, None);
        assert_eq!(app.drag_label_view(), None);
    }

    #[test]
    fn drop_on_a_folder_of_the_other_pane_copies_into_it() {
        let mut app = app();
        drag_start(&mut app, PaneSide::Left, 2);
        let folder = DropTarget::Folder {
            side: PaneSide::Right,
            index: 0,
        };
        drag_move(&mut app, folder);
        assert_eq!(app.drop_target(PaneSide::Right), Some(folder));
        let effects = drop_on(&mut app, folder);
        let request = submitted(&effects);
        assert_eq!(
            (request.kind, &request.destination),
            (JobKind::Copy, &web())
        );
        assert_eq!(
            request.targets,
            JobTargets::IntoDirectory {
                sources: vec!["/l/b.txt".into()],
                directory: "/srv/www".into(),
            }
        );
        assert_eq!(app.transfer.jobs[0].label, "copy b.txt -> web:/srv/www");
    }

    #[test]
    fn drop_on_a_folder_of_the_source_pane_moves_the_selection() {
        let mut app = app();
        app.core.pane_mut(PaneSide::Left).toggle(1);
        app.core.pane_mut(PaneSide::Left).toggle(2);
        drag_start(&mut app, PaneSide::Left, 2);
        let folder = DropTarget::Folder {
            side: PaneSide::Left,
            index: 0,
        };
        drag_move(&mut app, folder);
        assert_eq!(app.drop_target(PaneSide::Left), Some(folder));
        assert_eq!(app.drag_label_view().map(|view| view.count), Some(2));
        let effects = drop_on(&mut app, folder);
        let request = submitted(&effects);
        assert_eq!(request.kind, JobKind::Move);
        assert_eq!(
            (&request.source, &request.destination),
            (&Endpoint::Local, &Endpoint::Local)
        );
        assert_eq!(
            request.targets,
            JobTargets::IntoDirectory {
                sources: vec!["/l/a.txt".into(), "/l/b.txt".into()],
                directory: "/l/sub".into(),
            }
        );
    }

    #[test]
    fn drops_that_do_nothing_end_the_drag() {
        let targets = [
            DropTarget::Invalid,
            DropTarget::Pane(PaneSide::Left),
            // A file row is not a folder.
            DropTarget::Folder {
                side: PaneSide::Right,
                index: 1,
            },
            // A row index past the list.
            DropTarget::Folder {
                side: PaneSide::Right,
                index: 9,
            },
        ];
        for target in targets {
            let mut app = app();
            drag_start(&mut app, PaneSide::Left, 1);
            drag_move(&mut app, target);
            assert_eq!(app.drop_target(PaneSide::Left), None, "{target:?}");
            assert_eq!(app.drop_target(PaneSide::Right), None, "{target:?}");
            assert!(drop_on(&mut app, target).is_empty(), "{target:?}");
            assert_eq!(app.transfer.drag, None);
            assert!(app.transfer.jobs.is_empty());
        }
    }

    #[test]
    fn a_folder_dropped_on_itself_does_nothing() {
        let mut app = app();
        drag_start(&mut app, PaneSide::Left, 0);
        let itself = DropTarget::Folder {
            side: PaneSide::Left,
            index: 0,
        };
        drag_move(&mut app, itself);
        assert_eq!(app.drop_target(PaneSide::Left), None);
        assert!(drop_on(&mut app, itself).is_empty());
        assert!(app.transfer.jobs.is_empty());
    }

    #[test]
    fn drag_cancel_and_stray_drops_have_no_effect() {
        let mut app = app();
        drag_start(&mut app, PaneSide::Left, 1);
        assert!(act(&mut app, Action::DragCancel).is_empty());
        assert_eq!(app.transfer.drag, None);
        assert!(drop_on(&mut app, DropTarget::Pane(PaneSide::Right)).is_empty());
        // A move without a drag is ignored.
        drag_move(&mut app, DropTarget::Pane(PaneSide::Right));
        assert_eq!(app.transfer.drag, None);
        // A press past the list starts no drag.
        drag_start(&mut app, PaneSide::Left, 7);
        assert_eq!(app.transfer.drag, None);
        assert!(app.transfer.jobs.is_empty());
    }

    // Finder drop.

    #[test]
    fn paste_of_absolute_paths_checks_them() {
        let mut app = app();
        let text = "/tmp/a.txt '/tmp/with space'\n".to_string();
        assert_eq!(
            act(&mut app, Action::Paste(text.clone())),
            [Effect::CheckDroppedPaths {
                text,
                paths: vec!["/tmp/a.txt".into(), "/tmp/with space".into()],
            }]
        );
        focus_queue(&mut app);
        assert_eq!(act(&mut app, Action::Paste("/x".into())).len(), 1);
    }

    #[test]
    fn paste_of_other_text_is_handed_back() {
        let mut app = app();
        for text in ["hello", "relative/path", "", "'/unbalanced"] {
            assert_eq!(
                app.transfer_action(Action::Paste(text.into())),
                Err(Action::Paste(text.into()))
            );
        }
    }

    /// Delivers a successful check of `/tmp/a /tmp/b` and returns the
    /// destination of the dialog it opens.
    fn finder_destination(app: &mut App) -> Location {
        let paths = vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")];
        assert!(app
            .transfer_dropped_paths_checked("/tmp/a /tmp/b".into(), paths.clone(), true)
            .is_empty());
        let dialog = top_dialog(app);
        assert_eq!(dialog.buttons, [DialogButton::Upload, DialogButton::Cancel]);
        assert_eq!(dialog.focus, DialogFocus::Button(0));
        assert_eq!(dialog.title, "Upload");
        match &dialog.purpose {
            DialogPurpose::FinderDrop {
                paths: dropped,
                destination,
            } => {
                assert_eq!(*dropped, paths);
                destination.clone()
            }
            other => panic!("expected FinderDrop, got {other:?}"),
        }
    }

    #[test]
    fn finder_drop_prefers_the_focused_remote_pane() {
        let mut app = build((web(), "/srv"), (db(), "/l"));
        assert_eq!(
            finder_destination(&mut app),
            Location {
                endpoint: web(),
                path: "/srv".into()
            }
        );
        assert_eq!(top_dialog(&app).message, "Upload 2 items to web:/srv?");
        app.dialog.stack.clear();
        app.handle_action(Action::SwitchFocus);
        assert_eq!(
            finder_destination(&mut app),
            Location {
                endpoint: db(),
                path: "/l".into()
            }
        );
    }

    #[test]
    fn finder_drop_on_a_local_pane_uses_the_remote_one() {
        let mut app = app();
        assert_eq!(
            finder_destination(&mut app),
            Location {
                endpoint: web(),
                path: "/srv".into()
            }
        );
    }

    #[test]
    fn finder_drop_with_two_local_panes_uses_the_focused_one() {
        let mut app = build((Endpoint::Local, "/l"), (Endpoint::Local, "/srv"));
        app.handle_action(Action::SwitchFocus);
        assert_eq!(
            finder_destination(&mut app),
            Location {
                endpoint: Endpoint::Local,
                path: "/srv".into()
            }
        );
    }

    #[test]
    fn finder_drop_of_missing_paths_or_without_a_directory_opens_nothing() {
        let mut app = app();
        assert!(app
            .transfer_dropped_paths_checked("/nope".into(), vec!["/nope".into()], false)
            .is_empty());
        assert!(app.dialog.stack.is_empty());
        assert_eq!(
            status(&app),
            StatusMessage::info("Ignored dropped path: not every path exists locally")
        );

        let mut app = unlisted();
        assert!(app
            .transfer_dropped_paths_checked("/a".into(), vec!["/a".into()], true)
            .is_empty());
        assert!(app.dialog.stack.is_empty());
        assert!(status(&app).error);
    }

    #[test]
    fn finder_drop_upload_submits_a_copy_from_local() {
        let mut app = app();
        let destination = finder_destination(&mut app);
        let purpose = app
            .dialog
            .stack
            .pop()
            .map(|dialog| dialog.purpose)
            .expect("the drop dialog");
        let cancel = answer(purpose.clone(), DialogButton::Cancel, false);
        assert!(app.transfer_dialog_answer(cancel).is_empty());
        assert!(app.transfer.jobs.is_empty());

        let effects = app.transfer_dialog_answer(answer(purpose, DialogButton::Upload, false));
        let request = submitted(&effects);
        assert_eq!(request.kind, JobKind::Copy);
        assert_eq!(
            (&request.source, &request.destination),
            (&Endpoint::Local, &destination.endpoint)
        );
        assert_eq!(
            request.targets,
            JobTargets::IntoDirectory {
                sources: vec!["/tmp/a".into(), "/tmp/b".into()],
                directory: "/srv".into(),
            }
        );
        assert_eq!(app.transfer.jobs[0].label, "copy 2 items -> web:/srv");
    }

    // Queue mirror.

    #[test]
    fn job_created_completes_the_submitted_job() {
        let mut app = app();
        copied(
            &mut app,
            TransferMode::PerFile,
            vec![item(10, 1, "a.txt", 10, ItemState::Queued)],
        );
        assert_eq!(app.transfer.jobs.len(), 1);
        let job = &app.transfer.jobs[0];
        assert_eq!(job.mode, Some(TransferMode::PerFile));
        assert_eq!(job.label, "copy a.txt -> web:/srv");
        assert_eq!(job.items.len(), 1);
    }

    #[test]
    fn job_created_for_a_sync_keeps_its_queue_entry() {
        let mut app = app();
        app.transfer.jobs.push(sync_job(7));
        app.transfer_event(TransferEvent::JobCreated {
            job: JobId(7),
            kind: JobKind::Copy,
            mode: TransferMode::Tar,
            source: Endpoint::Local,
            destination: web(),
        });
        assert_eq!(app.transfer.jobs.len(), 1);
        let job = &app.transfer.jobs[0];
        assert!(job.sync);
        assert_eq!(job.mode, Some(TransferMode::Tar));
        assert_eq!(job.label, "sync -> web:/srv");
    }

    #[test]
    fn job_created_for_an_unknown_job_adds_it() {
        let mut app = app();
        app.transfer_event(TransferEvent::JobCreated {
            job: JobId(9),
            kind: JobKind::Move,
            mode: TransferMode::PerFile,
            source: web(),
            destination: Endpoint::Local,
        });
        let job = &app.transfer.jobs[0];
        assert_eq!(job.id, JobId(9));
        assert_eq!(job.label, "move web -> Local");
        assert!(!job.sync);
    }

    #[test]
    fn item_events_update_the_mirror() {
        let mut app = app();
        copied(
            &mut app,
            TransferMode::PerFile,
            vec![item(10, 1, "a.txt", 10, ItemState::Queued)],
        );
        app.transfer_event(TransferEvent::ItemChanged {
            item: ItemId(10),
            state: ItemState::Running { bytes: 4 },
        });
        app.transfer_event(TransferEvent::ItemRenamed {
            item: ItemId(10),
            destination: "/srv/a (1).txt".into(),
        });
        let changed = app.transfer.jobs[0].items[0].clone();
        assert_eq!(changed.state, ItemState::Running { bytes: 4 });
        assert_eq!(changed.destination, PathBuf::from("/srv/a (1).txt"));
        // Events for unknown ids are dropped.
        app.transfer_event(TransferEvent::ItemChanged {
            item: ItemId(99),
            state: ItemState::Done,
        });
        app.transfer_event(TransferEvent::ItemRenamed {
            item: ItemId(99),
            destination: "/y".into(),
        });
        app.transfer_event(TransferEvent::ItemsAdded {
            job: JobId(99),
            items: vec![changed.clone()],
        });
        assert_eq!(app.transfer.jobs.len(), 1);
        assert_eq!(app.transfer.jobs[0].items, [changed]);
    }

    #[test]
    fn job_finished_reports_and_refreshes_the_destination_pane() {
        let mut app = app();
        copied(
            &mut app,
            TransferMode::PerFile,
            vec![item(10, 1, "a.txt", 10, ItemState::Done)],
        );
        let summary = JobSummary {
            done: 1,
            skipped: 0,
            failed: 0,
        };
        let effects = app.transfer_event(TransferEvent::JobFinished {
            job: JobId(1),
            summary,
        });
        match effects.as_slice() {
            [Effect::ListDir { location, .. }] => assert_eq!(
                *location,
                Location {
                    endpoint: web(),
                    path: "/srv".into()
                }
            ),
            other => panic!("expected one refresh, got {other:?}"),
        }
        assert_eq!(
            app.transfer.jobs[0].outcome,
            Some(JobOutcome::Finished(summary))
        );
        assert_eq!(
            status(&app),
            StatusMessage::info("copy a.txt -> web:/srv: 1 done")
        );
    }

    #[test]
    fn finished_move_refreshes_source_and_destination() {
        let mut app = app();
        select_cursor(&mut app, PaneSide::Left, 1);
        act(&mut app, Action::Move);
        let effects = app.transfer_event(TransferEvent::JobFinished {
            job: JobId(1),
            summary: JobSummary {
                done: 1,
                skipped: 2,
                failed: 3,
            },
        });
        let refreshed: Vec<Endpoint> = effects
            .iter()
            .map(|effect| match effect {
                Effect::ListDir { location, .. } => location.endpoint.clone(),
                other => panic!("expected a refresh, got {other:?}"),
            })
            .collect();
        assert_eq!(refreshed, [Endpoint::Local, web()]);
        assert_eq!(
            status(&app),
            StatusMessage::error("move a.txt -> web:/srv: 1 done, 2 skipped, 3 failed")
        );
    }

    #[test]
    fn job_failed_records_the_error() {
        let mut app = app();
        select_cursor(&mut app, PaneSide::Left, 1);
        act(&mut app, Action::Copy);
        let effects = app.transfer_event(TransferEvent::JobFailed {
            job: JobId(1),
            error: not_found(),
        });
        assert_eq!(effects.len(), 1);
        assert_eq!(
            app.transfer.jobs[0].outcome,
            Some(JobOutcome::Failed(not_found()))
        );
        assert_eq!(
            status(&app),
            StatusMessage::error("copy a.txt -> web:/srv: /x does not exist")
        );
        // An unknown job changes nothing.
        let none = app.transfer_event(TransferEvent::JobFailed {
            job: JobId(42),
            error: not_found(),
        });
        assert!(none.is_empty());
        assert_eq!(app.transfer.jobs.len(), 1);
    }

    #[test]
    fn a_retried_item_reopens_its_finished_job() {
        let mut app = app();
        copied(
            &mut app,
            TransferMode::PerFile,
            vec![item(
                10,
                1,
                "a.txt",
                10,
                ItemState::Failed { error: not_found() },
            )],
        );
        app.transfer_event(TransferEvent::JobFinished {
            job: JobId(1),
            summary: JobSummary {
                done: 0,
                skipped: 0,
                failed: 1,
            },
        });
        assert_eq!(app.transfer_quit_requested(), [Effect::Quit]);
        app.transfer_event(TransferEvent::ItemChanged {
            item: ItemId(10),
            state: ItemState::Queued,
        });
        assert_eq!(app.transfer.jobs[0].outcome, None);
        assert!(app.transfer_quit_requested().is_empty());
    }

    #[test]
    fn jobs_cleared_keeps_the_cursor_on_its_row() {
        let mut app = app();
        app.transfer.jobs.push(queue_job(
            1,
            Some(TransferMode::PerFile),
            vec![item(10, 1, "a", 1, ItemState::Done)],
        ));
        app.transfer.jobs.push(queue_job(
            2,
            Some(TransferMode::PerFile),
            vec![item(20, 2, "b", 1, ItemState::Queued)],
        ));
        app.transfer.cursor = 3;
        app.transfer_event(TransferEvent::JobsCleared(vec![JobId(1)]));
        assert_eq!(app.transfer.jobs.len(), 1);
        assert_eq!(app.transfer.cursor, 1);
        assert_eq!(
            app.transfer.cursor_target(),
            Some(QueueTarget::Item(ItemId(20)))
        );
        app.transfer_event(TransferEvent::JobsCleared(vec![JobId(2)]));
        assert_eq!((app.transfer.cursor, app.transfer.first_visible), (0, 0));
        assert!(app.transfer.jobs.is_empty());
    }

    // Conflicts.

    fn conflict(partial: Option<PartialInfo>) -> Box<Conflict> {
        Box::new(Conflict {
            item: ItemId(10),
            job: JobId(1),
            source: file("a.txt", 2048),
            destination: file("a.txt", 300),
            partial,
        })
    }

    fn with_conflict(partial: Option<PartialInfo>) -> App {
        let mut app = app();
        copied(
            &mut app,
            TransferMode::PerFile,
            vec![item(10, 1, "a.txt", 2048, ItemState::WaitingForConflict)],
        );
        app.transfer_event(TransferEvent::ConflictRaised(conflict(partial)));
        app
    }

    #[test]
    fn conflict_opens_a_dialog_with_apply_to_all() {
        let app = with_conflict(None);
        let dialog = top_dialog(&app);
        assert_eq!(
            dialog.purpose,
            DialogPurpose::Conflict {
                item: ItemId(10),
                job: JobId(1),
                partial: false,
            }
        );
        assert_eq!(
            dialog.buttons,
            [
                DialogButton::Overwrite,
                DialogButton::Skip,
                DialogButton::Rename,
                DialogButton::OverwriteIfNewer,
            ]
        );
        assert_eq!(
            dialog.checkbox,
            Some(Checkbox {
                label: "Apply to all in this job".into(),
                checked: false,
            })
        );
        assert_eq!(dialog.title, "File exists");
        assert_eq!(
            dialog.message,
            "web:/srv/a.txt already exists.\n\nSource:   2.0 KiB\nExisting: 300 B"
        );
        assert!(dialog.field.is_none());
        assert_eq!(dialog.focus, DialogFocus::Button(0));
    }

    #[test]
    fn conflict_with_a_partial_offers_resume() {
        let app = with_conflict(Some(PartialInfo { size: 300 }));
        let dialog = top_dialog(&app);
        assert_eq!(dialog.buttons.last(), Some(&DialogButton::Resume));
        assert!(dialog
            .message
            .ends_with("A partial file of 300 B can be resumed."));
        assert!(matches!(
            dialog.purpose,
            DialogPurpose::Conflict { partial: true, .. }
        ));
    }

    #[test]
    fn conflict_answers_become_decisions() {
        let cases = [
            (
                DialogButton::Overwrite,
                false,
                ConflictChoice::Overwrite,
                false,
            ),
            (
                DialogButton::Overwrite,
                true,
                ConflictChoice::Overwrite,
                true,
            ),
            (DialogButton::Skip, true, ConflictChoice::Skip, true),
            (DialogButton::Rename, false, ConflictChoice::Rename, false),
            (
                DialogButton::OverwriteIfNewer,
                true,
                ConflictChoice::OverwriteIfNewer,
                true,
            ),
            (DialogButton::Resume, true, ConflictChoice::Resume, true),
            // Esc skips this item only, whatever the checkbox says.
            (DialogButton::Cancel, true, ConflictChoice::Skip, false),
        ];
        let purpose = |partial| DialogPurpose::Conflict {
            item: ItemId(10),
            job: JobId(1),
            partial,
        };
        let resolve = |choice, apply_to_all| {
            vec![Effect::Transfer(TransferCommand::ResolveConflict {
                item: ItemId(10),
                decision: ConflictDecision {
                    choice,
                    apply_to_all,
                },
            })]
        };
        let mut app = app();
        for (button, checked, choice, apply_to_all) in cases {
            assert_eq!(
                app.transfer_dialog_answer(answer(purpose(true), button, checked)),
                resolve(choice, apply_to_all),
                "{button:?}"
            );
        }
        // Resume is not a choice without a partial file.
        assert_eq!(
            app.transfer_dialog_answer(answer(purpose(false), DialogButton::Resume, true)),
            resolve(ConflictChoice::Skip, false)
        );
    }

    #[test]
    fn conflict_dialog_closes_when_its_item_moves_on() {
        let mut app = with_conflict(None);
        // Raising it again does not stack a second dialog.
        app.transfer_event(TransferEvent::ConflictRaised(conflict(None)));
        assert_eq!(app.dialog.stack.len(), 1);
        app.transfer_event(TransferEvent::ItemChanged {
            item: ItemId(10),
            state: ItemState::WaitingForConflict,
        });
        assert_eq!(app.dialog.stack.len(), 1);
        app.transfer_event(TransferEvent::ItemChanged {
            item: ItemId(10),
            state: ItemState::Queued,
        });
        assert!(app.dialog.stack.is_empty());
    }

    #[test]
    fn tar_conflict_explains_there_is_no_resume() {
        let mut app = app();
        copied(
            &mut app,
            TransferMode::Tar,
            vec![item(10, 1, "a.txt", 1, ItemState::Queued)],
        );
        let existing: Vec<PathBuf> = (1..=7).map(|n| format!("/srv/f{n}").into()).collect();
        app.transfer_event(TransferEvent::TarConflictRaised {
            job: JobId(1),
            existing,
        });
        let dialog = top_dialog(&app);
        assert_eq!(dialog.purpose, DialogPurpose::TarConflict { job: JobId(1) });
        assert_eq!(
            dialog.buttons,
            [DialogButton::OverwriteAll, DialogButton::Cancel]
        );
        assert!(dialog.checkbox.is_none());
        assert!(dialog
            .message
            .starts_with("7 items already exist:\nweb:/srv/f1\n"));
        assert!(dialog.message.contains("web:/srv/f5\nand 2 more\n"));
        assert!(!dialog.message.contains("f6"));
        assert!(dialog.message.contains("without .scpto-part protection"));
        assert!(dialog.message.contains("cannot resume"));

        let mut texts = Vec::new();
        let mut rows = Vec::new();
        assert_eq!(
            app.queue_view(&mut texts, &mut rows).rows[0].state,
            "conflict"
        );

        // The dialog goes away when the job ends.
        app.transfer_event(TransferEvent::JobFinished {
            job: JobId(1),
            summary: JobSummary::default(),
        });
        assert!(app.dialog.stack.is_empty());
    }

    #[test]
    fn tar_conflict_with_one_name() {
        let mut app = app();
        app.transfer_event(TransferEvent::TarConflictRaised {
            job: JobId(5),
            existing: vec!["/srv/only".into()],
        });
        assert!(top_dialog(&app)
            .message
            .starts_with("1 item already exists:\n/srv/only\n\n"));
    }

    #[test]
    fn tar_conflict_answers() {
        let mut app = app();
        let purpose = DialogPurpose::TarConflict { job: JobId(3) };
        for (button, choice) in [
            (DialogButton::OverwriteAll, TarConflictChoice::OverwriteAll),
            (DialogButton::Cancel, TarConflictChoice::Cancel),
        ] {
            assert_eq!(
                app.transfer_dialog_answer(answer(purpose.clone(), button, false)),
                [Effect::Transfer(TransferCommand::ResolveTarConflict {
                    job: JobId(3),
                    choice
                })]
            );
        }
        // Answers of other features' dialogs are not handled here.
        assert!(app
            .transfer_dialog_answer(answer(
                DialogPurpose::QuitWhileRunning,
                DialogButton::Yes,
                false
            ))
            .is_empty());
    }

    // Quit.

    #[test]
    fn quit_without_unfinished_jobs_quits() {
        let mut app = app();
        assert_eq!(app.transfer_quit_requested(), [Effect::Quit]);
        let mut done = queue_job(1, Some(TransferMode::PerFile), Vec::new());
        done.outcome = Some(JobOutcome::Failed(not_found()));
        app.transfer.jobs.push(done);
        assert_eq!(app.transfer_quit_requested(), [Effect::Quit]);
        assert!(app.dialog.stack.is_empty());
    }

    #[test]
    fn quit_while_running_asks_first() {
        let mut app = app();
        app.transfer
            .jobs
            .push(queue_job(1, Some(TransferMode::PerFile), Vec::new()));
        assert!(app.transfer_quit_requested().is_empty());
        let dialog = top_dialog(&app);
        assert_eq!(dialog.purpose, DialogPurpose::QuitWhileRunning);
        assert_eq!(dialog.buttons, [DialogButton::Yes, DialogButton::No]);
        assert_eq!(dialog.focus, DialogFocus::Button(1));
        assert_eq!(
            dialog.message,
            "1 transfer is running; partial files are kept for resume.\n\nQuit anyway?"
        );

        app.dialog.stack.clear();
        app.transfer
            .jobs
            .push(queue_job(2, Some(TransferMode::Tar), Vec::new()));
        assert!(app.transfer_quit_requested().is_empty());
        assert_eq!(
            top_dialog(&app).message,
            "2 transfers are running; partial files are kept for resume. \
             Tar transfers cannot resume and start over.\n\nQuit anyway?"
        );
    }

    // Queue controls.

    /// Job 1 (per-file): item 10 running 50 of 100 bytes, item 11 paused.
    fn with_queue() -> App {
        let mut app = app();
        copied(
            &mut app,
            TransferMode::PerFile,
            vec![
                item(10, 1, "a.txt", 100, ItemState::Running { bytes: 50 }),
                item(11, 1, "b.txt", 100, ItemState::Paused),
            ],
        );
        app
    }

    fn control(app: &mut App, control: QueueControl, target: QueueTarget) -> Vec<Effect> {
        act(app, queue(control, Some(target)))
    }

    #[test]
    fn toggle_pause_pauses_or_resumes() {
        let mut app = with_queue();
        let item = |id| QueueTarget::Item(ItemId(id));
        let job = QueueTarget::Job(JobId(1));
        assert_eq!(
            control(&mut app, QueueControl::TogglePause, item(10)),
            [Effect::Transfer(TransferCommand::Pause(item(10)))]
        );
        assert_eq!(
            control(&mut app, QueueControl::TogglePause, item(11)),
            [Effect::Transfer(TransferCommand::Resume(item(11)))]
        );
        assert_eq!(
            control(&mut app, QueueControl::TogglePause, job),
            [Effect::Transfer(TransferCommand::Pause(job))]
        );
        app.transfer_event(TransferEvent::ItemChanged {
            item: ItemId(10),
            state: ItemState::Paused,
        });
        assert_eq!(
            control(&mut app, QueueControl::TogglePause, job),
            [Effect::Transfer(TransferCommand::Resume(job))]
        );
    }

    #[test]
    fn a_job_being_expanded_can_be_paused() {
        let mut app = app();
        select_cursor(&mut app, PaneSide::Left, 1);
        act(&mut app, Action::Copy);
        let job = QueueTarget::Job(JobId(1));
        assert_eq!(
            control(&mut app, QueueControl::TogglePause, job),
            [Effect::Transfer(TransferCommand::Pause(job))]
        );
    }

    #[test]
    fn cancel_goes_to_the_engine_or_the_sync() {
        let mut app = with_queue();
        let job = QueueTarget::Job(JobId(1));
        assert_eq!(
            control(&mut app, QueueControl::Cancel, job),
            [Effect::Transfer(TransferCommand::Cancel(job))]
        );
        app.transfer.jobs.push(sync_job(5));
        assert_eq!(
            control(&mut app, QueueControl::Cancel, QueueTarget::Job(JobId(5))),
            [Effect::CancelSync(JobId(5))]
        );
        // An engine-based sync: items are canceled through the engine, the
        // whole job through the sync executor.
        let mut engine_sync = sync_job(6);
        engine_sync.mode = Some(TransferMode::PerFile);
        engine_sync
            .items
            .push(item(60, 6, "s", 1, ItemState::Queued));
        app.transfer.jobs.push(engine_sync);
        let item = QueueTarget::Item(ItemId(60));
        assert_eq!(
            control(&mut app, QueueControl::Cancel, item),
            [Effect::Transfer(TransferCommand::Cancel(item))]
        );
        assert_eq!(
            control(&mut app, QueueControl::Cancel, QueueTarget::Job(JobId(6))),
            [Effect::CancelSync(JobId(6))]
        );
    }

    #[test]
    fn retry_and_discard_apply_to_failed_or_canceled_entries() {
        let mut app = app();
        let canceled = ItemState::Skipped {
            reason: SkipReason::Canceled,
        };
        copied(
            &mut app,
            TransferMode::PerFile,
            vec![
                item(10, 1, "a.txt", 1, ItemState::Failed { error: not_found() }),
                item(11, 1, "b.txt", 1, canceled),
                item(12, 1, "c.txt", 1, ItemState::Done),
            ],
        );
        app.transfer_event(TransferEvent::JobFinished {
            job: JobId(1),
            summary: JobSummary {
                done: 1,
                skipped: 1,
                failed: 1,
            },
        });
        let item = |id| QueueTarget::Item(ItemId(id));
        let job = QueueTarget::Job(JobId(1));
        for target in [item(10), item(11), job] {
            assert_eq!(
                control(&mut app, QueueControl::Retry, target),
                [Effect::Transfer(TransferCommand::Retry(target))]
            );
            assert_eq!(
                control(&mut app, QueueControl::DiscardPartial, target),
                [Effect::Transfer(TransferCommand::DiscardPartial(target))]
            );
        }
        assert!(control(&mut app, QueueControl::Retry, item(12)).is_empty());
        assert_eq!(
            status(&app),
            StatusMessage::info("Only failed or canceled transfers can be retried")
        );
        assert!(control(&mut app, QueueControl::Cancel, job).is_empty());
        assert_eq!(
            status(&app),
            StatusMessage::info("This transfer has finished")
        );
    }

    #[test]
    fn unavailable_controls_explain_why() {
        let mut app = app();
        app.transfer.jobs.push(queue_job(
            1,
            Some(TransferMode::Tar),
            vec![item(10, 1, "a", 1, ItemState::Running { bytes: 0 })],
        ));
        app.transfer.jobs.push(sync_job(2));
        let mut failed = queue_job(3, None, Vec::new());
        failed.outcome = Some(JobOutcome::Failed(not_found()));
        app.transfer.jobs.push(failed);
        let mut synced = sync_job(4);
        synced.outcome = Some(JobOutcome::Finished(JobSummary::default()));
        app.transfer.jobs.push(synced);
        let mut tar_done = queue_job(
            5,
            Some(TransferMode::Tar),
            vec![item(
                50,
                5,
                "t",
                1,
                ItemState::Failed { error: not_found() },
            )],
        );
        tar_done.outcome = Some(JobOutcome::Finished(JobSummary {
            done: 0,
            skipped: 0,
            failed: 1,
        }));
        app.transfer.jobs.push(tar_done);

        let cases = [
            (
                QueueControl::TogglePause,
                QueueTarget::Job(JobId(1)),
                "Tar transfers cannot be paused",
            ),
            (
                QueueControl::TogglePause,
                QueueTarget::Item(ItemId(10)),
                "Tar transfers cannot be paused",
            ),
            (
                QueueControl::TogglePause,
                QueueTarget::Job(JobId(2)),
                "This sync can only be canceled",
            ),
            (
                QueueControl::Retry,
                QueueTarget::Job(JobId(2)),
                "This sync can only be canceled",
            ),
            (
                QueueControl::Retry,
                QueueTarget::Job(JobId(3)),
                "This job could not start; start it again",
            ),
            (
                QueueControl::Retry,
                QueueTarget::Job(JobId(4)),
                "Syncs cannot be retried; compare the directories again",
            ),
            (
                QueueControl::DiscardPartial,
                QueueTarget::Job(JobId(3)),
                "Only failed or canceled files have a partial file to discard",
            ),
            (
                QueueControl::DiscardPartial,
                QueueTarget::Job(JobId(5)),
                "Tar transfers leave no partial files",
            ),
            (
                QueueControl::DiscardPartial,
                QueueTarget::Item(ItemId(50)),
                "Tar transfers leave no partial files",
            ),
        ];
        for (queue_control, target, reason) in cases {
            assert!(control(&mut app, queue_control, target).is_empty());
            assert_eq!(
                status(&app),
                StatusMessage::info(reason),
                "{queue_control:?} {target:?}"
            );
        }
        // The running tar job can be canceled, the finished one retried.
        let tar_item = QueueTarget::Item(ItemId(10));
        assert_eq!(
            control(&mut app, QueueControl::Cancel, tar_item),
            [Effect::Transfer(TransferCommand::Cancel(tar_item))]
        );
        let tar_job = QueueTarget::Job(JobId(5));
        assert_eq!(
            control(&mut app, QueueControl::Retry, tar_job),
            [Effect::Transfer(TransferCommand::Retry(tar_job))]
        );
    }

    #[test]
    fn whole_queue_controls() {
        let mut app = app();
        assert_eq!(
            act(&mut app, queue(QueueControl::RetryAllFailed, None)),
            [Effect::Transfer(TransferCommand::RetryAllFailed)]
        );
        let mut synced = sync_job(1);
        synced.outcome = Some(JobOutcome::Finished(JobSummary::default()));
        let mut failed_sync = sync_job(2);
        failed_sync.outcome = Some(JobOutcome::Failed(not_found()));
        let mut engine_sync = sync_job(3);
        engine_sync.mode = Some(TransferMode::PerFile);
        engine_sync.outcome = Some(JobOutcome::Finished(JobSummary::default()));
        app.transfer
            .jobs
            .extend([synced, failed_sync, engine_sync, sync_job(4)]);
        app.transfer.cursor = 3;
        let target = Some(QueueTarget::Job(JobId(1)));
        assert_eq!(
            act(&mut app, queue(QueueControl::ClearFinished, target)),
            [Effect::Transfer(TransferCommand::ClearFinished)]
        );
        let left: Vec<JobId> = app.transfer.jobs.iter().map(|job| job.id).collect();
        // The engine clears job 3 with `JobsCleared`.
        assert_eq!(left, [JobId(3), JobId(4)]);
        assert_eq!(
            app.transfer.cursor_target(),
            Some(QueueTarget::Job(JobId(4)))
        );
    }

    #[test]
    fn controls_without_a_target_use_the_queue_cursor() {
        let mut app = with_queue();
        // The cursor starts on the job row.
        assert_eq!(
            act(&mut app, queue(QueueControl::Cancel, None)),
            [Effect::Transfer(TransferCommand::Cancel(QueueTarget::Job(
                JobId(1)
            )))]
        );
        focus_queue(&mut app);
        act(&mut app, Action::MoveCursor(CursorMove::Down));
        assert_eq!(
            act(&mut app, queue(QueueControl::TogglePause, None)),
            [Effect::Transfer(TransferCommand::Pause(QueueTarget::Item(
                ItemId(10)
            )))]
        );
        // Stale targets and an empty queue do nothing.
        let stale = Some(QueueTarget::Item(ItemId(99)));
        assert!(act(&mut app, queue(QueueControl::Cancel, stale)).is_empty());
        let mut empty = super::tests::app();
        assert!(act(&mut empty, queue(QueueControl::Cancel, None)).is_empty());
    }

    // Queue cursor.

    /// Three jobs of three items each: twelve rows.
    fn with_many_rows() -> App {
        let mut app = app();
        for job in 1..=3 {
            let items = (0..3)
                .map(|n| item(job * 10 + n, job, "f", 1, ItemState::Queued))
                .collect();
            app.transfer
                .jobs
                .push(queue_job(job, Some(TransferMode::PerFile), items));
        }
        app
    }

    #[test]
    fn queue_cursor_moves_only_in_queue_mode() {
        let mut app = with_many_rows();
        assert_eq!(
            app.transfer_action(Action::MoveCursor(CursorMove::Down)),
            Err(Action::MoveCursor(CursorMove::Down))
        );
        let click = Action::Click {
            target: ClickTarget::QueueRow(QueueTarget::Job(JobId(2))),
            modifiers: ClickModifiers::default(),
            at: Instant::now(),
        };
        assert_eq!(app.transfer_action(click.clone()), Err(click.clone()));

        focus_queue(&mut app);
        // 80x24 with the queue shown: a 7-row panel, 5 list rows.
        assert_eq!(app.queue_page(), 5);
        let moves = [
            (CursorMove::Down, 1, 0),
            (CursorMove::PageDown, 6, 2),
            (CursorMove::End, 11, 7),
            (CursorMove::Down, 11, 7),
            (CursorMove::Up, 10, 7),
            (CursorMove::PageUp, 5, 5),
            (CursorMove::Home, 0, 0),
            (CursorMove::Up, 0, 0),
        ];
        for (movement, cursor, first) in moves {
            act(&mut app, Action::MoveCursor(movement));
            assert_eq!(
                (app.transfer.cursor, app.transfer.first_visible),
                (cursor, first),
                "{movement:?}"
            );
        }
        act(&mut app, click);
        assert_eq!(app.transfer.cursor, 4);
        let double = Action::DoubleClick {
            target: ClickTarget::QueueRow(QueueTarget::Item(ItemId(32))),
            at: Instant::now(),
        };
        act(&mut app, double);
        assert_eq!((app.transfer.cursor, app.transfer.first_visible), (11, 7));
    }

    #[test]
    fn queue_cursor_survives_a_hidden_panel() {
        let mut app = with_many_rows();
        // Without a queue panel the page is one row.
        assert_eq!(app.queue_page(), 1);
        app.transfer.cursor = 5;
        let keep = app.transfer.cursor_target();
        app.transfer.place_cursor(keep, app.queue_page());
        assert_eq!((app.transfer.cursor, app.transfer.first_visible), (5, 5));
    }

    #[test]
    fn queue_wheel_scrolls_in_any_mode() {
        let mut app = with_many_rows();
        app.handle_action(Action::ToggleQueue);
        let scroll = |lines| Action::Scroll {
            region: ScrollRegion::Queue,
            lines,
        };
        act(&mut app, scroll(3));
        assert_eq!((app.transfer.first_visible, app.transfer.cursor), (3, 3));
        act(&mut app, scroll(30));
        assert_eq!((app.transfer.first_visible, app.transfer.cursor), (7, 7));
        app.transfer.cursor = 10;
        act(&mut app, scroll(-3));
        assert_eq!((app.transfer.first_visible, app.transfer.cursor), (4, 8));
        act(&mut app, scroll(-30));
        assert_eq!((app.transfer.first_visible, app.transfer.cursor), (0, 4));
        let pane_scroll = Action::Scroll {
            region: ScrollRegion::Pane(PaneSide::Left),
            lines: 3,
        };
        assert_eq!(app.transfer_action(pane_scroll.clone()), Err(pane_scroll));
    }

    #[test]
    fn other_actions_are_handed_back() {
        let mut app = app();
        for action in [
            Action::Open,
            Action::Quit,
            Action::ToggleQueue,
            Action::MakeDir,
            Action::Reveal,
            Action::Confirm,
        ] {
            assert_eq!(app.transfer_action(action.clone()), Err(action));
        }
    }

    // Views.

    type RowShape = (
        QueueTarget,
        bool,
        String,
        String,
        Progress,
        Vec<QueueControl>,
    );

    fn view_rows(app: &App) -> Vec<RowShape> {
        let mut texts = Vec::new();
        let mut rows = Vec::new();
        let view = app.queue_view(&mut texts, &mut rows);
        view.rows
            .iter()
            .map(|row| {
                (
                    row.target,
                    row.indent,
                    row.label.to_string(),
                    row.state.to_string(),
                    row.progress,
                    row.controls.to_vec(),
                )
            })
            .collect()
    }

    #[test]
    fn queue_view_shows_jobs_followed_by_items() {
        let mut app = with_queue();
        assert_eq!(
            view_rows(&app),
            [
                (
                    QueueTarget::Job(JobId(1)),
                    false,
                    "copy a.txt -> web:/srv".to_string(),
                    "running 25%".to_string(),
                    Progress::Bytes {
                        done: 50,
                        total: 200
                    },
                    PAUSE_CANCEL.to_vec(),
                ),
                (
                    QueueTarget::Item(ItemId(10)),
                    true,
                    "a.txt".to_string(),
                    "running".to_string(),
                    Progress::Bytes {
                        done: 50,
                        total: 100
                    },
                    PAUSE_CANCEL.to_vec(),
                ),
                (
                    QueueTarget::Item(ItemId(11)),
                    true,
                    "b.txt".to_string(),
                    "paused".to_string(),
                    Progress::None,
                    PAUSE_CANCEL.to_vec(),
                ),
            ]
        );
        {
            let mut texts = Vec::new();
            let mut rows = Vec::new();
            let view = app.queue_view(&mut texts, &mut rows);
            assert_eq!(
                (view.cursor, view.first_visible, view.focused),
                (Some(0), 0, false)
            );
        }
        focus_queue(&mut app);
        assert!(app.queue_view(&mut Vec::new(), &mut Vec::new()).focused);
    }

    #[test]
    fn queue_view_of_an_empty_queue() {
        let app = app();
        let mut texts = vec!["stale".to_string()];
        let mut rows = Vec::new();
        let view = app.queue_view(&mut texts, &mut rows);
        assert!(view.rows.is_empty());
        assert_eq!(view.cursor, None);
    }

    #[test]
    fn queue_view_job_states_and_badges() {
        let mut app = app();
        // Being expanded.
        app.transfer.jobs.push(queue_job(1, None, Vec::new()));
        // An rsync sync.
        app.transfer.jobs.push(sync_job(2));
        // A tar job with a file waiting for reconnect.
        app.transfer.jobs.push(queue_job(
            3,
            Some(TransferMode::Tar),
            vec![item(30, 3, "t", 4, ItemState::WaitingForReconnect)],
        ));
        // Finished with a failed file, a failed directory, and a skip.
        let failed_state = || ItemState::Failed { error: not_found() };
        let mut finished = queue_job(
            4,
            Some(TransferMode::PerFile),
            vec![
                item(40, 4, "bad", 4, failed_state()),
                Item {
                    kind: ItemKind::Dir,
                    ..item(41, 4, "d", 0, failed_state())
                },
                item(
                    42,
                    4,
                    "old",
                    4,
                    ItemState::Skipped {
                        reason: SkipReason::NotNewer,
                    },
                ),
            ],
        );
        finished.outcome = Some(JobOutcome::Finished(JobSummary {
            done: 0,
            skipped: 1,
            failed: 2,
        }));
        app.transfer.jobs.push(finished);
        // Could not start.
        let mut failed = queue_job(5, None, Vec::new());
        failed.outcome = Some(JobOutcome::Failed(not_found()));
        app.transfer.jobs.push(failed);

        let shown: Vec<(String, String, Progress, Vec<QueueControl>)> = view_rows(&app)
            .into_iter()
            .map(|(_, _, label, state, progress, controls)| (label, state, progress, controls))
            .collect();
        let failed_text = "failed: /x does not exist".to_string();
        let row = |label: &str, state: &str, progress, controls: &[QueueControl]| {
            (
                label.to_string(),
                state.to_string(),
                progress,
                controls.to_vec(),
            )
        };
        assert_eq!(
            shown,
            [
                row("job 1", "preparing", Progress::None, PAUSE_CANCEL),
                row(
                    "sync -> web:/srv",
                    "syncing",
                    Progress::Indeterminate,
                    CANCEL
                ),
                row(
                    "job 3 [tar, no resume]",
                    "reconnecting",
                    Progress::Bytes { done: 0, total: 4 },
                    CANCEL
                ),
                row("t", "reconnecting", Progress::None, CANCEL),
                row("job 4", "2 failed", Progress::None, RETRY_DISCARD_CLEAR),
                row("bad", &failed_text, Progress::None, RETRY_DISCARD),
                row("d/", &failed_text, Progress::None, RETRY),
                row("old", "not newer", Progress::None, NO_CONTROLS),
                row("job 5", &failed_text, Progress::None, CLEAR),
            ]
        );
    }

    #[test]
    fn job_state_text_covers_every_phase() {
        let app = app();
        let state = |state| {
            let items = vec![item(1, 1, "f", 10, state)];
            app.job_state_text(&queue_job(1, Some(TransferMode::PerFile), items))
        };
        assert_eq!(state(ItemState::WaitingForConflict), "conflict");
        assert_eq!(state(ItemState::Running { bytes: 5 }), "running 50%");
        assert_eq!(state(ItemState::WaitingForReconnect), "reconnecting");
        assert_eq!(state(ItemState::Queued), "queued");
        assert_eq!(state(ItemState::Paused), "paused");
        assert_eq!(state(ItemState::Done), "finishing");
        let finished = |done, skipped, failed| {
            let mut job = queue_job(1, Some(TransferMode::Tar), Vec::new());
            job.outcome = Some(JobOutcome::Finished(JobSummary {
                done,
                skipped,
                failed,
            }));
            app.job_state_text(&job)
        };
        assert_eq!(finished(3, 0, 0), "done");
        assert_eq!(finished(0, 0, 0), "done");
        assert_eq!(finished(2, 1, 0), "1 skipped");
        assert_eq!(finished(0, 2, 0), "skipped");
        assert_eq!(finished(1, 1, 1), "1 failed");
    }

    #[test]
    fn finished_retryable_tar_job_offers_retry_and_clear() {
        let mut job = queue_job(
            1,
            Some(TransferMode::Tar),
            vec![item(1, 1, "f", 1, ItemState::Failed { error: not_found() })],
        );
        job.outcome = Some(JobOutcome::Finished(JobSummary::default()));
        assert_eq!(job_controls(&job), RETRY_CLEAR);
        job.items.clear();
        assert_eq!(job_controls(&job), CLEAR);
    }

    #[test]
    fn item_texts_and_controls() {
        let job = queue_job(1, Some(TransferMode::PerFile), Vec::new());
        let skipped = |reason| ItemState::Skipped { reason };
        let cases = [
            (ItemState::Queued, "queued", PAUSE_CANCEL),
            (ItemState::Running { bytes: 1 }, "running", PAUSE_CANCEL),
            (ItemState::Paused, "paused", PAUSE_CANCEL),
            (ItemState::WaitingForReconnect, "reconnecting", CANCEL),
            (ItemState::WaitingForConflict, "conflict", CANCEL),
            (ItemState::Done, "done", NO_CONTROLS),
            (skipped(SkipReason::UserSkipped), "skipped", NO_CONTROLS),
            (skipped(SkipReason::NotNewer), "not newer", NO_CONTROLS),
            (
                skipped(SkipReason::SpecialFile),
                "special file",
                NO_CONTROLS,
            ),
            (skipped(SkipReason::Canceled), "canceled", RETRY_DISCARD),
        ];
        for (state, text, controls) in cases {
            assert_eq!(item_state_text(&state), text);
            let entry = item(1, 1, "f", 1, state);
            assert_eq!(item_controls(&job, &entry), controls, "{text}");
        }
        let tar = queue_job(1, Some(TransferMode::Tar), Vec::new());
        assert_eq!(
            item_controls(&tar, &item(1, 1, "f", 1, ItemState::Queued)),
            CANCEL
        );
        let link = Item {
            kind: ItemKind::Symlink { target: "t".into() },
            ..item(1, 1, "l", 0, ItemState::Failed { error: not_found() })
        };
        assert_eq!(item_controls(&job, &link), RETRY);
        assert_eq!(item_label(&link), "l");
    }

    #[test]
    fn percent_handles_edges() {
        assert_eq!(percent(0, 0), 100);
        assert_eq!(percent(1, 3), 33);
        assert_eq!(percent(5, 3), 100);
        assert_eq!(percent(u64::MAX - 1, u64::MAX), 99);
    }

    #[test]
    fn transfer_summary_counts_jobs() {
        let mut app = app();
        assert_eq!(app.transfer_summary(), "");
        let mut done = queue_job(1, Some(TransferMode::PerFile), Vec::new());
        done.outcome = Some(JobOutcome::Finished(JobSummary::default()));
        app.transfer.jobs.push(done);
        assert_eq!(app.transfer_summary(), "");
        app.transfer.jobs.push(queue_job(
            2,
            Some(TransferMode::PerFile),
            vec![item(20, 2, "f", 1, ItemState::Running { bytes: 0 })],
        ));
        app.transfer.jobs.push(sync_job(3));
        app.transfer.jobs.push(queue_job(
            4,
            Some(TransferMode::PerFile),
            vec![item(40, 4, "f", 1, ItemState::Paused)],
        ));
        let mut failed = queue_job(5, None, Vec::new());
        failed.outcome = Some(JobOutcome::Failed(not_found()));
        app.transfer.jobs.push(failed);
        let mut partly = queue_job(6, None, Vec::new());
        partly.outcome = Some(JobOutcome::Finished(JobSummary {
            done: 1,
            skipped: 0,
            failed: 1,
        }));
        app.transfer.jobs.push(partly);
        assert_eq!(app.transfer_summary(), "2 running, 1 paused, 2 failed");
    }

    #[test]
    fn entry_attributes_show_size_and_time() {
        let mut source = file("a.txt", 1);
        source.mtime = Some(0);
        let expected = format!("1 B, modified {}", format_local_mtime(0).expect("epoch"));
        assert_eq!(entry_attributes(&source), expected);
        let unknown = Entry {
            size: None,
            ..file("a", 0)
        };
        assert_eq!(entry_attributes(&unknown), "size unknown");
    }

    #[test]
    fn describe_marks_sudo_endpoints() {
        let sudo = Endpoint::Remote {
            host: "web".into(),
            sudo: true,
        };
        assert_eq!(describe(&sudo, Path::new("/etc")), "web:/etc [sudo]");
        assert_eq!(describe(&Endpoint::Local, Path::new("/etc")), "/etc");
        assert_eq!(file_name(Path::new("/")), "/");
        assert_eq!(count_noun(1, "item"), "1 item");
        assert_eq!(count_noun(0, "item"), "0 items");
    }
}
