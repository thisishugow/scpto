//! Dialogs in the app: the modal stack (confirmations, text fields such as
//! rename, mkdir, and chmod, delete and trash-failed prompts, and the
//! prompts other features open), focus and hotkeys, the picker lists (the
//! bookmark list and the host picker's mechanics), and the file operations
//! those dialogs start.
//!
//! # Opening dialogs from other features
//!
//! Every feature opens a dialog by pushing a complete [`Dialog`] onto
//! [`DialogState::stack`] (the last entry is on top and has the input), and
//! a picker by setting [`DialogState::picker`]. Focus starts on the text
//! field when there is one, else on the first button. A dialog closes when
//! it is answered: Enter on a button or the field (the focused button, or
//! the first button while the field has focus), a click on a button, its
//! hotkey, or Esc (answered as `DialogButton::Cancel`). The dialog feature
//! handles the purposes it owns itself (see [`DialogPurpose`]) and puts
//! every other answer into [`DialogState::answer`]; core takes it after the
//! action and routes it to its owner, so features never call each other.
//!
//! # Behavior of the dialogs owned here
//!
//! - `MakeDir` (F7): empty field; Ok → `Effect::FsOp` `FsOp::MakeDir` at
//!   `parent/name`. `Rename` (F2): field prefilled with the cursor entry's
//!   name; Ok → `FsOp::Rename`. `Chmod` (`p`): field prefilled with the
//!   cursor entry's mode in octal; Ok with a valid octal mode (at most
//!   `0o7777`) → `FsOp::Chmod` on every target. An empty name, a name with
//!   `/`, or an invalid mode keeps the dialog open with a message.
//! - `Delete` (F8): "Delete N items?" with Yes / No; the message says the
//!   deletion is permanent on remote endpoints; local endpoints use
//!   `DeleteMethod::Trash`, remote ones `DeleteMethod::Permanent`.
//! - `FsOpFinished`: refreshes the panes showing that endpoint
//!   (`App::refresh`; a new folder or a renamed entry is reselected). A
//!   failed trash (`FsError::TrashFailed`) opens `TrashFailed` with
//!   DeletePermanently / Cancel → `FsOp::Delete` with
//!   `DeleteMethod::Permanent` for the failed paths; any other failures open
//!   a `Notice` listing them.
//!   - Results are matched to the request ids this feature emitted; results
//!     for other ids are dropped.
//!   - A pane with a navigation in flight is not refreshed (that listing is
//!     newer than the operation).
//!   - Reselecting needs a plain listing, not a refresh (a refresh keeps
//!     the cursor entry): after a successful `MakeDir` or `Rename`, the pane
//!     the dialog was opened from re-lists its directory with the new name
//!     to reselect when it still shows the parent, which also clears its
//!     selection and filter. Every other pane on the endpoint refreshes.
//!   - When trashing failed for some paths and other paths failed for other
//!     reasons, the `Notice` is pushed first and `TrashFailed` on top of it,
//!     so the permanent-delete question comes first.
//!   - The status bar summarizes the outcome (the error itself for a single
//!     failure).
//! - Names: a rename of an entry whose name is not valid UTF-8 is refused
//!   with a status message (the field could not hold it without changing
//!   it); renaming to the same name closes the dialog without an
//!   operation; `.` and `..` are refused like an empty name.
//! - Buttons: every `DialogButton` has one label and hotkey (matched
//!   ignoring case), unique within each documented set of buttons. A double
//!   click on a button answers nothing (its first click already did; the
//!   second must not answer the dialog below).
//! - A checkbox toggles with `ToggleOption` while focused and with a click:
//!   the dialog widget records every checkbox as the clickable
//!   `DialogButton::ApplyToAll`, whatever the purpose, so a
//!   `ClickTarget::DialogButton(ApplyToAll)` toggles the checkbox of any
//!   dialog (conflict "apply to all", sync "delete extraneous") and never
//!   answers it.
//! - Text fields edit with `Action::Text`; a field with
//!   [`TextField::max_len`] rejects input beyond it, so a masked password
//!   buffer created with that capacity never reallocates.
//! - Bookmark list (`b`): a [`PickerList`] of `PickerKind::Bookmarks`
//!   with one row per bookmark (label `host:/path` or `/path`); Enter jumps
//!   with `App::switch_endpoint`; `PickerRemove` removes the row's bookmark
//!   (`Effect::RemoveBookmark`, and from `App::persisted`).
//! - Picker mechanics, shared by the bookmark list and the host picker:
//!   `Text` edits [`PickerList::filter`] (a digit typed while the filter is
//!   empty selects tab 1-9 instead when the list has tabs), `MoveCursor`,
//!   `Scroll { region: ScrollRegion::Picker }`, `Click` / `DoubleClick` on
//!   `ClickTarget::PickerRow`, Enter picks, Esc closes. The rows shown are
//!   computed from `items`, `active_tab`, and `filter` whenever they are
//!   needed, so the connection feature may change `items` while the host
//!   picker is open (resolved hostnames) without telling this feature.
//!   Picking a `PickerKind::Hosts` row closes the picker and puts
//!   [`Answer::Picked`] into [`DialogState::answer`].
//!
//! # Public API (Wave 3, ui/app/dialog)
//!
//! ```text
//! impl App {
//!     /// Claims `MakeDir`, `Rename`, `Chmod`, `Delete`, `ShowBookmarks`,
//!     /// and every action while a dialog or picker is open
//!     /// (`InputMode::TextField`, `Dialog`, `Picker`), including `Paste`
//!     /// (inserted into the field or picker filter) and clicks on
//!     /// `ClickTarget::DialogButton` / `PickerRow`; hands everything else
//!     /// back unchanged.
//!     pub fn dialog_action(&mut self, action: Action) -> Result<Vec<Effect>, Action>;
//!     /// `BgEvent::FsOpFinished`.
//!     pub fn dialog_fs_op_finished(&mut self, request: RequestId, endpoint: Endpoint, op: FsOp, failures: Vec<FsError>) -> Vec<Effect>;
//!     /// `Some(InputMode::TextField)` when the top dialog's field has
//!     /// focus, `Some(InputMode::Dialog)` for any other open dialog,
//!     /// `Some(InputMode::Picker)` while only a picker is open, else `None`.
//!     pub fn dialog_input_mode(&self) -> Option<InputMode>;
//!
//!     /// View accessors for render; `buttons` and `rows` are per-frame
//!     /// scratch buffers owned by render.
//!     pub fn dialog_view<'a>(&'a self, buttons: &'a mut Vec<ButtonView<'a>>) -> Option<DialogView<'a>>;
//!     pub fn picker_view<'a>(&'a self, tabs: &'a mut Vec<&'a str>, rows: &'a mut Vec<PickerRow<'a>>) -> Option<PickerView<'a>>;
//! }
//! ```

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;
use zeroize::{Zeroize, Zeroizing};

use super::core::StatusMessage;
use super::App;
use crate::error::FsError;
use crate::sync::SyncDirection;
use crate::transfer::model::{ItemId, JobId};
use crate::ui::action::{
    Action, ClickTarget, CursorMove, DeleteMethod, DialogButton, Effect, FocusStep, FsOp,
    InputMode, PaneSide, RequestId, ScrollRegion, TextEdit,
};
use crate::ui::widgets::dialog::{ButtonView, CheckboxView, DialogView};
use crate::ui::widgets::picker::{self, PickerRow, PickerView};
use crate::ui::widgets::text_input::TextInputView;
use crate::vfs::{Endpoint, Location};

/// State owned by this feature; part of [`super::App`]. Not `Debug`: text
/// fields may hold a password.
#[derive(Default)]
pub struct DialogState {
    /// Open dialogs; the last one is on top and has the input.
    pub stack: Vec<Dialog>,
    /// The open picker (host picker or bookmark list), drawn below the
    /// dialogs.
    pub picker: Option<PickerList>,
    /// An answer for another feature, left by the last action; core takes
    /// it and routes it by purpose or picker kind.
    pub answer: Option<Answer>,
    /// File operations in flight, by request id, with the pane their dialog
    /// was opened from.
    in_flight: HashMap<RequestId, PaneSide>,
}

/// One modal dialog.
pub struct Dialog {
    /// What the dialog asks for; decides who handles the answer.
    pub purpose: DialogPurpose,
    /// Title in the top border.
    pub title: String,
    /// The message, wrapped by the widget.
    pub message: String,
    /// A text field, for names, modes, and the sudo password.
    pub field: Option<TextField>,
    /// A checkbox (conflict "apply to all", sync "delete extraneous").
    pub checkbox: Option<Checkbox>,
    /// Buttons in display order. Each `DialogButton` has one fixed label
    /// and hotkey, defined by the dialog feature; the title and message say
    /// what the buttons do.
    pub buttons: Vec<DialogButton>,
    /// The focused element.
    pub focus: DialogFocus,
}

/// Which element of a dialog has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogFocus {
    /// The text field.
    Field,
    /// The checkbox.
    Checkbox,
    /// The button at this index of [`Dialog::buttons`].
    Button(usize),
}

/// A single-line text field. The value lives in zeroized memory because the
/// same type holds the sudo password.
pub struct TextField {
    /// The content.
    pub value: Zeroizing<String>,
    /// Caret position in characters.
    pub caret: usize,
    /// Drawn masked (`text_input::MASK_LEN` stars).
    pub masked: bool,
    /// Most bytes the value may hold; input beyond it is rejected. The sudo
    /// password field uses `sudo::PASSWORD_MAX_LEN` and allocates that
    /// capacity up front.
    pub max_len: Option<usize>,
}

/// A dialog checkbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkbox {
    /// Its label.
    pub label: String,
    /// Whether it is ticked.
    pub checked: bool,
}

/// Why a dialog is open. The comment on each variant names the feature that
/// handles its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogPurpose {
    /// Dialog feature: create a directory in `parent`.
    MakeDir {
        /// The pane to refresh and reselect in.
        side: PaneSide,
        /// The directory the new one goes in.
        parent: Location,
    },
    /// Dialog feature: rename `from` within its directory.
    Rename {
        /// The pane to refresh and reselect in.
        side: PaneSide,
        /// The entry renamed.
        from: Location,
    },
    /// Dialog feature: change the mode of `paths`.
    Chmod {
        /// The pane to refresh.
        side: PaneSide,
        /// Where the paths are.
        endpoint: Endpoint,
        /// The targets.
        paths: Vec<PathBuf>,
    },
    /// Dialog feature: confirm a delete.
    Delete {
        /// The pane to refresh.
        side: PaneSide,
        /// Where the paths are.
        endpoint: Endpoint,
        /// The targets.
        paths: Vec<PathBuf>,
        /// Trash (local) or permanent (remote).
        method: DeleteMethod,
    },
    /// Dialog feature: trashing failed; offer a permanent delete.
    TrashFailed {
        /// The pane to refresh.
        side: PaneSide,
        /// Where the paths are.
        endpoint: Endpoint,
        /// The paths that could not be trashed.
        paths: Vec<PathBuf>,
    },
    /// Dialog feature: a message with an Ok button; the answer is dropped.
    Notice,
    /// Transfer feature: a per-item conflict.
    Conflict {
        /// The waiting item.
        item: ItemId,
        /// Its job.
        job: JobId,
        /// Whether `Resume` is offered.
        partial: bool,
    },
    /// Transfer feature: a tar job found existing destinations.
    TarConflict {
        /// The waiting job.
        job: JobId,
    },
    /// Transfer feature: upload paths dropped from Finder.
    FinderDrop {
        /// The dropped local paths.
        paths: Vec<PathBuf>,
        /// The directory they go to.
        destination: Location,
    },
    /// Core: quit although transfers are running.
    QuitWhileRunning,
    /// Connection feature: the masked sudo password for `endpoint`.
    SudoPassword {
        /// The sudo endpoint.
        endpoint: Endpoint,
    },
    /// Connection feature: run the VPN command of a matching rule.
    Vpn {
        /// The unreachable endpoint.
        endpoint: Endpoint,
        /// The rule's command.
        command: String,
    },
    /// Sync feature: confirm the sync of the selected comparison entries;
    /// the checkbox, when shown, is "delete N entries that exist only on
    /// the destination".
    ConfirmSync {
        /// The copy direction.
        direction: SyncDirection,
    },
    /// Edit feature: the remote file changed while it was edited.
    EditConflict {
        /// The remote file.
        location: Location,
        /// The temp file with the edits.
        temp: PathBuf,
    },
    /// Edit feature: the upload failed; the edits are kept at `temp`.
    EditUploadFailed {
        /// The remote file.
        location: Location,
        /// The temp file with the edits.
        temp: PathBuf,
    },
}

