//! Status bar widget: messages, selection summary, transfer summary, key
//! hints.
//!
//! One row: the message (error style when `message_is_error`) on the left,
//! then the selection and transfer summaries, then key hints truncated to
//! the remaining width.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct StatusBarView<'a> {
//!     pub message: &'a str,
//!     pub message_is_error: bool,
//!     /// For example `3 selected, 12.4 MiB`.
//!     pub selection: &'a str,
//!     /// For example `2 running, 1 failed`.
//!     pub transfers: &'a str,
//!     pub hints: &'a str,
//! }
//!
//! pub fn render(view: &StatusBarView<'_>, area: Rect, buf: &mut Buffer);
//! ```
