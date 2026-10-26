//! Queue panel widget: jobs and items with state, progress, and clickable
//! controls.
//!
//! A bordered panel; each job row is followed by its item rows (indented).
//! Each row shows label, state, a progress gauge or an indeterminate marker,
//! and its control buttons at the right edge, drawn in the order given and
//! recorded in `QueueHit::buttons` in that same order.
//!
//! The gauge is `[#####-----]  50%`; a byte progress whose `done` reaches
//! `total` (including `0 / 0`) is full. The indeterminate marker (rsync
//! syncs report no byte progress) is `[~~~~~~~~~~]   ?%`. Buttons are
//! bracketed words (`[pause/resume]`, `[cancel]`, `[retry]`, `[discard]`,
//! `[retry failed]`, `[clear done]`). The label keeps at least
//! [`MIN_LABEL_WIDTH`] cells: buttons that would squeeze it further are
//! dropped from the end of the list, then the gauge, then the state. The cursor row is highlighted only while the
//! panel is focused, and a focused panel has a bold border.
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

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Widget};

use crate::transfer::model::QueueTarget;
use crate::ui::action::QueueControl;
use crate::ui::hit::QueueHit;

/// The narrowest the label gets before the gauge is dropped.
pub const MIN_LABEL_WIDTH: u16 = 12;

/// Cells inside the gauge brackets.
const BAR_CELLS: u16 = 10;
/// `[` + bar + `]` + a space + `100%`.
const GAUGE_WIDTH: u16 = BAR_CELLS + 2 + 1 + 4;
/// Width of the state column (fits `running`, `canceled`, `reconnecting`);
/// longer states are cut.
const STATE_WIDTH: u16 = 12;
const INDENT: u16 = 2;

/// What the queue panel shows.
#[derive(Debug, Clone, Copy)]
pub struct QueueView<'a> {
    /// Job rows, each followed by its item rows, in display order.
    pub rows: &'a [QueueRowView<'a>],
    /// Index into `rows` of the highlighted row, if any.
    pub cursor: Option<usize>,
    /// Index into `rows` of the first drawn row.
    pub first_visible: usize,
    /// Whether the panel has keyboard focus.
    pub focused: bool,
}

/// One job or item row.
#[derive(Debug, Clone, Copy)]
pub struct QueueRowView<'a> {
    /// The job or item the row (and its buttons) applies to.
    pub target: QueueTarget,
    /// Item rows are indented under their job.
    pub indent: bool,
    /// For example `copy 3 items -> web:/srv` or `tar (no resume)`.
    pub label: &'a str,
    /// For example `running`, `paused`, or `failed`.
    pub state: &'a str,
    /// What the gauge shows.
    pub progress: Progress,
    /// The row's buttons, drawn left to right at the right edge.
    pub controls: &'a [QueueControl],
}

/// A row's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// No gauge (for example a queued or finished row).
    None,
    /// Bytes transferred out of the total.
    Bytes {
        /// Bytes transferred so far.
        done: u64,
        /// Bytes to transfer in total.
        total: u64,
    },
    /// rsync syncs.
    Indeterminate,
}

