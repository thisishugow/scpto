//! Editing in the app: starting edits, the quick-exit editor hint, and the
//! remote-changed and upload-failed prompts.
//!
//! # Behavior
//!
//! - `e` on a regular file (or a symlink to one) of the focused pane
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
//!   Every answer that emits `ResolveEditConflict` leaves the location in
//!   [`EditState::active`]: the executor reports the upload or discard as
//!   another `EditFinished`, which removes it. `EditUploadFailed` Cancel
//!   removes it at once and repeats the temp path in an info status.
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

use std::borrow::Cow;
use std::path::Path;

use super::core::StatusMessage;
use super::dialog::{Dialog, DialogAnswer, DialogFocus, DialogPurpose};
use super::App;
use crate::edit::EditOutcome;
use crate::error::Error;
use crate::ui::action::{Action, DialogButton, Effect, PaneSide};
use crate::vfs::{EntryKind, Location};

/// Status shown when the editor returned at once without changes.
const QUICK_EXIT_HINT: &str =
    "The editor returned at once; GUI editors need a wait flag, for example `code --wait`";

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct EditState {
    /// Files with an edit in progress (download, editor, upload) or an
    /// unanswered edit dialog.
    pub active: Vec<Location>,
}

impl App {
    /// Claims `Edit`; hands every other action back unchanged.
    pub fn edit_action(&mut self, action: Action) -> Result<Vec<Effect>, Action> {
        let Action::Edit = action else {
            return Err(action);
        };
        Ok(self.edit_cursor_entry(self.core.focused()))
    }

    /// `BgEvent::EditFinished`.
    pub fn edit_finished(
        &mut self,
        location: Location,
        result: Result<EditOutcome, Error>,
    ) -> Vec<Effect> {
        self.edit.active.retain(|active| *active != location);
        let name = file_name(&location.path);
        match result {
            Ok(EditOutcome::Unchanged { quick_exit: true }) => {
                self.core.set_status(StatusMessage::info(QUICK_EXIT_HINT));
                Vec::new()
            }
            Ok(EditOutcome::Unchanged { quick_exit: false }) => Vec::new(),
            Ok(EditOutcome::LocalEdited) => {
                self.core
                    .set_status(StatusMessage::info(format!("Saved {name}")));
                self.refresh_directory_of(&location)
            }
            Ok(EditOutcome::Uploaded) => {
                self.core.set_status(StatusMessage::info(format!(
                    "Uploaded {name} to {}",
                    location.endpoint
                )));
                self.refresh_directory_of(&location)
            }
            Ok(EditOutcome::RemoteChanged { temp }) => {
                let message = format!(
                    "{name} changed on the server while you edited it. Overwrite replaces \
                     the server's version with your edits; Cancel discards your edits \
                     (kept in {} until you answer).",
                    temp.display()
                );
                self.push_edit_dialog(
                    DialogPurpose::EditConflict {
                        location: location.clone(),
                        temp,
                    },
                    "File changed on the server",
                    message,
                    vec![DialogButton::Overwrite, DialogButton::Cancel],
                );
                self.edit.active.push(location);
                Vec::new()
            }
            Ok(EditOutcome::UploadFailed { temp, error }) => {
                let message = format!(
                    "Uploading {name} to {} failed: {error}. Your edits are kept in {} \
                     until scpto exits. Retry uploads them again; Cancel leaves them there.",
                    location.endpoint,
                    temp.display()
                );
                self.push_edit_dialog(
                    DialogPurpose::EditUploadFailed {
                        location: location.clone(),
                        temp,
                    },
                    "Upload failed",
                    message,
                    vec![DialogButton::Retry, DialogButton::Cancel],
                );
                self.edit.active.push(location);
                Vec::new()
            }
            Err(error) => {
                self.core.set_status(StatusMessage::error(format!(
                    "Editing {name} failed: {error}"
                )));
                Vec::new()
            }
        }
    }

