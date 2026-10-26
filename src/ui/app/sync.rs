//! Directory comparison in the app: the comparison view, entry selection,
//! and starting a one-way sync.
//!
//! # Behavior
//!
//! - `=` (browse mode): when both panes are listed, emits `Effect::Compare`
//!   of their locations ([`SyncState::pending`] holds its request id) and
//!   shows `comparing...` in the status bar; a second `=` while pending
//!   cancels the first (`Effect::CancelRequest`) and compares again.
//! - `Compared` for the pending request opens [`SyncView`]: every entry
//!   except `DiffStatus::Same` files is visible (directories present on both
//!   sides stay visible so their contents read as a tree), none selected,
//!   cursor on the first row. A failure is an error status. Answers for
//!   other request ids are dropped.
//! - Comparison view (`InputMode::SyncView`): `MoveCursor`, `Scroll {
//!   region: ScrollRegion::SyncView }`, `Click` on `ClickTarget::SyncRow`
//!   move; `ToggleSelect` toggles the cursor entry and moves down;
//!   `Cancel` closes the view; `Help` is handed back (core passes it to the
//!   preview feature's help viewer).
//! - `StartSync(direction)` with at least one selected entry pushes a
//!   `DialogPurpose::ConfirmSync { direction }` dialog: "Copy N entries
//!   left -> right?" (or right -> left), Ok / Cancel, and when
//!   `sync::extraneous` is not empty for the selection, the checkbox
//!   "Delete M entries that exist only on the destination" (unchecked, so
//!   deleting is off by default). Its Ok answer
//!   (`App::sync_dialog_answer`) emits `Effect::Sync` with a new job id,
//!   the comparison, the selected relative paths, and `delete_extraneous`
//!   from the checkbox; pushes a `QueueJob { sync: true, mode: None, .. }`
//!   onto `TransferState::jobs`; records a [`RunningSync`]; and closes the
//!   view.
//! - `SyncFinished` sets the queue job's `outcome` (`Finished` with a
//!   default summary, or `Failed`), removes the [`RunningSync`], and
//!   refreshes the destination pane with `App::refresh`. A sync that runs as
//!   an engine job also finishes through `TransferEvent`s of the same job
//!   id, which the transfer feature handles; it removes the `RunningSync`
//!   here when its `QueueJob` has an outcome (checked by
//!   `App::sync_finished` and by `App::sync_jobs_settled`).
//!
//! Details this module settles:
//!
//! - `Compare` needs both panes listed and showing different directories;
//!   otherwise it only sets a status message. A successful comparison
//!   replaces `comparing...` with the number of differing entries.
//! - Toggling a directory (on either side) gives every visible entry beneath
//!   it the same new state, so one key selects a whole subtree; neither
//!   sync backend recurses on its own. `SelectAll` selects and
//!   `InvertSelection` inverts every visible entry.
//! - `StartSync` without a selection sets a status message. The dialog's
//!   N counts the selected entries that exist on the source side; M is
//!   `sync::extraneous(..).len()`, the paths the executor would delete.
//! - The view is drawn over every row above the status bar; its list rows
//!   are that height minus three (the two borders and the header row of
//!   `widgets::sync_view`); paging and scrolling use this. [`App::sync_view`] pulls `first_visible` onto the cursor's page
//!   so a terminal resize never hides the cursor.
//! - The queue job of a sync is a `JobKind::Copy` labeled
//!   `sync N entries -> <destination>`.
//!
//! # Public API (Wave 3, ui/app/sync)
//!
//! ```text
//! impl App {
//!     /// Claims `Compare` in browse mode and every action in
//!     /// `InputMode::SyncView` except `Help`.
//!     pub fn sync_action(&mut self, action: Action) -> Result<Vec<Effect>, Action>;
//!     /// `BgEvent::Compared`.
//!     pub fn sync_compared(&mut self, request: RequestId, result: Result<Box<Comparison>, Error>) -> Vec<Effect>;
//!     /// `BgEvent::SyncFinished`.
//!     pub fn sync_finished(&mut self, job: JobId, result: Result<(), Error>) -> Vec<Effect>;
//!     /// After any `BgEvent::Transfer`: forgets running syncs whose engine
//!     /// job finished.
//!     pub fn sync_jobs_settled(&mut self);
//!     /// The answer of `DialogPurpose::ConfirmSync`.
//!     pub fn sync_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect>;
//!     /// `Some(InputMode::SyncView)` while the view is open.
//!     pub fn sync_input_mode(&self) -> Option<InputMode>;
//!
//!     /// View accessor for render.
//!     pub fn sync_view(&self) -> Option<SyncListView<'_>>;
//! }
//! ```

use std::path::PathBuf;

use super::core::StatusMessage;
use super::dialog::{Checkbox, Dialog, DialogAnswer, DialogFocus, DialogPurpose};
use super::transfer::{JobOutcome, QueueJob};
use super::App;
use crate::error::Error;
use crate::sync::{Comparison, ComparisonEntry, DiffStatus, SyncDirection};
use crate::transfer::model::{JobId, JobKind, JobSummary};
use crate::ui::action::{
    Action, ClickTarget, CursorMove, DialogButton, Effect, InputMode, PaneSide, RequestId,
    ScrollRegion,
};
use crate::ui::widgets::sync_view::SyncListView;
use crate::vfs::{Endpoint, Entry, EntryKind, Location};

/// Rows of the comparison view that hold no list row: the top and bottom
/// borders and the header row drawn by `widgets::sync_view`.
const VIEW_CHROME_ROWS: u16 = 3;

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct SyncState {
    /// The comparison view while open.
    pub view: Option<SyncView>,
    /// The `Effect::Compare` in flight.
    pub pending: Option<RequestId>,
    /// Syncs started and not finished; their endpoints stay connected.
    pub running: Vec<RunningSync>,
}