/// The answer of a dialog another feature opened.
pub struct DialogAnswer {
    /// The dialog's purpose.
    pub purpose: DialogPurpose,
    /// The button chosen; `DialogButton::Cancel` for Esc.
    pub button: DialogButton,
    /// The field's value, when the dialog had a field.
    pub field: Option<Zeroizing<String>>,
    /// The checkbox state; `false` without a checkbox.
    pub checked: bool,
}

/// Something for core to route to the feature that owns it.
pub enum Answer {
    /// A dialog with a purpose this feature does not handle was answered.
    Dialog(DialogAnswer),
    /// A row of a picker this feature does not handle was picked.
    Picked {
        /// The picker.
        kind: PickerKind,
        /// The picked row's target.
        target: PickerTarget,
    },
}

/// Which list a picker shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Connection feature: the host picker (including Local) for `side`.
    Hosts {
        /// The pane whose endpoint is switched.
        side: PaneSide,
    },
    /// Dialog feature: the bookmark list; a pick shows the bookmark in
    /// `side`.
    Bookmarks {
        /// The pane that jumps.
        side: PaneSide,
    },
}

/// What a picker row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerTarget {
    /// The local machine.
    Local,
    /// A host alias.
    Host(String),
    /// A bookmarked directory.
    Bookmark(Location),
}

/// One row of a picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    /// The main text (alias, `Local`, or bookmark location).
    pub label: String,
    /// Dimmed detail, for example `user@hostname` once resolved.
    pub detail: String,
    /// Index into [`PickerList::tabs`] of the one tab besides `All` (index
    /// 0) that shows this row; ignored when the list has no tabs.
    pub tab: usize,
    /// What picking the row means.
    pub target: PickerTarget,
}

/// A filterable list: the host picker or the bookmark list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerList {
    /// Which list this is; decides who handles a pick.
    pub kind: PickerKind,
    /// Title in the top border.
    pub title: String,
    /// Tab captions; empty for no tab row. When present, tab 0 is `All`
    /// (every row), then one tab per sshto group, then `Other`.
    pub tabs: Vec<String>,
    /// The selected tab.
    pub active_tab: usize,
    /// The filter text.
    pub filter: String,
    /// Every row in display order (before tab and filter).
    pub items: Vec<PickerItem>,
    /// Cursor position among the rows shown.
    pub cursor: usize,
    /// First row shown, among the rows shown.
    pub first_visible: usize,
}

/// Most failures a notice lists one per line; the rest are counted.
const NOTICE_MAX_FAILURES: usize = 10;

/// Rows the picker popup keeps free above and below it
/// (`widgets::picker`'s margin, on both sides).
const PICKER_VERTICAL_MARGIN: u16 = 4;

/// The picker's rows besides the list: its border and the filter line (the
/// tab row is counted separately).
const PICKER_CHROME_ROWS: u16 = 3;

/// The fixed caption and hotkey of a button.
fn button_caption(button: DialogButton) -> (&'static str, char) {
    match button {
        DialogButton::Ok => ("OK", 'o'),
        DialogButton::Cancel => ("Cancel", 'c'),
        DialogButton::Yes => ("Yes", 'y'),
        DialogButton::No => ("No", 'n'),
        DialogButton::Overwrite => ("Overwrite", 'o'),
        DialogButton::Skip => ("Skip", 's'),
        DialogButton::Rename => ("Rename", 'r'),
        DialogButton::OverwriteIfNewer => ("Overwrite if newer", 'n'),
        DialogButton::Resume => ("Resume", 'e'),
        DialogButton::ApplyToAll => ("Apply to all", 'a'),
        DialogButton::OverwriteAll => ("Overwrite all", 'o'),
        DialogButton::Run => ("Run", 'r'),
        DialogButton::DeletePermanently => ("Delete permanently", 'd'),
        DialogButton::Upload => ("Upload", 'u'),
        DialogButton::Retry => ("Retry", 't'),
    }
}

impl TextField {
    /// An unmasked, unlimited field holding `value`, caret at the end.
    fn with_value(value: &str) -> TextField {
        TextField {
            value: Zeroizing::new(value.to_owned()),
            caret: value.chars().count(),
            masked: false,
            max_len: None,
        }
    }

    /// Byte offset of the character position `caret` (the end when past it).
    fn byte_at(&self, caret: usize) -> usize {
        self.value
            .char_indices()
            .nth(caret)
            .map_or(self.value.len(), |(index, _)| index)
    }

    fn char_count(&self) -> usize {
        self.value.chars().count()
    }

    /// Whether `extra` more bytes fit under `max_len`.
    fn fits(&self, extra: usize) -> bool {
        self.max_len
            .is_none_or(|max| self.value.len().saturating_add(extra) <= max)
    }

    /// Applies one edit at the caret. Control characters are never inserted
    /// and input beyond `max_len` is rejected. Removing text shifts the
    /// buffer in place, so its capacity never changes.
    fn edit(&mut self, edit: TextEdit) {
        let caret = self.caret.min(self.char_count());
        match edit {
            TextEdit::Insert(c) => {
                if !c.is_control() && self.fits(c.len_utf8()) {
                    let at = self.byte_at(caret);
                    self.value.insert(at, c);
                    self.caret = caret + 1;
                }
            }
            TextEdit::Backspace => {
                if caret > 0 {
                    let at = self.byte_at(caret - 1);
                    self.value.remove(at);
                    self.caret = caret - 1;
                }
            }
            TextEdit::DeleteForward => {
                if caret < self.char_count() {
                    let at = self.byte_at(caret);
                    self.value.remove(at);
                }
            }
            TextEdit::DeleteWord => {
                let before: Vec<char> = self.value.chars().take(caret).collect();
                let mut start = caret;
                while start > 0 && before[start - 1].is_whitespace() {
                    start -= 1;
                }
                while start > 0 && !before[start - 1].is_whitespace() {
                    start -= 1;
                }
                let range = self.byte_at(start)..self.byte_at(caret);
                self.value.replace_range(range, "");
                self.caret = start;
            }
            TextEdit::Clear => {
                self.value.zeroize();
                self.caret = 0;
            }
            TextEdit::Left => self.caret = caret.saturating_sub(1),
            TextEdit::Right => self.caret = (caret + 1).min(self.char_count()),
            TextEdit::Home => self.caret = 0,
            TextEdit::End => self.caret = self.char_count(),
        }
    }

    /// Inserts pasted text at the caret without its control characters (a
    /// trailing newline, tabs). Text that does not fit under `max_len` is
    /// rejected as a whole.
    fn paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() || !self.fits(clean.len()) {
            return;
        }
        let caret = self.caret.min(self.char_count());
        let at = self.byte_at(caret);
        self.value.insert_str(at, &clean);
        self.caret = caret + clean.chars().count();
    }
}

impl Dialog {
    /// The focusable elements in Tab order.
    fn focus_order(&self) -> Vec<DialogFocus> {
        let field = self.field.as_ref().map(|_| DialogFocus::Field);
        let checkbox = self.checkbox.as_ref().map(|_| DialogFocus::Checkbox);
        field
            .into_iter()
            .chain(checkbox)
            .chain((0..self.buttons.len()).map(DialogFocus::Button))
            .collect()
    }

    /// Moves focus to the next or previous element, wrapping around.
    fn step_focus(&mut self, step: FocusStep) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
        let next = match order.iter().position(|&focus| focus == self.focus) {
            Some(index) => match step {
                FocusStep::Next => (index + 1) % order.len(),
                FocusStep::Previous => (index + order.len() - 1) % order.len(),
            },
            None => 0,
        };
        self.focus = order[next];
    }

    /// The button Enter presses: the focused one, else the first.
    fn enter_button(&self) -> Option<DialogButton> {
        match self.focus {
            DialogFocus::Button(index) => self.buttons.get(index).copied(),
            DialogFocus::Field | DialogFocus::Checkbox => self.buttons.first().copied(),
        }
    }

    /// The button whose hotkey is `key`, ignoring case.
    fn hotkey_button(&self, key: char) -> Option<DialogButton> {
        let key: String = key.to_lowercase().collect();
        self.buttons.iter().copied().find(|&button| {
            let hotkey = button_caption(button).1;
            hotkey.to_lowercase().eq(key.chars())
        })
    }

    fn toggle_checkbox(&mut self) {
        if let Some(checkbox) = &mut self.checkbox {
            checkbox.checked = !checkbox.checked;
        }
    }
}

impl DialogPurpose {
    /// Whether the dialog feature handles this purpose's answer itself.
    fn is_owned(&self) -> bool {
        match self {
            DialogPurpose::MakeDir { .. }
            | DialogPurpose::Rename { .. }
            | DialogPurpose::Chmod { .. }
            | DialogPurpose::Delete { .. }
            | DialogPurpose::TrashFailed { .. }
            | DialogPurpose::Notice => true,
            DialogPurpose::Conflict { .. }
            | DialogPurpose::TarConflict { .. }
            | DialogPurpose::FinderDrop { .. }
            | DialogPurpose::QuitWhileRunning
            | DialogPurpose::SudoPassword { .. }
            | DialogPurpose::Vpn { .. }
            | DialogPurpose::ConfirmSync { .. }
            | DialogPurpose::EditConflict { .. }
            | DialogPurpose::EditUploadFailed { .. } => false,
        }
    }
}

/// A file operation an answered dialog starts.
struct PlannedOp {
    side: PaneSide,
    endpoint: Endpoint,
    op: FsOp,
}

/// What answering one of this feature's dialogs with `button` does:
/// `Ok(None)` closes it without an operation, `Err` keeps it open with that
/// problem.
fn plan(dialog: &Dialog, button: DialogButton) -> Result<Option<PlannedOp>, String> {
    let field = dialog
        .field
        .as_ref()
        .map_or("", |field| field.value.as_str());
    let planned = match (&dialog.purpose, button) {
        (DialogPurpose::MakeDir { side, parent }, DialogButton::Ok) => PlannedOp {
            side: *side,
            endpoint: parent.endpoint.clone(),
            op: FsOp::MakeDir {
                path: parent.path.join(check_name(field)?),
            },
        },
        (DialogPurpose::Rename { side, from }, DialogButton::Ok) => {
            let name = check_name(field)?;
            let Some(parent) = from.path.parent() else {
                return Ok(None);
            };
            if from.path.file_name() == Some(OsStr::new(name)) {
                return Ok(None);
            }
            PlannedOp {
                side: *side,
                endpoint: from.endpoint.clone(),
                op: FsOp::Rename {
                    from: from.path.clone(),
                    to: parent.join(name),
                },
            }
        }
        (
            DialogPurpose::Chmod {
                side,
                endpoint,
                paths,
            },
            DialogButton::Ok,
        ) => PlannedOp {
            side: *side,
            endpoint: endpoint.clone(),
            op: FsOp::Chmod {
                paths: paths.clone(),
                mode: parse_mode(field)?,
            },
        },
        (
            DialogPurpose::Delete {
                side,
                endpoint,
                paths,
                method,
            },
            DialogButton::Yes,
        ) => PlannedOp {
            side: *side,
            endpoint: endpoint.clone(),
            op: FsOp::Delete {
                paths: paths.clone(),
                method: *method,
            },
        },
        (
            DialogPurpose::TrashFailed {
                side,
                endpoint,
                paths,
            },
            DialogButton::DeletePermanently,
        ) => PlannedOp {
            side: *side,
            endpoint: endpoint.clone(),
            op: FsOp::Delete {
                paths: paths.clone(),
                method: DeleteMethod::Permanent,
            },
        },
        _ => return Ok(None),
    };
    Ok(Some(planned))
}

/// A file name typed into a dialog, or why it is not one.
fn check_name(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        Err("Enter a name.".to_owned())
    } else if name.contains('/') {
        Err("A name cannot contain \"/\".".to_owned())
    } else if name == "." || name == ".." {
        Err(format!("\"{name}\" is not a valid name."))
    } else {
        Ok(name)
    }
}

/// An octal permission mode of at most `0o7777`.
fn parse_mode(text: &str) -> Result<u32, String> {
    let problem = || "Enter an octal mode from 0 to 7777, for example 644.".to_owned();
    if text.is_empty() || !text.chars().all(|c| matches!(c, '0'..='7')) {
        return Err(problem());
    }
    match u32::from_str_radix(text, 8) {
        Ok(mode) if mode <= 0o7777 => Ok(mode),
        _ => Err(problem()),
    }
}

