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
//! Areas per layer, matching how each feature pages:
//!
//! - panes and the queue panel: their `AppLayout` rectangles. Only panes
//!   `CoreState::pane_drawn` reports are drawn (in single-pane mode, the
//!   focused one, under a tab row recorded as `HitMap::tabs`); a pane under
//!   the quick view (`AppLayout::quick_view`, recorded as `HitMap::viewer`)
//!   is not drawn or recorded. A collapsed queue panel is one summary row;
//! - below the minimum size (`CoreState::too_small`): only a centered
//!   "Terminal too small" message and an empty `HitMap`;
//! - the comparison view: every row above the status bar;
//! - the full-screen preview and the help screen: the whole screen;
//! - the picker and the dialog: centered in the whole screen by their
//!   widgets; a focused dialog text field places the terminal cursor.
//!
//! The status bar shows the status message, the focused pane's selection
//! (`N selected, <size of the selected files>`), the transfer summary, and
//! key hints for the current input mode.
//!
//! # Public API (Wave 3, integration owner)
//!
//! ```text
//! /// Draws one frame. `&mut` because image previews render statefully.
//! pub fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) -> HitMap;
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::ui::action::{DropTarget, InputMode, PaneSide};
use crate::ui::app::core::{AppLayout, MIN_HEIGHT, MIN_WIDTH};
use crate::ui::app::App;
use crate::ui::hit::{HitMap, QueueHit, TabHit, ViewerHit, ViewerKind};
use crate::ui::widgets::format::format_size;
use crate::ui::widgets::status_bar::StatusBarView;
use crate::ui::widgets::text_input::{fit_end, sanitize};
use crate::ui::widgets::{
    dialog, drag_label, help, pane, picker, preview, queue, status_bar, sync_view,
};
use crate::vfs::EntryKind;

const TAB_FOCUSED: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Cyan)
    .add_modifier(Modifier::BOLD);
const TAB_UNFOCUSED: Style = Style::new().fg(Color::DarkGray);
const TAB_DROP: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Yellow)
    .add_modifier(Modifier::BOLD);
const QUEUE_SUMMARY: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Draws one frame. `&mut` because image previews render statefully.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) -> HitMap {
    let screen = frame.area();
    if app.core.too_small() {
        too_small(screen, frame.buffer_mut());
        return HitMap::default();
    }
    let layout = app.core.layout();
    let mut hits = HitMap::default();

    // The quick view, while it has an entry to show; it covers the
    // non-focused pane or, when the panes are narrow, every pane.
    let mut title = String::new();
    let quick_area = match app.quick_preview_view(&mut title) {
        Some((_, view)) => {
            let area = layout.quick_view;
            preview::render(view, area, frame.buffer_mut());
            hits.viewer = Some(ViewerHit {
                area,
                kind: ViewerKind::Preview,
            });
            Some(area)
        }
        None => None,
    };
    if let Some(area) = layout.tabs {
        hits.tabs = tabs(app, area, frame.buffer_mut());
    }
    for side in [PaneSide::Left, PaneSide::Right] {
        let area = pane_area(&layout, side);
        if !app.core.pane_drawn(side) || quick_area.is_some_and(|quick| quick.intersects(area)) {
            continue;
        }
        let pane = app.core.pane(side);
        let status = app.connection_status_text(&pane.endpoint);
        let view = pane::PaneView {
            side,
            pane,
            focused: app.core.focused() == side && !app.core.queue_focused(),
            status: status.as_deref(),
            drop_target: app.drop_target(side),
        };
        hits.panes
            .push(pane::render(&view, area, frame.buffer_mut()));
    }

    // Queue panel, or its summary row on short terminals.
    if let Some(area) = layout.queue {
        if layout.queue_collapsed {
            let summary = app.queue_summary();
            Clear.render(area, frame.buffer_mut());
            Line::styled(fit_end(&summary, area.width).into_owned(), QUEUE_SUMMARY)
                .render(area, frame.buffer_mut());
            hits.queue = Some(QueueHit {
                area,
                rows: Vec::new(),
                buttons: Vec::new(),
            });
        } else {
            let mut texts = Vec::new();
            let mut rows = Vec::new();
            let view = app.queue_view(&mut texts, &mut rows);
            hits.queue = Some(queue::render(&view, area, frame.buffer_mut()));
        }
    }

    // Status bar (under every overlay that covers the whole screen).
    let message = app.core.status();
    let selection = selection_summary(app);
    let transfers = app.transfer_summary();
    status_bar::render(
        &StatusBarView {
            message: message.map_or("", |message| message.text.as_str()),
            message_is_error: message.is_some_and(|message| message.error),
            selection: &selection,
            transfers: &transfers,
            hints: hints(app.input_mode()),
        },
        layout.status_bar,
        frame.buffer_mut(),
    );

    // Comparison view over everything above the status bar.
    if let Some(view) = app.sync_view() {
        let area = Rect::new(0, 0, screen.width, layout.status_bar.y);
        hits.sync_view = Some(sync_view::render(&view, area, frame.buffer_mut()));
    }

    // Full-screen viewers: the preview, then help on top of it.
    let mut title = String::new();
    if let Some(view) = app.full_preview_view(&mut title) {
        preview::render(view, screen, frame.buffer_mut());
        hits.viewer = Some(ViewerHit {
            area: screen,
            kind: ViewerKind::Preview,
        });
    }
    if let Some(view) = app.help_view() {
        help::render(&view, screen, frame.buffer_mut());
        hits.viewer = Some(ViewerHit {
            area: screen,
            kind: ViewerKind::Help,
        });
    }

    // Picker and dialog.
    let mut tabs = Vec::new();
    let mut picker_rows = Vec::new();
    if let Some(view) = app.picker_view(&mut tabs, &mut picker_rows) {
        hits.picker = Some(picker::render(&view, screen, frame.buffer_mut()));
    }
    let mut buttons = Vec::new();
    if let Some(view) = app.dialog_view(&mut buttons) {
        let (hit, caret) = dialog::render(&view, screen, frame.buffer_mut());
        hits.dialog = Some(hit);
        if let Some(position) = caret {
            frame.set_cursor_position(position);
        }
    }

    // Drag label on top of everything.
    if let Some(view) = app.drag_label_view() {
        drag_label::render(&view, screen, frame.buffer_mut());
    }
    hits
}

