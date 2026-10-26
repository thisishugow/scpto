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
//! Row columns: a selection mark (`*`), the name, the size, and the
//! modification time in local time (`YYYY-MM-DD HH:MM`). Narrow panes drop
//! the time column first, then the size column. Names are cut with an
//! ellipsis by display width (wide CJK characters count two cells) and keep
//! their type suffix: `/` for directories and symlinks to directories, `@`
//! for other symlinks, `|` for FIFOs, `=` for sockets. Sizes are shown for
//! regular files only.
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

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Widget};

use crate::ui::action::{DropTarget, PaneSide};
use crate::ui::hit::PaneHit;
use crate::ui::pane::Pane;
use crate::ui::widgets::format::{format_local_mtime, format_size, SIZE_WIDTH};
use crate::ui::widgets::text_input::{cell_width, fit_end, fit_start, sanitize};
use crate::vfs::{Entry, EntryKind, SpecialKind};

/// Rows of `area` that are not list rows: top border, path line, bottom
/// border. `CoreState::page_height` subtracts the same number.
pub const CHROME_ROWS: u16 = 3;

/// Cells of the selection mark and the space after it.
const MARK_WIDTH: u16 = 2;
/// Cells of the time column (`2026-10-26 14:03`).
const MTIME_WIDTH: u16 = 16;
/// Narrowest name column kept before dropping the size and time columns.
const MIN_NAME_WIDTH: u16 = 12;

const BORDER_FOCUSED: Style = Style::new().fg(Color::Cyan);
const BORDER_UNFOCUSED: Style = Style::new().fg(Color::DarkGray);
const BORDER_DROP: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
const TITLE_ENDPOINT: Style = Style::new().add_modifier(Modifier::BOLD);
const TITLE_STATUS: Style = Style::new().fg(Color::Yellow);
const PATH_FOCUSED: Style = Style::new().add_modifier(Modifier::BOLD);
const PATH_UNFOCUSED: Style = Style::new();
const PLACEHOLDER: Style = Style::new().fg(Color::DarkGray);
const ROW_FILE: Style = Style::new();
const ROW_DIR: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
const ROW_SYMLINK: Style = Style::new().fg(Color::Cyan);
const ROW_SPECIAL: Style = Style::new().fg(Color::Magenta);
const ROW_SELECTED: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
const ROW_CURSOR: Style = Style::new().add_modifier(Modifier::REVERSED);
const ROW_DROP: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Yellow)
    .add_modifier(Modifier::BOLD);

/// One pane to draw, borrowed from the app state.
#[derive(Debug, Clone, Copy)]
pub struct PaneView<'a> {
    /// Which pane this is (recorded in the hit data).
    pub side: PaneSide,
    /// Read through its public fields (`visible`, `entries`, `cursor`,
    /// `scroll`, `selection`, `filter`, `cwd`, `endpoint`).
    pub pane: &'a Pane,
    /// Whether the pane has focus (border color, cursor highlight).
    pub focused: bool,
    /// Connection status for the title, for example `connecting` or
    /// `reconnecting in 4 s`.
    pub status: Option<&'a str>,
    /// The current drag's target when it is this pane:
    /// `DropTarget::Pane` highlights the border, `DropTarget::Folder`
    /// the row.
    pub drop_target: Option<DropTarget>,
}

/// Draws the pane, scrolled to `pane.scroll`, and returns its hit data.
pub fn render(view: &PaneView<'_>, area: Rect, buf: &mut Buffer) -> PaneHit {
    let pane = view.pane;
    let (border_drop, drop_row) = match view.drop_target {
        Some(DropTarget::Pane(side)) => (side == view.side, None),
        Some(DropTarget::Folder { side, index }) => (false, (side == view.side).then_some(index)),
        Some(DropTarget::Invalid) | None => (false, None),
    };
    let border = if border_drop {
        BORDER_DROP
    } else if view.focused {
        BORDER_FOCUSED
    } else {
        BORDER_UNFOCUSED
    };
    let title_width = area.width.saturating_sub(2);
    Block::bordered()
        .border_style(border)
        .title(title(view, title_width))
        .title_bottom(footer(pane, title_width))
        .render(area, buf);

    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let rows = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if !inner.is_empty() {
        draw_path(view, inner, buf);
    }

    let first_visible = pane.scroll.min(pane.visible.len());
    let drawn = (pane.visible.len() - first_visible).min(usize::from(rows.height));
    let mut enterable = Vec::with_capacity(drawn);
    if rows.width > 0 {
        let columns = Columns::for_width(rows.width);
        for offset in 0..drawn {
            let index = first_visible + offset;
            let entry = &pane.entries[pane.visible[index]];
            let row = RowState {
                selected: pane.selection.contains(&entry.name),
                cursor: view.focused && index == pane.cursor,
                drop: drop_row == Some(index),
            };
            // `offset < rows.height`, a u16.
            let y = rows.y + offset as u16;
            draw_row(entry, row, columns, rows.x, y, buf);
            enterable.push(entry.is_enterable());
        }
    }
    if drawn == 0 && pane.cwd.is_some() && !rows.is_empty() {
        let text = if pane.filter.is_empty() {
            "empty directory"
        } else {
            "no matches"
        };
        buf.set_stringn(
            rows.x + 1,
            rows.y,
            fit_end(text, rows.width.saturating_sub(1)),
            usize::from(rows.width),
            PLACEHOLDER,
        );
    }

    PaneHit {
        side: view.side,
        area,
        rows,
        first_visible,
        enterable,
    }
}