    /// Answers of `DialogPurpose::{EditConflict, EditUploadFailed}`.
    pub fn edit_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect> {
        match (answer.purpose, answer.button) {
            (DialogPurpose::EditConflict { location, temp }, button) => {
                vec![Effect::ResolveEditConflict {
                    location,
                    temp,
                    overwrite: button == DialogButton::Overwrite,
                }]
            }
            (DialogPurpose::EditUploadFailed { location, temp }, DialogButton::Retry) => {
                vec![Effect::ResolveEditConflict {
                    location,
                    temp,
                    overwrite: true,
                }]
            }
            (DialogPurpose::EditUploadFailed { location, temp }, _) => {
                self.edit.active.retain(|active| *active != location);
                self.core.set_status(StatusMessage::info(format!(
                    "Your edits of {} are kept in {} until scpto exits",
                    file_name(&location.path),
                    temp.display()
                )));
                Vec::new()
            }
            // Core routes only the two edit purposes here.
            _ => Vec::new(),
        }
    }

    /// Starts editing the cursor entry of `side` when it is a file.
    fn edit_cursor_entry(&mut self, side: PaneSide) -> Vec<Effect> {
        let pane = self.core.pane(side);
        let (Some(entry), Some(path)) = (pane.cursor_entry(), pane.path_of(pane.cursor)) else {
            return Vec::new();
        };
        let editable = match &entry.kind {
            EntryKind::File => true,
            EntryKind::Symlink { target_is_dir, .. } => !target_is_dir,
            EntryKind::Dir | EntryKind::Special(_) => false,
        };
        let location = Location {
            endpoint: pane.endpoint.clone(),
            path,
        };
        let name = file_name(&location.path).into_owned();
        if !editable {
            self.core.set_status(StatusMessage::info(format!(
                "Only files can be edited; {name} is not a regular file"
            )));
            return Vec::new();
        }
        if self.edit.active.contains(&location) {
            self.core.set_status(StatusMessage::info(format!(
                "{name} is already being edited"
            )));
            return Vec::new();
        }
        self.edit.active.push(location.clone());
        vec![Effect::Edit(location)]
    }

    /// Refreshes every pane showing the directory that holds `file`.
    fn refresh_directory_of(&mut self, file: &Location) -> Vec<Effect> {
        let directory = file.path.parent().map(|parent| Location {
            endpoint: file.endpoint.clone(),
            path: parent.to_path_buf(),
        });
        let sides: Vec<PaneSide> = [PaneSide::Left, PaneSide::Right]
            .into_iter()
            .filter(|&side| directory.is_some() && self.core.pane(side).location() == directory)
            .collect();
        sides
            .into_iter()
            .flat_map(|side| self.refresh(side))
            .collect()
    }

    /// Opens a buttons-only edit dialog with focus on the first button.
    fn push_edit_dialog(
        &mut self,
        purpose: DialogPurpose,
        title: &str,
        message: String,
        buttons: Vec<DialogButton>,
    ) {
        self.dialog.stack.push(Dialog {
            purpose,
            title: title.to_string(),
            message,
            field: None,
            checkbox: None,
            buttons,
            focus: DialogFocus::Button(0),
        });
    }
}