fn pane_area(layout: &AppLayout, side: PaneSide) -> Rect {
    match side {
        PaneSide::Left => layout.left,
        PaneSide::Right => layout.right,
    }
}

/// The single-pane tab row: one tab per pane (`1 <endpoint>`, with its
/// connection status), the focused one highlighted and a tab the drag would
/// drop into marked.
fn tabs(app: &App, area: Rect, buf: &mut Buffer) -> Vec<TabHit> {
    Clear.render(area, buf);
    let left_width = area.width / 2;
    [
        (
            PaneSide::Left,
            Rect {
                width: left_width,
                ..area
            },
        ),
        (
            PaneSide::Right,
            Rect {
                x: area.x + left_width,
                width: area.width - left_width,
                ..area
            },
        ),
    ]
    .into_iter()
    .map(|(side, tab)| {
        let pane = app.core.pane(side);
        let number = match side {
            PaneSide::Left => 1,
            PaneSide::Right => 2,
        };
        let mut label = format!(" {number} {}", sanitize(&pane.endpoint.to_string()));
        if let Some(status) = app.connection_status_text(&pane.endpoint) {
            label.push_str(&format!(" ({})", sanitize(&status)));
        }
        label.push(' ');
        let style = if app.drop_target(side) == Some(DropTarget::Pane(side)) {
            TAB_DROP
        } else if app.core.focused() == side {
            TAB_FOCUSED
        } else {
            TAB_UNFOCUSED
        };
        buf.set_style(tab, style);
        Line::styled(fit_end(&label, tab.width).into_owned(), style).render(tab, buf);
        TabHit { side, area: tab }
    })
    .collect()
}

/// The whole screen when it is below the minimum size.
fn too_small(screen: Rect, buf: &mut Buffer) {
    Clear.render(screen, buf);
    let text = format!(
        "Terminal too small: need at least {MIN_WIDTH}x{MIN_HEIGHT} (now {}x{})",
        screen.width, screen.height
    );
    // Start about half the wrapped height above the middle; word wrapping
    // may need a row more, which flows downwards.
    let width = usize::from(screen.width.max(1));
    let lines = u16::try_from(text.len().div_ceil(width)).unwrap_or(u16::MAX);
    let top = screen.height.saturating_sub(lines) / 2;
    let area = Rect {
        y: screen.y + top,
        height: screen.height - top,
        ..screen
    };
    Paragraph::new(text)
        .centered()
        .wrap(Wrap { trim: true })
        .render(area, buf);
}

/// `N selected, <size>` for the focused pane; the size counts the selected
/// regular files. Empty without a selection.
fn selection_summary(app: &App) -> String {
    let pane = app.core.pane(app.core.focused());
    let selected: Vec<_> = pane
        .entries
        .iter()
        .filter(|entry| pane.is_selected(entry))
        .collect();
    if selected.is_empty() {
        return String::new();
    }
    let bytes: u64 = selected
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::File))
        .filter_map(|entry| entry.size)
        .sum();
    format!("{} selected, {}", selected.len(), format_size(bytes))
}

