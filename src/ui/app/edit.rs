//! Editing in the app: starting edits, the quick-exit editor hint, and the
//! remote-changed and upload-failed prompts.
//!
//! # Behavior
//!
//! - `e` / F4 on a regular file (or a symlink to one) of the focused pane
//!   emits `Effect::Edit(location)` and adds the location to
//!   [`EditState::active`]; a second edit of a location already active is a
//!   status message. Directories and special files are a status message.
//! - `EditFinished` removes the location from [`EditState::active`] and
//!   handles the outcome:
//!   - `Unchanged { quick_exit: true }`: info status "The editor returned
//!     at once; GUI editors need a wait flag, for example `code --wait`".
//!   - `Unchanged { quick_exit: false }`: no message.
//!   - `LocalEdited` / `Uploaded`: info status, and every pane showing the
//!     file's directory is refreshed with `App::refresh`.
//!   - `RemoteChanged { temp }`: pushes `DialogPurpose::EditConflict`
//!     ("`<file>` changed on the server while you edited it", buttons
//!     Overwrite / Cancel; Cancel discards the edits) and re-adds the
//!     location to `active` until answered.
//!   - `UploadFailed { temp, error }`: pushes
//!     `DialogPurpose::EditUploadFailed` (the message has the error and
//!     says the edits are kept in `temp` until scpto exits; buttons Retry /
//!     Cancel; Cancel keeps the file) and re-adds the location to `active`
//!     until answered.
//!   - `Err(error)`: error status (nothing was edited, or the editor failed
//!     to start).
//! - Answers (`App::edit_dialog_answer`): `EditConflict` Overwrite →
//!   `Effect::ResolveEditConflict { overwrite: true }`, Cancel →
//!   `{ overwrite: false }`; `EditUploadFailed` Retry →
//!   `{ overwrite: true }`, Cancel → nothing (the temp file stays).
//!
//! # Public API (Wave 3, ui/app/edit)
//!
//! ```text
//! impl App {
//!     /// Claims `Edit`; hands every other action back unchanged.
//!     pub fn edit_action(&mut self, action: Action) -> Result<Vec<Effect>, Action>;
//!     /// `BgEvent::EditFinished`.
//!     pub fn edit_finished(&mut self, location: Location, result: Result<EditOutcome, Error>) -> Vec<Effect>;
//!     /// Answers of `DialogPurpose::{EditConflict, EditUploadFailed}`.
//!     pub fn edit_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect>;
//! }
//! ```
//!
//! The connection feature keeps the endpoints of [`EditState::active`]
//! connected.

use crate::vfs::Location;

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct EditState {
    /// Files with an edit in progress (download, editor, upload) or an
    /// unanswered edit dialog.
    pub active: Vec<Location>,
}
