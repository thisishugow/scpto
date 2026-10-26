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
//! # Details
//!
//! - Letters are matched by the character the terminal reports, so Shift is
//!   ignored for them (`Q` arrives as `Q` with Shift). Keys held with Alt,
//!   Super, Hyper, or Meta are unmapped. Named keys (arrows, F-keys) accept
//!   Shift only; Ctrl combinations are listed explicitly below.
//! - A double click needs two unmodified presses; a press with Ctrl, Super,
//!   or Shift is always a `Click` and resets the double-click timer, and so
//!   does the second press of a double click (a third press is a `Click`).
//! - The first `DragMove` follows the first motion after `DragStart`, since
//!   `DragStart` carries no pointer position.
//! - A left press or a buttonless move while dragging means the terminal
//!   lost the release (for example, released outside the window): it ends
//!   the drag with `DragCancel` and is otherwise consumed.
//!
//! # Key table
//!
//! `Browse` (plan 2.4): Tab `SwitchFocus`; Up/`k`, Down/`j`, PgUp, PgDn,
//! Home/`g`, End/`G` `MoveCursor`; Enter/Right/`l` `Open`; Backspace/Left/`h`
//! `Parent`; Space/Insert `ToggleSelect`; Ctrl+A `SelectAll`; `*`
//! `InvertSelection`; F5/`c` `Copy`; F6/`m` `Move`; F7/`n` `MakeDir`;
//! F8/Delete/`d` `Delete`; F2/`r` `Rename`; F4/`e` `Edit`; F3/`v` `Preview`;
//! `i` `ToggleQuickView`; `p` `Chmod`; `R` `Reveal`; `/` `StartFilter`; `.`
//! `ToggleHidden`; `s` `CycleSort`; `o` `OpenEndpointPicker`; `S`
//! `ReconnectWithSudo`; `b` `ShowBookmarks`; `B` `AddBookmark`; `=`
//! `Compare`; `t` `OpenShell`; F9/`Q` `ToggleQueue`; `?`/F1 `Help`;
//! `q`/F10/Ctrl+C `Quit`; Esc `Cancel`.
//!
//! Text editing, shared by `Filter`, `TextField`, and `Picker`: characters
//! `Text(Insert)`; Backspace `Text(Backspace)`; Delete `Text(DeleteForward)`
//! (not in `Picker`); Ctrl+W `Text(DeleteWord)`; Ctrl+U `Text(Clear)`;
//! Left/Right `Text(Left/Right)`; Home/Ctrl+A `Text(Home)`; End/Ctrl+E
//! `Text(End)`; Enter `Confirm`; Esc/Ctrl+C `Cancel`. On top of that:
//!
//! - `Filter`: Up, Down, PgUp, PgDn `MoveCursor` in the filtered list.
//! - `TextField`: Tab/Down `DialogFocus(Next)`, Shift+Tab/Up
//!   `DialogFocus(Previous)`.
//! - `Picker`: Up/Ctrl+P, Down/Ctrl+N, PgUp, PgDn `MoveCursor`; Delete
//!   `PickerRemove`.
//!
//! `Dialog`: Tab/Right/Down `DialogFocus(Next)`; Shift+Tab/Left/Up
//! `DialogFocus(Previous)`; Space `ToggleOption`; Enter `Confirm`;
//! Esc/Ctrl+C `Cancel`; letters `DialogHotkey` with the letter as typed.
//!
//! `Viewer`: the `Browse` movement keys plus Space (`PageDown`) `MoveCursor`;
//! `m` `ToggleMarkdown`; Esc/`q`/Ctrl+C `Cancel`.
//!
//! `Queue`: the `Browse` movement keys `MoveCursor`; Space/`p`
//! `TogglePause`; `c`/Delete `Cancel`; `r` `Retry`; `d` `DiscardPartial`;
//! `R` `RetryAllFailed`; `C` `ClearFinished` (all `Queue { target: None }`);
//! Tab `SwitchFocus`; Esc `Cancel`; F9/`Q` `ToggleQueue`; `?`/F1 `Help`;
//! `q`/F10/Ctrl+C `Quit`.
//!
//! `SyncView`: the `Browse` movement keys `MoveCursor`; Space/Insert
//! `ToggleSelect`; `>` `StartSync(LeftToRight)`; `<` `StartSync(RightToLeft)`;
//! `?`/F1 `Help`; Esc/`q`/Ctrl+C `Cancel`.
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

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::sync::SyncDirection;
use crate::ui::action::{
    Action, ClickModifiers, ClickTarget, CursorMove, FocusStep, InputMode, PaneSide, QueueControl,
    TextEdit,
};
use crate::ui::hit::HitMap;

/// Maximum interval between two presses of a double click.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(450);

/// Lines scrolled per mouse wheel notch.
const WHEEL_LINES: i32 = 3;

/// Mouse state between events (last press, drag in progress).
#[derive(Debug, Default)]
pub struct InputState {
    /// The last unmodified click that can become the first half of a double
    /// click.
    last_click: Option<(ClickTarget, Instant)>,
    /// What the held left button is doing.
    gesture: Gesture,
}

/// The left button's state between a press and its release.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    /// No press that can turn into a drag.
    #[default]
    Idle,
    /// Held after a press on a file row, not yet moved off its cell.
    Pressed {
        /// The pane pressed in.
        side: PaneSide,
        /// Visible index of the pressed row.
        index: usize,
        /// Column of the press.
        column: u16,
        /// Row of the press.
        row: u16,
    },
    /// Dragging; the cell last reported to the app.
    Dragging {
        /// Pointer column.
        column: u16,
        /// Pointer row.
        row: u16,
    },
}

impl InputState {
    /// A state with no press recorded.
    pub fn new() -> InputState {
        InputState::default()
    }

    /// Maps one event; `now` is when it was read.
    pub fn handle(
        &mut self,
        event: &Event,
        mode: InputMode,
        hits: &HitMap,
        now: Instant,
    ) -> Option<Action> {
        match event {
            Event::Key(key) => self.key(key, mode),
            Event::Mouse(mouse) => self.mouse(mouse, hits, now),
            Event::Paste(text) => Some(Action::Paste(text.clone())),
            Event::Resize(width, height) => Some(Action::Resize {
                width: *width,
                height: *height,
            }),
            Event::FocusGained | Event::FocusLost => None,
        }
    }