/// Key hints for the status bar.
fn hints(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Browse => {
            "F5 copy  F6 move  F7 mkdir  F8 del  o host  = compare  Q queue  ? help  q quit"
        }
        InputMode::Filter => "type to filter  Enter keep  Esc clear",
        InputMode::TextField => "Enter ok  Tab next  Esc cancel",
        InputMode::Dialog => "Enter choose  Tab next  Esc cancel",
        InputMode::Picker => "type to filter  Enter pick  Esc close",
        InputMode::Viewer => "arrows scroll  m markdown  Esc close",
        InputMode::Queue => "Space pause  c cancel  r retry  d discard  C clear  Tab leave",
        InputMode::SyncView => "Space select  > left to right  < right to left  Esc close",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups::Groups;
    use crate::hosts::HostList;
    use crate::preview::Background;
    use crate::settings::Settings;
    use crate::state::PersistedState;
    use crate::ui::action::{Action, BgEvent, ClickModifiers, ClickTarget, Effect};
    use crate::vfs::{Endpoint, Entry};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(size),
            mtime: Some(1),
            mode: Some(0o644),
        }
    }

    fn deliver(app: &mut App, effect: &Effect, entries: Vec<Entry>) {
        let Effect::ListDir { request, location } = effect else {
            panic!("expected ListDir, got {effect:?}");
        };
        app.handle_event(BgEvent::Listed {
            request: *request,
            location: location.clone(),
            result: Ok(entries),
        });
    }

    fn app(right: Option<(Endpoint, Option<std::path::PathBuf>)>) -> (App, Vec<Effect>) {
        App::new(
            Settings::default(),
            HostList {
                aliases: vec!["web".into()],
                warnings: Vec::new(),
            },
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (Endpoint::Local, Some("/l".into())),
            right,
        )
    }

    fn render(app: &mut App, width: u16, height: u16) -> (HitMap, String) {
        app.handle_action(Action::Resize { width, height });
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        let mut hits = HitMap::default();
        terminal
            .draw(|frame| hits = draw(frame, app))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let text: String = buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>() + "\n")
            .collect();
        (hits, text)
    }

    #[test]
    fn both_panes_status_and_selection_are_drawn_and_recorded() {
        let (mut app, effects) = app(Some((Endpoint::Local, Some("/r".into()))));
        deliver(&mut app, &effects[0], vec![file("alpha.txt", 2048)]);
        deliver(&mut app, &effects[1], vec![file("beta.txt", 1)]);
        app.handle_action(Action::Resize {
            width: 100,
            height: 30,
        });
        app.handle_action(Action::ToggleSelect);
        let (hits, text) = render(&mut app, 100, 30);
        assert_eq!(hits.panes.len(), 2);
        assert_eq!(hits.panes[0].area, app.core.layout().left);
        assert_eq!(hits.panes[1].area, app.core.layout().right);
        assert!(hits.dialog.is_none() && hits.picker.is_none() && hits.viewer.is_none());
        assert!(text.contains("alpha.txt"));
        assert!(text.contains("beta.txt"));
        assert!(text.contains("1 selected, 2.0 KiB"), "{text}");
        assert!(text.contains("F5 copy"));
        // A click on the first row of the left pane resolves to it.
        let rows = hits.panes[0].rows;
        assert_eq!(
            hits.resolve(rows.x, rows.y),
            Some(ClickTarget::PaneRow {
                side: PaneSide::Left,
                index: 0
            })
        );
    }

    #[test]
    fn the_startup_host_picker_is_modal() {
        let (mut app, _) = app(None);
        let (hits, text) = render(&mut app, 100, 30);
        let picker = hits.picker.clone().expect("picker drawn");
        assert!(text.contains("Local"));
        assert!(text.contains("web"));
        // Outside the modal picker nothing is hit.
        assert_eq!(hits.resolve(0, 0), None);
        assert_eq!(
            hits.resolve(picker.rows.x, picker.rows.y),
            Some(ClickTarget::PickerRow(0))
        );
    }

    #[test]
    fn quick_view_replaces_the_other_pane_only_while_it_shows_an_entry() {
        let (mut app, effects) = app(Some((Endpoint::Local, Some("/r".into()))));
        deliver(&mut app, &effects[1], vec![file("beta.txt", 1)]);
        app.handle_action(Action::Resize {
            width: 100,
            height: 30,
        });
        // The focused left pane is still empty: both panes stay lists.
        app.handle_action(Action::ToggleQuickView);
        let (hits, _) = render(&mut app, 100, 30);
        assert_eq!(hits.panes.len(), 2);
        assert_eq!(hits.viewer, None);

        // With an entry under the cursor the right pane is the quick view.
        deliver(&mut app, &effects[0], vec![file("alpha.txt", 1)]);
        let (hits, text) = render(&mut app, 100, 30);
        assert_eq!(hits.panes.len(), 1);
        assert_eq!(hits.panes[0].side, PaneSide::Left);
        assert_eq!(
            hits.viewer,
            Some(ViewerHit {
                area: app.core.layout().right,
                kind: ViewerKind::Preview
            })
        );
        assert!(text.contains("alpha.txt"));
    }

    #[test]
    fn full_screen_help_covers_the_screen() {
        let (mut app, _) = app(Some((Endpoint::Local, Some("/r".into()))));
        app.handle_action(Action::Resize {
            width: 80,
            height: 24,
        });
        app.handle_action(Action::Help);
        let (hits, _) = render(&mut app, 80, 24);
        assert_eq!(
            hits.viewer,
            Some(ViewerHit {
                area: Rect::new(0, 0, 80, 24),
                kind: ViewerKind::Help
            })
        );
    }

    #[test]
    fn narrow_screens_show_the_focused_pane_under_tabs() {
        let (mut app, effects) = app(Some((Endpoint::Local, Some("/r".into()))));
        deliver(&mut app, &effects[0], vec![file("alpha.txt", 1)]);
        deliver(&mut app, &effects[1], vec![file("beta.txt", 1)]);
        let (hits, text) = render(&mut app, 50, 20);
        assert_eq!(hits.panes.len(), 1);
        assert_eq!(hits.panes[0].side, PaneSide::Left);
        assert_eq!(hits.panes[0].area, Rect::new(0, 1, 50, 18));
        assert!(
            text.contains("alpha.txt") && !text.contains("beta.txt"),
            "{text}"
        );
        let first_row = text.lines().next().expect("tab row");
        assert!(first_row.contains(" 1 Local") && first_row.contains(" 2 Local"));
        // A click on the second tab shows the right pane.
        let tab = hits.tabs[1];
        assert_eq!(
            hits.resolve(tab.area.x, tab.area.y),
            Some(ClickTarget::PaneArea(PaneSide::Right))
        );
        app.handle_action(Action::Click {
            target: ClickTarget::PaneArea(PaneSide::Right),
            modifiers: ClickModifiers::default(),
            at: std::time::Instant::now(),
        });
        let (hits, text) = render(&mut app, 50, 20);
        assert_eq!(hits.panes.len(), 1);
        assert_eq!(hits.panes[0].side, PaneSide::Right);
        assert!(
            text.contains("beta.txt") && !text.contains("alpha.txt"),
            "{text}"
        );
    }

    #[test]
    fn short_screens_collapse_the_queue_to_a_summary_row() {
        let (mut app, _) = app(Some((Endpoint::Local, Some("/r".into()))));
        app.handle_action(Action::ToggleQueue);
        let (hits, text) = render(&mut app, 90, 20);
        let queue = hits.queue.expect("queue drawn");
        assert_eq!(queue.area, Rect::new(0, 18, 90, 1));
        assert!(text.contains("Queue: no transfers"), "{text}");
        // Tall enough: a panel of at most a third of the rows.
        let (hits, _) = render(&mut app, 90, 30);
        assert_eq!(hits.queue.expect("queue drawn").area.height, 9);
    }

    #[test]
    fn too_small_screens_show_only_a_message() {
        let (mut app, _) = app(Some((Endpoint::Local, Some("/r".into()))));
        let (hits, text) = render(&mut app, 30, 8);
        assert_eq!(hits, HitMap::default());
        let joined: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            joined.contains("Terminal too small: need at least 40x10 (now 30x8)"),
            "{text}"
        );
        let (hits, _) = render(&mut app, 40, 10);
        assert_eq!(hits.panes.len(), 1);
    }

    #[test]
    fn tiny_and_empty_screens_do_not_panic() {
        let (mut app, _) = app(None);
        for (width, height) in [(30, 8), (1, 1), (0, 0), (160, 45)] {
            render(&mut app, width.max(1), height.max(1));
            app.handle_action(Action::Resize { width, height });
        }
        app.handle_action(Action::ToggleQueue);
        render(&mut app, 30, 8);
    }
}
