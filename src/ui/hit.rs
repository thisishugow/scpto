//! Mouse hit testing shared by rendering and input.
//!
//! `ui/render.rs` builds a fresh [`HitMap`] for every frame it draws,
//! recording where it put each clickable thing; `ui/input.rs` only reads the
//! map of the last frame to turn a mouse position into a [`ClickTarget`],
//! queue button, [`ScrollRegion`], or [`DropTarget`]. Neither side computes
//! layout on its own, so a click always resolves against what is on screen.
//!
//! Layers are resolved top-down: dialog, picker, viewer, comparison view,
//! queue panel, pane tabs, panes. A layer claims every point inside its
//! `area`. The dialog and the picker are modal: while one is recorded,
//! points outside it hit nothing.
//!
//! In single-pane mode (plan 8.8) each pane has a tab in the tab row: a
//! click on a tab is `ClickTarget::PaneArea` of its pane (which focuses and
//! so shows it), and a drag released on a tab drops into that pane's
//! directory (`DropTarget::Pane`).

use ratatui::layout::{Position, Rect};

use crate::transfer::model::QueueTarget;
use crate::ui::action::{
    ClickTarget, DialogButton, DropTarget, PaneSide, QueueControl, ScrollRegion,
};

/// A pane's file list as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneHit {
    /// The pane.
    pub side: PaneSide,
    /// The whole pane, including its header and the blank space below the
    /// rows.
    pub area: Rect,
    /// The list body; row `n` of the body is screen row `rows.y + n`.
    pub rows: Rect,
    /// Visible-list index of the first drawn row.
    pub first_visible: usize,
    /// One flag per drawn row: whether that row is enterable (a directory or
    /// a symlink to one), which makes it a folder drop target.
    pub enterable: Vec<bool>,
}

/// A scrolled list (host picker, bookmark list, comparison view) as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListHit {
    /// The whole widget, including its border and title.
    pub area: Rect,
    /// The list body; row `n` of the body is screen row `rows.y + n`.
    pub rows: Rect,
    /// Visible-list index of the first drawn row.
    pub first_visible: usize,
    /// Number of drawn rows.
    pub row_count: usize,
}

/// The queue panel as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueHit {
    /// The whole panel.
    pub area: Rect,
    /// Each drawn job or item row.
    pub rows: Vec<(Rect, QueueTarget)>,
    /// Each drawn row button and the control it applies to its row. Buttons
    /// take precedence over the row they sit on.
    pub buttons: Vec<(Rect, QueueTarget, QueueControl)>,
}

/// A pane's tab in the single-pane tab row, as drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabHit {
    /// The pane the tab stands for.
    pub side: PaneSide,
    /// The tab's cells.
    pub area: Rect,
}

/// The open dialog as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogHit {
    /// The dialog box.
    pub area: Rect,
    /// Each drawn button.
    pub buttons: Vec<(Rect, DialogButton)>,
}

/// Which viewer is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerKind {
    /// The full-screen preview or the quick view.
    Preview,
    /// The help screen.
    Help,
}

/// The topmost viewer as drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewerHit {
    /// The viewer's area (the whole screen, or the quick-view pane).
    pub area: Rect,
    /// What it shows.
    pub kind: ViewerKind,
}

/// Everything clickable in the last rendered frame. `None` and empty fields
/// mean the element was not drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HitMap {
    /// Panes drawn as file lists (a pane showing the quick view is recorded
    /// as [`HitMap::viewer`] instead).
    pub panes: Vec<PaneHit>,
    /// The queue panel.
    pub queue: Option<QueueHit>,
    /// The tabs of the single-pane tab row.
    pub tabs: Vec<TabHit>,
    /// The directory comparison view.
    pub sync_view: Option<ListHit>,
    /// The topmost viewer (full-screen preview, help, or quick view).
    pub viewer: Option<ViewerHit>,
    /// The host picker or bookmark list.
    pub picker: Option<ListHit>,
    /// The open dialog.
    pub dialog: Option<DialogHit>,
}