/// The open comparison view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncView {
    /// The comparison shown.
    pub comparison: Box<Comparison>,
    /// Indices into `comparison.entries` shown, in order.
    pub visible: Vec<usize>,
    /// One flag per `comparison.entries` element.
    pub selected: Vec<bool>,
    /// Cursor position in `visible`.
    pub cursor: usize,
    /// First position of `visible` on screen.
    pub first_visible: usize,
}

/// A sync started with `Effect::Sync` that has not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningSync {
    /// Its job id.
    pub job: JobId,
    /// The left directory's endpoint.
    pub left: Endpoint,
    /// The right directory's endpoint.
    pub right: Endpoint,
}

impl SyncView {
    /// A view of `comparison` with nothing selected and the cursor on the
    /// first row.
    fn new(comparison: Box<Comparison>) -> SyncView {
        let visible = comparison
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| is_shown(entry))
            .map(|(index, _)| index)
            .collect();
        let selected = vec![false; comparison.entries.len()];
        SyncView {
            comparison,
            visible,
            selected,
            cursor: 0,
            first_visible: 0,
        }
    }

    fn move_cursor(&mut self, movement: CursorMove, page: usize) {
        let page = page.max(1);
        let last = self.visible.len().saturating_sub(1);
        self.cursor = match movement {
            CursorMove::Up => self.cursor.saturating_sub(1),
            CursorMove::Down => (self.cursor + 1).min(last),
            CursorMove::PageUp => self.cursor.saturating_sub(page),
            CursorMove::PageDown => (self.cursor + page).min(last),
            CursorMove::Home => 0,
            CursorMove::End => last,
        };
        self.clamp(page);
    }

    fn set_cursor(&mut self, position: usize, page: usize) {
        self.cursor = position;
        self.clamp(page);
    }

    /// Mouse wheel: moves `first_visible` by `lines` (negative is up),
    /// dragging the cursor along only when it would leave the page.
    fn scroll_by(&mut self, lines: i32, page: usize) {
        let page = page.max(1);
        let max_first = self.visible.len().saturating_sub(page);
        let distance = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
        self.first_visible = if lines < 0 {
            self.first_visible.saturating_sub(distance)
        } else {
            self.first_visible.saturating_add(distance)
        }
        .min(max_first);
        self.cursor = self
            .cursor
            .clamp(self.first_visible, self.first_visible + page - 1);
        self.clamp(page);
    }

    /// Keeps `cursor` on a visible row and on the page starting at
    /// `first_visible`.
    fn clamp(&mut self, page: usize) {
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
        self.first_visible = page_start(self.cursor, self.first_visible, self.visible.len(), page);
    }

    /// Toggles the cursor entry (and, for a directory, every visible entry
    /// beneath it), then moves down one row.
    fn toggle_and_advance(&mut self, page: usize) {
        let Some(&index) = self.visible.get(self.cursor) else {
            return;
        };
        let state = !self.selected[index];
        let entry = &self.comparison.entries[index];
        if is_dir(entry.left.as_ref()) || is_dir(entry.right.as_ref()) {
            for &other in &self.visible {
                if self.comparison.entries[other]
                    .relative
                    .starts_with(&entry.relative)
                {
                    self.selected[other] = state;
                }
            }
        } else {
            self.selected[index] = state;
        }
        self.move_cursor(CursorMove::Down, page);
    }

    fn select_all(&mut self) {
        for &index in &self.visible {
            self.selected[index] = true;
        }
    }

    fn invert_selection(&mut self) {
        for &index in &self.visible {
            self.selected[index] = !self.selected[index];
        }
    }

    /// The selected entries in comparison order.
    fn selected_entries(&self) -> impl Iterator<Item = &ComparisonEntry> {
        self.comparison
            .entries
            .iter()
            .zip(&self.selected)
            .filter(|(_, &selected)| selected)
            .map(|(entry, _)| entry)
    }

    /// The selected relative paths in comparison order.
    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected_entries()
            .map(|entry| entry.relative.clone())
            .collect()
    }

    /// The confirmation dialog for syncing the selection in `direction`;
    /// `None` when nothing is selected.
    fn confirm_dialog(&self, direction: SyncDirection) -> Option<Dialog> {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return None;
        }
        let copied = self
            .selected_entries()
            .filter(|entry| source_side(entry, direction).is_some())
            .count();
        let extraneous = crate::sync::extraneous(direction, &self.comparison, &paths).len();
        let (source, destination) = directories(&self.comparison, direction);
        let message = format!(
            "Copy {} {}?\nFrom: {}\nTo: {}",
            count(copied),
            arrow(direction),
            location_label(source),
            location_label(destination)
        );
        let checkbox = (extraneous > 0).then(|| Checkbox {
            label: format!(
                "Delete {} that {} only on the destination",
                count(extraneous),
                if extraneous == 1 { "exists" } else { "exist" }
            ),
            checked: false,
        });
        Some(Dialog {
            purpose: DialogPurpose::ConfirmSync { direction },
            title: "Sync".to_owned(),
            message,
            field: None,
            checkbox,
            buttons: vec![DialogButton::Ok, DialogButton::Cancel],
            focus: DialogFocus::Button(0),
        })
    }
}

