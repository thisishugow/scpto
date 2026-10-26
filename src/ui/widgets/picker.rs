//! Filterable list widget for the host picker (grouped, with Local) and
//! the bookmark list.
//!
//! A centered popup: title, a tab row (host picker: `All`, each sshto group,
//! `Other`; selected with digit keys like sshto; omitted when empty), the
//! filter line, and the rows (label, then dimmed detail such as
//! `user@hostname` once resolved).
//!
//! The popup has a fixed size (at most [`MAX_WIDTH`] by [`MAX_LIST_ROWS`]
//! list rows, within the screen minus a 2-cell margin) so it does not jump
//! while the filter narrows the rows. The first nine tabs are prefixed with
//! their digit key; the active tab is highlighted and scrolled into view.
//! The filter line shows `/` and the filter with a block caret, or a dimmed
//! hint while empty. The cursor row is highlighted; an empty list shows
//! `No matches`. The bottom border shows the row count.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct PickerView<'a> {
//!     pub title: &'a str,
//!     pub filter: &'a str,
//!     pub tabs: &'a [&'a str],
//!     pub active_tab: usize,
//!     /// The rows of the active tab after filtering, in display order.
//!     pub rows: &'a [PickerRow<'a>],
//!     pub cursor: usize,
//!     pub first_visible: usize,
//! }
//!
//! pub struct PickerRow<'a> {
//!     pub label: &'a str,
//!     pub detail: &'a str,
//! }
//!
//! /// Draws the popup centered in `screen` and returns its hit data
//! /// (`first_visible` and `row_count` as drawn).
//! pub fn render(view: &PickerView<'_>, screen: Rect, buf: &mut Buffer) -> ListHit;
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Widget};

use crate::ui::hit::ListHit;

/// Widest the popup gets, border included.
pub const MAX_WIDTH: u16 = 80;
/// Most list rows the popup shows at once.
pub const MAX_LIST_ROWS: u16 = 20;

/// Cells kept free on every side of the popup.
const MARGIN: u16 = 2;
/// Cells between a row's label and its detail.
const DETAIL_GAP: u16 = 2;

/// What the picker popup shows.
#[derive(Debug, Clone, Copy)]
pub struct PickerView<'a> {
    /// Popup title, for example `Connect` or `Bookmarks`.
    pub title: &'a str,
    /// The filter text typed so far.
    pub filter: &'a str,
    /// Tab names; empty hides the tab row.
    pub tabs: &'a [&'a str],
    /// Index into `tabs` of the selected tab.
    pub active_tab: usize,
    /// The rows of the active tab after filtering, in display order.
    pub rows: &'a [PickerRow<'a>],
    /// Index into `rows` of the highlighted row.
    pub cursor: usize,
    /// Index into `rows` of the first drawn row.
    pub first_visible: usize,
}

/// One selectable row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerRow<'a> {
    /// The host alias, `Local`, or a bookmark location.
    pub label: &'a str,
    /// Dimmed text after the label, for example `user@hostname`; may be
    /// empty.
    pub detail: &'a str,
}

/// Draws the popup centered in `screen` and returns its hit data
/// (`first_visible` and `row_count` as drawn).
pub fn render(view: &PickerView<'_>, screen: Rect, buf: &mut Buffer) -> ListHit {
    let tab_rows = u16::from(!view.tabs.is_empty());
    // Border, optional tab row, filter line.
    let chrome = 2 + tab_rows + 1;
    let width = screen.width.saturating_sub(2 * MARGIN).min(MAX_WIDTH);
    let height = screen
        .height
        .saturating_sub(2 * MARGIN)
        .min(chrome + MAX_LIST_ROWS);
    let area = screen.centered(Constraint::Length(width), Constraint::Length(height));

    crate::ui::widgets::overlay::clear(area, buf);
    let block = Block::bordered()
        .title(format!(" {} ", view.title))
        .title_bottom(Line::raw(format!(" {} ", count_label(view.rows.len()))).right_aligned());
    let inner = block.inner(area);
    block.render(area, buf);

    let mut y = inner.y;
    if tab_rows > 0 && y < inner.bottom() {
        draw_tabs(
            view.tabs,
            view.active_tab,
            Rect {
                y,
                height: 1,
                ..inner
            },
            buf,
        );
        y += 1;
    }
    if y < inner.bottom() {
        draw_filter(
            view.filter,
            Rect {
                y,
                height: 1,
                ..inner
            },
            buf,
        );
        y += 1;
    }
    let rows = Rect {
        y,
        height: inner.bottom() - y,
        ..inner
    };

    if view.rows.is_empty() && rows.height > 0 {
        buf.set_stringn(
            rows.x + 1,
            rows.y,
            "No matches",
            usize::from(rows.width.saturating_sub(1)),
            Style::new().add_modifier(Modifier::DIM),
        );
    }
    let drawn = view.rows.iter().enumerate().skip(view.first_visible);
    let mut row_count = 0;
    for ((index, row), y) in drawn.zip(rows.y..rows.bottom()) {
        draw_row(
            row,
            index == view.cursor,
            Rect {
                y,
                height: 1,
                ..rows
            },
            buf,
        );
        row_count += 1;
    }

    ListHit {
        area,
        rows,
        first_visible: view.first_visible,
        row_count,
    }
}