/// The last path component as shown in messages.
fn file_name(path: &Path) -> Cow<'_, str> {
    path.file_name()
        .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::error::FsError;
    use crate::groups::Groups;
    use crate::hosts::HostList;
    use crate::preview::Background;
    use crate::settings::Settings;
    use crate::state::PersistedState;
    use crate::ui::action::{BgEvent, CursorMove};
    use crate::vfs::{Endpoint, Entry, SpecialKind};

    fn web() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        }
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

    fn at(endpoint: Endpoint, path: &str) -> Location {
        Location {
            endpoint,
            path: path.into(),
        }
    }

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

    /// Entries of every kind, sorted by name as the pane shows them.
    fn entries() -> Vec<Entry> {
        vec![
            entry("a.txt", EntryKind::File),
            entry(
                "b-link",
                EntryKind::Symlink {
                    target: "a.txt".into(),
                    target_is_dir: false,
                },
            ),
            entry(
                "c-dirlink",
                EntryKind::Symlink {
                    target: "/tmp".into(),
                    target_is_dir: true,
                },
            ),
            entry("d-fifo", EntryKind::Special(SpecialKind::Fifo)),
            entry("sub", EntryKind::Dir),
        ]
    }

    fn build(left: &str, right: (Endpoint, &str)) -> (App, Vec<Effect>) {
        App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (Endpoint::Local, Some(left.into())),
            Some((right.0, Some(right.1.into()))),
        )
    }

    /// Left pane local at `/l`, right pane remote `web` at `/srv`, both
    /// listed with [`entries`]; the left pane has focus.
    fn app() -> App {
        let (mut app, effects) = build("/l", (web(), "/srv"));
        app.handle_action(Action::Resize {
            width: 120,
            height: 40,
        });
        let left = effects
            .iter()
            .find(|effect| matches!(effect, Effect::ListDir { .. }))
            .expect("left listing")
            .clone();
        deliver(&mut app, &left, entries());
        let connected = app.handle_event(BgEvent::Connected {
            endpoint: web(),
            home: "/home/u".into(),
        });
        deliver(&mut app, &connected[0], entries());
        app
    }

    /// Moves the focused pane's cursor to the row named `name`.
    fn point_at(app: &mut App, name: &str) {
        let side = app.core.focused();
        app.core.pane_mut(side).move_cursor(CursorMove::Home, 10);
        while app
            .core
            .pane(side)
            .cursor_entry()
            .is_none_or(|entry| entry.name != name)
        {
            let before = app.core.pane(side).cursor;
            app.core.pane_mut(side).move_cursor(CursorMove::Down, 10);
            assert_ne!(app.core.pane(side).cursor, before, "{name} is not listed");
        }
    }

    fn status(app: &App) -> (String, bool) {
        let message = app.core.status().expect("status");
        (message.text.clone(), message.error)
    }

    fn answer(purpose: DialogPurpose, button: DialogButton) -> DialogAnswer {
        DialogAnswer {
            purpose,
            button,
            field: None,
            checked: false,
        }
    }

    fn listed_locations(effects: &[Effect]) -> Vec<Location> {
        effects
            .iter()
            .map(|effect| match effect {
                Effect::ListDir { location, .. } => location.clone(),
                other => panic!("expected ListDir, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn other_actions_are_handed_back() {
        let mut app = app();
        assert_eq!(app.edit_action(Action::Copy), Err(Action::Copy));
        assert!(app.edit.active.is_empty());
    }

    #[test]
    fn edit_of_a_file_emits_the_effect_and_marks_it_active() {
        let mut app = app();
        point_at(&mut app, "a.txt");
        let file = at(Endpoint::Local, "/l/a.txt");
        assert_eq!(
            app.edit_action(Action::Edit),
            Ok(vec![Effect::Edit(file.clone())])
        );
        assert_eq!(app.edit.active, vec![file]);
    }

    #[test]
    fn edit_follows_the_focused_pane_and_file_symlinks() {
        let mut app = app();
        app.handle_action(Action::SwitchFocus);
        point_at(&mut app, "b-link");
        let file = at(web(), "/srv/b-link");
        assert_eq!(
            app.edit_action(Action::Edit),
            Ok(vec![Effect::Edit(file.clone())])
        );
        assert_eq!(app.edit.active, vec![file]);
    }

    #[test]
    fn directories_and_special_files_are_refused_with_a_status() {
        for name in ["sub", "c-dirlink", "d-fifo"] {
            let mut app = app();
            point_at(&mut app, name);
            assert_eq!(app.edit_action(Action::Edit), Ok(Vec::new()), "{name}");
            assert!(app.edit.active.is_empty(), "{name}");
            let (text, error) = status(&app);
            assert!(text.contains(name) && text.contains("Only files"), "{text}");
            assert!(!error);
        }
    }

    #[test]
    fn a_second_edit_of_an_active_file_is_a_status() {
        let mut app = app();
        point_at(&mut app, "a.txt");
        app.edit_action(Action::Edit).expect("claimed");
        assert_eq!(app.edit_action(Action::Edit), Ok(Vec::new()));
        assert_eq!(app.edit.active.len(), 1);
        assert_eq!(status(&app).0, "a.txt is already being edited");
    }

    #[test]
    fn edit_in_an_empty_pane_does_nothing() {
        let (mut app, effects) = build("/l", (Endpoint::Local, "/r"));
        deliver(&mut app, &effects[0], Vec::new());
        assert_eq!(app.edit_action(Action::Edit), Ok(Vec::new()));
        assert!(app.edit.active.is_empty());
        assert!(app.core.status().is_none());
    }

    #[test]
    fn quick_exit_shows_the_gui_editor_hint() {
        let mut app = app();
        let file = at(Endpoint::Local, "/l/a.txt");
        app.edit.active.push(file.clone());
        let effects = app.edit_finished(file, Ok(EditOutcome::Unchanged { quick_exit: true }));
        assert!(effects.is_empty());
        assert!(app.edit.active.is_empty());
        assert_eq!(status(&app), (QUICK_EXIT_HINT.to_string(), false));
    }

    #[test]
    fn unchanged_without_quick_exit_is_silent() {
        let mut app = app();
        app.core.set_status(StatusMessage::info("before"));
        let file = at(web(), "/srv/a.txt");
        app.edit.active.push(file.clone());
        let effects = app.edit_finished(file, Ok(EditOutcome::Unchanged { quick_exit: false }));
        assert!(effects.is_empty());
        assert!(app.edit.active.is_empty());
        assert_eq!(status(&app).0, "before");
        assert!(app.dialog.stack.is_empty());
    }

    #[test]
    fn upload_refreshes_every_pane_showing_the_directory() {
        let mut app = app();
        let file = at(web(), "/srv/a.txt");
        app.edit.active.push(file.clone());
        let effects = app.edit_finished(file, Ok(EditOutcome::Uploaded));
        assert_eq!(listed_locations(&effects), vec![at(web(), "/srv")]);
        assert!(app.edit.active.is_empty());
        assert_eq!(status(&app), ("Uploaded a.txt to web".to_string(), false));
    }

    #[test]
    fn local_edit_refreshes_both_panes_on_the_same_directory() {
        let (mut app, effects) = build("/l", (Endpoint::Local, "/l"));
        deliver(&mut app, &effects[0], entries());
        deliver(&mut app, &effects[1], entries());
        let effects = app.edit_finished(
            at(Endpoint::Local, "/l/a.txt"),
            Ok(EditOutcome::LocalEdited),
        );
        assert_eq!(
            listed_locations(&effects),
            vec![at(Endpoint::Local, "/l"), at(Endpoint::Local, "/l")]
        );
        assert_eq!(status(&app).0, "Saved a.txt");
    }

    #[test]
    fn an_edit_outside_the_shown_directories_refreshes_nothing() {
        let mut app = app();
        let effects = app.edit_finished(at(web(), "/etc/hosts"), Ok(EditOutcome::Uploaded));
        assert!(effects.is_empty());
        // Same path as the left pane's directory, but another endpoint.
        let effects = app.edit_finished(at(web(), "/l/a.txt"), Ok(EditOutcome::Uploaded));
        assert!(effects.is_empty());
    }

    #[test]
    fn remote_change_opens_the_conflict_dialog_and_stays_active() {
        let mut app = app();
        let file = at(web(), "/srv/a.txt");
        let temp = PathBuf::from("/tmp/scpto/edit-1/a.txt");
        app.edit.active.push(file.clone());
        let effects = app.edit_finished(
            file.clone(),
            Ok(EditOutcome::RemoteChanged { temp: temp.clone() }),
        );
        assert!(effects.is_empty());
        assert_eq!(app.edit.active, vec![file.clone()]);
        let dialog = app.dialog.stack.last().expect("dialog");
        assert_eq!(
            dialog.purpose,
            DialogPurpose::EditConflict {
                location: file,
                temp
            }
        );
        assert_eq!(
            dialog.buttons,
            vec![DialogButton::Overwrite, DialogButton::Cancel]
        );
        assert_eq!(dialog.focus, DialogFocus::Button(0));
        assert!(dialog.field.is_none() && dialog.checkbox.is_none());
        assert!(dialog
            .message
            .starts_with("a.txt changed on the server while you edited it"));
        assert!(dialog.message.contains("/tmp/scpto/edit-1/a.txt"));
    }

    #[test]
    fn upload_failure_shows_the_error_and_the_temp_path() {
        let mut app = app();
        let file = at(web(), "/srv/a.txt");
        let temp = PathBuf::from("/tmp/scpto/edit-2/a.txt");
        let error = Error::Fs(FsError::PermissionDenied {
            path: "/srv/a.txt".into(),
        });
        let effects = app.edit_finished(
            file.clone(),
            Ok(EditOutcome::UploadFailed {
                temp: temp.clone(),
                error,
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(app.edit.active, vec![file.clone()]);
        let dialog = app.dialog.stack.last().expect("dialog");
        assert_eq!(
            dialog.purpose,
            DialogPurpose::EditUploadFailed {
                location: file,
                temp
            }
        );
        assert_eq!(
            dialog.buttons,
            vec![DialogButton::Retry, DialogButton::Cancel]
        );
        assert!(dialog.message.contains("Permission denied: /srv/a.txt"));
        assert!(dialog
            .message
            .contains("kept in /tmp/scpto/edit-2/a.txt until scpto exits"));
    }

    #[test]
    fn a_failed_edit_is_an_error_status() {
        let mut app = app();
        let file = at(Endpoint::Local, "/l/a.txt");
        app.edit.active.push(file.clone());
        let error = Error::Fs(FsError::NotFound {
            path: "/l/a.txt".into(),
        });
        let effects = app.edit_finished(file, Err(error));
        assert!(effects.is_empty());
        assert!(app.edit.active.is_empty());
        assert_eq!(
            status(&app),
            (
                "Editing a.txt failed: /l/a.txt does not exist".to_string(),
                true
            )
        );
    }

    #[test]
    fn conflict_answers_resolve_and_keep_the_file_active() {
        for (button, overwrite) in [
            (DialogButton::Overwrite, true),
            (DialogButton::Cancel, false),
        ] {
            let mut app = app();
            let file = at(web(), "/srv/a.txt");
            let temp = PathBuf::from("/tmp/t/a.txt");
            app.edit.active.push(file.clone());
            let purpose = DialogPurpose::EditConflict {
                location: file.clone(),
                temp: temp.clone(),
            };
            assert_eq!(
                app.edit_dialog_answer(answer(purpose, button)),
                vec![Effect::ResolveEditConflict {
                    location: file.clone(),
                    temp,
                    overwrite,
                }]
            );
            assert_eq!(app.edit.active, vec![file]);
        }
    }

    #[test]
    fn upload_failure_retry_uploads_again() {
        let mut app = app();
        let file = at(web(), "/srv/a.txt");
        let temp = PathBuf::from("/tmp/t/a.txt");
        app.edit.active.push(file.clone());
        let purpose = DialogPurpose::EditUploadFailed {
            location: file.clone(),
            temp: temp.clone(),
        };
        assert_eq!(
            app.edit_dialog_answer(answer(purpose, DialogButton::Retry)),
            vec![Effect::ResolveEditConflict {
                location: file.clone(),
                temp,
                overwrite: true,
            }]
        );
        assert_eq!(app.edit.active, vec![file]);
    }

    #[test]
    fn upload_failure_cancel_keeps_the_temp_file_and_repeats_its_path() {
        let mut app = app();
        let file = at(web(), "/srv/a.txt");
        app.edit.active.push(file.clone());
        let purpose = DialogPurpose::EditUploadFailed {
            location: file,
            temp: "/tmp/t/a.txt".into(),
        };
        let cancel = answer(purpose, DialogButton::Cancel);
        assert!(app.edit_dialog_answer(cancel).is_empty());
        assert!(app.edit.active.is_empty());
        assert_eq!(
            status(&app),
            (
                "Your edits of a.txt are kept in /tmp/t/a.txt until scpto exits".to_string(),
                false
            )
        );
    }

    #[test]
    fn the_full_round_trip_through_a_conflict_ends_inactive() {
        let mut app = app();
        app.handle_action(Action::SwitchFocus);
        point_at(&mut app, "a.txt");
        let file = at(web(), "/srv/a.txt");
        assert_eq!(
            app.edit_action(Action::Edit),
            Ok(vec![Effect::Edit(file.clone())])
        );
        app.edit_finished(
            file.clone(),
            Ok(EditOutcome::RemoteChanged {
                temp: "/tmp/t/a.txt".into(),
            }),
        );
        let dialog = app.dialog.stack.pop().expect("dialog");
        let effects = app.edit_dialog_answer(answer(dialog.purpose, DialogButton::Overwrite));
        assert_eq!(effects.len(), 1);
        assert_eq!(app.edit.active, vec![file.clone()]);
        let effects = app.edit_finished(file, Ok(EditOutcome::Uploaded));
        assert_eq!(listed_locations(&effects), vec![at(web(), "/srv")]);
        assert!(app.edit.active.is_empty());
    }
}