    fn key(&mut self, key: &KeyEvent, mode: InputMode) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let key = Key::classify(key)?;
        if key == Key::Named(KeyCode::Esc) && matches!(self.gesture, Gesture::Dragging { .. }) {
            self.gesture = Gesture::Idle;
            return Some(Action::DragCancel);
        }
        match mode {
            InputMode::Browse => browse_key(key),
            InputMode::Filter => filter_key(key),
            InputMode::TextField => text_field_key(key),
            InputMode::Dialog => dialog_key(key),
            InputMode::Picker => picker_key(key),
            InputMode::Viewer => viewer_key(key),
            InputMode::Queue => queue_key(key),
            InputMode::SyncView => sync_view_key(key),
        }
    }

    fn mouse(&mut self, mouse: &MouseEvent, hits: &HitMap, now: Instant) -> Option<Action> {
        let (column, row) = (mouse.column, mouse.row);
        let dragging = matches!(self.gesture, Gesture::Dragging { .. });
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Moved if dragging => {
                self.gesture = Gesture::Idle;
                Some(Action::DragCancel)
            }
            MouseEventKind::Down(MouseButton::Left) => self.press(mouse, hits, now),
            MouseEventKind::Drag(MouseButton::Left) => self.drag(column, row, hits, now),
            MouseEventKind::Up(MouseButton::Left) => {
                let gesture = std::mem::take(&mut self.gesture);
                match gesture {
                    Gesture::Dragging { .. } => Some(Action::Drop {
                        target: hits.drop_target(column, row),
                        at: now,
                    }),
                    Gesture::Idle | Gesture::Pressed { .. } => None,
                }
            }
            MouseEventKind::ScrollUp => scroll(hits, column, row, -WHEEL_LINES),
            MouseEventKind::ScrollDown => scroll(hits, column, row, WHEEL_LINES),
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => None,
        }
    }

    fn press(&mut self, mouse: &MouseEvent, hits: &HitMap, now: Instant) -> Option<Action> {
        let (column, row) = (mouse.column, mouse.row);
        self.gesture = Gesture::Idle;
        if let Some((target, control)) = hits.queue_button(column, row) {
            self.last_click = None;
            return Some(Action::Queue {
                control,
                target: Some(target),
            });
        }
        let Some(target) = hits.resolve(column, row) else {
            self.last_click = None;
            return None;
        };
        if let ClickTarget::PaneRow { side, index } = target {
            self.gesture = Gesture::Pressed {
                side,
                index,
                column,
                row,
            };
        }
        let modifiers = ClickModifiers {
            toggle: mouse
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER),
            range: mouse.modifiers.contains(KeyModifiers::SHIFT),
        };
        if modifiers != ClickModifiers::default() {
            self.last_click = None;
            return Some(Action::Click {
                target,
                modifiers,
                at: now,
            });
        }
        let double = self.last_click.is_some_and(|(previous, at)| {
            previous == target && now.saturating_duration_since(at) <= DOUBLE_CLICK
        });
        if double {
            self.last_click = None;
            Some(Action::DoubleClick { target, at: now })
        } else {
            self.last_click = Some((target, now));
            Some(Action::Click {
                target,
                modifiers,
                at: now,
            })
        }
    }

    fn drag(&mut self, column: u16, row: u16, hits: &HitMap, now: Instant) -> Option<Action> {
        match self.gesture {
            Gesture::Idle => None,
            Gesture::Pressed {
                side,
                index,
                column: pressed_column,
                row: pressed_row,
            } => {
                if (column, row) == (pressed_column, pressed_row) {
                    return None;
                }
                self.gesture = Gesture::Dragging { column, row };
                self.last_click = None;
                Some(Action::DragStart {
                    side,
                    index,
                    at: now,
                })
            }
            Gesture::Dragging {
                column: last_column,
                row: last_row,
            } => {
                if (column, row) == (last_column, last_row) {
                    return None;
                }
                self.gesture = Gesture::Dragging { column, row };
                Some(Action::DragMove {
                    target: hits.drop_target(column, row),
                    column,
                    row,
                })
            }
        }
    }
}

/// Finder drop: splits pasted text with `shell-words` (a trailing newline is
/// ignored). `Some` only when it yields at least one token and every token
/// is an absolute path; existence is checked later by
/// `Effect::CheckDroppedPaths`.
pub fn split_dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    // Unbalanced quotes mean the paste is not a shell-quoted path list.
    let tokens = shell_words::split(text).ok()?;
    if tokens.is_empty() || !tokens.iter().all(|token| Path::new(token).is_absolute()) {
        return None;
    }
    Some(tokens.into_iter().map(PathBuf::from).collect())
}

fn scroll(hits: &HitMap, column: u16, row: u16, lines: i32) -> Option<Action> {
    hits.scroll_region(column, row)
        .map(|region| Action::Scroll { region, lines })
}

/// A key press reduced to what the tables match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    /// A character without Ctrl, Alt, Super, Hyper, or Meta (Shift is part
    /// of the character).
    Char(char),
    /// A character with Ctrl, lowercased.
    Ctrl(char),
    /// A non-character key without modifiers other than Shift.
    Named(KeyCode),
}

impl Key {
    fn classify(key: &KeyEvent) -> Option<Key> {
        let foreign =
            KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META;
        if key.modifiers.intersects(foreign) {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char(c) if ctrl => Some(Key::Ctrl(c.to_ascii_lowercase())),
            KeyCode::Char(c) => Some(Key::Char(c)),
            _ if ctrl => None,
            code => Some(Key::Named(code)),
        }
    }
}

/// The list movement keys of `Browse`, also used by the viewer, queue, and
/// comparison view.
fn list_move(key: Key) -> Option<CursorMove> {
    Some(match key {
        Key::Named(KeyCode::Up) | Key::Char('k') => CursorMove::Up,
        Key::Named(KeyCode::Down) | Key::Char('j') => CursorMove::Down,
        Key::Named(KeyCode::PageUp) => CursorMove::PageUp,
        Key::Named(KeyCode::PageDown) => CursorMove::PageDown,
        Key::Named(KeyCode::Home) | Key::Char('g') => CursorMove::Home,
        Key::Named(KeyCode::End) | Key::Char('G') => CursorMove::End,
        _ => return None,
    })
}

/// List movement without letters, for modes where letters are text.
fn named_move(key: Key) -> Option<CursorMove> {
    Some(match key {
        Key::Named(KeyCode::Up) => CursorMove::Up,
        Key::Named(KeyCode::Down) => CursorMove::Down,
        Key::Named(KeyCode::PageUp) => CursorMove::PageUp,
        Key::Named(KeyCode::PageDown) => CursorMove::PageDown,
        _ => return None,
    })
}

/// Text field editing shared by the filter, text fields, and pickers.
fn text_edit(key: Key) -> Option<TextEdit> {
    Some(match key {
        Key::Char(c) => TextEdit::Insert(c),
        Key::Named(KeyCode::Backspace) => TextEdit::Backspace,
        Key::Named(KeyCode::Delete) => TextEdit::DeleteForward,
        Key::Ctrl('w') => TextEdit::DeleteWord,
        Key::Ctrl('u') => TextEdit::Clear,
        Key::Named(KeyCode::Left) => TextEdit::Left,
        Key::Named(KeyCode::Right) => TextEdit::Right,
        Key::Named(KeyCode::Home) | Key::Ctrl('a') => TextEdit::Home,
        Key::Named(KeyCode::End) | Key::Ctrl('e') => TextEdit::End,
        _ => return None,
    })
}

