//! Dialogs in the app: the modal stack (confirmations, text fields such as
//! rename, mkdir, chmod, and bookmark, delete and trash-failed prompts),
//! focus and hotkeys, and the bookmark list.

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct DialogState {}
