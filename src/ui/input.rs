//! Terminal events to `Action`s: key mapping per `InputMode`, the mouse
//! state machine (double click within 450 ms on the same row, drag after
//! moving at least one cell, drop and cancel), hit testing against the last
//! rendered layout, and bracketed paste.
//!
//! # Contract
//!
//! - Keys follow plan 2.4 in `InputMode::Browse` and the per-mode rules in
//!   `ui::action::InputMode`. Only key press and repeat events are mapped;
//!   releases and unmapped keys produce no action.
//! - Mouse, resolved only through `ui::hit::HitMap` of the last frame:
//!   - left press: a queue button (`HitMap::queue_button`) first becomes
//!     `Action::Queue { target: Some(..) }`; otherwise `HitMap::resolve`.
//!     A press on the same `ClickTarget` as the previous press within
//!     `DOUBLE_CLICK` becomes `Action::DoubleClick`, else `Action::Click`
//!     with modifiers (Ctrl or Super → toggle, Shift → range).
//!   - drag with the button held: once the pointer is at least one cell
//!     away from a press on a `ClickTarget::PaneRow`, emit `DragStart`
//!     once, then `DragMove` (with `HitMap::drop_target`) whenever the cell
//!     changes. Release while dragging → `Drop`; Esc while dragging →
//!     `DragCancel` (key handling is bypassed for that key).
//!   - wheel: `Scroll` with `HitMap::scroll_region`, 3 lines per notch.
//!   - moves without a held button produce nothing.
//! - Paste: `Event::Paste(text)` → `Action::Paste(text)` unchanged; the app
//!   decides by mode, using `split_dropped_paths` for Finder drops.
//! - Resize → `Action::Resize`.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! /// Maximum interval between two presses of a double click.
//! pub const DOUBLE_CLICK: Duration = Duration::from_millis(450);
//!
//! /// Mouse state between events (last press, drag in progress).
//! #[derive(Debug, Default)]
//! pub struct InputState { /* private */ }
//!
//! impl InputState {
//!     pub fn new() -> InputState;
//!     /// Maps one event; `now` is when it was read.
//!     pub fn handle(&mut self, event: &Event, mode: InputMode, hits: &HitMap, now: Instant) -> Option<Action>;
//! }
//!
//! /// Finder drop: splits pasted text with `shell-words` (a trailing
//! /// newline is ignored). `Some` only when it yields at least one token and
//! /// every token is an absolute path; existence is checked later by
//! /// `Effect::CheckDroppedPaths`.
//! pub fn split_dropped_paths(text: &str) -> Option<Vec<PathBuf>>;
//! ```
//!
//! `Event` is `ratatui::crossterm::event::Event`.
