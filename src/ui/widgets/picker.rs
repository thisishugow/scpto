//! Filterable list widget for the host picker (grouped, with Local) and
//! the bookmark list.
//!
//! A centered popup: title, a tab row (host picker: `All`, each sshto group,
//! `Other`; selected with digit keys like sshto; omitted when empty), the
//! filter line, and the rows (label, then dimmed detail such as
//! `user@hostname` once resolved).
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct PickerView<'a> {
//!     pub title: &'a str,
//!     pub filter: &'a str,
//!     pub tabs: &'a [&'a str],
//!     pub active_tab: usize,
//!     /// The rows of the active tab after filtering, in display order.
//!     pub rows: &'a [PickerRow<'a>],
//!     pub cursor: usize,
//!     pub first_visible: usize,
//! }
//!
//! pub struct PickerRow<'a> {
//!     pub label: &'a str,
//!     pub detail: &'a str,
//! }
//!
//! /// Draws the popup centered in `screen` and returns its hit data
//! /// (`first_visible` and `row_count` as drawn).
//! pub fn render(view: &PickerView<'_>, screen: Rect, buf: &mut Buffer) -> ListHit;
//! ```
