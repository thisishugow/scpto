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

use std::path::PathBuf;

use zeroize::Zeroizing;

use crate::sync::SyncDirection;
use crate::transfer::model::{ItemId, JobId};
use crate::ui::action::{DeleteMethod, DialogButton, PaneSide};
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
