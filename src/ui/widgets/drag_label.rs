//! Floating drag label (for example "⇢ 3 items") drawn next to the pointer.
//!
//! Drawn one cell right of the pointer, moved left or up as needed to stay
//! inside the screen; "1 item" in the singular.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct DragLabelView {
//!     pub count: usize,
//!     pub column: u16,
//!     pub row: u16,
//! }
//!
//! pub fn render(view: &DragLabelView, screen: Rect, buf: &mut Buffer);
//! ```
