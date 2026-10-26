//! Help screen listing key and mouse bindings.
//!
//! The content is static (plan 2.4 key table, mouse gestures, and the
//! Terminal.app / iTerm2 modifier notes) and scrolls by line.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct HelpView {
//!     /// First line shown; clamped to the content.
//!     pub scroll: usize,
//! }
//!
//! /// Number of content lines, for scroll clamping by the app.
//! pub fn line_count() -> usize;
//!
//! pub fn render(view: &HelpView, area: Rect, buf: &mut Buffer);
//! ```