/// The question a field dialog of this feature asks; problems with the
/// input are shown below it.
fn field_prompt(purpose: &DialogPurpose) -> String {
    match purpose {
        DialogPurpose::MakeDir { parent, .. } => {
            format!("New directory in {}:", describe(parent))
        }
        DialogPurpose::Rename { from, .. } => {
            format!("Rename \"{}\" to:", file_name(&from.path))
        }
        DialogPurpose::Chmod { paths, .. } => {
            format!("Permissions of {} (octal):", subject(paths))
        }
        _ => String::new(),
    }
}

/// `"name"` for one path, else `N items`.
fn subject(paths: &[PathBuf]) -> String {
    match paths {
        [path] => format!("\"{}\"", file_name(path)),
        paths => items(paths.len()),
    }
}

fn items(count: usize) -> String {
    match count {
        1 => "1 item".to_owned(),
        count => format!("{count} items"),
    }
}

/// The last component of a path for messages (the path itself at `/`).
fn file_name(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.display().to_string(),
    }
}

/// A location as messages and bookmark rows show it: `/path` locally,
/// `host:/path` remotely (a bookmark row shows sudo as its detail).
fn describe(location: &Location) -> String {
    let path = location.path.display();
    match &location.endpoint {
        Endpoint::Local => path.to_string(),
        Endpoint::Remote { host, .. } => format!("{host}:{path}"),
    }
}

/// Case-insensitive, NFC-normalized text for filtering.
fn fold(text: &str) -> String {
    text.nfc().collect::<String>().to_lowercase()
}

/// Whether every character of `needle` occurs in `haystack` in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle.chars().all(|wanted| rest.any(|c| c == wanted))
}

/// `cursor` and `first_visible` kept inside a list of `len` rows shown
/// `page` at a time, with the cursor on screen.
fn clamp_view(cursor: usize, first_visible: usize, len: usize, page: usize) -> (usize, usize) {
    let page = page.max(1);
    let cursor = cursor.min(len.saturating_sub(1));
    let mut first = first_visible.min(len.saturating_sub(page)).min(cursor);
    if cursor >= first + page {
        first = cursor + 1 - page;
    }
    (cursor, first)
}

