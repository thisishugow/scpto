//! Queue panel widget: jobs and items with state, progress, and clickable
//! controls.
//!
//! A bordered panel; each job row is followed by its item rows (indented).
//! Each row shows label, state, a progress gauge or an indeterminate marker,
//! and its control buttons at the right edge, drawn in the order given and
//! recorded in `QueueHit::buttons` in that same order.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct QueueView<'a> {
//!     pub rows: &'a [QueueRowView<'a>],
//!     pub cursor: Option<usize>,
//!     pub first_visible: usize,
//!     pub focused: bool,
//! }
//!
//! pub struct QueueRowView<'a> {
//!     pub target: QueueTarget,
//!     /// Item rows are indented under their job.
//!     pub indent: bool,
//!     /// For example `copy 3 items -> web:/srv` or `tar (no resume)`.
//!     pub label: &'a str,
//!     pub state: &'a str,
//!     pub progress: Progress,
//!     pub controls: &'a [QueueControl],
//! }
//!
//! pub enum Progress {
//!     None,
//!     Bytes { done: u64, total: u64 },
//!     /// rsync syncs.
//!     Indeterminate,
//! }
//!
//! pub fn render(view: &QueueView<'_>, area: Rect, buf: &mut Buffer) -> QueueHit;
//! ```