/// Draws the queue panel filling `area` and returns its hit data.
pub fn render(view: &QueueView<'_>, area: Rect, buf: &mut Buffer) -> QueueHit {
    Clear.render(area, buf);
    let border_style = if view.focused {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let block = Block::bordered()
        .title(" Queue ")
        .border_style(border_style);
    let inner = block.inner(area);
    block.render(area, buf);

    let mut hit = QueueHit {
        area,
        rows: Vec::new(),
        buttons: Vec::new(),
    };
    if view.rows.is_empty() && inner.height > 0 {
        buf.set_stringn(
            inner.x + 1,
            inner.y,
            "No transfers",
            usize::from(inner.width.saturating_sub(1)),
            Style::new().add_modifier(Modifier::DIM),
        );
    }
    let drawn = view
        .rows
        .iter()
        .enumerate()
        .skip(view.first_visible)
        .take(usize::from(inner.height));
    for ((index, row), y) in drawn.zip(inner.y..inner.bottom()) {
        let rect = Rect::new(inner.x, y, inner.width, 1);
        let highlighted = view.focused && view.cursor == Some(index);
        draw_row(row, rect, highlighted, buf, &mut hit);
        hit.rows.push((rect, row.target));
    }
    hit
}

fn draw_row(
    row: &QueueRowView<'_>,
    rect: Rect,
    highlighted: bool,
    buf: &mut Buffer,
    hit: &mut QueueHit,
) {
    let base = if highlighted {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    };
    buf.set_style(rect, base);

    let indent = if row.indent { INDENT } else { 0 };
    let start = rect.x + indent;
    let mut right = rect.right();

    // Buttons claim the right edge first, leaving the label its minimum;
    // those that do not fit are dropped from the end so the recorded order
    // still matches `controls`.
    let mut buttons = Vec::new();
    let mut buttons_width = 0;
    for &control in row.controls {
        let label = control_label(control);
        let width = text_width(label);
        let gap = u16::from(!buttons.is_empty());
        if start + MIN_LABEL_WIDTH + 1 + buttons_width + gap + width > right {
            break;
        }
        buttons_width += gap + width;
        buttons.push((control, label, width));
    }
    let mut x = right - buttons_width;
    for (index, (control, label, width)) in buttons.into_iter().enumerate() {
        if index > 0 {
            x += 1;
        }
        buf.set_stringn(x, rect.y, label, usize::from(width), base);
        hit.buttons
            .push((Rect::new(x, rect.y, width, 1), row.target, control));
        x += width;
    }
    right -= buttons_width;
    if buttons_width > 0 {
        right = right.saturating_sub(1).max(start);
    }

    let gauge = gauge_text(row.progress);
    if let Some(gauge) = gauge.filter(|_| start + MIN_LABEL_WIDTH + 1 + GAUGE_WIDTH <= right) {
        right -= GAUGE_WIDTH;
        buf.set_stringn(right, rect.y, &gauge, usize::from(GAUGE_WIDTH), base);
        right -= 1;
    }

    if !row.state.is_empty() && start + MIN_LABEL_WIDTH + 1 + STATE_WIDTH <= right {
        buf.set_stringn(
            right - STATE_WIDTH,
            rect.y,
            row.state,
            usize::from(STATE_WIDTH),
            base.add_modifier(Modifier::BOLD),
        );
        right -= STATE_WIDTH + 1;
    }

    buf.set_stringn(
        start,
        rect.y,
        row.label,
        usize::from(right.saturating_sub(start)),
        base,
    );
}

/// The gauge text, or `None` when the row has no progress to show.
fn gauge_text(progress: Progress) -> Option<String> {
    let cells = usize::from(BAR_CELLS);
    match progress {
        Progress::None => None,
        Progress::Bytes { done, total } => {
            let (filled, percent) = if done >= total {
                (cells, 100)
            } else {
                // u128 keeps `done * 100` from overflowing; `done < total`
                // keeps both quotients below 100.
                let scaled = |scale: u128| u128::from(done) * scale / u128::from(total);
                (
                    usize::try_from(scaled(u128::from(BAR_CELLS))).expect("below BAR_CELLS"),
                    u8::try_from(scaled(100)).expect("below 100"),
                )
            };
            Some(format!(
                "[{}{}] {percent:>3}%",
                "#".repeat(filled),
                "-".repeat(cells - filled)
            ))
        }
        Progress::Indeterminate => Some(format!("[{}]   ?%", "~".repeat(cells))),
    }
}

/// The bracketed text of a row button.
fn control_label(control: QueueControl) -> &'static str {
    match control {
        QueueControl::TogglePause => "[pause/resume]",
        QueueControl::Cancel => "[cancel]",
        QueueControl::Retry => "[retry]",
        QueueControl::DiscardPartial => "[discard]",
        QueueControl::RetryAllFailed => "[retry failed]",
        QueueControl::ClearFinished => "[clear done]",
    }
}