impl App {
    /// Claims `Compare` in browse mode and every action in
    /// `InputMode::SyncView` except `Help`.
    pub fn sync_action(&mut self, action: Action) -> Result<Vec<Effect>, Action> {
        let page = self.sync_page_height();
        let Some(view) = self.sync.view.as_mut() else {
            return match action {
                Action::Compare => Ok(self.compare()),
                other => Err(other),
            };
        };
        match action {
            Action::Help => return Err(Action::Help),
            Action::MoveCursor(movement) => view.move_cursor(movement, page),
            Action::Scroll {
                region: ScrollRegion::SyncView,
                lines,
            } => view.scroll_by(lines, page),
            Action::Click {
                target: ClickTarget::SyncRow(position),
                ..
            } => view.set_cursor(position, page),
            Action::ToggleSelect => view.toggle_and_advance(page),
            Action::SelectAll => view.select_all(),
            Action::InvertSelection => view.invert_selection(),
            Action::StartSync(direction) => match view.confirm_dialog(direction) {
                Some(dialog) => self.dialog.stack.push(dialog),
                None => self.core.set_status(StatusMessage::info(
                    "Nothing selected: Space selects entries to sync",
                )),
            },
            Action::Cancel => self.sync.view = None,
            // The view is modal: everything else is claimed and ignored.
            _ => {}
        }
        Ok(Vec::new())
    }

    /// `BgEvent::Compared`.
    pub fn sync_compared(
        &mut self,
        request: RequestId,
        result: Result<Box<Comparison>, Error>,
    ) -> Vec<Effect> {
        if self.sync.pending != Some(request) {
            return Vec::new();
        }
        self.sync.pending = None;
        match result {
            Ok(comparison) => {
                let differing = comparison
                    .entries
                    .iter()
                    .filter(|entry| entry.status != DiffStatus::Same)
                    .count();
                let text = match differing {
                    0 => "No differences".to_owned(),
                    1 => "1 entry differs".to_owned(),
                    n => format!("{n} entries differ"),
                };
                self.core.set_status(StatusMessage::info(text));
                self.sync.view = Some(SyncView::new(comparison));
            }
            Err(error) => self
                .core
                .set_status(StatusMessage::error(format!("Compare failed: {error}"))),
        }
        Vec::new()
    }

    /// `BgEvent::SyncFinished`.
    pub fn sync_finished(&mut self, job: JobId, result: Result<(), Error>) -> Vec<Effect> {
        let Some(queued) = self
            .transfer
            .jobs
            .iter_mut()
            .find(|queued| queued.id == job)
        else {
            self.sync_jobs_settled();
            return Vec::new();
        };
        let status = match &result {
            Ok(()) => StatusMessage::info(format!("Finished {}", queued.label)),
            Err(error) => StatusMessage::error(format!("Failed {}: {error}", queued.label)),
        };
        queued.outcome = Some(match result {
            Ok(()) => JobOutcome::Finished(JobSummary::default()),
            Err(error) => JobOutcome::Failed(error),
        });
        let destination = queued.destination.clone();
        self.core.set_status(status);
        self.sync_jobs_settled();
        let mut effects = Vec::new();
        for side in [PaneSide::Left, PaneSide::Right] {
            if self.core.pane(side).endpoint == destination {
                effects.extend(self.refresh(side));
            }
        }
        effects
    }

    /// After any `BgEvent::Transfer`: forgets running syncs whose engine
    /// job finished.
    pub fn sync_jobs_settled(&mut self) {
        let jobs = &self.transfer.jobs;
        self.sync.running.retain(|running| {
            jobs.iter()
                .any(|queued| queued.id == running.job && queued.outcome.is_none())
        });
    }

    /// The answer of `DialogPurpose::ConfirmSync`.
    pub fn sync_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect> {
        let DialogPurpose::ConfirmSync { direction } = answer.purpose else {
            return Vec::new();
        };
        if answer.button != DialogButton::Ok {
            return Vec::new();
        }
        let Some(view) = self.sync.view.take() else {
            return Vec::new();
        };
        let paths = view.selected_paths();
        let comparison = view.comparison;
        let job = self.core.next_job();
        let (source, destination) = directories(&comparison, direction);
        let label = format!(
            "sync {} -> {}",
            count(paths.len()),
            location_label(destination)
        );
        self.core
            .set_status(StatusMessage::info(format!("Started {label}")));
        self.transfer.jobs.push(QueueJob {
            id: job,
            kind: JobKind::Copy,
            mode: None,
            source: source.endpoint.clone(),
            destination: destination.endpoint.clone(),
            label,
            sync: true,
            items: Vec::new(),
            outcome: None,
        });
        self.sync.running.push(RunningSync {
            job,
            left: comparison.left.endpoint.clone(),
            right: comparison.right.endpoint.clone(),
        });
        vec![Effect::Sync {
            job,
            direction,
            comparison,
            paths,
            delete_extraneous: answer.checked,
        }]
    }

    /// `Some(InputMode::SyncView)` while the view is open.
    pub fn sync_input_mode(&self) -> Option<InputMode> {
        self.sync.view.as_ref().map(|_| InputMode::SyncView)
    }

    /// View accessor for render.
    pub fn sync_view(&self) -> Option<SyncListView<'_>> {
        let page = self.sync_page_height();
        self.sync.view.as_ref().map(|view| SyncListView {
            comparison: &view.comparison,
            visible: &view.visible,
            selected: &view.selected,
            cursor: view.cursor,
            first_visible: page_start(view.cursor, view.first_visible, view.visible.len(), page),
        })
    }

    /// `=`: compares the directories of both panes.
    fn compare(&mut self) -> Vec<Effect> {
        let (Some(left), Some(right)) = (
            self.core.pane(PaneSide::Left).location(),
            self.core.pane(PaneSide::Right).location(),
        ) else {
            self.core.set_status(StatusMessage::info(
                "Both panes must show a directory to compare",
            ));
            return Vec::new();
        };
        if left == right {
            self.core
                .set_status(StatusMessage::info("Both panes show the same directory"));
            return Vec::new();
        }
        let mut effects = Vec::new();
        if let Some(previous) = self.sync.pending.take() {
            effects.push(Effect::CancelRequest(previous));
        }
        let request = self.core.next_request();
        self.sync.pending = Some(request);
        self.core.set_status(StatusMessage::info("comparing..."));
        effects.push(Effect::Compare {
            request,
            left,
            right,
        });
        effects
    }

    /// List rows of the comparison view: every row above the status bar
    /// minus the view's chrome.
    fn sync_page_height(&self) -> usize {
        let layout = self.core.layout();
        usize::from(layout.status_bar.y.saturating_sub(VIEW_CHROME_ROWS))
    }
}

