//! Application state machine (pure logic, no I/O).
//!
//! `App` handles `Action`s and `BgEvent`s and returns `Effect`s. Its state is
//! split by feature: each feature module owns one state struct and the
//! `impl App` block that operates on it, so features are developed and
//! tested independently.
//!
//! Inputs that exist before the first frame are passed to the `App`
//! constructor rather than requested through `Effect`s: the host aliases
//! discovered from `~/.ssh/config` and the persisted state (bookmarks and
//! last directories per endpoint). Only changes to persisted state go out as
//! effects (`RememberDirectory`, `SaveBookmark`, `RemoveBookmark`).

pub mod connection;
pub mod core;
pub mod dialog;
pub mod edit;
pub mod preview;
pub mod sync;
pub mod transfer;

/// The whole application state. It deliberately derives nothing, so feature
/// state structs are free to hold non-`Debug` values such as image protocols.
pub struct App {
    /// Panes, focus, layout, input mode, status.
    pub core: core::CoreState,
    /// Endpoint connection status and prompts.
    pub connection: connection::ConnectionState,
    /// Queue mirror, drag, and conflicts.
    pub transfer: transfer::TransferState,
    /// Modal dialogs and the bookmark list.
    pub dialog: dialog::DialogState,
    /// Directory comparison view.
    pub sync: sync::SyncState,
    /// Edit sessions.
    pub edit: edit::EditState,
    /// Previews and quick view.
    pub preview: preview::PreviewState,
}