/// The top border title: endpoint, then the status when set.
fn title(view: &PaneView<'_>, width: u16) -> Line<'static> {
    let endpoint = format!(" {} ", sanitize(&view.pane.endpoint.to_string()));
    let endpoint_width = cell_width(&endpoint);
    if endpoint_width >= width {
        return Line::from(Span::styled(
            fit_end(&endpoint, width).into_owned(),
            TITLE_ENDPOINT,
        ));
    }
    let mut spans = vec![Span::styled(endpoint, TITLE_ENDPOINT)];
    if let Some(status) = view.status {
        let status = format!("{} ", sanitize(status));
        spans.push(Span::styled(
            fit_end(&status, width - endpoint_width).into_owned(),
            TITLE_STATUS,
        ));
    }
    Line::from(spans)
}

/// The bottom border: the filter while one is set, else the counts.
fn footer(pane: &Pane, width: u16) -> Line<'static> {
    let text = if pane.filter.is_empty() {
        let items = pane.visible.len();
        let noun = if items == 1 { "item" } else { "items" };
        let selected = if pane.selection.is_empty() {
            0
        } else {
            pane.visible
                .iter()
                .filter(|&&index| pane.selection.contains(&pane.entries[index].name))
                .count()
        };
        if selected == 0 {
            format!(" {items} {noun} ")
        } else {
            format!(" {items} {noun}, {selected} selected ")
        }
    } else {
        format!(" /{} ", sanitize(&pane.filter))
    };
    Line::from(fit_end(&text, width).into_owned())
}

/// The path line: the current directory, left-truncated.
fn draw_path(view: &PaneView<'_>, inner: Rect, buf: &mut Buffer) {
    let Some(cwd) = &view.pane.cwd else {
        return;
    };
    let path = cwd.to_string_lossy();
    let path = sanitize(&path);
    let style = if view.focused {
        PATH_FOCUSED
    } else {
        PATH_UNFOCUSED
    };
    buf.set_stringn(
        inner.x,
        inner.y,
        fit_start(&path, inner.width),
        usize::from(inner.width),
        style,
    );
}

/// Which optional columns fit, and the name column width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Columns {
    name: u16,
    size: bool,
    mtime: bool,
}

impl Columns {
    fn for_width(width: u16) -> Columns {
        let size = 1 + SIZE_WIDTH;
        let mtime = 1 + MTIME_WIDTH;
        let name_room = width.saturating_sub(MARK_WIDTH);
        if name_room >= MIN_NAME_WIDTH + size + mtime {
            Columns {
                name: name_room - size - mtime,
                size: true,
                mtime: true,
            }
        } else if name_room >= MIN_NAME_WIDTH + size {
            Columns {
                name: name_room - size,
                size: true,
                mtime: false,
            }
        } else {
            Columns {
                name: name_room,
                size: false,
                mtime: false,
            }
        }
    }
}

/// How one drawn row is highlighted.
#[derive(Debug, Clone, Copy)]
struct RowState {
    selected: bool,
    cursor: bool,
    drop: bool,
}

