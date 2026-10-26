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
//! - panes and the queue panel: their `AppLayout` rectangles; the pane that
//!   shows the quick view is recorded as `HitMap::viewer` instead of a pane;
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

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::ui::action::{InputMode, PaneSide};
use crate::ui::app::App;
use crate::ui::hit::{HitMap, ViewerHit, ViewerKind};
use crate::ui::widgets::format::format_size;
use crate::ui::widgets::status_bar::StatusBarView;
use crate::ui::widgets::{
    dialog, drag_label, help, pane, picker, preview, queue, status_bar, sync_view,
};
use crate::vfs::EntryKind;

/// Draws one frame. `&mut` because image previews render statefully.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) -> HitMap {
    let screen = frame.area();
    let layout = app.core.layout();
    let mut hits = HitMap::default();

    // The quick view in the non-focused pane, while it has an entry to
    // show; otherwise both panes are file lists.
    let mut title = String::new();
    let quick_side = match app.quick_preview_view(&mut title) {
        Some((side, view)) => {
            let area = pane_area(&layout, side);
            preview::render(view, area, frame.buffer_mut());
            hits.viewer = Some(ViewerHit {
                area,
                kind: ViewerKind::Preview,
            });
            Some(side)
        }
        None => None,
    };
    for side in [PaneSide::Left, PaneSide::Right] {
        if Some(side) == quick_side {
            continue;
        }
        let area = pane_area(&layout, side);
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

    // Queue panel.
    if let Some(area) = layout.queue {
        let mut texts = Vec::new();
        let mut rows = Vec::new();
        let view = app.queue_view(&mut texts, &mut rows);
        hits.queue = Some(queue::render(&view, area, frame.buffer_mut()));
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

fn pane_area(layout: &crate::ui::app::core::AppLayout, side: PaneSide) -> Rect {
    match side {
        PaneSide::Left => layout.left,
        PaneSide::Right => layout.right,
    }
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
    use crate::ui::action::{Action, BgEvent, ClickTarget, Effect};
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
