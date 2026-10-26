//! Directory comparison view widget.
//!
//! A bordered list titled with both directories; each row shows the
//! selection mark, the relative path, the status (`<-`, `->`, `new left`,
//! `new right`, `size`, `kind`, `=`), and both sides' size and mtime.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct SyncListView<'a> {
//!     pub comparison: &'a Comparison,
//!     /// Indices into `comparison.entries` shown, in order.
//!     pub visible: &'a [usize],
//!     /// One flag per `comparison.entries` element.
//!     pub selected: &'a [bool],
//!     pub cursor: usize,
//!     pub first_visible: usize,
//! }
//!
//! pub fn render(view: &SyncListView<'_>, area: Rect, buf: &mut Buffer) -> ListHit;
//! ```