fn draw_row(entry: &Entry, state: RowState, columns: Columns, x: u16, y: u16, buf: &mut Buffer) {
    let mut line = String::new();
    line.push_str(if state.selected { "* " } else { "  " });
    push_padded(&mut line, &name_cell(entry, columns.name), columns.name);
    if columns.size {
        let size = match (&entry.kind, entry.size) {
            (EntryKind::File, Some(size)) => format_size(size),
            _ => String::new(),
        };
        line.push(' ');
        line.push_str(&format!("{size:>width$}", width = usize::from(SIZE_WIDTH)));
    }
    if columns.mtime {
        let mtime = entry.mtime.and_then(format_local_mtime).unwrap_or_default();
        line.push(' ');
        line.push_str(&format!(
            "{mtime:<width$}",
            width = usize::from(MTIME_WIDTH)
        ));
    }

    let style = if state.drop {
        ROW_DROP
    } else {
        let mut style = kind_style(&entry.kind);
        if state.selected {
            style = style.patch(ROW_SELECTED);
        }
        if state.cursor {
            style = style.patch(ROW_CURSOR);
        }
        style
    };
    let width = MARK_WIDTH
        + columns.name
        + if columns.size { 1 + SIZE_WIDTH } else { 0 }
        + if columns.mtime { 1 + MTIME_WIDTH } else { 0 };
    buf.set_stringn(x, y, line, usize::from(width), style);
}

/// The name with its type suffix, cut to `width` cells keeping the suffix.
fn name_cell(entry: &Entry, width: u16) -> String {
    let suffix = match &entry.kind {
        _ if entry.is_enterable() => "/",
        EntryKind::Symlink { .. } => "@",
        EntryKind::Special(SpecialKind::Fifo) => "|",
        EntryKind::Special(SpecialKind::Socket) => "=",
        EntryKind::File | EntryKind::Dir | EntryKind::Special(_) => "",
    };
    let name = entry.name.to_string_lossy();
    let name = sanitize(&name);
    let suffix_width = cell_width(suffix);
    if width <= suffix_width {
        return fit_end(&format!("{name}{suffix}"), width).into_owned();
    }
    format!("{}{suffix}", fit_end(&name, width - suffix_width))
}

/// Appends `text` and pads it with spaces to `width` cells.
fn push_padded(line: &mut String, text: &str, width: u16) {
    line.push_str(text);
    let pad = width.saturating_sub(cell_width(text));
    line.extend(std::iter::repeat_n(' ', usize::from(pad)));
}