impl PickerList {
    /// Indices into `items` of the rows shown for the active tab and the
    /// filter, in display order.
    fn shown(&self) -> Vec<usize> {
        let filter = fold(&self.filter);
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                self.tabs.is_empty() || self.active_tab == 0 || item.tab == self.active_tab
            })
            .filter(|(_, item)| {
                filter.is_empty()
                    || is_subsequence(&filter, &fold(&item.label))
                    || is_subsequence(&filter, &fold(&item.detail))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Re-applies the cursor invariants to the rows shown now.
    fn clamp(&mut self, page: usize) {
        let len = self.shown().len();
        (self.cursor, self.first_visible) = clamp_view(self.cursor, self.first_visible, len, page);
    }

    /// The filter or tab changed: back to the first row.
    fn restart(&mut self) {
        self.cursor = 0;
        self.first_visible = 0;
    }

    /// Edits the filter at its end; a digit 1-9 typed while it is empty
    /// selects that tab instead when the list has tabs.
    fn edit_filter(&mut self, edit: TextEdit) {
        match edit {
            TextEdit::Insert(c) => {
                let tab = c
                    .to_digit(10)
                    .filter(|_| self.filter.is_empty() && !self.tabs.is_empty())
                    .and_then(|digit| usize::try_from(digit).ok())
                    .and_then(|digit| digit.checked_sub(1));
                match tab {
                    Some(tab) => {
                        if tab < self.tabs.len() {
                            self.active_tab = tab;
                            self.restart();
                        }
                    }
                    None if c.is_control() => {}
                    None => {
                        self.filter.push(c);
                        self.restart();
                    }
                }
            }
            TextEdit::Backspace => {
                if self.filter.pop().is_some() {
                    self.restart();
                }
            }
            TextEdit::DeleteWord => {
                let kept = self.filter.trim_end().len();
                let start = self.filter[..kept]
                    .rfind(char::is_whitespace)
                    .map_or(0, |index| index + 1);
                self.filter.truncate(start);
                self.restart();
            }
            TextEdit::Clear => {
                self.filter.clear();
                self.restart();
            }
            // The filter is edited at its end only.
            TextEdit::DeleteForward
            | TextEdit::Left
            | TextEdit::Right
            | TextEdit::Home
            | TextEdit::End => {}
        }
    }

    fn move_cursor(&mut self, movement: CursorMove, page: usize) {
        let page = page.max(1);
        let last = self.shown().len().saturating_sub(1);
        self.cursor = match movement {
            CursorMove::Up => self.cursor.saturating_sub(1),
            CursorMove::Down => self.cursor + 1,
            CursorMove::PageUp => self.cursor.saturating_sub(page),
            CursorMove::PageDown => self.cursor + page,
            CursorMove::Home => 0,
            CursorMove::End => last,
        }
        .min(last);
        self.clamp(page);
    }

    /// Mouse wheel: moves the first row by `lines` (negative is up),
    /// dragging the cursor along only when it would leave the page.
    fn scroll_by(&mut self, lines: i32, page: usize) {
        let page = page.max(1);
        let len = self.shown().len();
        let delta = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
        let first = if lines < 0 {
            self.first_visible.saturating_sub(delta)
        } else {
            self.first_visible
                .saturating_add(delta)
                .min(len.saturating_sub(page))
        };
        let cursor = self.cursor.clamp(first, first + page - 1);
        self.first_visible = first;
        self.cursor = cursor;
        self.clamp(page);
    }
}

impl App {
    /// Claims `MakeDir`, `Rename`, `Chmod`, `Delete`, `ShowBookmarks`, and
    /// every action while a dialog or picker is open (`InputMode::TextField`,
    /// `Dialog`, `Picker`), including `Paste` (inserted into the field or
    /// picker filter) and clicks on `ClickTarget::DialogButton` /
    /// `PickerRow`; hands everything else back unchanged.
    pub fn dialog_action(&mut self, action: Action) -> Result<Vec<Effect>, Action> {
        if !self.dialog.stack.is_empty() {
            return Ok(self.top_dialog_action(action));
        }
        if self.dialog.picker.is_some() {
            return Ok(self.picker_action(action));
        }
        match action {
            Action::MakeDir => self.open_make_dir(),
            Action::Rename => self.open_rename(),
            Action::Chmod => self.open_chmod(),
            Action::Delete => self.open_delete(),
            Action::ShowBookmarks => self.open_bookmarks(),
            other => return Err(other),
        }
        Ok(Vec::new())
    }

    /// `BgEvent::FsOpFinished`.
    pub fn dialog_fs_op_finished(
        &mut self,
        request: RequestId,
        endpoint: Endpoint,
        op: FsOp,
        failures: Vec<FsError>,
    ) -> Vec<Effect> {
        let Some(side) = self.dialog.in_flight.remove(&request) else {
            return Vec::new();
        };
        let created = match &op {
            FsOp::MakeDir { path } if failures.is_empty() => Some(path.as_path()),
            FsOp::Rename { to, .. } if failures.is_empty() => Some(to.as_path()),
            _ => None,
        };
        let effects = self.refresh_after_op(side, &endpoint, created);
        self.report_op(side, endpoint, &op, failures);
        effects
    }

    /// `Some(InputMode::TextField)` when the top dialog's field has focus,
    /// `Some(InputMode::Dialog)` for any other open dialog,
    /// `Some(InputMode::Picker)` while only a picker is open, else `None`.
    pub fn dialog_input_mode(&self) -> Option<InputMode> {
        match self.dialog.stack.last() {
            Some(dialog) if dialog.focus == DialogFocus::Field && dialog.field.is_some() => {
                Some(InputMode::TextField)
            }
            Some(_) => Some(InputMode::Dialog),
            None => self.dialog.picker.as_ref().map(|_| InputMode::Picker),
        }
    }

    /// View of the top dialog for render; `buttons` is a per-frame scratch
    /// buffer owned by render.
    pub fn dialog_view<'a>(
        &'a self,
        buttons: &'a mut Vec<ButtonView<'a>>,
    ) -> Option<DialogView<'a>> {
        let dialog = self.dialog.stack.last()?;
        buttons.clear();
        buttons.extend(dialog.buttons.iter().enumerate().map(|(index, &button)| {
            let (label, hotkey) = button_caption(button);
            ButtonView {
                button,
                label,
                hotkey: Some(hotkey),
                focused: dialog.focus == DialogFocus::Button(index),
            }
        }));
        let buttons: &'a [ButtonView<'a>] = buttons;
        Some(DialogView {
            title: &dialog.title,
            message: &dialog.message,
            field: dialog.field.as_ref().map(|field| TextInputView {
                value: &field.value,
                caret: field.caret.min(field.char_count()),
                masked: field.masked,
                focused: dialog.focus == DialogFocus::Field,
            }),
            checkbox: dialog.checkbox.as_ref().map(|checkbox| CheckboxView {
                label: &checkbox.label,
                checked: checkbox.checked,
                focused: dialog.focus == DialogFocus::Checkbox,
            }),
            buttons,
        })
    }

    /// View of the open picker for render; `tabs` and `rows` are per-frame
    /// scratch buffers owned by render.
    pub fn picker_view<'a>(
        &'a self,
        tabs: &'a mut Vec<&'a str>,
        rows: &'a mut Vec<PickerRow<'a>>,
    ) -> Option<PickerView<'a>> {
        let list = self.dialog.picker.as_ref()?;
        let shown = list.shown();
        let page = self.picker_page(list);
        let (cursor, first_visible) =
            clamp_view(list.cursor, list.first_visible, shown.len(), page);
        tabs.clear();
        tabs.extend(list.tabs.iter().map(String::as_str));
        rows.clear();
        rows.extend(shown.iter().map(|&index| PickerRow {
            label: &list.items[index].label,
            detail: &list.items[index].detail,
        }));
        Some(PickerView {
            title: &list.title,
            filter: &list.filter,
            tabs,
            active_tab: list.active_tab,
            rows,
            cursor,
            first_visible,
        })
    }

    // Opening this feature's dialogs.

    fn push_field_dialog(&mut self, purpose: DialogPurpose, title: &str, value: &str) {
        self.dialog.stack.push(Dialog {
            message: field_prompt(&purpose),
            purpose,
            title: title.to_owned(),
            field: Some(TextField::with_value(value)),
            checkbox: None,
            buttons: vec![DialogButton::Ok, DialogButton::Cancel],
            focus: DialogFocus::Field,
        });
    }

    /// Pushes a buttons-only dialog of this feature (focus on the first
    /// button).
    fn push_question(
        &mut self,
        purpose: DialogPurpose,
        title: String,
        message: String,
        buttons: Vec<DialogButton>,
    ) {
        self.dialog.stack.push(Dialog {
            purpose,
            title,
            message,
            field: None,
            checkbox: None,
            buttons,
            focus: DialogFocus::Button(0),
        });
    }

    fn open_make_dir(&mut self) {
        let side = self.core.focused();
        if let Some(parent) = self.core.pane(side).location() {
            let purpose = DialogPurpose::MakeDir { side, parent };
            self.push_field_dialog(purpose, "Create directory", "");
        }
    }

    fn open_rename(&mut self) {
        let side = self.core.focused();
        let pane = self.core.pane(side);
        let (Some(entry), Some(path)) = (pane.cursor_entry(), pane.path_of(pane.cursor)) else {
            return;
        };
        let Some(name) = entry.name.to_str().map(str::to_owned) else {
            let shown = entry.name.to_string_lossy().into_owned();
            self.core.set_status(StatusMessage::error(format!(
                "Cannot rename \"{shown}\": its name is not valid UTF-8"
            )));
            return;
        };
        let from = Location {
            endpoint: pane.endpoint.clone(),
            path,
        };
        self.push_field_dialog(DialogPurpose::Rename { side, from }, "Rename", &name);
    }

    fn open_chmod(&mut self) {
        let side = self.core.focused();
        let pane = self.core.pane(side);
        let paths = pane.targets();
        if paths.is_empty() {
            return;
        }
        let mode = pane
            .cursor_entry()
            .and_then(|entry| entry.mode)
            .map(|mode| format!("{mode:o}"))
            .unwrap_or_default();
        let purpose = DialogPurpose::Chmod {
            side,
            endpoint: pane.endpoint.clone(),
            paths,
        };
        self.push_field_dialog(purpose, "Permissions", &mode);
    }

    fn open_delete(&mut self) {
        let side = self.core.focused();
        let pane = self.core.pane(side);
        let paths = pane.targets();
        if paths.is_empty() {
            return;
        }
        let endpoint = pane.endpoint.clone();
        let (method, consequence) = match &endpoint {
            Endpoint::Local => {
                let goes = if paths.len() == 1 {
                    "It goes"
                } else {
                    "They go"
                };
                (DeleteMethod::Trash, format!("{goes} to the trash."))
            }
            Endpoint::Remote { .. } => (
                DeleteMethod::Permanent,
                format!("Deleting on {endpoint} is permanent and cannot be undone."),
            ),
        };
        let message = format!("Delete {}?\n\n{consequence}", subject(&paths));
        let purpose = DialogPurpose::Delete {
            side,
            endpoint,
            paths,
            method,
        };
        let buttons = vec![DialogButton::Yes, DialogButton::No];
        self.push_question(purpose, "Delete".to_owned(), message, buttons);
    }

    fn open_bookmarks(&mut self) {
        if self.persisted.bookmarks.is_empty() {
            self.core.set_status(StatusMessage::info(
                "No bookmarks yet; B bookmarks the current directory",
            ));
            return;
        }
        let items = self
            .persisted
            .bookmarks
            .iter()
            .map(|location| PickerItem {
                label: describe(location),
                detail: match location.endpoint {
                    Endpoint::Remote { sudo: true, .. } => "sudo".to_owned(),
                    Endpoint::Remote { sudo: false, .. } | Endpoint::Local => String::new(),
                },
                tab: 0,
                target: PickerTarget::Bookmark(location.clone()),
            })
            .collect();
        self.dialog.picker = Some(PickerList {
            kind: PickerKind::Bookmarks {
                side: self.core.focused(),
            },
            title: "Bookmarks".to_owned(),
            tabs: Vec::new(),
            active_tab: 0,
            filter: String::new(),
            items,
            cursor: 0,
            first_visible: 0,
        });
    }

    // Input to the top dialog.

    fn top_dialog_action(&mut self, action: Action) -> Vec<Effect> {
        let Some(dialog) = self.dialog.stack.last_mut() else {
            return Vec::new();
        };
        let field_focused = dialog.focus == DialogFocus::Field;
        match action {
            Action::Text(edit) => {
                if let (true, Some(field)) = (field_focused, &mut dialog.field) {
                    field.edit(edit);
                }
            }
            Action::Paste(text) => {
                if let (true, Some(field)) = (field_focused, &mut dialog.field) {
                    field.paste(&text);
                }
            }
            Action::Confirm => {
                if let Some(button) = dialog.enter_button() {
                    return self.answer_top(button);
                }
            }
            Action::Cancel => return self.answer_top(DialogButton::Cancel),
            Action::DialogFocus(step) => dialog.step_focus(step),
            Action::ToggleOption => {
                if dialog.focus == DialogFocus::Checkbox {
                    dialog.toggle_checkbox();
                }
            }
            Action::DialogHotkey(key) => {
                if let Some(button) = dialog.hotkey_button(key) {
                    return self.answer_top(button);
                }
            }
            Action::Click {
                target: ClickTarget::DialogButton(DialogButton::ApplyToAll),
                ..
            }
            | Action::DoubleClick {
                target: ClickTarget::DialogButton(DialogButton::ApplyToAll),
                ..
            } => {
                if dialog.checkbox.is_some() {
                    dialog.toggle_checkbox();
                    dialog.focus = DialogFocus::Checkbox;
                }
            }
            Action::Click {
                target: ClickTarget::DialogButton(button),
                ..
            } if dialog.buttons.contains(&button) => return self.answer_top(button),
            // Every other action means nothing while a dialog is open.
            _ => {}
        }
        Vec::new()
    }

    /// Answers the top dialog with `button`: handles this feature's purposes
    /// (an invalid field keeps the dialog open with the problem) and leaves
    /// every other answer for core to route.
    fn answer_top(&mut self, button: DialogButton) -> Vec<Effect> {
        let Some(top) = self.dialog.stack.last_mut() else {
            return Vec::new();
        };
        if !top.purpose.is_owned() {
            if let Some(dialog) = self.dialog.stack.pop() {
                self.dialog.answer = Some(Answer::Dialog(DialogAnswer {
                    purpose: dialog.purpose,
                    button,
                    field: dialog.field.map(|field| field.value),
                    checked: dialog.checkbox.is_some_and(|checkbox| checkbox.checked),
                }));
            }
            return Vec::new();
        }
        match plan(top, button) {
            Err(problem) => {
                top.message = format!("{}\n\n{problem}", field_prompt(&top.purpose));
                Vec::new()
            }
            Ok(planned) => {
                self.dialog.stack.pop();
                let Some(PlannedOp { side, endpoint, op }) = planned else {
                    return Vec::new();
                };
                let request = self.core.next_request();
                self.dialog.in_flight.insert(request, side);
                vec![Effect::FsOp {
                    request,
                    endpoint,
                    op,
                }]
            }
        }
    }

    // File operation results.

    /// Refreshes every pane on `endpoint` except one that is navigating
    /// (its listing in flight is newer than the operation). The pane the
    /// operation came from re-lists with `created` reselected while it
    /// shows the parent of `created`.
    fn refresh_after_op(
        &mut self,
        origin: PaneSide,
        endpoint: &Endpoint,
        created: Option<&Path>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        for side in [PaneSide::Left, PaneSide::Right] {
            let pane = self.core.pane(side);
            let navigating = pane
                .pending
                .as_ref()
                .is_some_and(|pending| !pending.refresh);
            if pane.endpoint != *endpoint || navigating {
                continue;
            }
            let reselect = created.filter(|_| side == origin).and_then(|path| {
                let cwd = pane.cwd.as_deref()?;
                let name = path.file_name()?;
                (path.parent() == Some(cwd)).then(|| (cwd.to_path_buf(), name.to_os_string()))
            });
            match reselect {
                Some((cwd, name)) => {
                    let request = self.core.next_request();
                    let location = self
                        .core
                        .pane_mut(side)
                        .begin_list(request, cwd, Some(name));
                    effects.push(Effect::ListDir { request, location });
                }
                None => effects.extend(self.refresh(side)),
            }
        }
        effects
    }

    /// Status message for the outcome; a `Notice` for failures and, on top,
    /// a `TrashFailed` question for the paths the trash refused.
    fn report_op(&mut self, side: PaneSide, endpoint: Endpoint, op: &FsOp, failures: Vec<FsError>) {
        let (title, done) = match op {
            FsOp::MakeDir { path } => ("Create directory", format!("Created {}", file_name(path))),
            FsOp::Rename { from, to } => (
                "Rename",
                format!("Renamed {} to {}", file_name(from), file_name(to)),
            ),
            FsOp::Delete {
                paths,
                method: DeleteMethod::Trash,
            } => ("Delete", format!("Moved {} to the trash", subject(paths))),
            FsOp::Delete {
                paths,
                method: DeleteMethod::Permanent,
            } => ("Delete", format!("Deleted {}", subject(paths))),
            FsOp::Chmod { paths, mode } => (
                "Permissions",
                format!("Set the permissions of {} to {mode:o}", subject(paths)),
            ),
        };
        self.core.set_status(match failures.as_slice() {
            [] => StatusMessage::info(done),
            [failure] => StatusMessage::error(failure.to_string()),
            failures => {
                StatusMessage::error(format!("{title} failed for {}", items(failures.len())))
            }
        });

        let trashing = matches!(
            op,
            FsOp::Delete {
                method: DeleteMethod::Trash,
                ..
            }
        );
        let mut untrashed = Vec::new();
        let mut reason = None;
        let mut others = Vec::new();
        for failure in failures {
            match failure {
                FsError::TrashFailed { path, message } if trashing => {
                    reason.get_or_insert(message);
                    untrashed.push(path);
                }
                other => others.push(other),
            }
        }
        if !others.is_empty() {
            let mut lines: Vec<String> = others
                .iter()
                .take(NOTICE_MAX_FAILURES)
                .map(ToString::to_string)
                .collect();
            if others.len() > NOTICE_MAX_FAILURES {
                lines.push(format!("and {} more", others.len() - NOTICE_MAX_FAILURES));
            }
            self.push_question(
                DialogPurpose::Notice,
                format!("{title} failed"),
                lines.join("\n"),
                vec![DialogButton::Ok],
            );
        }
        if let Some(reason) = reason {
            let message = format!(
                "{} could not be moved to the trash: {reason}\n\n\
                 Delete permanently instead? This cannot be undone.",
                subject(&untrashed)
            );
            let purpose = DialogPurpose::TrashFailed {
                side,
                endpoint,
                paths: untrashed,
            };
            let buttons = vec![DialogButton::DeletePermanently, DialogButton::Cancel];
            self.push_question(purpose, "Trash failed".to_owned(), message, buttons);
        }
    }

    // Pickers.

    /// Rows the picker popup shows on the current screen (at least 1); the
    /// same arithmetic as `widgets::picker::render`.
    fn picker_page(&self, list: &PickerList) -> usize {
        let screen_rows = self.core.layout().status_bar.bottom();
        let tab_rows = u16::from(!list.tabs.is_empty());
        let rows = screen_rows
            .saturating_sub(PICKER_VERTICAL_MARGIN)
            .saturating_sub(PICKER_CHROME_ROWS + tab_rows)
            .min(picker::MAX_LIST_ROWS);
        usize::from(rows).max(1)
    }

    fn picker_action(&mut self, action: Action) -> Vec<Effect> {
        let Some(list) = self.dialog.picker.as_ref() else {
            return Vec::new();
        };
        let page = self.picker_page(list);
        let Some(list) = self.dialog.picker.as_mut() else {
            return Vec::new();
        };
        match action {
            Action::Text(edit) => list.edit_filter(edit),
            Action::Paste(text) => {
                let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                if !clean.is_empty() {
                    list.filter.push_str(&clean);
                    list.restart();
                }
            }
            Action::MoveCursor(movement) => list.move_cursor(movement, page),
            Action::Scroll {
                region: ScrollRegion::Picker,
                lines,
            } => list.scroll_by(lines, page),
            Action::Click {
                target: ClickTarget::PickerRow(index),
                ..
            } => {
                list.cursor = index;
                list.clamp(page);
            }
            Action::DoubleClick {
                target: ClickTarget::PickerRow(index),
                ..
            } => {
                if index < list.shown().len() {
                    list.cursor = index;
                    return self.pick();
                }
            }
            Action::Confirm => return self.pick(),
            Action::Cancel => self.dialog.picker = None,
            Action::PickerRemove => return self.remove_bookmark_row(page),
            // Every other action means nothing while a picker is open.
            _ => {}
        }
        Vec::new()
    }

    /// Enter on the cursor row: closes the picker; a bookmark jumps there,
    /// any other row is answered for core to route.
    fn pick(&mut self) -> Vec<Effect> {
        let Some(list) = self.dialog.picker.as_ref() else {
            return Vec::new();
        };
        let shown = list.shown();
        let Some(&index) = shown.get(list.cursor.min(shown.len().saturating_sub(1))) else {
            return Vec::new();
        };
        let target = list.items[index].target.clone();
        let kind = list.kind;
        self.dialog.picker = None;
        match (kind, target) {
            (PickerKind::Bookmarks { side }, PickerTarget::Bookmark(location)) => {
                self.switch_endpoint(side, location.endpoint, Some(location.path))
            }
            (kind, target) => {
                self.dialog.answer = Some(Answer::Picked { kind, target });
                Vec::new()
            }
        }
    }

    /// Delete in the bookmark list: forgets the cursor row's bookmark and
    /// keeps the list open.
    fn remove_bookmark_row(&mut self, page: usize) -> Vec<Effect> {
        let Some(list) = self.dialog.picker.as_mut() else {
            return Vec::new();
        };
        if !matches!(list.kind, PickerKind::Bookmarks { .. }) {
            return Vec::new();
        }
        let shown = list.shown();
        let Some(&index) = shown.get(list.cursor) else {
            return Vec::new();
        };
        let PickerTarget::Bookmark(location) = list.items[index].target.clone() else {
            return Vec::new();
        };
        list.items.remove(index);
        list.clamp(page);
        self.persisted.remove_bookmark(&location);
        self.core.set_status(StatusMessage::info(format!(
            "Removed bookmark {}",
            describe(&location)
        )));
        vec![Effect::RemoveBookmark(location)]
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;
    use std::time::Instant;

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;
    use crate::groups::Groups;
    use crate::hosts::HostList;
    use crate::preview::Background;
    use crate::settings::Settings;
    use crate::state::PersistedState;
    use crate::sudo::PASSWORD_MAX_LEN;
    use crate::ui::action::BgEvent;
    use crate::vfs::{Entry, EntryKind};

    fn web() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        }
    }

    fn file(name: &str, mode: Option<u32>) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(1),
            mtime: Some(1),
            mode,
        }
    }

    fn dir(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::Dir,
            size: Some(1),
            mtime: Some(1),
            mode: Some(0o755),
        }
    }

    fn local(path: &str) -> Location {
        Location {
            endpoint: Endpoint::Local,
            path: path.into(),
        }
    }

    /// Answers every connect with home `/home` and every listing from
    /// `listing`, until no effect is left.
    fn settle(app: &mut App, effects: Vec<Effect>, listing: &dyn Fn(&Location) -> Vec<Entry>) {
        let mut queue = effects;
        while let Some(effect) = queue.pop() {
            let more = match effect {
                Effect::Connect(endpoint) => app.handle_event(BgEvent::Connected {
                    endpoint,
                    home: "/home".into(),
                }),
                Effect::ListDir { request, location } => {
                    let entries = listing(&location);
                    app.handle_event(BgEvent::Listed {
                        request,
                        location,
                        result: Ok(entries),
                    })
                }
                _ => Vec::new(),
            };
            queue.extend(more);
        }
    }

    /// Delivers one listing.
    fn deliver(app: &mut App, effect: &Effect, entries: Vec<Entry>) {
        let Effect::ListDir { request, location } = effect else {
            panic!("expected ListDir, got {effect:?}");
        };
        app.handle_event(BgEvent::Listed {
            request: *request,
            location: location.clone(),
            result: Ok(entries),
        });
    }

    /// The left pane at `/l` and the right pane at `/r`, both listed, on an
    /// 80x24 screen.
    fn app_with(
        state: PersistedState,
        left: (Endpoint, Vec<Entry>),
        right: (Endpoint, Vec<Entry>),
    ) -> App {
        let (mut app, effects) = App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            state,
            Background::Dark,
            (left.0, Some("/l".into())),
            Some((right.0, Some("/r".into()))),
        );
        app.handle_action(Action::Resize {
            width: 80,
            height: 24,
        });
        let (left_entries, right_entries) = (left.1, right.1);
        settle(&mut app, effects, &|location| {
            if location.path == Path::new("/l") {
                left_entries.clone()
            } else {
                right_entries.clone()
            }
        });
        app
    }

    /// Local panes: `/l` holds `sub/` and `a` (mode 644), `/r` holds `x`.
    fn two_local() -> App {
        app_with(
            PersistedState::default(),
            (Endpoint::Local, vec![dir("sub"), file("a", Some(0o644))]),
            (Endpoint::Local, vec![file("x", Some(0o600))]),
        )
    }

    fn act(app: &mut App, action: Action) -> Vec<Effect> {
        match app.dialog_action(action) {
            Ok(effects) => effects,
            Err(action) => panic!("{action:?} was not claimed"),
        }
    }

    fn typing(app: &mut App, text: &str) {
        for c in text.chars() {
            act(app, Action::Text(TextEdit::Insert(c)));
        }
    }

    fn top(app: &App) -> &Dialog {
        app.dialog.stack.last().expect("an open dialog")
    }

    fn field_value(app: &App) -> String {
        let field = top(app).field.as_ref().expect("a field");
        field.value.as_str().to_owned()
    }

    /// The single `FsOp` effect of an answer.
    fn fs_op(effects: &[Effect]) -> (RequestId, Endpoint, FsOp) {
        match effects {
            [Effect::FsOp {
                request,
                endpoint,
                op,
            }] => (*request, endpoint.clone(), op.clone()),
            other => panic!("expected one FsOp, got {other:?}"),
        }
    }

    fn cursor_name(app: &App, side: PaneSide) -> String {
        let entry = app.core.pane(side).cursor_entry().expect("cursor");
        entry.name.to_string_lossy().into_owned()
    }

    fn set_cursor(app: &mut App, side: PaneSide, index: usize) {
        let page = app.core.page_height(side);
        app.core.pane_mut(side).set_cursor(index, page);
    }

    fn click(target: ClickTarget) -> Action {
        Action::Click {
            target,
            modifiers: Default::default(),
            at: Instant::now(),
        }
    }

    fn double_click(target: ClickTarget) -> Action {
        Action::DoubleClick {
            target,
            at: Instant::now(),
        }
    }

    /// A dialog another feature would push.
    fn foreign(purpose: DialogPurpose, buttons: Vec<DialogButton>) -> Dialog {
        Dialog {
            purpose,
            title: "Title".into(),
            message: "Message".into(),
            field: None,
            checkbox: None,
            buttons,
            focus: DialogFocus::Button(0),
        }
    }

    fn take_answer(app: &mut App) -> DialogAnswer {
        match app.dialog.answer.take() {
            Some(Answer::Dialog(answer)) => answer,
            Some(Answer::Picked { .. }) => panic!("expected a dialog answer, got a pick"),
            None => panic!("expected a dialog answer"),
        }
    }

    // Claiming.

    #[test]
    fn browse_actions_of_other_features_are_handed_back() {
        let mut app = two_local();
        for action in [
            Action::Copy,
            Action::Paste("/tmp/x".into()),
            Action::Open,
            Action::Confirm,
            Action::Cancel,
            click(ClickTarget::DialogButton(DialogButton::Ok)),
        ] {
            assert_eq!(app.dialog_action(action.clone()), Err(action));
        }
        assert_eq!(app.dialog_input_mode(), None);
    }

    #[test]
    fn open_dialogs_and_pickers_claim_every_action() {
        let mut app = two_local();
        act(&mut app, Action::MakeDir);
        for action in [Action::Copy, Action::Quit, Action::MakeDir, Action::Open] {
            assert_eq!(app.dialog_action(action), Ok(Vec::new()));
        }
        assert_eq!(app.dialog.stack.len(), 1);
        act(&mut app, Action::Cancel);
        app.persisted.bookmarks.push(local("/tmp"));
        act(&mut app, Action::ShowBookmarks);
        for action in [Action::Copy, Action::Delete, Action::Quit] {
            assert_eq!(app.dialog_action(action), Ok(Vec::new()));
        }
        assert!(app.dialog.stack.is_empty());
        assert!(app.dialog.picker.is_some());
    }

    // MakeDir.

    #[test]
    fn make_dir_creates_and_reselects_the_new_folder() {
        let mut app = two_local();
        assert_eq!(act(&mut app, Action::MakeDir), Vec::new());
        assert_eq!(app.dialog_input_mode(), Some(InputMode::TextField));
        let dialog = top(&app);
        assert_eq!(dialog.title, "Create directory");
        assert_eq!(dialog.message, "New directory in /l:");
        assert_eq!(field_value(&app), "");
        assert_eq!(dialog.buttons, [DialogButton::Ok, DialogButton::Cancel]);

        typing(&mut app, "new");
        let effects = act(&mut app, Action::Confirm);
        let (request, endpoint, op) = fs_op(&effects);
        assert_eq!(endpoint, Endpoint::Local);
        assert_eq!(
            op,
            FsOp::MakeDir {
                path: "/l/new".into()
            }
        );
        assert!(app.dialog.stack.is_empty());
        assert_eq!(app.dialog_input_mode(), None);

        let effects = app.dialog_fs_op_finished(request, Endpoint::Local, op, Vec::new());
        // The left pane re-lists to reselect; the right pane (also local)
        // refreshes.
        assert_eq!(effects.len(), 2);
        let left = app.core.pane(PaneSide::Left).pending.clone().expect("left");
        assert!(!left.refresh);
        assert_eq!(left.reselect, Some("new".into()));
        let right = app
            .core
            .pane(PaneSide::Right)
            .pending
            .clone()
            .expect("right");
        assert!(right.refresh);
        deliver(
            &mut app,
            &effects[0],
            vec![dir("sub"), dir("new"), file("a", Some(0o644))],
        );
        assert_eq!(cursor_name(&app, PaneSide::Left), "new");
        assert_eq!(app.core.status(), Some(&StatusMessage::info("Created new")));
        assert!(app.dialog.stack.is_empty());
    }

    #[test]
    fn make_dir_keeps_the_dialog_open_for_bad_names() {
        let mut app = two_local();
        act(&mut app, Action::MakeDir);
        assert_eq!(act(&mut app, Action::Confirm), Vec::new());
        assert_eq!(top(&app).message, "New directory in /l:\n\nEnter a name.");
        for (name, problem) in [
            ("a/b", "A name cannot contain \"/\"."),
            (".", "\".\" is not a valid name."),
            ("..", "\"..\" is not a valid name."),
        ] {
            act(&mut app, Action::Text(TextEdit::Clear));
            typing(&mut app, name);
            assert_eq!(act(&mut app, Action::Confirm), Vec::new());
            assert_eq!(
                top(&app).message,
                format!("New directory in /l:\n\n{problem}")
            );
            assert_eq!(app.dialog_input_mode(), Some(InputMode::TextField));
        }
        // Cancel and Esc close without an operation.
        assert_eq!(act(&mut app, Action::Cancel), Vec::new());
        assert!(app.dialog.stack.is_empty());
        act(&mut app, Action::MakeDir);
        typing(&mut app, "ok");
        act(&mut app, Action::DialogFocus(FocusStep::Next));
        act(&mut app, Action::DialogFocus(FocusStep::Next));
        assert_eq!(act(&mut app, Action::Confirm), Vec::new());
        assert!(app.dialog.stack.is_empty());
        assert!(app.dialog.answer.is_none());
    }

    #[test]
    fn nothing_opens_without_a_listing_or_targets() {
        // The right pane waits for its remote connection: nothing listed.
        let (mut app, _effects) = App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (web(), Some("/srv".into())),
            Some((Endpoint::Local, Some("/r".into()))),
        );
        for action in [
            Action::MakeDir,
            Action::Rename,
            Action::Chmod,
            Action::Delete,
        ] {
            assert_eq!(app.dialog_action(action), Ok(Vec::new()));
        }
        assert!(app.dialog.stack.is_empty());

        // A listed but empty directory: only MakeDir applies.
        let mut app = app_with(
            PersistedState::default(),
            (Endpoint::Local, Vec::new()),
            (Endpoint::Local, Vec::new()),
        );
        for action in [Action::Rename, Action::Chmod, Action::Delete] {
            act(&mut app, action);
        }
        assert!(app.dialog.stack.is_empty());
        act(&mut app, Action::MakeDir);
        assert_eq!(app.dialog.stack.len(), 1);
    }

    // Rename.

    #[test]
    fn rename_prefills_the_name_and_reselects_the_new_one() {
        let mut app = two_local();
        set_cursor(&mut app, PaneSide::Left, 1);
        act(&mut app, Action::Rename);
        assert_eq!(top(&app).message, "Rename \"a\" to:");
        assert_eq!(field_value(&app), "a");
        assert_eq!(top(&app).field.as_ref().map(|field| field.caret), Some(1));

        // The same name closes without an operation.
        assert_eq!(act(&mut app, Action::Confirm), Vec::new());
        assert!(app.dialog.stack.is_empty());

        act(&mut app, Action::Rename);
        typing(&mut app, "b");
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        assert_eq!(
            op,
            FsOp::Rename {
                from: "/l/a".into(),
                to: "/l/ab".into()
            }
        );
        let effects = app.dialog_fs_op_finished(request, endpoint, op, Vec::new());
        deliver(
            &mut app,
            &effects[0],
            vec![dir("sub"), file("ab", Some(0o644))],
        );
        assert_eq!(cursor_name(&app, PaneSide::Left), "ab");
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::info("Renamed a to ab"))
        );
    }

    #[test]
    fn rename_refuses_names_that_are_not_utf8() {
        let name = OsStr::from_bytes(b"bad\xff").to_os_string();
        let entry = Entry {
            name,
            ..file("unused", None)
        };
        let mut app = app_with(
            PersistedState::default(),
            (Endpoint::Local, vec![entry]),
            (Endpoint::Local, Vec::new()),
        );
        assert_eq!(act(&mut app, Action::Rename), Vec::new());
        assert!(app.dialog.stack.is_empty());
        let status = app.core.status().expect("status");
        assert!(status.error);
        assert!(status.text.contains("not valid UTF-8"), "{}", status.text);
    }

    #[test]
    fn a_failed_rename_refreshes_without_reselecting() {
        let mut app = two_local();
        set_cursor(&mut app, PaneSide::Left, 1);
        act(&mut app, Action::Rename);
        act(&mut app, Action::Text(TextEdit::Clear));
        typing(&mut app, "sub");
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        let failure = FsError::AlreadyExists {
            path: "/l/sub".into(),
        };
        app.dialog_fs_op_finished(request, endpoint, op, vec![failure.clone()]);
        let pending = app
            .core
            .pane(PaneSide::Left)
            .pending
            .clone()
            .expect("pending");
        assert!(pending.refresh);
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::error(failure.to_string()))
        );
        let notice = top(&app);
        assert_eq!(notice.purpose, DialogPurpose::Notice);
        assert_eq!(notice.title, "Rename failed");
        assert_eq!(notice.message, failure.to_string());
        assert_eq!(notice.buttons, [DialogButton::Ok]);
        assert_eq!(act(&mut app, Action::DialogHotkey('o')), Vec::new());
        assert!(app.dialog.stack.is_empty());
        assert!(app.dialog.answer.is_none());
    }

    // Chmod.

    #[test]
    fn chmod_prefills_the_mode_and_validates_octal() {
        let mut app = two_local();
        set_cursor(&mut app, PaneSide::Left, 1);
        act(&mut app, Action::Chmod);
        assert_eq!(top(&app).title, "Permissions");
        assert_eq!(top(&app).message, "Permissions of \"a\" (octal):");
        assert_eq!(field_value(&app), "644");
        for bad in ["", "8", "64a", "-1", "12345", "77777"] {
            act(&mut app, Action::Text(TextEdit::Clear));
            typing(&mut app, bad);
            assert_eq!(act(&mut app, Action::Confirm), Vec::new(), "{bad}");
            assert!(top(&app).message.ends_with("for example 644."), "{bad}");
        }
        act(&mut app, Action::Text(TextEdit::Clear));
        typing(&mut app, "04755");
        let (_, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        assert_eq!(endpoint, Endpoint::Local);
        assert_eq!(
            op,
            FsOp::Chmod {
                paths: vec!["/l/a".into()],
                mode: 0o4755
            }
        );
    }

    #[test]
    fn chmod_applies_to_every_selected_entry() {
        let mut app = two_local();
        app.core.pane_mut(PaneSide::Left).select_all();
        set_cursor(&mut app, PaneSide::Left, 0);
        act(&mut app, Action::Chmod);
        // The cursor entry (`sub`, 755) gives the prefill.
        assert_eq!(field_value(&app), "755");
        assert_eq!(top(&app).message, "Permissions of 2 items (octal):");
        act(&mut app, Action::Text(TextEdit::Clear));
        typing(&mut app, "700");
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        assert_eq!(
            op,
            FsOp::Chmod {
                paths: vec!["/l/sub".into(), "/l/a".into()],
                mode: 0o700
            }
        );
        app.dialog_fs_op_finished(request, endpoint, op, Vec::new());
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::info(
                "Set the permissions of 2 items to 700"
            ))
        );
    }

    #[test]
    fn chmod_of_an_unknown_mode_starts_empty() {
        let mut app = app_with(
            PersistedState::default(),
            (Endpoint::Local, vec![file("a", None)]),
            (Endpoint::Local, Vec::new()),
        );
        act(&mut app, Action::Chmod);
        assert_eq!(field_value(&app), "");
    }

    // Delete.

    #[test]
    fn local_delete_asks_and_trashes() {
        let mut app = two_local();
        set_cursor(&mut app, PaneSide::Left, 1);
        act(&mut app, Action::Delete);
        assert_eq!(app.dialog_input_mode(), Some(InputMode::Dialog));
        let dialog = top(&app);
        assert_eq!(dialog.title, "Delete");
        assert_eq!(dialog.message, "Delete \"a\"?\n\nIt goes to the trash.");
        assert_eq!(dialog.buttons, [DialogButton::Yes, DialogButton::No]);
        assert_eq!(dialog.focus, DialogFocus::Button(0));

        // No (hotkey, either case) closes without an operation.
        assert_eq!(act(&mut app, Action::DialogHotkey('N')), Vec::new());
        assert!(app.dialog.stack.is_empty());

        act(&mut app, Action::Delete);
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::DialogHotkey('Y')));
        assert_eq!(endpoint, Endpoint::Local);
        assert_eq!(
            op,
            FsOp::Delete {
                paths: vec!["/l/a".into()],
                method: DeleteMethod::Trash
            }
        );
        app.dialog_fs_op_finished(request, endpoint, op, Vec::new());
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::info("Moved \"a\" to the trash"))
        );
        assert!(app.dialog.stack.is_empty());
    }

    #[test]
    fn remote_delete_says_it_is_permanent() {
        let mut app = app_with(
            PersistedState::default(),
            (Endpoint::Local, vec![file("a", None)]),
            (web(), vec![file("x", None), file("y", None)]),
        );
        app.handle_action(Action::SwitchFocus);
        app.core.pane_mut(PaneSide::Right).select_all();
        act(&mut app, Action::Delete);
        assert_eq!(
            top(&app).message,
            "Delete 2 items?\n\nDeleting on web is permanent and cannot be undone."
        );
        let effects = act(
            &mut app,
            click(ClickTarget::DialogButton(DialogButton::Yes)),
        );
        let (request, endpoint, op) = fs_op(&effects);
        assert_eq!(endpoint, web());
        assert_eq!(
            op,
            FsOp::Delete {
                paths: vec!["/r/x".into(), "/r/y".into()],
                method: DeleteMethod::Permanent
            }
        );
        // Only the pane showing `web` refreshes.
        let effects = app.dialog_fs_op_finished(request, endpoint, op, Vec::new());
        assert_eq!(effects.len(), 1);
        assert!(app.core.pane(PaneSide::Left).pending.is_none());
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::info("Deleted 2 items"))
        );
    }

    #[test]
    fn several_local_items_go_to_the_trash() {
        let mut app = two_local();
        app.core.pane_mut(PaneSide::Left).select_all();
        act(&mut app, Action::Delete);
        assert_eq!(
            top(&app).message,
            "Delete 2 items?\n\nThey go to the trash."
        );
    }

    #[test]
    fn a_failed_trash_offers_a_permanent_delete() {
        let mut app = two_local();
        app.core.pane_mut(PaneSide::Left).select_all();
        act(&mut app, Action::Delete);
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        let denied = FsError::PermissionDenied {
            path: "/l/sub".into(),
        };
        let failures = vec![
            denied.clone(),
            FsError::TrashFailed {
                path: "/l/a".into(),
                message: "no trash on this volume".into(),
            },
        ];
        app.dialog_fs_op_finished(request, endpoint, op, failures);
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::error("Delete failed for 2 items"))
        );
        // The trash question is on top of the notice.
        assert_eq!(app.dialog.stack.len(), 2);
        let question = top(&app);
        assert_eq!(question.title, "Trash failed");
        assert_eq!(
            question.message,
            "\"a\" could not be moved to the trash: no trash on this volume\n\n\
             Delete permanently instead? This cannot be undone."
        );
        assert_eq!(
            question.buttons,
            [DialogButton::DeletePermanently, DialogButton::Cancel]
        );
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        assert_eq!(endpoint, Endpoint::Local);
        assert_eq!(
            op,
            FsOp::Delete {
                paths: vec!["/l/a".into()],
                method: DeleteMethod::Permanent
            }
        );
        let notice = top(&app);
        assert_eq!(notice.purpose, DialogPurpose::Notice);
        assert_eq!(notice.title, "Delete failed");
        assert_eq!(notice.message, denied.to_string());
        act(&mut app, Action::Confirm);
        assert!(app.dialog.stack.is_empty());

        // The permanent delete reports like any other.
        app.dialog_fs_op_finished(request, endpoint, op, Vec::new());
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::info("Deleted \"a\""))
        );
        assert!(app.dialog.stack.is_empty());
    }

    #[test]
    fn cancelling_the_trash_question_deletes_nothing() {
        let mut app = two_local();
        set_cursor(&mut app, PaneSide::Left, 1);
        act(&mut app, Action::Delete);
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        let failure = FsError::TrashFailed {
            path: "/l/a".into(),
            message: "no trash".into(),
        };
        app.dialog_fs_op_finished(request, endpoint, op, vec![failure]);
        assert_eq!(app.dialog.stack.len(), 1);
        assert_eq!(act(&mut app, Action::DialogHotkey('c')), Vec::new());
        assert!(app.dialog.stack.is_empty());
        assert!(app.dialog.answer.is_none());
    }

    #[test]
    fn many_failures_are_listed_up_to_a_limit() {
        let mut app = two_local();
        app.core.pane_mut(PaneSide::Left).select_all();
        act(&mut app, Action::Chmod);
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        let failures: Vec<FsError> = (0..12)
            .map(|index| FsError::PermissionDenied {
                path: format!("/l/f{index}").into(),
            })
            .collect();
        app.dialog_fs_op_finished(request, endpoint, op, failures.clone());
        let lines: Vec<&str> = top(&app).message.lines().collect();
        assert_eq!(lines.len(), NOTICE_MAX_FAILURES + 1);
        assert_eq!(lines[0], failures[0].to_string());
        assert_eq!(lines[9], failures[9].to_string());
        assert_eq!(lines[10], "and 2 more");
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::error("Permissions failed for 12 items"))
        );
    }

    #[test]
    fn results_of_unknown_requests_are_dropped() {
        let mut app = two_local();
        let effects = app.dialog_fs_op_finished(
            RequestId(9999),
            Endpoint::Local,
            FsOp::MakeDir {
                path: "/l/x".into(),
            },
            vec![FsError::Disconnected],
        );
        assert_eq!(effects, Vec::new());
        assert!(app.dialog.stack.is_empty());
        assert_eq!(app.core.status(), None);
        // A request is answered once.
        act(&mut app, Action::MakeDir);
        typing(&mut app, "x");
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        assert_eq!(
            app.dialog_fs_op_finished(request, endpoint.clone(), op.clone(), Vec::new())
                .len(),
            2
        );
        assert_eq!(
            app.dialog_fs_op_finished(request, endpoint, op, Vec::new()),
            Vec::new()
        );
    }

    #[test]
    fn navigating_panes_are_not_refreshed_and_moved_panes_do_not_reselect() {
        let mut app = two_local();
        act(&mut app, Action::MakeDir);
        typing(&mut app, "new");
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        // The left pane started entering `sub` before the result arrived.
        app.handle_action(Action::Open);
        let navigation = app.core.pane(PaneSide::Left).pending.clone();
        let effects = app.dialog_fs_op_finished(request, endpoint, op, Vec::new());
        assert_eq!(effects.len(), 1, "only the right pane refreshes");
        assert_eq!(app.core.pane(PaneSide::Left).pending, navigation);

        // Once the left pane shows `sub`, a new folder in `/l` refreshes it
        // without reselecting.
        let mut app = two_local();
        act(&mut app, Action::MakeDir);
        typing(&mut app, "new");
        let (request, endpoint, op) = fs_op(&act(&mut app, Action::Confirm));
        let open = app.handle_action(Action::Open);
        deliver(&mut app, &open[0], vec![file("inner", None)]);
        app.dialog_fs_op_finished(request, endpoint, op, Vec::new());
        let pending = app
            .core
            .pane(PaneSide::Left)
            .pending
            .clone()
            .expect("pending");
        assert!(pending.refresh);
        assert_eq!(pending.path, PathBuf::from("/l/sub"));
    }

    // Generic dialog mechanics.

    #[test]
    fn focus_cycles_and_the_checkbox_toggles_without_answering() {
        let mut app = two_local();
        let mut dialog = foreign(
            DialogPurpose::ConfirmSync {
                direction: SyncDirection::LeftToRight,
            },
            vec![DialogButton::Ok, DialogButton::Cancel],
        );
        dialog.checkbox = Some(Checkbox {
            label: "Delete 2 entries".into(),
            checked: false,
        });
        app.dialog.stack.push(dialog);
        let focus = |app: &App| top(app).focus;
        let checked = |app: &App| top(app).checkbox.as_ref().is_some_and(|c| c.checked);

        act(&mut app, Action::DialogFocus(FocusStep::Next));
        assert_eq!(focus(&app), DialogFocus::Button(1));
        act(&mut app, Action::DialogFocus(FocusStep::Next));
        assert_eq!(focus(&app), DialogFocus::Checkbox);
        act(&mut app, Action::DialogFocus(FocusStep::Previous));
        assert_eq!(focus(&app), DialogFocus::Button(1));
        act(&mut app, Action::DialogFocus(FocusStep::Next));

        act(&mut app, Action::ToggleOption);
        assert!(checked(&app));
        // Space on a button does nothing.
        act(&mut app, Action::DialogFocus(FocusStep::Next));
        act(&mut app, Action::ToggleOption);
        assert!(checked(&app));

        // A click on the checkbox toggles and focuses it; so does the second
        // click of a double click.
        act(
            &mut app,
            click(ClickTarget::DialogButton(DialogButton::ApplyToAll)),
        );
        assert!(!checked(&app));
        assert_eq!(focus(&app), DialogFocus::Checkbox);
        act(
            &mut app,
            double_click(ClickTarget::DialogButton(DialogButton::ApplyToAll)),
        );
        assert!(checked(&app));
        assert_eq!(app.dialog.stack.len(), 1);
        assert!(app.dialog.answer.is_none());

        // Enter on the checkbox presses the first button.
        act(&mut app, Action::Confirm);
        assert!(app.dialog.stack.is_empty());
        let answer = take_answer(&mut app);
        assert_eq!(
            answer.purpose,
            DialogPurpose::ConfirmSync {
                direction: SyncDirection::LeftToRight
            }
        );
        assert_eq!(answer.button, DialogButton::Ok);
        assert!(answer.checked);
        assert!(answer.field.is_none());
    }

    #[test]
    fn foreign_answers_carry_the_field_without_copying_it() {
        let mut app = two_local();
        let endpoint = Endpoint::Remote {
            host: "web".into(),
            sudo: true,
        };
        let mut dialog = foreign(
            DialogPurpose::SudoPassword {
                endpoint: endpoint.clone(),
            },
            vec![DialogButton::Ok, DialogButton::Cancel],
        );
        dialog.field = Some(TextField {
            value: Zeroizing::new(String::with_capacity(PASSWORD_MAX_LEN)),
            caret: 0,
            masked: true,
            max_len: Some(PASSWORD_MAX_LEN),
        });
        dialog.focus = DialogFocus::Field;
        app.dialog.stack.push(dialog);
        assert_eq!(app.dialog_input_mode(), Some(InputMode::TextField));
        let buffer = top(&app).field.as_ref().map(|field| field.value.as_ptr());
        typing(&mut app, "hunter2");
        act(&mut app, Action::Confirm);
        let answer = take_answer(&mut app);
        assert_eq!(answer.purpose, DialogPurpose::SudoPassword { endpoint });
        assert_eq!(answer.button, DialogButton::Ok);
        assert!(!answer.checked);
        let value = answer.field.expect("field");
        assert_eq!(value.as_str(), "hunter2");
        assert_eq!(Some(value.as_ptr()), buffer);
        assert_eq!(value.capacity(), PASSWORD_MAX_LEN);
    }

    #[test]
    fn esc_answers_cancel_even_without_a_cancel_button() {
        let mut app = two_local();
        app.dialog.stack.push(foreign(
            DialogPurpose::QuitWhileRunning,
            vec![DialogButton::Yes, DialogButton::No],
        ));
        act(&mut app, Action::Cancel);
        let answer = take_answer(&mut app);
        assert_eq!(answer.purpose, DialogPurpose::QuitWhileRunning);
        assert_eq!(answer.button, DialogButton::Cancel);
    }

    #[test]
    fn only_the_top_dialog_answers_and_absent_buttons_do_nothing() {
        let mut app = two_local();
        app.dialog.stack.push(foreign(
            DialogPurpose::QuitWhileRunning,
            vec![DialogButton::Yes, DialogButton::No],
        ));
        app.dialog.stack.push(foreign(
            DialogPurpose::TarConflict { job: JobId(4) },
            vec![DialogButton::OverwriteAll, DialogButton::Cancel],
        ));
        for action in [
            click(ClickTarget::DialogButton(DialogButton::Yes)),
            click(ClickTarget::DialogButton(DialogButton::ApplyToAll)),
            double_click(ClickTarget::DialogButton(DialogButton::OverwriteAll)),
            Action::DialogHotkey('y'),
            Action::ToggleOption,
        ] {
            act(&mut app, action);
        }
        assert_eq!(app.dialog.stack.len(), 2);
        assert!(app.dialog.answer.is_none());
        act(
            &mut app,
            click(ClickTarget::DialogButton(DialogButton::OverwriteAll)),
        );
        let answer = take_answer(&mut app);
        assert_eq!(answer.purpose, DialogPurpose::TarConflict { job: JobId(4) });
        assert_eq!(answer.button, DialogButton::OverwriteAll);
        assert_eq!(top(&app).purpose, DialogPurpose::QuitWhileRunning);
    }

    #[test]
    fn hotkeys_are_unique_within_every_button_set() {
        use DialogButton::*;
        let sets: [&[DialogButton]; 9] = [
            &[Ok, Cancel],
            &[Yes, No],
            &[Ok],
            &[Overwrite, Skip, Rename, OverwriteIfNewer, Resume, Cancel],
            &[OverwriteAll, Cancel],
            &[Upload, Cancel],
            &[Run, Cancel],
            &[DeletePermanently, Cancel],
            &[Retry, Overwrite, Cancel],
        ];
        for set in sets {
            let mut keys: Vec<char> = set.iter().map(|&b| button_caption(b).1).collect();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), set.len(), "{set:?}");
        }
        let mut app = two_local();
        app.dialog.stack.push(foreign(
            DialogPurpose::Conflict {
                item: ItemId(1),
                job: JobId(1),
                partial: true,
            },
            vec![Overwrite, Skip, Rename, OverwriteIfNewer, Resume],
        ));
        act(&mut app, Action::DialogHotkey('E'));
        assert_eq!(take_answer(&mut app).button, Resume);
    }

    #[test]
    fn paste_goes_into_the_focused_field_only() {
        let mut app = two_local();
        act(&mut app, Action::MakeDir);
        act(&mut app, Action::Paste("new\tdir\n".into()));
        assert_eq!(field_value(&app), "newdir");
        act(&mut app, Action::DialogFocus(FocusStep::Next));
        assert_eq!(app.dialog_input_mode(), Some(InputMode::Dialog));
        act(&mut app, Action::Paste("more".into()));
        act(&mut app, Action::Text(TextEdit::Insert('z')));
        assert_eq!(field_value(&app), "newdir");
        act(&mut app, Action::DialogFocus(FocusStep::Previous));
        assert_eq!(app.dialog_input_mode(), Some(InputMode::TextField));
    }

    #[test]
    fn text_field_edits_at_the_caret() {
        let mut field = TextField::with_value("ab\u{e9}d");
        assert_eq!(field.caret, 4);
        field.edit(TextEdit::Left);
        field.edit(TextEdit::Insert('\u{4e2d}'));
        assert_eq!(field.value.as_str(), "ab\u{e9}\u{4e2d}d");
        assert_eq!(field.caret, 4);
        field.edit(TextEdit::Backspace);
        field.edit(TextEdit::Backspace);
        assert_eq!(field.value.as_str(), "abd");
        assert_eq!(field.caret, 2);
        field.edit(TextEdit::DeleteForward);
        assert_eq!(field.value.as_str(), "ab");
        field.edit(TextEdit::DeleteForward);
        assert_eq!(field.value.as_str(), "ab");
        field.edit(TextEdit::Home);
        field.edit(TextEdit::Backspace);
        field.edit(TextEdit::Left);
        assert_eq!((field.value.as_str(), field.caret), ("ab", 0));
        field.edit(TextEdit::Insert('\n'));
        assert_eq!(field.value.as_str(), "ab");
        field.edit(TextEdit::End);
        field.edit(TextEdit::Right);
        assert_eq!(field.caret, 2);

        let mut field = TextField::with_value("foo bar  baz");
        field.edit(TextEdit::Left);
        field.edit(TextEdit::Left);
        field.edit(TextEdit::Left);
        field.edit(TextEdit::DeleteWord);
        assert_eq!((field.value.as_str(), field.caret), ("foo baz", 4));
        field.edit(TextEdit::DeleteWord);
        assert_eq!((field.value.as_str(), field.caret), ("baz", 0));
        field.edit(TextEdit::DeleteWord);
        assert_eq!(field.value.as_str(), "baz");
        field.edit(TextEdit::Clear);
        assert_eq!((field.value.as_str(), field.caret), ("", 0));
    }

    #[test]
    fn a_limited_field_rejects_input_beyond_it_without_reallocating() {
        let mut field = TextField {
            value: Zeroizing::new(String::with_capacity(4)),
            caret: 0,
            masked: true,
            max_len: Some(4),
        };
        let buffer = field.value.as_ptr();
        for c in "abcde".chars() {
            field.edit(TextEdit::Insert(c));
        }
        assert_eq!(field.value.as_str(), "abcd");
        field.edit(TextEdit::Backspace);
        // A two-byte character does not fit into the one byte left.
        field.edit(TextEdit::Insert('\u{e9}'));
        assert_eq!(field.value.as_str(), "abc");
        // Paste is all or nothing.
        field.paste("xy");
        assert_eq!(field.value.as_str(), "abc");
        field.paste("x\n");
        assert_eq!(field.value.as_str(), "abcx");
        field.edit(TextEdit::Clear);
        field.paste("wxyz");
        assert_eq!(field.value.as_str(), "wxyz");
        assert_eq!(field.value.as_ptr(), buffer);
        assert_eq!(field.value.capacity(), 4);
    }

    #[test]
    fn input_mode_follows_the_top_dialog_and_the_picker() {
        let mut app = two_local();
        assert_eq!(app.dialog_input_mode(), None);
        app.persisted.bookmarks.push(local("/tmp"));
        act(&mut app, Action::ShowBookmarks);
        assert_eq!(app.dialog_input_mode(), Some(InputMode::Picker));
        // A dialog above the picker takes the input.
        app.dialog
            .stack
            .push(foreign(DialogPurpose::Notice, vec![DialogButton::Ok]));
        assert_eq!(app.dialog_input_mode(), Some(InputMode::Dialog));
        act(&mut app, Action::Confirm);
        assert_eq!(app.dialog_input_mode(), Some(InputMode::Picker));
        // A dialog whose focus names a field it does not have is a button
        // dialog.
        let mut dialog = foreign(DialogPurpose::Notice, vec![DialogButton::Ok]);
        dialog.focus = DialogFocus::Field;
        app.dialog.stack.push(dialog);
        assert_eq!(app.dialog_input_mode(), Some(InputMode::Dialog));
    }

    #[test]
    fn dialog_view_shows_the_top_dialog() {
        let mut app = two_local();
        let mut buttons = Vec::new();
        assert!(app.dialog_view(&mut buttons).is_none());

        act(&mut app, Action::MakeDir);
        typing(&mut app, "ab");
        act(&mut app, Action::Text(TextEdit::Left));

        let mut buttons = Vec::new();
        let view = app.dialog_view(&mut buttons).expect("view");
        assert_eq!(view.title, "Create directory");
        assert_eq!(view.message, "New directory in /l:");
        let field = view.field.expect("field");
        assert_eq!((field.value, field.caret), ("ab", 1));
        assert!(field.focused && !field.masked);
        assert!(view.checkbox.is_none());
        let captions: Vec<_> = view
            .buttons
            .iter()
            .map(|b| (b.button, b.label, b.hotkey, b.focused))
            .collect();
        assert_eq!(
            captions,
            [
                (DialogButton::Ok, "OK", Some('o'), false),
                (DialogButton::Cancel, "Cancel", Some('c'), false),
            ]
        );

        act(&mut app, Action::Cancel);
        let mut dialog = foreign(
            DialogPurpose::Conflict {
                item: ItemId(1),
                job: JobId(1),
                partial: false,
            },
            vec![DialogButton::Overwrite, DialogButton::Skip],
        );
        dialog.checkbox = Some(Checkbox {
            label: "Apply to all in this job".into(),
            checked: true,
        });
        dialog.focus = DialogFocus::Button(1);
        app.dialog.stack.push(dialog);
        let mut buttons = Vec::new();
        let view = app.dialog_view(&mut buttons).expect("view");
        assert_eq!(view.title, "Title");
        let checkbox = view.checkbox.expect("checkbox");
        assert_eq!(checkbox.label, "Apply to all in this job");
        assert!(checkbox.checked && !checkbox.focused);
        assert!(view.field.is_none());
        let focused: Vec<_> = view.buttons.iter().map(|b| (b.label, b.focused)).collect();
        assert_eq!(focused, [("Overwrite", false), ("Skip", true)]);
    }

    // Bookmark list.

    fn bookmarked() -> App {
        let state = PersistedState {
            last_directories: Vec::new(),
            bookmarks: vec![
                local("/tmp"),
                Location {
                    endpoint: web(),
                    path: "/srv/www".into(),
                },
                Location {
                    endpoint: Endpoint::Remote {
                        host: "web".into(),
                        sudo: true,
                    },
                    path: "/etc".into(),
                },
            ],
        };
        app_with(
            state,
            (Endpoint::Local, vec![file("a", None)]),
            (Endpoint::Local, Vec::new()),
        )
    }

    fn picker_rows(app: &App) -> Vec<(String, String)> {
        let (mut tabs, mut rows) = (Vec::new(), Vec::new());
        let view = app.picker_view(&mut tabs, &mut rows).expect("picker");
        view.rows
            .iter()
            .map(|row| (row.label.to_owned(), row.detail.to_owned()))
            .collect()
    }

    #[test]
    fn the_bookmark_list_shows_every_bookmark() {
        let mut app = bookmarked();
        act(&mut app, Action::ShowBookmarks);
        let list = app.dialog.picker.as_ref().expect("picker");
        assert_eq!(
            list.kind,
            PickerKind::Bookmarks {
                side: PaneSide::Left
            }
        );
        assert_eq!(
            picker_rows(&app),
            [
                ("/tmp".to_owned(), String::new()),
                ("web:/srv/www".to_owned(), String::new()),
                ("web:/etc".to_owned(), "sudo".to_owned()),
            ]
        );
        let (mut tabs, mut rows) = (Vec::new(), Vec::new());
        let view = app.picker_view(&mut tabs, &mut rows).expect("picker");
        assert_eq!(view.title, "Bookmarks");
        assert!(view.tabs.is_empty());
        assert_eq!((view.cursor, view.first_visible), (0, 0));

        act(&mut app, Action::Cancel);
        assert!(app.dialog.picker.is_none());
    }

    #[test]
    fn without_bookmarks_the_list_does_not_open() {
        let mut app = two_local();
        act(&mut app, Action::ShowBookmarks);
        assert!(app.dialog.picker.is_none());
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::info(
                "No bookmarks yet; B bookmarks the current directory"
            ))
        );
    }

    #[test]
    fn picking_a_bookmark_jumps_there() {
        let mut app = bookmarked();
        act(&mut app, Action::ShowBookmarks);
        typing(&mut app, "SRV");
        assert_eq!(picker_rows(&app).len(), 1);
        let effects = act(&mut app, Action::Confirm);
        assert!(app.dialog.picker.is_none());
        assert!(app.dialog.answer.is_none());
        // Another endpoint: the pane switches and connects.
        assert_eq!(effects, [Effect::Connect(web())]);
        assert_eq!(app.core.pane(PaneSide::Left).endpoint, web());
        settle(&mut app, effects, &|_| vec![file("index.html", None)]);
        assert_eq!(
            app.core.pane(PaneSide::Left).location(),
            Some(Location {
                endpoint: web(),
                path: "/srv/www".into()
            })
        );

        // The same endpoint lists the directory.
        let mut app = bookmarked();
        act(&mut app, Action::ShowBookmarks);
        let effects = act(&mut app, double_click(ClickTarget::PickerRow(0)));
        let [Effect::ListDir { location, .. }] = effects.as_slice() else {
            panic!("expected one listing, got {effects:?}");
        };
        assert_eq!(*location, local("/tmp"));
    }

    #[test]
    fn delete_in_the_bookmark_list_removes_the_bookmark() {
        let mut app = bookmarked();
        act(&mut app, Action::ShowBookmarks);
        act(&mut app, Action::MoveCursor(CursorMove::End));
        let removed = Location {
            endpoint: Endpoint::Remote {
                host: "web".into(),
                sudo: true,
            },
            path: "/etc".into(),
        };
        assert_eq!(
            act(&mut app, Action::PickerRemove),
            [Effect::RemoveBookmark(removed.clone())]
        );
        assert!(!app.persisted.bookmarks.contains(&removed));
        assert_eq!(app.persisted.bookmarks.len(), 2);
        let list = app.dialog.picker.as_ref().expect("still open");
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.cursor, 1);
        assert_eq!(
            app.core.status(),
            Some(&StatusMessage::info("Removed bookmark web:/etc"))
        );

        // A filter that hides every row leaves nothing to remove.
        typing(&mut app, "zzz");
        assert_eq!(act(&mut app, Action::PickerRemove), Vec::new());
        assert_eq!(app.persisted.bookmarks.len(), 2);
    }

    // Picker mechanics.

    fn host_item(label: &str, detail: &str, tab: usize) -> PickerItem {
        PickerItem {
            label: label.into(),
            detail: detail.into(),
            tab,
            target: if label == "Local" {
                PickerTarget::Local
            } else {
                PickerTarget::Host(label.into())
            },
        }
    }

    fn host_picker(app: &mut App, items: Vec<PickerItem>, tabs: &[&str]) {
        app.dialog.picker = Some(PickerList {
            kind: PickerKind::Hosts {
                side: PaneSide::Right,
            },
            title: "Connect".into(),
            tabs: tabs.iter().map(|&tab| tab.to_owned()).collect(),
            active_tab: 0,
            filter: String::new(),
            items,
            cursor: 0,
            first_visible: 0,
        });
    }

    fn labels(app: &App) -> Vec<String> {
        picker_rows(app)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    #[test]
    fn digits_select_tabs_while_the_filter_is_empty() {
        let mut app = two_local();
        host_picker(
            &mut app,
            vec![
                host_item("Local", "", 2),
                host_item("web1", "deploy@web1.example", 1),
                host_item("db", "root@db.internal", 2),
            ],
            &["All", "prod", "Other"],
        );
        assert_eq!(labels(&app), ["Local", "web1", "db"]);
        act(&mut app, Action::Text(TextEdit::Insert('2')));
        assert_eq!(app.dialog.picker.as_ref().map(|l| l.active_tab), Some(1));
        assert_eq!(labels(&app), ["web1"]);
        act(&mut app, Action::Text(TextEdit::Insert('3')));
        assert_eq!(labels(&app), ["Local", "db"]);
        // No fourth tab: the digit is ignored, not typed.
        act(&mut app, Action::Text(TextEdit::Insert('4')));
        assert_eq!(
            app.dialog.picker.as_ref().map(|l| l.filter.as_str()),
            Some("")
        );
        act(&mut app, Action::Text(TextEdit::Insert('1')));
        // The filter matches the detail too.
        typing(&mut app, "intern");
        assert_eq!(labels(&app), ["db"]);
        act(&mut app, Action::Text(TextEdit::Clear));
        typing(&mut app, "w1");
        assert_eq!(labels(&app), ["web1"]);
        act(&mut app, Action::Text(TextEdit::Backspace));
        act(&mut app, Action::Text(TextEdit::Backspace));
        assert_eq!(labels(&app).len(), 3);

        act(&mut app, Action::MoveCursor(CursorMove::Down));
        act(&mut app, Action::Confirm);
        assert!(app.dialog.picker.is_none());
        match app.dialog.answer.take() {
            Some(Answer::Picked { kind, target }) => {
                assert_eq!(
                    kind,
                    PickerKind::Hosts {
                        side: PaneSide::Right
                    }
                );
                assert_eq!(target, PickerTarget::Host("web1".into()));
            }
            _ => panic!("expected a pick"),
        }
    }

    #[test]
    fn without_tabs_digits_are_filter_text() {
        let mut app = two_local();
        host_picker(
            &mut app,
            vec![host_item("web1", "", 0), host_item("web2", "", 0)],
            &[],
        );
        act(&mut app, Action::Text(TextEdit::Insert('2')));
        assert_eq!(labels(&app), ["web2"]);
        act(&mut app, Action::Paste("x\n".into()));
        assert_eq!(
            app.dialog.picker.as_ref().map(|l| l.filter.as_str()),
            Some("2x")
        );
        assert!(labels(&app).is_empty());
        // Enter on no rows picks nothing; Delete removes no host.
        assert_eq!(act(&mut app, Action::Confirm), Vec::new());
        assert_eq!(act(&mut app, Action::PickerRemove), Vec::new());
        assert!(app.dialog.picker.is_some());
        assert!(app.dialog.answer.is_none());
        act(&mut app, Action::Text(TextEdit::DeleteWord));
        assert_eq!(labels(&app).len(), 2);
    }

    #[test]
    fn rows_follow_item_changes_while_open() {
        let mut app = two_local();
        host_picker(
            &mut app,
            (0..3).map(|i| host_item(&format!("h{i}"), "", 0)).collect(),
            &[],
        );
        act(&mut app, Action::MoveCursor(CursorMove::End));
        // The connection feature replaces the rows.
        if let Some(list) = app.dialog.picker.as_mut() {
            list.items.truncate(1);
        }
        let (mut tabs, mut rows) = (Vec::new(), Vec::new());
        let view = app.picker_view(&mut tabs, &mut rows).expect("picker");
        assert_eq!((view.cursor, view.first_visible), (0, 0));
        act(&mut app, Action::Confirm);
        assert!(matches!(
            app.dialog.answer.take(),
            Some(Answer::Picked {
                target: PickerTarget::Host(ref host),
                ..
            }) if host == "h0"
        ));
    }

    #[test]
    fn cursor_scroll_and_clicks_keep_the_cursor_on_the_page() {
        let mut app = two_local();
        host_picker(
            &mut app,
            (0..40)
                .map(|i| host_item(&format!("h{i:02}"), "", 0))
                .collect(),
            &[],
        );
        // 24 rows: 4 margin, 3 chrome, no tabs.
        let page = 17;
        let state = |app: &App| {
            let list = app.dialog.picker.as_ref().expect("picker");
            (list.cursor, list.first_visible)
        };
        act(&mut app, Action::MoveCursor(CursorMove::End));
        assert_eq!(state(&app), (39, 40 - page));
        act(&mut app, Action::MoveCursor(CursorMove::Home));
        assert_eq!(state(&app), (0, 0));
        act(&mut app, Action::MoveCursor(CursorMove::PageDown));
        assert_eq!(state(&app), (page, 1));
        act(&mut app, Action::MoveCursor(CursorMove::PageUp));
        assert_eq!(state(&app), (0, 0));
        act(&mut app, Action::MoveCursor(CursorMove::Up));
        assert_eq!(state(&app), (0, 0));

        let scroll = |lines| Action::Scroll {
            region: ScrollRegion::Picker,
            lines,
        };
        act(&mut app, scroll(3));
        assert_eq!(state(&app), (3, 3));
        act(&mut app, scroll(100));
        assert_eq!(state(&app), (40 - page, 40 - page));
        act(&mut app, Action::MoveCursor(CursorMove::End));
        act(&mut app, scroll(-3));
        assert_eq!(state(&app), (39 - 3, 40 - page - 3));
        act(&mut app, scroll(-100));
        assert_eq!(state(&app), (page - 1, 0));
        // The wheel over another region does nothing here.
        act(
            &mut app,
            Action::Scroll {
                region: ScrollRegion::Queue,
                lines: 5,
            },
        );
        assert_eq!(state(&app), (page - 1, 0));

        act(&mut app, click(ClickTarget::PickerRow(4)));
        assert_eq!(state(&app), (4, 0));
        act(&mut app, click(ClickTarget::PickerRow(400)));
        assert_eq!(state(&app).0, 39);
        // A double click beyond the rows picks nothing.
        act(&mut app, double_click(ClickTarget::PickerRow(400)));
        assert!(app.dialog.picker.is_some());
        act(&mut app, double_click(ClickTarget::PickerRow(2)));
        assert!(app.dialog.picker.is_none());
        assert!(matches!(
            app.dialog.answer.take(),
            Some(Answer::Picked {
                target: PickerTarget::Host(ref host),
                ..
            }) if host == "h02"
        ));
    }

    #[test]
    fn the_picker_page_matches_the_widget() {
        let items: Vec<PickerItem> = (0..60)
            .map(|i| host_item(&format!("h{i}"), "", 1))
            .collect();
        for (width, height) in [(80, 24), (40, 10), (60, 20), (160, 45), (100, 30)] {
            for tabs in [&[][..], &["All", "Other"][..]] {
                let mut app = two_local();
                app.handle_action(Action::Resize { width, height });
                host_picker(&mut app, items.clone(), tabs);
                let list = app.dialog.picker.as_ref().expect("picker");
                let page = app.picker_page(list);
                let (mut tab_names, mut rows) = (Vec::new(), Vec::new());
                let view = app.picker_view(&mut tab_names, &mut rows).expect("picker");
                let screen = Rect::new(0, 0, width, height);
                let mut buf = Buffer::empty(screen);
                let hit = picker::render(&view, screen, &mut buf);
                assert_eq!(hit.row_count, page, "{width}x{height} tabs {tabs:?}");
            }
        }
    }
}