/// The first position of the page that shows `cursor`, staying at
/// `first_visible` when it already does; a page of 0 rows counts as 1.
fn page_start(cursor: usize, first_visible: usize, len: usize, page: usize) -> usize {
    let page = page.max(1);
    let first = first_visible.min(len.saturating_sub(page));
    if cursor < first {
        cursor
    } else if cursor >= first + page {
        cursor + 1 - page
    } else {
        first
    }
}

/// Every entry except `Same` files is shown; directories present on both
/// sides (always `Same`) stay so their contents read as a tree.
fn is_shown(entry: &ComparisonEntry) -> bool {
    entry.status != DiffStatus::Same || is_dir(entry.left.as_ref())
}

fn is_dir(entry: Option<&Entry>) -> bool {
    entry.is_some_and(|entry| entry.kind == EntryKind::Dir)
}

/// The side of `entry` that `direction` copies from.
fn source_side(entry: &ComparisonEntry, direction: SyncDirection) -> Option<&Entry> {
    match direction {
        SyncDirection::LeftToRight => entry.left.as_ref(),
        SyncDirection::RightToLeft => entry.right.as_ref(),
    }
}

/// The source and destination directories of a sync.
fn directories(comparison: &Comparison, direction: SyncDirection) -> (&Location, &Location) {
    match direction {
        SyncDirection::LeftToRight => (&comparison.left, &comparison.right),
        SyncDirection::RightToLeft => (&comparison.right, &comparison.left),
    }
}

fn arrow(direction: SyncDirection) -> &'static str {
    match direction {
        SyncDirection::LeftToRight => "left -> right",
        SyncDirection::RightToLeft => "right -> left",
    }
}

/// `1 entry`, `3 entries`.
fn count(n: usize) -> String {
    if n == 1 {
        "1 entry".to_owned()
    } else {
        format!("{n} entries")
    }
}