/// The layer that owns a point.
enum Layer<'a> {
    Dialog(&'a DialogHit),
    Picker(&'a ListHit),
    Viewer(&'a ViewerHit),
    SyncView(&'a ListHit),
    Queue(&'a QueueHit),
    Tab(&'a TabHit),
    Pane(&'a PaneHit),
}

impl HitMap {
    /// What a click at (`column`, `row`) hits, excluding queue row buttons
    /// (see [`HitMap::queue_button`], which input checks first).
    pub fn resolve(&self, column: u16, row: u16) -> Option<ClickTarget> {
        let point = Position::new(column, row);
        match self.layer_at(point)? {
            Layer::Dialog(dialog) => find(&dialog.buttons, point).map(ClickTarget::DialogButton),
            Layer::Picker(list) => list_index(list, point).map(ClickTarget::PickerRow),
            Layer::Viewer(_) => None,
            Layer::SyncView(list) => list_index(list, point).map(ClickTarget::SyncRow),
            Layer::Queue(queue) => find(&queue.rows, point).map(ClickTarget::QueueRow),
            Layer::Tab(tab) => Some(ClickTarget::PaneArea(tab.side)),
            Layer::Pane(pane) => Some(match pane_index(pane, point) {
                Some(index) => ClickTarget::PaneRow {
                    side: pane.side,
                    index,
                },
                None => ClickTarget::PaneArea(pane.side),
            }),
        }
    }

    /// The queue row button at (`column`, `row`), if any; a click on it
    /// becomes `Action::Queue { control, target: Some(target) }`.
    pub fn queue_button(&self, column: u16, row: u16) -> Option<(QueueTarget, QueueControl)> {
        let point = Position::new(column, row);
        match self.layer_at(point)? {
            Layer::Queue(queue) => queue
                .buttons
                .iter()
                .find(|(rect, _, _)| rect.contains(point))
                .map(|&(_, target, control)| (target, control)),
            _ => None,
        }
    }

    /// The area the mouse wheel scrolls at (`column`, `row`), if any.
    pub fn scroll_region(&self, column: u16, row: u16) -> Option<ScrollRegion> {
        Some(match self.layer_at(Position::new(column, row))? {
            Layer::Dialog(_) | Layer::Tab(_) => return None,
            Layer::Picker(_) => ScrollRegion::Picker,
            Layer::Viewer(viewer) => match viewer.kind {
                ViewerKind::Preview => ScrollRegion::Preview,
                ViewerKind::Help => ScrollRegion::Help,
            },
            Layer::SyncView(_) => ScrollRegion::SyncView,
            Layer::Queue(_) => ScrollRegion::Queue,
            Layer::Pane(pane) => ScrollRegion::Pane(pane.side),
        })
    }

    /// Where a drag released at (`column`, `row`) would drop.
    pub fn drop_target(&self, column: u16, row: u16) -> DropTarget {
        let point = Position::new(column, row);
        match self.layer_at(point) {
            Some(Layer::Pane(pane)) => match row_offset(pane.rows, pane.enterable.len(), point) {
                Some(offset) if pane.enterable[offset] => DropTarget::Folder {
                    side: pane.side,
                    index: pane.first_visible + offset,
                },
                _ => DropTarget::Pane(pane.side),
            },
            Some(Layer::Tab(tab)) => DropTarget::Pane(tab.side),
            _ => DropTarget::Invalid,
        }
    }

    fn layer_at(&self, point: Position) -> Option<Layer<'_>> {
        if let Some(dialog) = &self.dialog {
            return dialog.area.contains(point).then_some(Layer::Dialog(dialog));
        }
        if let Some(picker) = &self.picker {
            return picker.area.contains(point).then_some(Layer::Picker(picker));
        }
        if let Some(viewer) = self.viewer.as_ref().filter(|v| v.area.contains(point)) {
            return Some(Layer::Viewer(viewer));
        }
        if let Some(list) = self.sync_view.as_ref().filter(|l| l.area.contains(point)) {
            return Some(Layer::SyncView(list));
        }
        if let Some(queue) = self.queue.as_ref().filter(|q| q.area.contains(point)) {
            return Some(Layer::Queue(queue));
        }
        if let Some(tab) = self.tabs.iter().find(|tab| tab.area.contains(point)) {
            return Some(Layer::Tab(tab));
        }
        self.panes
            .iter()
            .find(|pane| pane.area.contains(point))
            .map(Layer::Pane)
    }
}

fn find<T: Copy>(entries: &[(Rect, T)], point: Position) -> Option<T> {
    entries
        .iter()
        .find(|(rect, _)| rect.contains(point))
        .map(|&(_, value)| value)
}

/// The drawn row offset of `point` within a list body of `row_count` rows.
fn row_offset(rows: Rect, row_count: usize, point: Position) -> Option<usize> {
    let offset = usize::from(point.y.checked_sub(rows.y)?);
    (rows.contains(point) && offset < row_count).then_some(offset)
}

fn list_index(list: &ListHit, point: Position) -> Option<usize> {
    row_offset(list.rows, list.row_count, point).map(|offset| list.first_visible + offset)
}

fn pane_index(pane: &PaneHit, point: Position) -> Option<usize> {
    row_offset(pane.rows, pane.enterable.len(), point).map(|offset| pane.first_visible + offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::model::{ItemId, JobId};

    fn left_pane() -> PaneHit {
        // Pane at columns 0..40, rows 0..20; list body starts at row 2.
        PaneHit {
            side: PaneSide::Left,
            area: Rect::new(0, 0, 40, 20),
            rows: Rect::new(1, 2, 38, 17),
            first_visible: 100,
            enterable: vec![true, false, false],
        }
    }

    #[test]
    fn pane_rows_map_to_visible_indices() {
        let map = HitMap {
            panes: vec![left_pane()],
            ..HitMap::default()
        };
        assert_eq!(
            map.resolve(5, 3),
            Some(ClickTarget::PaneRow {
                side: PaneSide::Left,
                index: 101
            })
        );
        // Header and blank space below the drawn rows.
        assert_eq!(
            map.resolve(5, 0),
            Some(ClickTarget::PaneArea(PaneSide::Left))
        );
        assert_eq!(
            map.resolve(5, 10),
            Some(ClickTarget::PaneArea(PaneSide::Left))
        );
        assert_eq!(map.resolve(50, 3), None);
        assert_eq!(
            map.scroll_region(5, 10),
            Some(ScrollRegion::Pane(PaneSide::Left))
        );
    }

    #[test]
    fn only_enterable_rows_are_folder_drop_targets() {
        let map = HitMap {
            panes: vec![left_pane()],
            ..HitMap::default()
        };
        assert_eq!(
            map.drop_target(5, 2),
            DropTarget::Folder {
                side: PaneSide::Left,
                index: 100
            }
        );
        assert_eq!(map.drop_target(5, 3), DropTarget::Pane(PaneSide::Left));
        assert_eq!(map.drop_target(50, 3), DropTarget::Invalid);
    }

    #[test]
    fn tabs_focus_on_click_and_take_drops() {
        let map = HitMap {
            panes: vec![left_pane()],
            tabs: vec![
                TabHit {
                    side: PaneSide::Left,
                    area: Rect::new(0, 0, 20, 1),
                },
                TabHit {
                    side: PaneSide::Right,
                    area: Rect::new(20, 0, 20, 1),
                },
            ],
            ..HitMap::default()
        };
        // The tab row sits over the pane's top row.
        assert_eq!(
            map.resolve(25, 0),
            Some(ClickTarget::PaneArea(PaneSide::Right))
        );
        assert_eq!(map.drop_target(25, 0), DropTarget::Pane(PaneSide::Right));
        assert_eq!(map.drop_target(5, 0), DropTarget::Pane(PaneSide::Left));
        assert_eq!(map.scroll_region(25, 0), None);
        // Below the tabs the pane resolves as usual.
        assert_eq!(
            map.drop_target(5, 2),
            DropTarget::Folder {
                side: PaneSide::Left,
                index: 100
            }
        );
    }

    #[test]
    fn modal_dialog_blocks_everything_else() {
        let map = HitMap {
            panes: vec![left_pane()],
            dialog: Some(DialogHit {
                area: Rect::new(10, 5, 20, 6),
                buttons: vec![(Rect::new(12, 9, 4, 1), DialogButton::Ok)],
            }),
            ..HitMap::default()
        };
        assert_eq!(
            map.resolve(13, 9),
            Some(ClickTarget::DialogButton(DialogButton::Ok))
        );
        assert_eq!(map.resolve(11, 6), None);
        assert_eq!(map.resolve(2, 3), None);
        assert_eq!(map.scroll_region(2, 3), None);
        assert_eq!(map.drop_target(2, 3), DropTarget::Invalid);
    }

    #[test]
    fn queue_buttons_take_precedence_over_rows() {
        let job = QueueTarget::Job(JobId(1));
        let item = QueueTarget::Item(ItemId(7));
        let map = HitMap {
            panes: vec![left_pane()],
            queue: Some(QueueHit {
                area: Rect::new(0, 15, 40, 5),
                rows: vec![
                    (Rect::new(0, 16, 40, 1), job),
                    (Rect::new(0, 17, 40, 1), item),
                ],
                buttons: vec![(Rect::new(35, 16, 3, 1), job, QueueControl::Cancel)],
            }),
            ..HitMap::default()
        };
        assert_eq!(map.queue_button(36, 16), Some((job, QueueControl::Cancel)));
        assert_eq!(map.queue_button(5, 16), None);
        assert_eq!(map.resolve(5, 17), Some(ClickTarget::QueueRow(item)));
        // The queue panel covers the bottom of the pane.
        assert_eq!(map.resolve(5, 15), None);
        assert_eq!(map.scroll_region(5, 15), Some(ScrollRegion::Queue));
    }

    #[test]
    fn picker_and_viewer_layers() {
        let picker = ListHit {
            area: Rect::new(10, 2, 20, 10),
            rows: Rect::new(11, 4, 18, 7),
            first_visible: 3,
            row_count: 2,
        };
        let map = HitMap {
            picker: Some(picker),
            ..HitMap::default()
        };
        assert_eq!(map.resolve(12, 5), Some(ClickTarget::PickerRow(4)));
        assert_eq!(map.resolve(12, 6), None);
        assert_eq!(map.scroll_region(12, 6), Some(ScrollRegion::Picker));
        assert_eq!(map.scroll_region(0, 0), None);

        let map = HitMap {
            panes: vec![left_pane()],
            viewer: Some(ViewerHit {
                area: Rect::new(0, 0, 80, 24),
                kind: ViewerKind::Help,
            }),
            ..HitMap::default()
        };
        assert_eq!(map.resolve(5, 3), None);
        assert_eq!(map.scroll_region(5, 3), Some(ScrollRegion::Help));
    }
}