fn count_label(count: usize) -> String {
    match count {
        1 => "1 entry".to_owned(),
        count => format!("{count} entries"),
    }
}

/// The tab captions: the first nine carry their digit key.
fn tab_captions(tabs: &[&str]) -> Vec<String> {
    tabs.iter()
        .enumerate()
        .map(|(index, name)| match index {
            0..=8 => format!(" {} {name} ", index + 1),
            _ => format!(" {name} "),
        })
        .collect()
}

fn draw_tabs(tabs: &[&str], active: usize, line: Rect, buf: &mut Buffer) {
    let captions = tab_captions(tabs);
    let widths: Vec<u16> = captions.iter().map(|caption| text_width(caption)).collect();
    // Skip leading tabs until the active one fits.
    let last = active.min(widths.len() - 1);
    let mut first = 0;
    while first < last && widths[first..=last].iter().sum::<u16>() > line.width {
        first += 1;
    }
    let spans: Vec<Span<'_>> = captions
        .iter()
        .enumerate()
        .skip(first)
        .map(|(index, caption)| {
            let style = if index == active {
                Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::new()
            };
            Span::styled(caption.as_str(), style)
        })
        .collect();
    buf.set_line(line.x, line.y, &Line::from(spans), line.width);
}

fn draw_filter(filter: &str, line: Rect, buf: &mut Buffer) {
    let caret = Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED));
    let spans = if filter.is_empty() {
        vec![
            Span::raw("/"),
            caret,
            Span::styled(" type to filter", Style::new().add_modifier(Modifier::DIM)),
        ]
    } else {
        // Keep the end of a long filter (where typing happens) in view.
        let budget = usize::from(line.width.saturating_sub(2));
        let visible = tail_within(filter, budget);
        vec![Span::raw("/"), Span::raw(visible), caret]
    };
    buf.set_line(line.x, line.y, &Line::from(spans), line.width);
}

/// The longest suffix of `text` at most `width` cells wide.
fn tail_within(text: &str, width: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        let mut encoded = [0; 4];
        used += Line::raw(&*ch.encode_utf8(&mut encoded)).width();
        if used > width {
            break;
        }
        start = index;
    }
    &text[start..]
}

