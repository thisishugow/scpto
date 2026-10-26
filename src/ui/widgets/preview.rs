//! Preview widget: highlighted text, rendered Markdown, binary summary, or
//! image, full screen or as quick view.
//!
//! A bordered block titled with the file name (plus `truncated` when the
//! text was cut, and `raw` / `rendered` for Markdown). Text scrolls by
//! line; binary files show size, type, and mode; images render through the
//! `ThreadProtocol` (resized in the background).
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct PreviewView<'a> {
//!     pub title: &'a str,
//!     pub body: PreviewBody<'a>,
//!     pub scroll: usize,
//!     pub truncated: bool,
//! }
//!
//! pub enum PreviewBody<'a> {
//!     Loading,
//!     /// Highlighted or rendered text, produced by `preview::Highlighter`
//!     /// or `preview::render_markdown`.
//!     Text(&'a Text<'static>),
//!     Binary(&'a Entry),
//!     Image(&'a mut ThreadProtocol),
//!     Failed(&'a str),
//! }
//!
//! pub fn render(view: PreviewView<'_>, area: Rect, buf: &mut Buffer);
//! ```