/// Enter and Esc in modes with a text field.
fn confirm_or_cancel(key: Key) -> Option<Action> {
    match key {
        Key::Named(KeyCode::Enter) => Some(Action::Confirm),
        Key::Named(KeyCode::Esc) | Key::Ctrl('c') => Some(Action::Cancel),
        _ => None,
    }
}

fn browse_key(key: Key) -> Option<Action> {
    if let Some(step) = list_move(key) {
        return Some(Action::MoveCursor(step));
    }
    Some(match key {
        Key::Named(KeyCode::Tab) => Action::SwitchFocus,
        Key::Named(KeyCode::Enter | KeyCode::Right) | Key::Char('l') => Action::Open,
        Key::Named(KeyCode::Backspace | KeyCode::Left) | Key::Char('h') => Action::Parent,
        Key::Named(KeyCode::Insert) | Key::Char(' ') => Action::ToggleSelect,
        Key::Ctrl('a') => Action::SelectAll,
        Key::Char('*') => Action::InvertSelection,
        Key::Named(KeyCode::F(5)) | Key::Char('c') => Action::Copy,
        Key::Named(KeyCode::F(6)) | Key::Char('m') => Action::Move,
        Key::Named(KeyCode::F(7)) | Key::Char('n') => Action::MakeDir,
        Key::Named(KeyCode::F(8) | KeyCode::Delete) | Key::Char('d') => Action::Delete,
        Key::Named(KeyCode::F(2)) | Key::Char('r') => Action::Rename,
        Key::Named(KeyCode::F(4)) | Key::Char('e') => Action::Edit,
        Key::Named(KeyCode::F(3)) | Key::Char('v') => Action::Preview,
        Key::Char('i') => Action::ToggleQuickView,
        Key::Char('p') => Action::Chmod,
        Key::Char('R') => Action::Reveal,
        Key::Char('/') => Action::StartFilter,
        Key::Char('.') => Action::ToggleHidden,
        Key::Char('s') => Action::CycleSort,
        Key::Char('o') => Action::OpenEndpointPicker,
        Key::Char('S') => Action::ReconnectWithSudo,
        Key::Char('b') => Action::ShowBookmarks,
        Key::Char('B') => Action::AddBookmark,
        Key::Char('=') => Action::Compare,
        Key::Char('t') => Action::OpenShell,
        Key::Named(KeyCode::F(9)) | Key::Char('Q') => Action::ToggleQueue,
        Key::Named(KeyCode::F(1)) | Key::Char('?') => Action::Help,
        Key::Named(KeyCode::F(10)) | Key::Char('q') | Key::Ctrl('c') => Action::Quit,
        Key::Named(KeyCode::Esc) => Action::Cancel,
        _ => return None,
    })
}

fn filter_key(key: Key) -> Option<Action> {
    confirm_or_cancel(key)
        .or_else(|| named_move(key).map(Action::MoveCursor))
        .or_else(|| text_edit(key).map(Action::Text))
}

fn text_field_key(key: Key) -> Option<Action> {
    confirm_or_cancel(key)
        .or(match key {
            Key::Named(KeyCode::Tab | KeyCode::Down) => Some(Action::DialogFocus(FocusStep::Next)),
            Key::Named(KeyCode::BackTab | KeyCode::Up) => {
                Some(Action::DialogFocus(FocusStep::Previous))
            }
            _ => None,
        })
        .or_else(|| text_edit(key).map(Action::Text))
}

fn picker_key(key: Key) -> Option<Action> {
    confirm_or_cancel(key)
        .or(match key {
            Key::Named(KeyCode::Delete) => Some(Action::PickerRemove),
            Key::Ctrl('p') => Some(Action::MoveCursor(CursorMove::Up)),
            Key::Ctrl('n') => Some(Action::MoveCursor(CursorMove::Down)),
            _ => None,
        })
        .or_else(|| named_move(key).map(Action::MoveCursor))
        .or_else(|| text_edit(key).map(Action::Text))
}

fn dialog_key(key: Key) -> Option<Action> {
    Some(match key {
        Key::Named(KeyCode::Enter) => Action::Confirm,
        Key::Named(KeyCode::Esc) | Key::Ctrl('c') => Action::Cancel,
        Key::Named(KeyCode::Tab | KeyCode::Right | KeyCode::Down) => {
            Action::DialogFocus(FocusStep::Next)
        }
        Key::Named(KeyCode::BackTab | KeyCode::Left | KeyCode::Up) => {
            Action::DialogFocus(FocusStep::Previous)
        }
        Key::Char(' ') => Action::ToggleOption,
        Key::Char(c) if c.is_alphabetic() => Action::DialogHotkey(c),
        _ => return None,
    })
}

fn viewer_key(key: Key) -> Option<Action> {
    if let Some(step) = list_move(key) {
        return Some(Action::MoveCursor(step));
    }
    Some(match key {
        Key::Char(' ') => Action::MoveCursor(CursorMove::PageDown),
        Key::Char('m') => Action::ToggleMarkdown,
        Key::Named(KeyCode::Esc) | Key::Char('q') | Key::Ctrl('c') => Action::Cancel,
        _ => return None,
    })
}

fn queue_key(key: Key) -> Option<Action> {
    if let Some(step) = list_move(key) {
        return Some(Action::MoveCursor(step));
    }
    let control = match key {
        Key::Char(' ' | 'p') => QueueControl::TogglePause,
        Key::Char('c') | Key::Named(KeyCode::Delete) => QueueControl::Cancel,
        Key::Char('r') => QueueControl::Retry,
        Key::Char('d') => QueueControl::DiscardPartial,
        Key::Char('R') => QueueControl::RetryAllFailed,
        Key::Char('C') => QueueControl::ClearFinished,
        _ => {
            return Some(match key {
                Key::Named(KeyCode::Tab) => Action::SwitchFocus,
                Key::Named(KeyCode::Esc) => Action::Cancel,
                Key::Named(KeyCode::F(9)) | Key::Char('Q') => Action::ToggleQueue,
                Key::Named(KeyCode::F(1)) | Key::Char('?') => Action::Help,
                Key::Named(KeyCode::F(10)) | Key::Char('q') | Key::Ctrl('c') => Action::Quit,
                _ => return None,
            })
        }
    };
    Some(Action::Queue {
        control,
        target: None,
    })
}