fn draw_row(row: &PickerRow<'_>, is_cursor: bool, line: Rect, buf: &mut Buffer) {
    let base = if is_cursor {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    };
    buf.set_style(line, base);
    let (end, _) = buf.set_stringn(
        line.x + 1,
        line.y,
        row.label,
        usize::from(line.width.saturating_sub(1)),
        base,
    );
    if !row.detail.is_empty() {
        let detail_x = end + DETAIL_GAP;
        if detail_x < line.right() {
            buf.set_stringn(
                detail_x,
                line.y,
                row.detail,
                usize::from(line.right() - detail_x),
                base.add_modifier(Modifier::DIM),
            );
        }
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
    use crate::ui::action::ClickTarget;
    use crate::ui::hit::HitMap;

    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    /// The column where `needle` starts on row `y` (one cell per char).
    fn column_of(buf: &Buffer, y: u16, needle: &str) -> u16 {
        let cells: Vec<&str> = (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect();
        let needle: Vec<String> = needle.chars().map(String::from).collect();
        let start = cells
            .windows(needle.len())
            .position(|window| window.iter().zip(&needle).all(|(cell, ch)| cell == ch))
            .unwrap();
        u16::try_from(start).unwrap()
    }

    fn draw(view: &PickerView<'_>, width: u16, height: u16) -> (Buffer, ListHit) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hit = None;
        terminal
            .draw(|frame| hit = Some(render(view, frame.area(), frame.buffer_mut())))
            .unwrap();
        (terminal.backend().buffer().clone(), hit.unwrap())
    }

    const ROWS: [PickerRow<'static>; 3] = [
        PickerRow {
            label: "Local",
            detail: "",
        },
        PickerRow {
            label: "web",
            detail: "deploy@web.example.com",
        },
        PickerRow {
            label: "db",
            detail: "",
        },
    ];

    #[test]
    fn host_picker_shows_tabs_filter_and_rows() {
        let tabs = ["All", "prod", "Other"];
        let view = PickerView {
            title: "Connect",
            filter: "",
            tabs: &tabs,
            active_tab: 1,
            rows: &ROWS,
            cursor: 1,
            first_visible: 0,
        };
        let (buf, hit) = draw(&view, 100, 30);
        // 80 wide, 2 + 1 + 1 + 20 tall, centered.
        assert_eq!(hit.area, Rect::new(10, 3, 80, 24));
        assert!(row_text(&buf, 3).contains(" Connect "));
        let tabs_line = row_text(&buf, 4);
        assert!(
            tabs_line.contains(" 1 All  2 prod  3 Other "),
            "{tabs_line:?}"
        );
        let active_x = column_of(&buf, 4, "2 prod");
        assert!(buf[(active_x, 4)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(column_of(&buf, 4, "1 All"), 4)]
            .modifier
            .contains(Modifier::REVERSED));
        assert!(row_text(&buf, 5).contains("/  type to filter"));

        assert_eq!(hit.rows, Rect::new(11, 6, 78, 20));
        assert_eq!(hit.row_count, 3);
        assert!(row_text(&buf, 6).contains(" Local "));
        let web = row_text(&buf, 7);
        assert!(web.contains(" web  deploy@web.example.com"), "{web:?}");
        assert!(buf[(12, 7)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(17, 7)].modifier.contains(Modifier::DIM));
        assert!(!buf[(12, 6)].modifier.contains(Modifier::REVERSED));
        assert!(row_text(&buf, 26).contains(" 3 entries "));

        let map = HitMap {
            picker: Some(hit),
            ..HitMap::default()
        };
        assert_eq!(map.resolve(20, 8), Some(ClickTarget::PickerRow(2)));
        assert_eq!(map.resolve(20, 9), None);
    }

    #[test]
    fn bookmark_list_has_no_tab_row() {
        let view = PickerView {
            title: "Bookmarks",
            filter: "srv",
            tabs: &[],
            active_tab: 0,
            rows: &ROWS[1..2],
            cursor: 0,
            first_visible: 0,
        };
        let (buf, hit) = draw(&view, 100, 30);
        assert_eq!(hit.area, Rect::new(10, 4, 80, 23));
        assert!(row_text(&buf, 5).contains("/srv "));
        assert!(buf[(15, 5)].modifier.contains(Modifier::REVERSED));
        assert_eq!(hit.rows.y, 6);
        assert_eq!(hit.row_count, 1);
        assert!(row_text(&buf, 26).contains(" 1 entry "));
    }

    #[test]
    fn small_screen_clips_rows_and_reports_scroll() {
        let view = PickerView {
            title: "Connect",
            filter: "",
            tabs: &[],
            active_tab: 0,
            rows: &ROWS,
            cursor: 2,
            first_visible: 1,
        };
        // 10 rows: 6 for the popup, leaving 3 list rows of which the last
        // two entries fill two.
        let (buf, hit) = draw(&view, 40, 10);
        assert_eq!(hit.area, Rect::new(2, 2, 36, 6));
        assert_eq!(hit.rows, Rect::new(3, 4, 34, 3));
        assert_eq!(hit.first_visible, 1);
        assert_eq!(hit.row_count, 2);
        assert!(row_text(&buf, 4).contains(" web "));
        assert!(row_text(&buf, 5).contains(" db "));
        assert!(buf[(4, 5)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn empty_rows_say_no_matches() {
        let view = PickerView {
            title: "Connect",
            filter: "zzz",
            tabs: &["All"],
            active_tab: 0,
            rows: &[],
            cursor: 0,
            first_visible: 0,
        };
        let (buf, hit) = draw(&view, 60, 20);
        assert_eq!(hit.row_count, 0);
        assert!(row_text(&buf, hit.rows.y).contains("No matches"));
    }

    #[test]
    fn active_tab_scrolls_into_view() {
        let tabs = [
            "All", "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
            "india", "Other",
        ];
        let view = PickerView {
            title: "Connect",
            filter: "",
            tabs: &tabs,
            active_tab: 10,
            rows: &ROWS,
            cursor: 0,
            first_visible: 0,
        };
        let (buf, _) = draw(&view, 50, 20);
        let line = row_text(&buf, 3);
        assert!(line.contains(" Other "), "{line:?}");
        assert!(!line.contains(" 1 All "));
        // Tabs past the ninth have no digit key.
        assert!(line.contains(" india "));
    }

    #[test]
    fn long_filter_keeps_its_end_visible() {
        assert_eq!(tail_within("abcdef", 3), "def");
        assert_eq!(tail_within("abc", 10), "abc");
        assert_eq!(tail_within("a\u{4f60}\u{597d}", 3), "\u{597d}");
    }
}
