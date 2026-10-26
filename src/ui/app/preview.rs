//! Previews in the app: full-screen preview and quick view, debounced
//! loading with cancellation of superseded requests, scrolling, and the
//! rendered/raw Markdown toggle.

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct PreviewState {}