fn text_width(text: &str) -> u16 {
    u16::try_from(Line::raw(text).width()).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::transfer::model::{ItemId, JobId};
    use crate::ui::hit::HitMap;

    const JOB: QueueTarget = QueueTarget::Job(JobId(1));
    const ITEM: QueueTarget = QueueTarget::Item(ItemId(10));
    const SYNC: QueueTarget = QueueTarget::Job(JobId(2));

    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn text_at(buf: &Buffer, rect: Rect) -> String {
        (rect.x..rect.right())
            .map(|x| buf[(x, rect.y)].symbol())
            .collect()
    }

    fn draw(view: &QueueView<'_>, width: u16, height: u16) -> (Buffer, QueueHit) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hit = None;
        terminal
            .draw(|frame| hit = Some(render(view, frame.area(), frame.buffer_mut())))
            .unwrap();
        (terminal.backend().buffer().clone(), hit.unwrap())
    }

    fn sample_rows() -> Vec<QueueRowView<'static>> {
        vec![
            QueueRowView {
                target: JOB,
                indent: false,
                label: "copy 3 items -> web:/srv tar (no resume)",
                state: "running",
                progress: Progress::Bytes {
                    done: 50,
                    total: 100,
                },
                controls: &[QueueControl::TogglePause, QueueControl::Cancel],
            },
            QueueRowView {
                target: ITEM,
                indent: true,
                label: "big.iso",
                state: "failed",
                progress: Progress::None,
                controls: &[QueueControl::Retry, QueueControl::DiscardPartial],
            },
            QueueRowView {
                target: SYNC,
                indent: false,
                label: "sync -> web:/srv (rsync)",
                state: "running",
                progress: Progress::Indeterminate,
                controls: &[QueueControl::Cancel],
            },
        ]
    }

    #[test]
    fn rows_show_label_state_gauge_and_buttons() {
        let rows = sample_rows();
        let view = QueueView {
            rows: &rows,
            cursor: Some(1),
            first_visible: 0,
            focused: true,
        };
        let (buf, hit) = draw(&view, 100, 5);
        assert!(row_text(&buf, 0).contains(" Queue "));

        let job = row_text(&buf, 1);
        assert!(
            job.starts_with("\u{2502}copy 3 items -> web:/srv tar (no resume)"),
            "{job:?}"
        );
        assert!(job.contains("running"));
        assert!(job.contains("[#####-----]  50%"));
        assert!(job.ends_with("[pause/resume] [cancel]\u{2502}"), "{job:?}");

        let item = row_text(&buf, 2);
        assert!(item.starts_with("\u{2502}  big.iso"), "{item:?}");
        assert!(item.contains("failed") && !item.contains('%'));
        assert!(item.ends_with("[retry] [discard]\u{2502}"));

        let sync = row_text(&buf, 3);
        assert!(sync.contains("[~~~~~~~~~~]   ?%"), "{sync:?}");

        assert_eq!(hit.area, Rect::new(0, 0, 100, 5));
        assert_eq!(
            hit.rows,
            vec![
                (Rect::new(1, 1, 98, 1), JOB),
                (Rect::new(1, 2, 98, 1), ITEM),
                (Rect::new(1, 3, 98, 1), SYNC),
            ]
        );
        let recorded: Vec<_> = hit
            .buttons
            .iter()
            .map(|&(rect, target, control)| (text_at(&buf, rect), target, control))
            .collect();
        assert_eq!(
            recorded,
            vec![
                ("[pause/resume]".to_owned(), JOB, QueueControl::TogglePause),
                ("[cancel]".to_owned(), JOB, QueueControl::Cancel),
                ("[retry]".to_owned(), ITEM, QueueControl::Retry),
                ("[discard]".to_owned(), ITEM, QueueControl::DiscardPartial),
                ("[cancel]".to_owned(), SYNC, QueueControl::Cancel),
            ]
        );

        // The focused cursor row is highlighted.
        assert!(buf[(5, 2)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(5, 1)].modifier.contains(Modifier::REVERSED));

        // Clicks resolve through the hit map to the drawn buttons and rows.
        let (cancel_rect, _, _) = hit.buttons[1];
        let map = HitMap {
            queue: Some(hit),
            ..HitMap::default()
        };
        assert_eq!(
            map.queue_button(cancel_rect.x + 2, 1),
            Some((JOB, QueueControl::Cancel))
        );
        assert_eq!(
            map.resolve(10, 2),
            Some(crate::ui::action::ClickTarget::QueueRow(ITEM))
        );
    }

    #[test]
    fn unfocused_panel_does_not_highlight_the_cursor() {
        let rows = sample_rows();
        let view = QueueView {
            rows: &rows,
            cursor: Some(0),
            first_visible: 0,
            focused: false,
        };
        let (buf, _) = draw(&view, 100, 5);
        assert!(!buf[(5, 1)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn scrolling_skips_rows_and_clips_to_height() {
        let rows = sample_rows();
        let view = QueueView {
            rows: &rows,
            cursor: None,
            first_visible: 1,
            focused: false,
        };
        let (buf, hit) = draw(&view, 100, 3);
        assert_eq!(hit.rows, vec![(Rect::new(1, 1, 98, 1), ITEM)]);
        assert_eq!(hit.buttons.len(), 2);
        assert!(row_text(&buf, 1).contains("big.iso"));
    }

    #[test]
    fn narrow_panel_drops_trailing_buttons_then_the_gauge() {
        let rows = sample_rows();
        let view = QueueView {
            rows: &rows[..1],
            cursor: None,
            first_visible: 0,
            focused: false,
        };
        // 30 inner cells: only `[pause/resume]` fits beside a label, and the
        // gauge would squeeze the label below its minimum.
        let (buf, hit) = draw(&view, 32, 3);
        let recorded: Vec<_> = hit.buttons.iter().map(|&(_, _, control)| control).collect();
        assert_eq!(recorded, vec![QueueControl::TogglePause]);
        let row = row_text(&buf, 1);
        assert!(!row.contains('%'), "{row:?}");
        assert!(row.starts_with("\u{2502}copy 3 items"));
    }

    #[test]
    fn empty_queue_says_so() {
        let view = QueueView {
            rows: &[],
            cursor: None,
            first_visible: 0,
            focused: true,
        };
        let (buf, hit) = draw(&view, 40, 4);
        assert!(row_text(&buf, 1).contains("No transfers"));
        assert!(hit.rows.is_empty() && hit.buttons.is_empty());
    }

    #[test]
    fn gauge_handles_edge_values() {
        assert_eq!(gauge_text(Progress::None), None);
        assert_eq!(
            gauge_text(Progress::Bytes { done: 0, total: 0 }).unwrap(),
            "[##########] 100%"
        );
        assert_eq!(
            gauge_text(Progress::Bytes { done: 7, total: 5 }).unwrap(),
            "[##########] 100%"
        );
        assert_eq!(
            gauge_text(Progress::Bytes {
                done: 999,
                total: 1000
            })
            .unwrap(),
            "[#########-]  99%"
        );
        assert_eq!(
            gauge_text(Progress::Bytes {
                done: u64::MAX - 1,
                total: u64::MAX
            })
            .unwrap(),
            "[#########-]  99%"
        );
        assert_eq!(
            gauge_text(Progress::Bytes { done: 0, total: 1 }).unwrap(),
            "[----------]   0%"
        );
    }
}