fn sync_view_key(key: Key) -> Option<Action> {
    if let Some(step) = list_move(key) {
        return Some(Action::MoveCursor(step));
    }
    Some(match key {
        Key::Named(KeyCode::Insert) | Key::Char(' ') => Action::ToggleSelect,
        Key::Char('>') => Action::StartSync(SyncDirection::LeftToRight),
        Key::Char('<') => Action::StartSync(SyncDirection::RightToLeft),
        Key::Named(KeyCode::F(1)) | Key::Char('?') => Action::Help,
        Key::Named(KeyCode::Esc) | Key::Char('q') | Key::Ctrl('c') => Action::Cancel,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyEventState;
    use ratatui::layout::Rect;

    use super::*;
    use crate::transfer::model::{ItemId, JobId, QueueTarget};
    use crate::ui::action::{DialogButton, DropTarget, ScrollRegion};
    use crate::ui::hit::{DialogHit, PaneHit, QueueHit, ViewerHit, ViewerKind};

    const JOB: QueueTarget = QueueTarget::Job(JobId(1));
    const ITEM: QueueTarget = QueueTarget::Item(ItemId(2));

    /// Left pane at columns 0..40, right pane at 40..80, both 20 rows high
    /// with the list body starting at row 2; the queue panel covers rows
    /// 20..24.
    fn hits() -> HitMap {
        HitMap {
            panes: vec![
                PaneHit {
                    side: PaneSide::Left,
                    area: Rect::new(0, 0, 40, 20),
                    rows: Rect::new(0, 2, 40, 17),
                    first_visible: 0,
                    enterable: vec![true, false, false, true],
                },
                PaneHit {
                    side: PaneSide::Right,
                    area: Rect::new(40, 0, 40, 20),
                    rows: Rect::new(40, 2, 40, 17),
                    first_visible: 10,
                    enterable: vec![false, true],
                },
            ],
            queue: Some(QueueHit {
                area: Rect::new(0, 20, 80, 4),
                rows: vec![
                    (Rect::new(0, 21, 80, 1), JOB),
                    (Rect::new(0, 22, 80, 1), ITEM),
                ],
                buttons: vec![(Rect::new(70, 21, 3, 1), JOB, QueueControl::TogglePause)],
            }),
            ..HitMap::default()
        }
    }

    fn row(side: PaneSide, index: usize) -> ClickTarget {
        ClickTarget::PaneRow { side, index }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers,
        })
    }

    fn down(column: u16, row: u16) -> Event {
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            KeyModifiers::NONE,
        )
    }

    fn drag(column: u16, row: u16) -> Event {
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            KeyModifiers::NONE,
        )
    }

    fn up(column: u16, row: u16) -> Event {
        mouse(
            MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            KeyModifiers::NONE,
        )
    }

    fn key_with(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn key(code: KeyCode) -> Event {
        key_with(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> Event {
        key(KeyCode::Char(c))
    }

    fn ctrl(c: char) -> Event {
        key_with(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Maps one event in `mode` with a fresh state.
    fn map(mode: InputMode, event: Event) -> Option<Action> {
        InputState::new().handle(&event, mode, &hits(), Instant::now())
    }

    fn map_mouse(hits: &HitMap, event: Event) -> Option<Action> {
        InputState::new().handle(&event, InputMode::Browse, hits, Instant::now())
    }

    fn click(target: ClickTarget, at: Instant) -> Option<Action> {
        Some(Action::Click {
            target,
            modifiers: ClickModifiers::default(),
            at,
        })
    }

    #[test]
    fn browse_follows_the_plan_key_table() {
        use Action::*;
        let table: Vec<(Event, Action)> = vec![
            (key(KeyCode::Tab), SwitchFocus),
            (key(KeyCode::Up), MoveCursor(CursorMove::Up)),
            (ch('k'), MoveCursor(CursorMove::Up)),
            (key(KeyCode::Down), MoveCursor(CursorMove::Down)),
            (ch('j'), MoveCursor(CursorMove::Down)),
            (key(KeyCode::PageUp), MoveCursor(CursorMove::PageUp)),
            (key(KeyCode::PageDown), MoveCursor(CursorMove::PageDown)),
            (key(KeyCode::Home), MoveCursor(CursorMove::Home)),
            (ch('g'), MoveCursor(CursorMove::Home)),
            (key(KeyCode::End), MoveCursor(CursorMove::End)),
            (ch('G'), MoveCursor(CursorMove::End)),
            (key(KeyCode::Enter), Open),
            (key(KeyCode::Right), Open),
            (ch('l'), Open),
            (key(KeyCode::Backspace), Parent),
            (key(KeyCode::Left), Parent),
            (ch('h'), Parent),
            (ch(' '), ToggleSelect),
            (key(KeyCode::Insert), ToggleSelect),
            (ctrl('a'), SelectAll),
            (ch('*'), InvertSelection),
            (key(KeyCode::F(5)), Copy),
            (ch('c'), Copy),
            (key(KeyCode::F(6)), Move),
            (ch('m'), Move),
            (key(KeyCode::F(7)), MakeDir),
            (ch('n'), MakeDir),
            (key(KeyCode::F(8)), Delete),
            (key(KeyCode::Delete), Delete),
            (ch('d'), Delete),
            (key(KeyCode::F(2)), Rename),
            (ch('r'), Rename),
            (key(KeyCode::F(4)), Edit),
            (ch('e'), Edit),
            (key(KeyCode::F(3)), Preview),
            (ch('v'), Preview),
            (ch('i'), ToggleQuickView),
            (ch('p'), Chmod),
            (ch('R'), Reveal),
            (ch('/'), StartFilter),
            (ch('.'), ToggleHidden),
            (ch('s'), CycleSort),
            (ch('o'), OpenEndpointPicker),
            (ch('S'), ReconnectWithSudo),
            (ch('b'), ShowBookmarks),
            (ch('B'), AddBookmark),
            (ch('='), Compare),
            (ch('t'), OpenShell),
            (key(KeyCode::F(9)), ToggleQueue),
            (ch('Q'), ToggleQueue),
            (key(KeyCode::F(1)), Help),
            (ch('?'), Help),
            (ch('q'), Quit),
            (key(KeyCode::F(10)), Quit),
            (ctrl('c'), Quit),
            (key(KeyCode::Esc), Cancel),
        ];
        for (event, expected) in table {
            assert_eq!(
                map(InputMode::Browse, event.clone()),
                Some(expected),
                "{event:?}"
            );
        }
    }

    #[test]
    fn shift_is_part_of_the_character() {
        // Terminals report uppercase letters and shifted symbols with Shift.
        let shifted = |c| key_with(KeyCode::Char(c), KeyModifiers::SHIFT);
        let cases = [
            ('Q', Action::ToggleQueue),
            ('S', Action::ReconnectWithSudo),
            ('B', Action::AddBookmark),
            ('R', Action::Reveal),
            ('G', Action::MoveCursor(CursorMove::End)),
            ('?', Action::Help),
            ('*', Action::InvertSelection),
        ];
        for (c, expected) in cases {
            assert_eq!(map(InputMode::Browse, shifted(c)), Some(expected), "{c}");
        }
        // Ctrl+Shift+A is still Ctrl+A.
        let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        assert_eq!(
            map(InputMode::Browse, key_with(KeyCode::Char('A'), ctrl_shift)),
            Some(Action::SelectAll)
        );
        assert_eq!(
            map(
                InputMode::Browse,
                key_with(KeyCode::Down, KeyModifiers::SHIFT)
            ),
            Some(Action::MoveCursor(CursorMove::Down))
        );
    }

    #[test]
    fn foreign_modifiers_and_unknown_keys_are_unmapped() {
        for modifiers in [
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
            KeyModifiers::META,
            KeyModifiers::HYPER,
        ] {
            assert_eq!(
                map(InputMode::Browse, key_with(KeyCode::Char('q'), modifiers)),
                None
            );
            assert_eq!(
                map(
                    InputMode::TextField,
                    key_with(KeyCode::Char('x'), modifiers)
                ),
                None
            );
        }
        assert_eq!(
            map(
                InputMode::Browse,
                key_with(KeyCode::Up, KeyModifiers::CONTROL)
            ),
            None
        );
        assert_eq!(map(InputMode::Browse, ctrl('z')), None);
        assert_eq!(map(InputMode::Browse, ch('x')), None);
        assert_eq!(map(InputMode::Browse, key(KeyCode::F(11))), None);
        assert_eq!(map(InputMode::Browse, key(KeyCode::BackTab)), None);
    }

    #[test]
    fn releases_are_ignored_and_repeats_are_mapped() {
        let with_kind = |kind| {
            Event::Key(KeyEvent::new_with_kind_and_state(
                KeyCode::Down,
                KeyModifiers::NONE,
                kind,
                KeyEventState::NONE,
            ))
        };
        assert_eq!(
            map(InputMode::Browse, with_kind(KeyEventKind::Release)),
            None
        );
        assert_eq!(
            map(InputMode::Browse, with_kind(KeyEventKind::Repeat)),
            Some(Action::MoveCursor(CursorMove::Down))
        );
    }

    #[test]
    fn text_editing_in_filter_text_field_and_picker() {
        use TextEdit::*;
        let shared: Vec<(Event, Action)> = vec![
            (ch('q'), Action::Text(Insert('q'))),
            (ch('Q'), Action::Text(Insert('Q'))),
            (ch(' '), Action::Text(Insert(' '))),
            (ch('\u{e9}'), Action::Text(Insert('\u{e9}'))),
            (key(KeyCode::Backspace), Action::Text(Backspace)),
            (ctrl('w'), Action::Text(DeleteWord)),
            (ctrl('u'), Action::Text(Clear)),
            (key(KeyCode::Left), Action::Text(Left)),
            (key(KeyCode::Right), Action::Text(Right)),
            (key(KeyCode::Home), Action::Text(Home)),
            (ctrl('a'), Action::Text(Home)),
            (key(KeyCode::End), Action::Text(End)),
            (ctrl('e'), Action::Text(End)),
            (key(KeyCode::Enter), Action::Confirm),
            (key(KeyCode::Esc), Action::Cancel),
            (ctrl('c'), Action::Cancel),
        ];
        for mode in [InputMode::Filter, InputMode::TextField, InputMode::Picker] {
            for (event, expected) in &shared {
                assert_eq!(
                    map(mode, event.clone()),
                    Some(expected.clone()),
                    "{mode:?} {event:?}"
                );
            }
            assert_eq!(map(mode, key(KeyCode::F(5))), None, "{mode:?}");
        }
    }

    #[test]
    fn filter_moves_the_list_with_arrows() {
        let mode = InputMode::Filter;
        let cases = [
            (KeyCode::Up, CursorMove::Up),
            (KeyCode::Down, CursorMove::Down),
            (KeyCode::PageUp, CursorMove::PageUp),
            (KeyCode::PageDown, CursorMove::PageDown),
        ];
        for (code, step) in cases {
            assert_eq!(map(mode, key(code)), Some(Action::MoveCursor(step)));
        }
        assert_eq!(
            map(mode, key(KeyCode::Delete)),
            Some(Action::Text(TextEdit::DeleteForward))
        );
        assert_eq!(map(mode, key(KeyCode::Tab)), None);
    }

    #[test]
    fn text_field_moves_focus() {
        let mode = InputMode::TextField;
        let next = Some(Action::DialogFocus(FocusStep::Next));
        let previous = Some(Action::DialogFocus(FocusStep::Previous));
        assert_eq!(map(mode, key(KeyCode::Tab)), next);
        assert_eq!(map(mode, key(KeyCode::Down)), next);
        assert_eq!(
            map(mode, key_with(KeyCode::BackTab, KeyModifiers::SHIFT)),
            previous
        );
        assert_eq!(map(mode, key(KeyCode::Up)), previous);
        assert_eq!(
            map(mode, key(KeyCode::Delete)),
            Some(Action::Text(TextEdit::DeleteForward))
        );
    }

    #[test]
    fn picker_moves_and_removes() {
        let mode = InputMode::Picker;
        let cases = [
            (key(KeyCode::Up), CursorMove::Up),
            (ctrl('p'), CursorMove::Up),
            (key(KeyCode::Down), CursorMove::Down),
            (ctrl('n'), CursorMove::Down),
            (key(KeyCode::PageUp), CursorMove::PageUp),
            (key(KeyCode::PageDown), CursorMove::PageDown),
        ];
        for (event, step) in cases {
            assert_eq!(map(mode, event), Some(Action::MoveCursor(step)));
        }
        assert_eq!(map(mode, key(KeyCode::Delete)), Some(Action::PickerRemove));
        // Digits and letters are filter text; the picker decides whether a
        // digit picks a tab.
        assert_eq!(
            map(mode, ch('2')),
            Some(Action::Text(TextEdit::Insert('2')))
        );
        assert_eq!(
            map(mode, ch('j')),
            Some(Action::Text(TextEdit::Insert('j')))
        );
    }

    #[test]
    fn dialog_keys() {
        let mode = InputMode::Dialog;
        let next = Some(Action::DialogFocus(FocusStep::Next));
        let previous = Some(Action::DialogFocus(FocusStep::Previous));
        for code in [KeyCode::Tab, KeyCode::Right, KeyCode::Down] {
            assert_eq!(map(mode, key(code)), next, "{code:?}");
        }
        for code in [KeyCode::BackTab, KeyCode::Left, KeyCode::Up] {
            assert_eq!(map(mode, key(code)), previous, "{code:?}");
        }
        assert_eq!(map(mode, ch(' ')), Some(Action::ToggleOption));
        assert_eq!(map(mode, key(KeyCode::Enter)), Some(Action::Confirm));
        assert_eq!(map(mode, key(KeyCode::Esc)), Some(Action::Cancel));
        assert_eq!(map(mode, ctrl('c')), Some(Action::Cancel));
        assert_eq!(map(mode, ch('y')), Some(Action::DialogHotkey('y')));
        assert_eq!(map(mode, ch('O')), Some(Action::DialogHotkey('O')));
        assert_eq!(map(mode, ch('1')), None);
        assert_eq!(map(mode, ch('?')), None);
        assert_eq!(map(mode, key(KeyCode::F(10))), None);
    }

    #[test]
    fn viewer_keys() {
        let mode = InputMode::Viewer;
        let moves = [
            (ch('j'), CursorMove::Down),
            (ch('k'), CursorMove::Up),
            (key(KeyCode::PageUp), CursorMove::PageUp),
            (ch(' '), CursorMove::PageDown),
            (ch('g'), CursorMove::Home),
            (ch('G'), CursorMove::End),
        ];
        for (event, step) in moves {
            assert_eq!(map(mode, event), Some(Action::MoveCursor(step)));
        }
        assert_eq!(map(mode, ch('m')), Some(Action::ToggleMarkdown));
        assert_eq!(map(mode, ch('q')), Some(Action::Cancel));
        assert_eq!(map(mode, key(KeyCode::Esc)), Some(Action::Cancel));
        assert_eq!(map(mode, ctrl('c')), Some(Action::Cancel));
        assert_eq!(map(mode, key(KeyCode::Enter)), None);
        assert_eq!(map(mode, ch('c')), None);
    }

    #[test]
    fn queue_keys_control_the_selected_entry() {
        let mode = InputMode::Queue;
        let controls = [
            (ch(' '), QueueControl::TogglePause),
            (ch('p'), QueueControl::TogglePause),
            (ch('c'), QueueControl::Cancel),
            (key(KeyCode::Delete), QueueControl::Cancel),
            (ch('r'), QueueControl::Retry),
            (ch('d'), QueueControl::DiscardPartial),
            (ch('R'), QueueControl::RetryAllFailed),
            (ch('C'), QueueControl::ClearFinished),
        ];
        for (event, control) in controls {
            assert_eq!(
                map(mode, event.clone()),
                Some(Action::Queue {
                    control,
                    target: None
                }),
                "{event:?}"
            );
        }
        let others = [
            (ch('j'), Action::MoveCursor(CursorMove::Down)),
            (key(KeyCode::Up), Action::MoveCursor(CursorMove::Up)),
            (key(KeyCode::Tab), Action::SwitchFocus),
            (key(KeyCode::Esc), Action::Cancel),
            (ch('Q'), Action::ToggleQueue),
            (key(KeyCode::F(9)), Action::ToggleQueue),
            (ch('?'), Action::Help),
            (ch('q'), Action::Quit),
            (key(KeyCode::F(10)), Action::Quit),
            (ctrl('c'), Action::Quit),
        ];
        for (event, expected) in others {
            assert_eq!(map(mode, event.clone()), Some(expected), "{event:?}");
        }
        assert_eq!(map(mode, ch('m')), None);
        assert_eq!(map(mode, key(KeyCode::Enter)), None);
    }

    #[test]
    fn sync_view_keys() {
        let mode = InputMode::SyncView;
        let cases = [
            (ch('j'), Action::MoveCursor(CursorMove::Down)),
            (key(KeyCode::End), Action::MoveCursor(CursorMove::End)),
            (ch(' '), Action::ToggleSelect),
            (key(KeyCode::Insert), Action::ToggleSelect),
            (ch('>'), Action::StartSync(SyncDirection::LeftToRight)),
            (ch('<'), Action::StartSync(SyncDirection::RightToLeft)),
            (ch('?'), Action::Help),
            (key(KeyCode::Esc), Action::Cancel),
            (ch('q'), Action::Cancel),
            (ctrl('c'), Action::Cancel),
        ];
        for (event, expected) in cases {
            assert_eq!(map(mode, event.clone()), Some(expected), "{event:?}");
        }
        // Arrows never start a sync by accident.
        assert_eq!(map(mode, key(KeyCode::Right)), None);
        assert_eq!(map(mode, key(KeyCode::Left)), None);
    }

    #[test]
    fn click_then_double_click_on_the_same_row() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        let target = row(PaneSide::Left, 1);
        assert_eq!(
            state.handle(&down(5, 3), mode, &hits, t0),
            click(target, t0)
        );
        assert_eq!(state.handle(&up(5, 3), mode, &hits, t0), None);
        // Another column of the same row is the same target.
        let t1 = t0 + DOUBLE_CLICK;
        assert_eq!(
            state.handle(&down(6, 3), mode, &hits, t1),
            Some(Action::DoubleClick { target, at: t1 })
        );
        // A third press starts over.
        let t2 = t1 + Duration::from_millis(100);
        assert_eq!(
            state.handle(&down(6, 3), mode, &hits, t2),
            click(target, t2)
        );
    }

    #[test]
    fn double_click_interval_boundary() {
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        let target = row(PaneSide::Right, 11);
        for (millis, double) in [(0, true), (449, true), (450, true), (451, false)] {
            let mut state = InputState::new();
            state.handle(&down(45, 3), mode, &hits, t0);
            state.handle(&up(45, 3), mode, &hits, t0);
            let t1 = t0 + Duration::from_millis(millis);
            let expected = if double {
                Some(Action::DoubleClick { target, at: t1 })
            } else {
                click(target, t1)
            };
            assert_eq!(
                state.handle(&down(45, 3), mode, &hits, t1),
                expected,
                "{millis} ms"
            );
        }
    }

    #[test]
    fn presses_on_different_targets_are_single_clicks() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        state.handle(&down(5, 3), mode, &hits, t0);
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(
            state.handle(&down(5, 4), mode, &hits, t1),
            click(row(PaneSide::Left, 2), t1)
        );
        // A row in the other pane, then a press on the same screen row there
        // that hits the blank space below its rows.
        let t2 = t1 + Duration::from_millis(100);
        assert_eq!(
            state.handle(&down(45, 3), mode, &hits, t2),
            click(row(PaneSide::Right, 11), t2)
        );
        assert_eq!(
            state.handle(&down(45, 4), mode, &hits, t2),
            click(ClickTarget::PaneArea(PaneSide::Right), t2)
        );
    }

    #[test]
    fn a_press_on_nothing_resets_the_double_click() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        state.handle(&down(5, 3), mode, &hits, t0);
        assert_eq!(state.handle(&down(90, 3), mode, &hits, t0), None);
        assert_eq!(
            state.handle(&down(5, 3), mode, &hits, t0),
            click(row(PaneSide::Left, 1), t0)
        );
    }

    #[test]
    fn click_modifiers() {
        let hits = hits();
        let t0 = Instant::now();
        let target = row(PaneSide::Left, 0);
        let cases = [
            (KeyModifiers::CONTROL, true, false),
            (KeyModifiers::SUPER, true, false),
            (KeyModifiers::SHIFT, false, true),
            (KeyModifiers::CONTROL | KeyModifiers::SHIFT, true, true),
            (KeyModifiers::ALT, false, false),
        ];
        for (modifiers, toggle, range) in cases {
            let event = mouse(MouseEventKind::Down(MouseButton::Left), 5, 2, modifiers);
            assert_eq!(
                InputState::new().handle(&event, InputMode::Browse, &hits, t0),
                Some(Action::Click {
                    target,
                    modifiers: ClickModifiers { toggle, range },
                    at: t0
                }),
                "{modifiers:?}"
            );
        }
    }

    #[test]
    fn modified_clicks_never_form_a_double_click() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        let target = row(PaneSide::Left, 0);
        let ctrl_down = mouse(
            MouseEventKind::Down(MouseButton::Left),
            5,
            2,
            KeyModifiers::CONTROL,
        );
        let toggled = Some(Action::Click {
            target,
            modifiers: ClickModifiers {
                toggle: true,
                range: false,
            },
            at: t0,
        });
        assert_eq!(state.handle(&ctrl_down, mode, &hits, t0), toggled);
        assert_eq!(state.handle(&ctrl_down, mode, &hits, t0), toggled);
        // A plain press right after a modified one is a single click, and a
        // modified press right after a plain one stays a toggle.
        assert_eq!(
            state.handle(&down(5, 2), mode, &hits, t0),
            click(target, t0)
        );
        assert_eq!(state.handle(&ctrl_down, mode, &hits, t0), toggled);
    }

    #[test]
    fn queue_button_takes_precedence_over_its_row() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        let pause = Some(Action::Queue {
            control: QueueControl::TogglePause,
            target: Some(JOB),
        });
        assert_eq!(state.handle(&down(71, 21), mode, &hits, t0), pause);
        // A second quick press is the button again, not a double click.
        assert_eq!(state.handle(&down(71, 21), mode, &hits, t0), pause);
        // The same row outside the button is the row.
        assert_eq!(
            state.handle(&down(5, 21), mode, &hits, t0),
            click(ClickTarget::QueueRow(JOB), t0)
        );
        assert_eq!(
            state.handle(&down(5, 22), mode, &hits, t0),
            click(ClickTarget::QueueRow(ITEM), t0)
        );
        // A queue button press never starts a drag.
        assert_eq!(state.handle(&down(71, 21), mode, &hits, t0), pause);
        assert_eq!(state.handle(&drag(10, 5), mode, &hits, t0), None);
    }

    #[test]
    fn dialog_buttons_and_pane_areas_are_click_targets() {
        let t0 = Instant::now();
        let mut map = hits();
        assert_eq!(
            InputState::new().handle(&down(5, 0), InputMode::Browse, &map, t0),
            click(ClickTarget::PaneArea(PaneSide::Left), t0)
        );
        map.dialog = Some(DialogHit {
            area: Rect::new(20, 5, 40, 8),
            buttons: vec![(Rect::new(30, 11, 6, 1), DialogButton::Yes)],
        });
        assert_eq!(
            InputState::new().handle(&down(32, 11), InputMode::Dialog, &map, t0),
            click(ClickTarget::DialogButton(DialogButton::Yes), t0)
        );
        // Outside the modal dialog nothing is hit.
        assert_eq!(
            InputState::new().handle(&down(5, 3), InputMode::Dialog, &map, t0),
            None
        );
    }

    #[test]
    fn wheel_scrolls_the_region_under_the_pointer() {
        let none = KeyModifiers::NONE;
        let mut map = hits();
        let scroll = |region, lines| Some(Action::Scroll { region, lines });
        assert_eq!(
            map_mouse(&map, mouse(MouseEventKind::ScrollDown, 5, 10, none)),
            scroll(ScrollRegion::Pane(PaneSide::Left), 3)
        );
        assert_eq!(
            map_mouse(&map, mouse(MouseEventKind::ScrollUp, 50, 10, none)),
            scroll(ScrollRegion::Pane(PaneSide::Right), -3)
        );
        assert_eq!(
            map_mouse(&map, mouse(MouseEventKind::ScrollUp, 5, 22, none)),
            scroll(ScrollRegion::Queue, -3)
        );
        for kind in [
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
            MouseEventKind::ScrollDown,
        ] {
            assert_eq!(map_mouse(&map, mouse(kind, 90, 30, none)), None);
        }
        assert_eq!(
            map_mouse(&map, mouse(MouseEventKind::ScrollLeft, 5, 10, none)),
            None
        );
        map.viewer = Some(ViewerHit {
            area: Rect::new(40, 0, 40, 20),
            kind: ViewerKind::Preview,
        });
        assert_eq!(
            map_mouse(&map, mouse(MouseEventKind::ScrollDown, 50, 10, none)),
            scroll(ScrollRegion::Preview, 3)
        );
    }

    #[test]
    fn other_buttons_and_plain_moves_do_nothing() {
        let map = hits();
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Down(MouseButton::Middle),
            MouseEventKind::Up(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Right),
            MouseEventKind::Moved,
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            assert_eq!(
                map_mouse(&map, mouse(kind, 5, 3, KeyModifiers::NONE)),
                None,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn drag_from_a_row_to_a_folder_in_the_other_pane() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        assert_eq!(
            state.handle(&down(5, 4), mode, &hits, t0),
            click(row(PaneSide::Left, 2), t0)
        );
        // Motion within the pressed cell is below the threshold.
        assert_eq!(state.handle(&drag(5, 4), mode, &hits, t0), None);
        let t1 = t0 + Duration::from_millis(30);
        assert_eq!(
            state.handle(&drag(6, 4), mode, &hits, t1),
            Some(Action::DragStart {
                side: PaneSide::Left,
                index: 2,
                at: t1
            })
        );
        // The same cell again reports nothing new.
        assert_eq!(state.handle(&drag(6, 4), mode, &hits, t1), None);
        let moves = [
            ((7, 4), DropTarget::Pane(PaneSide::Left)),
            (
                (5, 5),
                DropTarget::Folder {
                    side: PaneSide::Left,
                    index: 3,
                },
            ),
            (
                (45, 3),
                DropTarget::Folder {
                    side: PaneSide::Right,
                    index: 11,
                },
            ),
            ((45, 10), DropTarget::Pane(PaneSide::Right)),
            ((5, 22), DropTarget::Invalid),
            (
                (46, 3),
                DropTarget::Folder {
                    side: PaneSide::Right,
                    index: 11,
                },
            ),
        ];
        for ((column, row), target) in moves {
            assert_eq!(
                state.handle(&drag(column, row), mode, &hits, t1),
                Some(Action::DragMove {
                    target,
                    column,
                    row
                }),
                "({column}, {row})"
            );
        }
        let t2 = t1 + Duration::from_millis(500);
        assert_eq!(
            state.handle(&up(46, 3), mode, &hits, t2),
            Some(Action::Drop {
                target: DropTarget::Folder {
                    side: PaneSide::Right,
                    index: 11
                },
                at: t2
            })
        );
        // The drag is over.
        assert_eq!(state.handle(&drag(47, 3), mode, &hits, t2), None);
        assert_eq!(state.handle(&up(47, 3), mode, &hits, t2), None);
    }

    #[test]
    fn one_row_of_vertical_motion_starts_a_drag() {
        let mut state = InputState::new();
        let hits = hits();
        let t0 = Instant::now();
        state.handle(&down(5, 2), InputMode::Browse, &hits, t0);
        assert_eq!(
            state.handle(&drag(5, 3), InputMode::Browse, &hits, t0),
            Some(Action::DragStart {
                side: PaneSide::Left,
                index: 0,
                at: t0
            })
        );
    }

    #[test]
    fn release_outside_any_pane_drops_invalid() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        state.handle(&down(5, 2), mode, &hits, t0);
        state.handle(&drag(5, 3), mode, &hits, t0);
        assert_eq!(
            state.handle(&up(5, 23), mode, &hits, t0),
            Some(Action::Drop {
                target: DropTarget::Invalid,
                at: t0
            })
        );
    }

    #[test]
    fn a_click_after_a_drag_is_not_a_double_click() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        state.handle(&down(5, 2), mode, &hits, t0);
        state.handle(&drag(5, 3), mode, &hits, t0);
        state.handle(&drag(5, 2), mode, &hits, t0);
        state.handle(&up(5, 2), mode, &hits, t0);
        assert_eq!(
            state.handle(&down(5, 2), mode, &hits, t0),
            click(row(PaneSide::Left, 0), t0)
        );
    }

    #[test]
    fn esc_cancels_a_drag_instead_of_the_mode_action() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        state.handle(&down(5, 2), mode, &hits, t0);
        // Before the threshold Esc is an ordinary key.
        assert_eq!(
            state.handle(&key(KeyCode::Esc), mode, &hits, t0),
            Some(Action::Cancel)
        );
        state.handle(&drag(9, 9), mode, &hits, t0);
        assert_eq!(
            state.handle(&key(KeyCode::Esc), InputMode::Dialog, &hits, t0),
            Some(Action::DragCancel)
        );
        // The rest of the gesture is inert, and Esc is ordinary again.
        assert_eq!(state.handle(&drag(10, 9), mode, &hits, t0), None);
        assert_eq!(state.handle(&up(10, 9), mode, &hits, t0), None);
        assert_eq!(
            state.handle(&key(KeyCode::Esc), mode, &hits, t0),
            Some(Action::Cancel)
        );
    }

    #[test]
    fn other_keys_during_a_drag_follow_the_mode() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        state.handle(&down(5, 2), mode, &hits, t0);
        state.handle(&drag(9, 9), mode, &hits, t0);
        assert_eq!(
            state.handle(&key(KeyCode::Down), mode, &hits, t0),
            Some(Action::MoveCursor(CursorMove::Down))
        );
        let released = mouse(
            MouseEventKind::Up(MouseButton::Left),
            45,
            3,
            KeyModifiers::NONE,
        );
        assert_eq!(
            state.handle(&released, mode, &hits, t0),
            Some(Action::Drop {
                target: DropTarget::Folder {
                    side: PaneSide::Right,
                    index: 11
                },
                at: t0
            })
        );
    }

    #[test]
    fn a_lost_release_cancels_the_drag() {
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        let dragging = || {
            let mut state = InputState::new();
            state.handle(&down(5, 2), mode, &hits, t0);
            state.handle(&drag(9, 9), mode, &hits, t0);
            state
        };
        let moved = mouse(MouseEventKind::Moved, 12, 9, KeyModifiers::NONE);
        let mut state = dragging();
        assert_eq!(
            state.handle(&moved, mode, &hits, t0),
            Some(Action::DragCancel)
        );
        assert_eq!(state.handle(&moved, mode, &hits, t0), None);

        let mut state = dragging();
        assert_eq!(
            state.handle(&down(5, 3), mode, &hits, t0),
            Some(Action::DragCancel)
        );
        assert_eq!(state.handle(&drag(9, 9), mode, &hits, t0), None);
        assert_eq!(
            state.handle(&down(5, 3), mode, &hits, t0),
            click(row(PaneSide::Left, 1), t0)
        );
    }

    #[test]
    fn only_pane_rows_start_drags() {
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        // Pane header, blank pane space, queue row, outside everything.
        for (column, row) in [(5, 0), (5, 10), (5, 22), (90, 5)] {
            let mut state = InputState::new();
            state.handle(&down(column, row), mode, &hits, t0);
            assert_eq!(
                state.handle(&drag(column + 3, row + 1), mode, &hits, t0),
                None,
                "({column}, {row})"
            );
        }
    }

    #[test]
    fn release_without_drag_does_nothing() {
        let mut state = InputState::new();
        let hits = hits();
        let mode = InputMode::Browse;
        let t0 = Instant::now();
        state.handle(&down(5, 2), mode, &hits, t0);
        assert_eq!(state.handle(&up(5, 2), mode, &hits, t0), None);
        // The released press can no longer start a drag.
        assert_eq!(state.handle(&drag(9, 9), mode, &hits, t0), None);
    }

    #[test]
    fn mouse_mapping_ignores_the_mode() {
        let hits = hits();
        let t0 = Instant::now();
        for mode in [InputMode::Filter, InputMode::Queue, InputMode::Viewer] {
            assert_eq!(
                InputState::new().handle(&down(5, 3), mode, &hits, t0),
                click(row(PaneSide::Left, 1), t0),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn paste_resize_and_focus() {
        let text = "/Users/me/a b.txt\n".to_string();
        for mode in [InputMode::Browse, InputMode::TextField, InputMode::Dialog] {
            assert_eq!(
                map(mode, Event::Paste(text.clone())),
                Some(Action::Paste(text.clone()))
            );
        }
        assert_eq!(
            map(InputMode::Browse, Event::Resize(120, 40)),
            Some(Action::Resize {
                width: 120,
                height: 40
            })
        );
        assert_eq!(map(InputMode::Browse, Event::FocusGained), None);
        assert_eq!(map(InputMode::Browse, Event::FocusLost), None);
    }

    fn paths(list: &[&str]) -> Option<Vec<PathBuf>> {
        Some(list.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn dropped_paths_are_split_like_a_shell() {
        // Terminal.app and iTerm2 escape with backslashes and add a space.
        assert_eq!(
            split_dropped_paths("/Users/me/My\\ File.txt /tmp/b "),
            paths(&["/Users/me/My File.txt", "/tmp/b"])
        );
        assert_eq!(
            split_dropped_paths("'/Users/me/it'\\''s here' \"/tmp/x y\""),
            paths(&["/Users/me/it's here", "/tmp/x y"])
        );
        assert_eq!(split_dropped_paths("/tmp/a\n"), paths(&["/tmp/a"]));
        assert_eq!(split_dropped_paths("/tmp/a\r\n"), paths(&["/tmp/a"]));
        assert_eq!(
            split_dropped_paths("/tmp/a\n/tmp/b"),
            paths(&["/tmp/a", "/tmp/b"])
        );
        // Non-ASCII names (CJK, NFD) pass through untouched.
        assert_eq!(
            split_dropped_paths("/tmp/\u{4e2d}\u{6587} /tmp/e\u{301}"),
            paths(&["/tmp/\u{4e2d}\u{6587}", "/tmp/e\u{301}"])
        );
    }

    #[test]
    fn non_path_pastes_are_not_drops() {
        for text in [
            "",
            "  \n",
            "hello world",
            "/tmp/a relative",
            "~/Downloads/a",
            "./a",
            "'/tmp/unterminated",
            "file:///tmp/a",
        ] {
            assert_eq!(split_dropped_paths(text), None, "{text:?}");
        }
    }
}
