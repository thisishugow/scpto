//! Frame composition: lays out panes, status bar, queue panel, dialogs,
//! previews, and the drag label from widgets, and records the layout used
//! for mouse hit testing.
//!
//! Render maps App state to each widget's borrowed view struct; widgets
//! never see the `App`. Areas come from `CoreState::layout`, so paging keys
//! and the screen agree. Layers are drawn bottom-up (panes or quick view,
//! queue panel, comparison view, full-screen viewer, picker, dialog, drag
//! label) and recorded in the returned `HitMap` as drawn.
//!
//! # Public API (Wave 3, integration owner)
//!
//! ```text
//! /// Draws one frame. `&mut` because image previews render statefully.
//! pub fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) -> HitMap;
//! ```
