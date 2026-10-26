//! File pane widget: title with endpoint (and `[sudo]` / reconnect status),
//! path, rows with selection and drop-target highlight, filter line.
//!
//! Layout inside `area`: a bordered block; the top border carries the
//! endpoint (`Endpoint` display, so sudo shows `[sudo]`) and `status`; the
//! first inner row is the current path (left-truncated with an ellipsis);
//! the rest are entry rows (name, size, modified time); the bottom border
//! shows the filter (`/text`) while one is set, else the item and
//! selection counts. The cursor row is highlighted only when `focused`;
//! selected rows are marked; enterable rows end in `/`.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! /// Rows of `area` that are not list rows: top border, path line, bottom
//! /// border. `CoreState::page_height` subtracts the same number.
//! pub const CHROME_ROWS: u16 = 3;
//!
//! pub struct PaneView<'a> {
//!     pub side: PaneSide,
//!     /// Read through its public fields (`visible`, `entries`, `cursor`,
//!     /// `scroll`, `selection`, `filter`, `cwd`, `endpoint`).
//!     pub pane: &'a Pane,
//!     pub focused: bool,
//!     /// Connection status for the title, for example `connecting` or
//!     /// `reconnecting in 4 s`.
//!     pub status: Option<&'a str>,
//!     /// The current drag's target when it is this pane:
//!     /// `DropTarget::Pane` highlights the border, `DropTarget::Folder`
//!     /// the row.
//!     pub drop_target: Option<DropTarget>,
//! }
//!
//! /// Draws the pane, scrolled to `pane.scroll`, and returns its hit data.
//! pub fn render(view: &PaneView<'_>, area: Rect, buf: &mut Buffer) -> PaneHit;
//! ```
