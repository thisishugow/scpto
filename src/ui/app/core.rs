//! Core of the app state machine: construction, `Action` and `BgEvent`
//! dispatch, both panes, focus, layout size, input mode, status messages,
//! navigation, selection, filter, sort, hidden files, and quit.

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct CoreState {}
