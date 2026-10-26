//! Single-line text input widget with caret and optional masking.
//!
//! The visible window scrolls horizontally to keep the caret in view. A
//! masked field draws one `*` per character and never copies the value
//! (the sudo password lives only in its `Zeroizing` buffer).
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct TextInputView<'a> {
//!     pub value: &'a str,
//!     /// Caret position in characters, `0..=value.chars().count()`.
//!     pub caret: usize,
//!     pub masked: bool,
//!     pub focused: bool,
//! }
//!
//! /// Draws one row; returns the caret's screen position when focused.
//! pub fn render(view: &TextInputView<'_>, area: Rect, buf: &mut Buffer) -> Option<Position>;
//! ```