fn kind_style(kind: &EntryKind) -> Style {
    match kind {
        EntryKind::File => ROW_FILE,
        EntryKind::Dir => ROW_DIR,
        EntryKind::Symlink { .. } => ROW_SYMLINK,
        EntryKind::Special(_) => ROW_SPECIAL,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;
    use crate::ui::action::RequestId;
    use crate::ui::widgets::text_input::testing::{lines, row_text};
    use crate::vfs::Endpoint;

    fn entry(name: &str, kind: EntryKind, size: Option<u64>) -> Entry {
        Entry {
            name: OsString::from(name),
            kind,
            size,
            mtime: None,
            mode: Some(0o644),
        }
    }

    /// Builds a pane listed at `/srv/www` through the real loading path, so
    /// the entries are sorted the way the app shows them.
    fn pane(entries: Vec<Entry>) -> Pane {
        let mut pane = Pane::new(Endpoint::Local, false);
        pane.begin_list(RequestId(1), PathBuf::from("/srv/www"), None);
        let applied = pane
            .finish_list(RequestId(1), Ok(entries))
            .expect("listing applies");
        assert!(applied);
        pane
    }

    fn sample() -> Pane {
        pane(vec![
            entry("src", EntryKind::Dir, Some(4096)),
            entry(
                "current",
                EntryKind::Symlink {
                    target: PathBuf::from("releases/7"),
                    target_is_dir: true,
                },
                Some(10),
            ),
            entry(
                "latest.log",
                EntryKind::Symlink {
                    target: PathBuf::from("app.log"),
                    target_is_dir: false,
                },
                Some(7),
            ),
            entry("Cargo.toml", EntryKind::File, Some(1536)),
            entry("pipe", EntryKind::Special(SpecialKind::Fifo), None),
        ])
    }

    fn view(pane: &Pane) -> PaneView<'_> {
        PaneView {
            side: PaneSide::Left,
            pane,
            focused: true,
            status: None,
            drop_target: None,
        }
    }

    fn draw(view: &PaneView<'_>, width: u16, height: u16) -> (Buffer, PaneHit) {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let hit = render(view, area, &mut buf);
        (buf, hit)
    }

    #[test]
    fn draws_title_path_rows_and_counts() {
        let pane = sample();
        let (buf, hit) = draw(&view(&pane), 30, 9);
        let h = "\u{2500}";
        let v = "\u{2502}";
        assert_eq!(
            lines(&buf),
            [
                format!("\u{250c} Local {}\u{2510}", h.repeat(21)),
                format!("{v}/srv/www                    {v}"),
                format!("{v}  current/                  {v}"),
                format!("{v}  src/                      {v}"),
                format!("{v}  Cargo.toml         1.5 KiB{v}"),
                format!("{v}  latest.log@               {v}"),
                format!("{v}  pipe|                     {v}"),
                format!("{v}                            {v}"),
                format!("\u{2514} 5 items {}\u{2518}", h.repeat(19)),
            ]
        );
        assert_eq!(hit.side, PaneSide::Left);
        assert_eq!(hit.area, Rect::new(0, 0, 30, 9));
        assert_eq!(hit.rows, Rect::new(1, 2, 28, 6));
        assert_eq!(hit.rows.height + CHROME_ROWS, 9);
        assert_eq!(hit.first_visible, 0);
        assert_eq!(hit.enterable, [true, true, false, false, false]);
    }

    #[test]
    fn wide_pane_shows_the_time_column() {
        let mut pane = sample();
        pane.entries[3].mtime = Some(1_700_000_000);
        let (buf, _) = draw(&view(&pane), 50, 9);
        let expected_time = format_local_mtime(1_700_000_000).expect("representable");
        assert_eq!(
            row_text(&buf, 4),
            format!("\u{2502}  Cargo.toml            1.5 KiB {expected_time}\u{2502}")
        );
        // Rows without a time leave the column blank.
        assert_eq!(
            row_text(&buf, 3),
            format!("\u{2502}  src/{}\u{2502}", " ".repeat(42))
        );
    }

    #[test]
    fn cursor_is_highlighted_only_when_focused() {
        let mut pane = sample();
        pane.cursor = 3;
        let (buf, _) = draw(&view(&pane), 30, 9);
        assert!(buf[(3, 5)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(3, 4)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(0, 0)].fg, Color::Cyan);

        let unfocused = PaneView {
            focused: false,
            ..view(&pane)
        };
        let (buf, _) = draw(&unfocused, 30, 9);
        assert!(!buf[(3, 5)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(0, 0)].fg, Color::DarkGray);
    }

    #[test]
    fn selected_rows_are_marked_and_counted() {
        let mut pane = sample();
        pane.selection.insert(OsString::from("Cargo.toml"));
        pane.selection.insert(OsString::from("src"));
        // A selected name that is filtered out is not counted.
        pane.selection.insert(OsString::from("gone"));
        let (buf, _) = draw(&view(&pane), 30, 9);
        assert_eq!(
            row_text(&buf, 4),
            "\u{2502}* Cargo.toml         1.5 KiB\u{2502}"
        );
        assert_eq!(buf[(3, 4)].fg, Color::Yellow);
        assert_eq!(
            row_text(&buf, 8),
            format!(
                "\u{2514} 5 items, 2 selected {}\u{2518}",
                "\u{2500}".repeat(7)
            )
        );
    }

    #[test]
    fn filter_replaces_counts_and_empty_states_are_labelled() {
        let mut pane = sample();
        pane.filter = String::from("zz");
        pane.visible.clear();
        let (buf, hit) = draw(&view(&pane), 30, 6);
        assert!(row_text(&buf, 5).starts_with("\u{2514} /zz \u{2500}"));
        assert_eq!(
            row_text(&buf, 2).trim_end(),
            "\u{2502} no matches                 \u{2502}"
        );
        assert!(hit.enterable.is_empty());

        let empty = pane_with_no_entries();
        let (buf, _) = draw(&view(&empty), 30, 6);
        assert!(row_text(&buf, 2).starts_with("\u{2502} empty directory"));
        assert!(row_text(&buf, 5).starts_with("\u{2514} 0 items \u{2500}"));
    }

    fn pane_with_no_entries() -> Pane {
        pane(Vec::new())
    }

    #[test]
    fn scrolled_pane_reports_first_visible_and_drawn_rows() {
        let entries = (0..20)
            .map(|i| entry(&format!("dir{i:02}"), EntryKind::Dir, None))
            .collect();
        let mut pane = pane(entries);
        pane.scroll = 15;
        pane.cursor = 16;
        let (buf, hit) = draw(&view(&pane), 30, 10);
        assert_eq!(hit.first_visible, 15);
        assert_eq!(hit.enterable.len(), 5);
        assert_eq!(
            row_text(&buf, 2),
            format!("\u{2502}  dir15/{}\u{2502}", " ".repeat(20))
        );
        assert!(buf[(3, 3)].modifier.contains(Modifier::REVERSED));

        // A page taller than the remaining rows draws only those rows.
        let (_, hit) = draw(&view(&pane), 30, 30);
        assert_eq!(hit.enterable.len(), 5);
        assert_eq!(hit.rows.height, 27);
    }

    #[test]
    fn drop_targets_highlight_border_or_folder_row() {
        let pane = sample();
        let on_pane = PaneView {
            drop_target: Some(DropTarget::Pane(PaneSide::Left)),
            ..view(&pane)
        };
        let (buf, _) = draw(&on_pane, 30, 9);
        assert_eq!(buf[(0, 0)].fg, Color::Yellow);

        let on_folder = PaneView {
            drop_target: Some(DropTarget::Folder {
                side: PaneSide::Left,
                index: 1,
            }),
            ..view(&pane)
        };
        let (buf, _) = draw(&on_folder, 30, 9);
        assert_eq!(buf[(5, 3)].bg, Color::Yellow);
        assert_eq!(buf[(5, 2)].bg, Color::Reset);
        assert_eq!(buf[(0, 0)].fg, Color::Cyan);

        // A target in the other pane highlights nothing here.
        let other = PaneView {
            drop_target: Some(DropTarget::Folder {
                side: PaneSide::Right,
                index: 1,
            }),
            ..view(&pane)
        };
        let (buf, _) = draw(&other, 30, 9);
        assert_eq!(buf[(5, 3)].bg, Color::Reset);
    }

    #[test]
    fn title_shows_sudo_and_status() {
        let mut pane = sample();
        pane.endpoint = Endpoint::Remote {
            host: String::from("web"),
            sudo: true,
        };
        let status = PaneView {
            status: Some("reconnecting in 4 s"),
            ..view(&pane)
        };
        let (buf, _) = draw(&status, 40, 9);
        assert!(
            row_text(&buf, 0).starts_with("\u{250c} web [sudo] reconnecting in 4 s \u{2500}"),
            "{}",
            row_text(&buf, 0)
        );
        assert_eq!(buf[(13, 0)].fg, Color::Yellow);

        // Too narrow: the status is cut with an ellipsis.
        let (buf, _) = draw(&status, 22, 9);
        assert_eq!(
            row_text(&buf, 0),
            "\u{250c} web [sudo] reconne\u{2026}\u{2510}"
        );
    }

    #[test]
    fn long_and_wide_names_are_cut_by_display_width() {
        let mut pane = pane(vec![
            entry("a_very_long_directory_name", EntryKind::Dir, None),
            entry(
                "\u{4e2d}\u{6587}\u{6a94}\u{6848}\u{540d}\u{7a31}\u{5f88}\u{9577}.txt",
                EntryKind::File,
                Some(12),
            ),
            entry("tab\tname", EntryKind::File, Some(1)),
        ]);
        pane.cwd = Some(PathBuf::from("/home/user/projects/scpto/src/ui/widgets"));
        let (buf, _) = draw(&view(&pane), 20, 7);
        assert_eq!(
            row_text(&buf, 1),
            "\u{2502}\u{2026}to/src/ui/widgets\u{2502}"
        );
        assert_eq!(
            row_text(&buf, 2),
            "\u{2502}  a_very_long_di\u{2026}/\u{2502}"
        );
        assert_eq!(row_text(&buf, 3), "\u{2502}  tab?name        \u{2502}");
        assert_eq!(
            row_text(&buf, 4),
            "\u{2502}  \u{4e2d}\u{6587}\u{6a94}\u{6848}\u{540d}\u{7a31}\u{5f88}\u{2026} \u{2502}"
        );
    }

    #[test]
    fn degenerate_areas_do_not_panic() {
        let pane = sample();
        for (width, height) in [(0, 0), (1, 1), (2, 2), (2, 9), (30, 2), (30, 3)] {
            let (_, hit) = draw(&view(&pane), width, height);
            assert!(hit.enterable.is_empty(), "{width}x{height}");
        }
        let (_, hit) = draw(&view(&pane), 3, 4);
        assert_eq!(hit.enterable, [true]);
    }

    #[test]
    fn columns_drop_time_then_size() {
        assert_eq!(
            Columns::for_width(60),
            Columns {
                name: 32,
                size: true,
                mtime: true
            }
        );
        assert_eq!(
            Columns::for_width(39),
            Columns {
                name: 28,
                size: true,
                mtime: false
            }
        );
        assert_eq!(
            Columns::for_width(16),
            Columns {
                name: 14,
                size: false,
                mtime: false
            }
        );
    }
}