/// `endpoint:path`, as the comparison view's title writes locations.
fn location_label(location: &Location) -> String {
    format!("{}:{}", location.endpoint, location.path.display())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::groups::Groups;
    use crate::hosts::HostList;
    use crate::preview::Background;
    use crate::settings::Settings;
    use crate::state::PersistedState;
    use crate::ui::action::{BgEvent, ClickModifiers};

    fn web() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        }
    }

    fn location(endpoint: Endpoint, path: &str) -> Location {
        Location {
            endpoint,
            path: path.into(),
        }
    }

    /// An app whose panes show `left` and `right`, both listed, on an
    /// 80x24 screen (20 view rows).
    fn app_with(left: Location, right: Location) -> App {
        let (mut app, mut pending) = App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (left.endpoint, Some(left.path)),
            Some((right.endpoint, Some(right.path))),
        );
        while let Some(effect) = pending.pop() {
            let follow = match effect {
                Effect::ListDir { request, location } => app.handle_event(BgEvent::Listed {
                    request,
                    location,
                    result: Ok(Vec::new()),
                }),
                Effect::Connect(endpoint) => app.handle_event(BgEvent::Connected {
                    endpoint,
                    home: "/home".into(),
                }),
                _ => Vec::new(),
            };
            pending.extend(follow);
        }
        app.handle_action(Action::Resize {
            width: 80,
            height: 24,
        });
        app
    }

    fn two_local() -> App {
        app_with(
            location(Endpoint::Local, "/l"),
            location(Endpoint::Local, "/r"),
        )
    }

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.into(),
            kind,
            size: Some(1),
            mtime: Some(1),
            mode: Some(0o644),
        }
    }

    /// A compared path of `kind`, present on the sides `status` implies.
    fn compared(relative: &str, status: DiffStatus, kind: EntryKind) -> ComparisonEntry {
        let side = |present: bool| present.then(|| entry(relative, kind.clone()));
        ComparisonEntry {
            relative: relative.into(),
            status,
            left: side(status != DiffStatus::RightOnly),
            right: side(status != DiffStatus::LeftOnly),
        }
    }

    fn file(relative: &str, status: DiffStatus) -> ComparisonEntry {
        compared(relative, status, EntryKind::File)
    }

    fn dir(relative: &str, status: DiffStatus) -> ComparisonEntry {
        compared(relative, status, EntryKind::Dir)
    }

    /// Differing files `f000`, `f001`, ...
    fn files(n: usize) -> Vec<ComparisonEntry> {
        (0..n)
            .map(|i| file(&format!("f{i:03}"), DiffStatus::LeftNewer))
            .collect()
    }

    fn comparison(app: &App, entries: Vec<ComparisonEntry>) -> Box<Comparison> {
        Box::new(Comparison {
            left: app.core.pane(PaneSide::Left).location().unwrap(),
            right: app.core.pane(PaneSide::Right).location().unwrap(),
            entries,
        })
    }

    /// The request id of the only effect, an `Effect::Compare`.
    fn compare_request(effects: &[Effect]) -> RequestId {
        match effects {
            [Effect::Compare { request, .. }] => *request,
            other => panic!("expected one Compare, got {other:?}"),
        }
    }

    /// Runs `=` and delivers `entries` as its comparison.
    fn open_view(app: &mut App, entries: Vec<ComparisonEntry>) {
        let request = compare_request(&app.sync_action(Action::Compare).unwrap());
        let result = Ok(comparison(app, entries));
        assert!(app.sync_compared(request, result).is_empty());
        assert_eq!(app.sync_input_mode(), Some(InputMode::SyncView));
    }

    /// Sends a view action, which must be claimed without effects.
    fn act(app: &mut App, action: Action) {
        assert_eq!(app.sync_action(action), Ok(Vec::new()));
    }

    fn view(app: &App) -> &SyncView {
        app.sync.view.as_ref().unwrap()
    }

    fn cursor_and_first(app: &App) -> (usize, usize) {
        let view = view(app);
        (view.cursor, view.first_visible)
    }

    fn status_text(app: &App) -> &str {
        &app.core.status().unwrap().text
    }

    fn answer(direction: SyncDirection, button: DialogButton, checked: bool) -> DialogAnswer {
        DialogAnswer {
            purpose: DialogPurpose::ConfirmSync { direction },
            button,
            field: None,
            checked,
        }
    }

    /// Opens a view of one differing file, selects it, confirms a sync in
    /// `direction`, and returns the sync's job id.
    fn start(app: &mut App, direction: SyncDirection) -> JobId {
        open_view(app, files(1));
        act(app, Action::SelectAll);
        match app
            .sync_dialog_answer(answer(direction, DialogButton::Ok, false))
            .as_slice()
        {
            [Effect::Sync { job, .. }] => *job,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn list_dir_locations(effects: &[Effect]) -> Vec<&Location> {
        effects
            .iter()
            .map(|effect| match effect {
                Effect::ListDir { location, .. } => location,
                other => panic!("expected ListDir, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn compare_emits_both_locations_and_shows_progress() {
        let mut app = two_local();
        let effects = app.sync_action(Action::Compare).unwrap();
        let [Effect::Compare {
            request,
            left,
            right,
        }] = effects.as_slice()
        else {
            panic!("unexpected {effects:?}");
        };
        assert_eq!(app.sync.pending, Some(*request));
        assert_eq!(*left, location(Endpoint::Local, "/l"));
        assert_eq!(*right, location(Endpoint::Local, "/r"));
        assert_eq!(status_text(&app), "comparing...");
        assert_eq!(app.sync_input_mode(), None);
    }

    #[test]
    fn second_compare_cancels_the_first_and_drops_its_answer() {
        let mut app = two_local();
        let first = compare_request(&app.sync_action(Action::Compare).unwrap());
        let effects = app.sync_action(Action::Compare).unwrap();
        let [Effect::CancelRequest(canceled), rest @ ..] = effects.as_slice() else {
            panic!("unexpected {effects:?}");
        };
        assert_eq!(*canceled, first);
        let second = compare_request(rest);
        assert_ne!(first, second);

        let stale = Ok(comparison(&app, files(1)));
        assert!(app.sync_compared(first, stale).is_empty());
        assert!(app.sync.view.is_none());
        assert_eq!(app.sync.pending, Some(second));

        let current = Ok(comparison(&app, files(2)));
        app.sync_compared(second, current);
        assert_eq!(view(&app).visible.len(), 2);
        assert_eq!(app.sync.pending, None);
        assert_eq!(status_text(&app), "2 entries differ");
    }

    #[test]
    fn compare_needs_two_listed_different_directories() {
        let mut app = app_with(
            location(Endpoint::Local, "/same"),
            location(Endpoint::Local, "/same"),
        );
        assert_eq!(app.sync_action(Action::Compare), Ok(Vec::new()));
        assert_eq!(status_text(&app), "Both panes show the same directory");
        assert_eq!(app.sync.pending, None);

        // A remote pane that has not connected yet has listed nothing.
        let (mut app, _) = App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (Endpoint::Local, Some("/l".into())),
            Some((web(), Some("/srv".into()))),
        );
        assert_eq!(app.sync_action(Action::Compare), Ok(Vec::new()));
        assert_eq!(
            status_text(&app),
            "Both panes must show a directory to compare"
        );
        assert_eq!(app.sync.pending, None);
    }

    #[test]
    fn browse_actions_other_than_compare_are_handed_back() {
        let mut app = two_local();
        assert_eq!(app.sync_action(Action::Copy), Err(Action::Copy));
        assert_eq!(
            app.sync_action(Action::StartSync(SyncDirection::LeftToRight)),
            Err(Action::StartSync(SyncDirection::LeftToRight))
        );
        assert_eq!(app.sync_action(Action::Cancel), Err(Action::Cancel));
        assert!(app.sync_view().is_none());
    }

    #[test]
    fn failed_comparison_is_an_error_status() {
        let mut app = two_local();
        let request = compare_request(&app.sync_action(Action::Compare).unwrap());
        app.sync_compared(request, Err(Error::Canceled));
        let status = app.core.status().unwrap();
        assert!(status.error);
        assert_eq!(status.text, "Compare failed: Canceled");
        assert!(app.sync.view.is_none());
        assert_eq!(app.sync.pending, None);
    }

    #[test]
    fn same_files_are_hidden_and_same_directories_kept() {
        let mut app = two_local();
        open_view(
            &mut app,
            vec![
                dir("d", DiffStatus::Same),
                file("d/a", DiffStatus::Same),
                file("d/b", DiffStatus::LeftNewer),
                file("e", DiffStatus::RightOnly),
                dir("k", DiffStatus::KindDiffers),
                file("s", DiffStatus::SizeDiffers),
                file("z", DiffStatus::Same),
            ],
        );
        let view = view(&app);
        assert_eq!(view.visible, [0, 2, 3, 4, 5]);
        assert_eq!(view.selected, [false; 7]);
        assert_eq!((view.cursor, view.first_visible), (0, 0));
        // The `Same` directory is shown but does not differ.
        assert_eq!(status_text(&app), "4 entries differ");
    }

    #[test]
    fn identical_trees_open_an_empty_view() {
        let mut app = two_local();
        open_view(&mut app, vec![file("a", DiffStatus::Same)]);
        assert!(view(&app).visible.is_empty());
        assert_eq!(status_text(&app), "No differences");
        act(&mut app, Action::MoveCursor(CursorMove::End));
        act(&mut app, Action::ToggleSelect);
        assert_eq!(cursor_and_first(&app), (0, 0));
        assert_eq!(view(&app).selected, [false]);
    }

    #[test]
    fn cursor_moves_page_by_page_and_stays_on_screen() {
        let mut app = two_local();
        open_view(&mut app, files(50));
        // 24 rows: 23 above the status bar, minus 3 rows of chrome.
        assert_eq!(app.sync_page_height(), 20);
        act(&mut app, Action::MoveCursor(CursorMove::PageDown));
        assert_eq!(cursor_and_first(&app), (20, 1));
        act(&mut app, Action::MoveCursor(CursorMove::End));
        assert_eq!(cursor_and_first(&app), (49, 30));
        act(&mut app, Action::MoveCursor(CursorMove::Down));
        assert_eq!(cursor_and_first(&app), (49, 30));
        act(&mut app, Action::MoveCursor(CursorMove::PageUp));
        assert_eq!(cursor_and_first(&app), (29, 29));
        act(&mut app, Action::MoveCursor(CursorMove::Home));
        assert_eq!(cursor_and_first(&app), (0, 0));
        act(&mut app, Action::MoveCursor(CursorMove::Up));
        assert_eq!(cursor_and_first(&app), (0, 0));
    }

    #[test]
    fn wheel_scrolls_and_drags_the_cursor_along() {
        let mut app = two_local();
        open_view(&mut app, files(50));
        let scroll = |lines| Action::Scroll {
            region: ScrollRegion::SyncView,
            lines,
        };
        act(&mut app, scroll(5));
        assert_eq!(cursor_and_first(&app), (5, 5));
        act(&mut app, scroll(100));
        assert_eq!(cursor_and_first(&app), (30, 30));
        act(&mut app, Action::MoveCursor(CursorMove::End));
        act(&mut app, scroll(-3));
        assert_eq!(cursor_and_first(&app), (46, 27));
        act(&mut app, scroll(-100));
        assert_eq!(cursor_and_first(&app), (19, 0));
        // The wheel over another region is claimed and ignored.
        act(
            &mut app,
            Action::Scroll {
                region: ScrollRegion::Pane(PaneSide::Left),
                lines: 5,
            },
        );
        assert_eq!(cursor_and_first(&app), (19, 0));
    }

    #[test]
    fn click_moves_the_cursor_clamped_to_the_last_row() {
        let mut app = two_local();
        open_view(&mut app, files(5));
        let click = |position| Action::Click {
            target: ClickTarget::SyncRow(position),
            modifiers: ClickModifiers::default(),
            at: Instant::now(),
        };
        act(&mut app, click(3));
        assert_eq!(view(&app).cursor, 3);
        act(&mut app, click(99));
        assert_eq!(view(&app).cursor, 4);
        assert_eq!(view(&app).selected, [false; 5]);
    }

    #[test]
    fn view_accessor_keeps_the_cursor_visible_after_a_resize() {
        let mut app = two_local();
        open_view(&mut app, files(50));
        act(&mut app, Action::MoveCursor(CursorMove::End));
        app.handle_action(Action::Resize {
            width: 80,
            height: 10,
        });
        // 9 rows above the status bar leave 6 list rows.
        let shown = app.sync_view().unwrap();
        assert_eq!(shown.cursor, 49);
        assert_eq!(shown.first_visible, 44);
        assert_eq!(shown.visible, view(&app).visible.as_slice());
        assert_eq!(shown.selected, view(&app).selected.as_slice());
        assert_eq!(shown.comparison, view(&app).comparison.as_ref());
    }

    #[test]
    fn toggle_selects_and_advances_without_passing_the_end() {
        let mut app = two_local();
        open_view(&mut app, files(2));
        act(&mut app, Action::ToggleSelect);
        assert_eq!(view(&app).selected, [true, false]);
        assert_eq!(view(&app).cursor, 1);
        act(&mut app, Action::ToggleSelect);
        assert_eq!(view(&app).selected, [true, true]);
        assert_eq!(view(&app).cursor, 1);
        act(&mut app, Action::ToggleSelect);
        assert_eq!(view(&app).selected, [true, false]);
    }

    #[test]
    fn toggling_a_directory_selects_its_visible_subtree() {
        let mut app = two_local();
        open_view(
            &mut app,
            vec![
                dir("d", DiffStatus::Same),
                file("d/a", DiffStatus::LeftNewer),
                file("d/same", DiffStatus::Same),
                dir("d/sub", DiffStatus::LeftOnly),
                file("d/sub/x", DiffStatus::LeftOnly),
                file("d-other", DiffStatus::LeftOnly),
                file("e", DiffStatus::RightOnly),
            ],
        );
        act(&mut app, Action::ToggleSelect);
        assert_eq!(
            view(&app).selected,
            [true, true, false, true, true, false, false]
        );
        assert_eq!(view(&app).cursor, 1);
        // Toggling the selected subdirectory clears only its subtree.
        act(&mut app, Action::MoveCursor(CursorMove::Down));
        act(&mut app, Action::ToggleSelect);
        assert_eq!(
            view(&app).selected,
            [true, true, false, false, false, false, false]
        );
    }

    #[test]
    fn select_all_and_invert_cover_visible_entries_only() {
        let mut app = two_local();
        open_view(
            &mut app,
            vec![
                file("a", DiffStatus::LeftOnly),
                file("b", DiffStatus::Same),
                file("c", DiffStatus::RightNewer),
            ],
        );
        act(&mut app, Action::SelectAll);
        assert_eq!(view(&app).selected, [true, false, true]);
        act(&mut app, Action::ToggleSelect);
        act(&mut app, Action::InvertSelection);
        assert_eq!(view(&app).selected, [true, false, false]);
    }

    #[test]
    fn start_sync_without_selection_only_sets_a_status() {
        let mut app = two_local();
        open_view(&mut app, files(3));
        act(&mut app, Action::StartSync(SyncDirection::LeftToRight));
        assert!(app.dialog.stack.is_empty());
        assert_eq!(
            status_text(&app),
            "Nothing selected: Space selects entries to sync"
        );
    }

    #[test]
    fn confirmation_without_destination_only_entries_has_no_checkbox() {
        let mut app = two_local();
        open_view(
            &mut app,
            vec![
                file("a", DiffStatus::LeftNewer),
                file("b", DiffStatus::LeftOnly),
            ],
        );
        act(&mut app, Action::SelectAll);
        act(&mut app, Action::StartSync(SyncDirection::LeftToRight));
        let [dialog] = app.dialog.stack.as_slice() else {
            panic!("expected one dialog");
        };
        assert_eq!(
            dialog.purpose,
            DialogPurpose::ConfirmSync {
                direction: SyncDirection::LeftToRight
            }
        );
        assert_eq!(dialog.title, "Sync");
        assert_eq!(
            dialog.message,
            "Copy 2 entries left -> right?\nFrom: Local:/l\nTo: Local:/r"
        );
        assert_eq!(dialog.checkbox, None);
        assert!(dialog.field.is_none());
        assert_eq!(dialog.buttons, [DialogButton::Ok, DialogButton::Cancel]);
        assert_eq!(dialog.focus, DialogFocus::Button(0));
        // The view stays open below the dialog.
        assert!(app.sync.view.is_some());
    }

    #[test]
    fn confirmation_offers_deleting_destination_only_entries_unchecked() {
        let mut app = two_local();
        open_view(
            &mut app,
            vec![
                file("a", DiffStatus::LeftNewer),
                dir("old", DiffStatus::LeftOnly),
                file("old/x", DiffStatus::LeftOnly),
                file("old/y", DiffStatus::LeftOnly),
            ],
        );
        act(&mut app, Action::SelectAll);
        // Right to left, `old` and its files exist only on the destination;
        // the executor deletes the directory as one entry.
        act(&mut app, Action::StartSync(SyncDirection::RightToLeft));
        let dialog = app.dialog.stack.last().unwrap();
        assert_eq!(
            dialog.message,
            "Copy 1 entry right -> left?\nFrom: Local:/r\nTo: Local:/l"
        );
        assert_eq!(
            dialog.checkbox,
            Some(Checkbox {
                label: "Delete 1 entry that exists only on the destination".to_owned(),
                checked: false,
            })
        );
    }

    #[test]
    fn ok_emits_the_sync_queues_a_job_and_closes_the_view() {
        let mut app = app_with(location(Endpoint::Local, "/l"), location(web(), "/srv"));
        open_view(
            &mut app,
            vec![
                file("a", DiffStatus::LeftNewer),
                file("b", DiffStatus::Same),
                file("c", DiffStatus::RightOnly),
                file("d", DiffStatus::LeftOnly),
            ],
        );
        act(&mut app, Action::ToggleSelect);
        act(&mut app, Action::ToggleSelect);
        act(&mut app, Action::StartSync(SyncDirection::LeftToRight));
        let shown = view(&app).comparison.clone();
        // The dialog feature pops the dialog and leaves its answer for core.
        app.dialog.stack.pop();

        let effects =
            app.sync_dialog_answer(answer(SyncDirection::LeftToRight, DialogButton::Ok, true));
        let [Effect::Sync {
            job,
            direction,
            comparison,
            paths,
            delete_extraneous,
        }] = effects.as_slice()
        else {
            panic!("unexpected {effects:?}");
        };
        assert_eq!(*direction, SyncDirection::LeftToRight);
        assert_eq!(*comparison, shown);
        assert_eq!(paths, &[PathBuf::from("a"), PathBuf::from("c")]);
        assert!(*delete_extraneous);
        assert_eq!(
            app.transfer.jobs,
            [QueueJob {
                id: *job,
                kind: JobKind::Copy,
                mode: None,
                source: Endpoint::Local,
                destination: web(),
                label: "sync 2 entries -> web:/srv".to_owned(),
                sync: true,
                items: Vec::new(),
                outcome: None,
            }]
        );
        assert_eq!(
            app.sync.running,
            [RunningSync {
                job: *job,
                left: Endpoint::Local,
                right: web(),
            }]
        );
        assert!(app.sync.view.is_none());
        assert_eq!(app.sync_input_mode(), None);
        assert_eq!(status_text(&app), "Started sync 2 entries -> web:/srv");
    }

    #[test]
    fn unchecked_answer_keeps_destination_only_entries() {
        let mut app = two_local();
        open_view(&mut app, vec![file("only-right", DiffStatus::RightOnly)]);
        act(&mut app, Action::SelectAll);
        let effects =
            app.sync_dialog_answer(answer(SyncDirection::LeftToRight, DialogButton::Ok, false));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Sync {
                delete_extraneous: false,
                ..
            }]
        ));
    }

    #[test]
    fn every_sync_gets_a_new_job_id() {
        let mut app = app_with(location(Endpoint::Local, "/l"), location(web(), "/srv"));
        let first = start(&mut app, SyncDirection::LeftToRight);
        let second = start(&mut app, SyncDirection::RightToLeft);
        assert_ne!(first, second);
        let second_job = &app.transfer.jobs[1];
        assert_eq!(second_job.id, second);
        assert_eq!(second_job.source, web());
        assert_eq!(second_job.destination, Endpoint::Local);
        assert_eq!(second_job.label, "sync 1 entry -> Local:/l");
        assert_eq!(app.sync.running.len(), 2);
    }

    #[test]
    fn cancel_answer_keeps_the_view_open() {
        let mut app = two_local();
        open_view(&mut app, files(1));
        act(&mut app, Action::SelectAll);
        let effects = app.sync_dialog_answer(answer(
            SyncDirection::LeftToRight,
            DialogButton::Cancel,
            true,
        ));
        assert!(effects.is_empty());
        assert!(app.transfer.jobs.is_empty());
        assert!(app.sync.running.is_empty());
        assert_eq!(view(&app).selected, [true]);
    }

    #[test]
    fn esc_closes_the_view_and_help_is_handed_back() {
        let mut app = two_local();
        open_view(&mut app, files(1));
        assert_eq!(app.sync_action(Action::Help), Err(Action::Help));
        // Browse actions are claimed while the view is open.
        act(&mut app, Action::Copy);
        act(&mut app, Action::Compare);
        assert_eq!(app.sync.pending, None);
        act(&mut app, Action::Cancel);
        assert!(app.sync.view.is_none());
        assert_eq!(app.sync_input_mode(), None);
        assert!(app.sync_view().is_none());
    }

    #[test]
    fn finished_sync_records_the_outcome_and_refreshes_the_destination() {
        let mut app = app_with(location(Endpoint::Local, "/l"), location(web(), "/srv"));
        let job = start(&mut app, SyncDirection::LeftToRight);
        let effects = app.sync_finished(job, Ok(()));
        assert_eq!(list_dir_locations(&effects), [&location(web(), "/srv")]);
        assert_eq!(
            app.transfer.jobs[0].outcome,
            Some(JobOutcome::Finished(JobSummary::default()))
        );
        assert!(app.sync.running.is_empty());
        assert_eq!(status_text(&app), "Finished sync 1 entry -> web:/srv");
    }

    #[test]
    fn failed_sync_records_the_error() {
        let mut app = app_with(location(Endpoint::Local, "/l"), location(web(), "/srv"));
        let job = start(&mut app, SyncDirection::RightToLeft);
        let effects = app.sync_finished(job, Err(Error::Canceled));
        assert_eq!(
            list_dir_locations(&effects),
            [&location(Endpoint::Local, "/l")]
        );
        assert_eq!(
            app.transfer.jobs[0].outcome,
            Some(JobOutcome::Failed(Error::Canceled))
        );
        assert!(app.sync.running.is_empty());
        let status = app.core.status().unwrap();
        assert!(status.error);
        assert_eq!(status.text, "Failed sync 1 entry -> Local:/l: Canceled");
    }

    #[test]
    fn every_pane_on_the_destination_endpoint_is_refreshed() {
        let mut app = two_local();
        let job = start(&mut app, SyncDirection::LeftToRight);
        let effects = app.sync_finished(job, Ok(()));
        assert_eq!(
            list_dir_locations(&effects),
            [
                &location(Endpoint::Local, "/l"),
                &location(Endpoint::Local, "/r")
            ]
        );
    }

    #[test]
    fn sync_finished_for_an_unknown_job_is_ignored() {
        let mut app = two_local();
        let job = start(&mut app, SyncDirection::LeftToRight);
        assert!(app.sync_finished(JobId(job.0 + 100), Ok(())).is_empty());
        assert_eq!(app.transfer.jobs[0].outcome, None);
        assert_eq!(app.sync.running.len(), 1);
    }

    #[test]
    fn running_syncs_are_forgotten_once_their_job_has_an_outcome() {
        let mut app = two_local();
        let first = start(&mut app, SyncDirection::LeftToRight);
        let second = start(&mut app, SyncDirection::LeftToRight);
        app.sync_jobs_settled();
        assert_eq!(app.sync.running.len(), 2);
        // The transfer feature records the engine job's end.
        app.transfer.jobs[0].outcome = Some(JobOutcome::Finished(JobSummary::default()));
        app.sync_jobs_settled();
        let running: Vec<JobId> = app.sync.running.iter().map(|sync| sync.job).collect();
        assert_eq!(running, [second]);
        assert_ne!(first, second);
        // A job no longer in the queue keeps nothing running.
        app.transfer.jobs.clear();
        app.sync_jobs_settled();
        assert!(app.sync.running.is_empty());
    }

    #[test]
    fn page_start_keeps_the_cursor_on_the_page() {
        assert_eq!(page_start(0, 0, 0, 10), 0);
        assert_eq!(page_start(5, 0, 50, 10), 0);
        assert_eq!(page_start(10, 0, 50, 10), 1);
        assert_eq!(page_start(3, 8, 50, 10), 3);
        assert_eq!(page_start(49, 45, 50, 10), 40);
        // A page of 0 rows shows the cursor row alone.
        assert_eq!(page_start(7, 0, 50, 0), 7);
    }
}
