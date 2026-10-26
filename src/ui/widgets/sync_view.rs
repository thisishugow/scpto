//! Directory comparison view widget.
//!
//! A bordered list titled with both directories; a header row names the
//! columns, then each row shows the selection mark (`*`), the relative path
//! (directories end in `/`), the left side's size and mtime, the status
//! (`<-`, `->`, `new left`, `new right`, `size`, `kind`, `=`), and the right
//! side's size and mtime. On narrow screens the mtime columns are dropped
//! first, then the size columns, so the path keeps at least
//! [`MIN_PATH_WIDTH`] cells. The cursor row is highlighted; the bottom
//! border shows the selected and shown counts.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct SyncListView<'a> {
//!     pub comparison: &'a Comparison,
//!     /// Indices into `comparison.entries` shown, in order.
//!     pub visible: &'a [usize],
//!     /// One flag per `comparison.entries` element.
//!     pub selected: &'a [bool],
//!     pub cursor: usize,
//!     pub first_visible: usize,
//! }
//!
//! pub fn render(view: &SyncListView<'_>, area: Rect, buf: &mut Buffer) -> ListHit;
//! ```
//!
//! `ListHit::first_visible` and `ListHit::row_count` count positions in
//! `visible`, like `cursor`.

use std::mem::MaybeUninit;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Widget};

use crate::sync::{Comparison, ComparisonEntry, DiffStatus};
use crate::ui::hit::ListHit;
use crate::vfs::{Entry, EntryKind, Location};

/// The narrowest the path column gets before detail columns are dropped.
pub const MIN_PATH_WIDTH: u16 = 20;

const MARK_WIDTH: u16 = 2;
/// Wide enough for the longest status, `new right`.
const STATUS_WIDTH: u16 = 9;
/// Wide enough for `1023.9 KiB`.
const SIZE_WIDTH: u16 = 10;
/// `YYYY-MM-DD HH:MM`.
const TIME_WIDTH: u16 = 16;

/// What the comparison view shows.
#[derive(Debug, Clone, Copy)]
pub struct SyncListView<'a> {
    /// The comparison result.
    pub comparison: &'a Comparison,
    /// Indices into `comparison.entries` shown, in order.
    pub visible: &'a [usize],
    /// One flag per `comparison.entries` element.
    pub selected: &'a [bool],
    /// Position in `visible` of the highlighted row.
    pub cursor: usize,
    /// Position in `visible` of the first drawn row.
    pub first_visible: usize,
}

/// Which per-side detail columns fit next to the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detail {
    None,
    Size,
    SizeAndTime,
}

impl Detail {
    /// Width of one side's detail columns, including their leading gap.
    fn side_width(self) -> u16 {
        match self {
            Detail::None => 0,
            Detail::Size => 1 + SIZE_WIDTH,
            Detail::SizeAndTime => 1 + SIZE_WIDTH + 1 + TIME_WIDTH,
        }
    }

    /// The widest detail that leaves the path at least [`MIN_PATH_WIDTH`].
    fn for_width(width: u16) -> Detail {
        [Detail::SizeAndTime, Detail::Size]
            .into_iter()
            .find(|detail| fixed_width(*detail) + MIN_PATH_WIDTH <= width)
            .unwrap_or(Detail::None)
    }
}

/// Width of everything on a row except the path.
fn fixed_width(detail: Detail) -> u16 {
    MARK_WIDTH + 2 * detail.side_width() + 1 + STATUS_WIDTH
}

/// Column positions for one drawn row.
struct Columns {
    detail: Detail,
    path_x: u16,
    path_width: u16,
    left_x: u16,
    status_x: u16,
    right_x: u16,
}

impl Columns {
    fn new(rows: Rect) -> Columns {
        let detail = Detail::for_width(rows.width);
        let path_x = rows.x + MARK_WIDTH;
        let path_width = rows.width.saturating_sub(fixed_width(detail));
        let left_x = path_x + path_width;
        let status_x = left_x + detail.side_width() + 1;
        let right_x = status_x + STATUS_WIDTH;
        Columns {
            detail,
            path_x,
            path_width,
            left_x,
            status_x,
            right_x,
        }
    }

    /// Draws one side's detail columns starting at `x` (the leading gap).
    fn draw_side(&self, buf: &mut Buffer, x: u16, y: u16, entry: Option<&Entry>, style: Style) {
        if self.detail == Detail::None {
            return;
        }
        let Some(entry) = entry else {
            return;
        };
        let size = size_cell(entry);
        let size_x = x + 1 + SIZE_WIDTH.saturating_sub(text_width(&size));
        buf.set_stringn(size_x, y, &size, usize::from(SIZE_WIDTH), style);
        if self.detail == Detail::SizeAndTime {
            if let Some(time) = entry.mtime.and_then(format_mtime) {
                let time_x = x + 1 + SIZE_WIDTH + 1;
                buf.set_stringn(time_x, y, &time, usize::from(TIME_WIDTH), style);
            }
        }
    }
}

/// Draws the comparison view filling `area` and returns its hit data.
///
/// # Panics
///
/// When `visible` holds an index outside `comparison.entries` or
/// `selected` is shorter than `comparison.entries` (caller invariants).
pub fn render(view: &SyncListView<'_>, area: Rect, buf: &mut Buffer) -> ListHit {
    Clear.render(area, buf);
    let comparison = view.comparison;
    let selected_count = view
        .visible
        .iter()
        .filter(|&&index| view.selected[index])
        .count();
    let block = Block::bordered()
        .title(format!(
            " {} <-> {} ",
            location_label(&comparison.left),
            location_label(&comparison.right)
        ))
        .title_bottom(format!(
            " {selected_count} selected, {} shown ",
            view.visible.len()
        ));
    let inner = block.inner(area);
    block.render(area, buf);

    let rows = Rect {
        y: inner.y.saturating_add(1).min(inner.bottom()),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let columns = Columns::new(rows);
    if inner.height > 0 {
        draw_header(&columns, inner.y, buf);
    }

    if view.visible.is_empty() && rows.height > 0 {
        buf.set_stringn(
            rows.x + MARK_WIDTH,
            rows.y,
            "No entries to show",
            usize::from(rows.width.saturating_sub(MARK_WIDTH)),
            Style::new().add_modifier(Modifier::DIM),
        );
    }

    let drawn = view.visible.iter().enumerate().skip(view.first_visible);
    let mut row_count = 0;
    for ((position, &index), y) in drawn.zip(rows.y..rows.bottom()) {
        draw_row(
            &columns,
            &comparison.entries[index],
            view.selected[index],
            position == view.cursor,
            rows,
            y,
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

fn draw_header(columns: &Columns, y: u16, buf: &mut Buffer) {
    let style = Style::new().add_modifier(Modifier::BOLD);
    buf.set_stringn(
        columns.path_x,
        y,
        "Path",
        usize::from(columns.path_width),
        style,
    );
    if columns.detail != Detail::None {
        buf.set_stringn(
            columns.left_x + 1,
            y,
            "Left",
            usize::from(SIZE_WIDTH),
            style,
        );
        buf.set_stringn(
            columns.right_x + 1,
            y,
            "Right",
            usize::from(SIZE_WIDTH),
            style,
        );
    }
}

fn draw_row(
    columns: &Columns,
    entry: &ComparisonEntry,
    is_selected: bool,
    is_cursor: bool,
    rows: Rect,
    y: u16,
    buf: &mut Buffer,
) {
    let base = if is_cursor {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    };
    buf.set_style(Rect::new(rows.x, y, rows.width, 1), base);
    if is_selected {
        buf.set_stringn(rows.x, y, "*", 1, base.add_modifier(Modifier::BOLD));
    }
    let mut path = entry.relative.display().to_string();
    if [&entry.left, &entry.right]
        .into_iter()
        .flatten()
        .any(|side| side.kind == EntryKind::Dir)
        && entry.status != DiffStatus::KindDiffers
    {
        path.push('/');
    }
    buf.set_stringn(
        columns.path_x,
        y,
        &path,
        usize::from(columns.path_width),
        base,
    );
    columns.draw_side(buf, columns.left_x, y, entry.left.as_ref(), base);
    let status = status_label(entry.status);
    let status_x = columns.status_x + (STATUS_WIDTH - text_width(status)) / 2;
    buf.set_stringn(
        status_x,
        y,
        status,
        usize::from(STATUS_WIDTH),
        base.add_modifier(Modifier::BOLD),
    );
    columns.draw_side(buf, columns.right_x, y, entry.right.as_ref(), base);
}

/// The status column text for a difference.
fn status_label(status: DiffStatus) -> &'static str {
    match status {
        DiffStatus::LeftOnly => "new left",
        DiffStatus::RightOnly => "new right",
        DiffStatus::LeftNewer => "->",
        DiffStatus::RightNewer => "<-",
        DiffStatus::SizeDiffers => "size",
        DiffStatus::KindDiffers => "kind",
        DiffStatus::Same => "=",
    }
}

/// `endpoint:path`, for example `web:/srv` or `Local:/Users/me`.
fn location_label(location: &Location) -> String {
    format!("{}:{}", location.endpoint, location.path.display())
}

/// The size column for one side: the size for files, the kind otherwise.
fn size_cell(entry: &Entry) -> String {
    match &entry.kind {
        EntryKind::File => entry.size.map(format_size).unwrap_or_default(),
        EntryKind::Dir => "<dir>".to_owned(),
        EntryKind::Symlink { .. } => "<link>".to_owned(),
        EntryKind::Special(_) => "<special>".to_owned(),
    }
}

fn text_width(text: &str) -> u16 {
    u16::try_from(Line::raw(text).width()).unwrap_or(u16::MAX)
}

/// Formats a byte count with binary units, for example `512 B` or
/// `12.4 MiB`. Shared by the widgets that show sizes.
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // 1023.95 and up would print as `1024.0`; promote it to the next unit.
    while value >= 1023.95 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Formats seconds since the Unix epoch as local `YYYY-MM-DD HH:MM`;
/// `None` when the C library cannot represent the time (a year beyond
/// `i32`).
fn format_mtime(seconds: i64) -> Option<String> {
    // time_t is 64-bit on every supported target (64-bit macOS and Linux).
    let time: libc::time_t = seconds;
    let mut tm = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for the duration of the call;
    // localtime_r either fills the whole `tm` and returns its address or
    // returns null without promising anything about it.
    let result = unsafe { libc::localtime_r(&time, tm.as_mut_ptr()) };
    if result.is_null() {
        return None;
    }
    // SAFETY: localtime_r returned non-null, so it initialized `tm`.
    let tm = unsafe { tm.assume_init() };
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        i64::from(tm.tm_year) + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::vfs::Endpoint;

    fn file(name: &str, size: u64, mtime: i64) -> Entry {
        Entry {
            name: OsString::from(name),
            kind: EntryKind::File,
            size: Some(size),
            mtime: Some(mtime),
            mode: Some(0o644),
        }
    }

    fn dir(name: &str) -> Entry {
        Entry {
            name: OsString::from(name),
            kind: EntryKind::Dir,
            size: None,
            mtime: Some(1_700_000_000),
            mode: Some(0o755),
        }
    }

    fn entry(
        relative: &str,
        status: DiffStatus,
        left: Option<Entry>,
        right: Option<Entry>,
    ) -> ComparisonEntry {
        ComparisonEntry {
            relative: PathBuf::from(relative),
            status,
            left,
            right,
        }
    }

    fn comparison() -> Comparison {
        Comparison {
            left: Location {
                endpoint: Endpoint::Local,
                path: PathBuf::from("/home/me/site"),
            },
            right: Location {
                endpoint: Endpoint::Remote {
                    host: "web".to_owned(),
                    sudo: false,
                },
                path: PathBuf::from("/srv/site"),
            },
            entries: vec![
                entry(
                    "assets",
                    DiffStatus::Same,
                    Some(dir("assets")),
                    Some(dir("assets")),
                ),
                entry(
                    "index.html",
                    DiffStatus::LeftNewer,
                    Some(file("index.html", 2048, 1_700_000_100)),
                    Some(file("index.html", 1536, 1_700_000_000)),
                ),
                entry(
                    "new.txt",
                    DiffStatus::LeftOnly,
                    Some(file("new.txt", 12, 1_700_000_000)),
                    None,
                ),
                entry(
                    "old.txt",
                    DiffStatus::RightOnly,
                    None,
                    Some(file("old.txt", 5, 1_700_000_000)),
                ),
            ],
        }
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn draw(view: &SyncListView<'_>, width: u16, height: u16) -> (Buffer, ListHit) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hit = None;
        terminal
            .draw(|frame| hit = Some(render(view, frame.area(), frame.buffer_mut())))
            .unwrap();
        (terminal.backend().buffer().clone(), hit.unwrap())
    }

    #[test]
    fn rows_show_mark_path_status_and_sizes() {
        let comparison = comparison();
        let visible = [1, 2, 3];
        let selected = [false, true, false, true];
        let view = SyncListView {
            comparison: &comparison,
            visible: &visible,
            selected: &selected,
            cursor: 0,
            first_visible: 0,
        };
        let (buf, hit) = draw(&view, 100, 8);
        assert!(row_text(&buf, 0).contains("Local:/home/me/site <-> web:/srv/site"));
        let header = row_text(&buf, 1);
        assert!(header.contains("Path") && header.contains("Left") && header.contains("Right"));

        let first = row_text(&buf, 2);
        assert!(first.starts_with("\u{2502}* index.html"), "{first:?}");
        assert!(first.contains("2.0 KiB") && first.contains("1.5 KiB"));
        assert!(first.contains(" -> "));
        let second = row_text(&buf, 3);
        assert!(second.starts_with("\u{2502}  new.txt"), "{second:?}");
        assert!(second.contains("new left") && second.contains("12 B"));
        let third = row_text(&buf, 4);
        assert!(third.starts_with("\u{2502}* old.txt"));
        assert!(third.contains("new right"));
        assert!(row_text(&buf, 7).contains("2 selected, 3 shown"));

        assert_eq!(hit.rows, Rect::new(1, 2, 98, 5));
        assert_eq!(hit.row_count, 3);
        assert_eq!(hit.first_visible, 0);
        // The cursor row is highlighted, the others are not.
        assert!(buf[(5, 2)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(5, 3)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn mtime_columns_appear_only_when_wide_enough() {
        let comparison = comparison();
        let visible = [1];
        let selected = [false; 4];
        let view = SyncListView {
            comparison: &comparison,
            visible: &visible,
            selected: &selected,
            cursor: 0,
            first_visible: 0,
        };
        let time = format_mtime(1_700_000_100).unwrap();
        assert_eq!(time.len(), 16);
        let (wide, _) = draw(&view, 100, 5);
        assert!(row_text(&wide, 2).contains(&time));
        let (medium, _) = draw(&view, 60, 5);
        let row = row_text(&medium, 2);
        assert!(!row.contains(&time) && row.contains("2.0 KiB") && row.contains("->"));
        let (narrow, _) = draw(&view, 32, 5);
        let row = row_text(&narrow, 2);
        assert!(!row.contains("KiB") && row.contains("index.html") && row.contains("->"));
    }

    #[test]
    fn directories_show_kind_and_trailing_slash() {
        let comparison = comparison();
        let visible = [0];
        let selected = [false; 4];
        let view = SyncListView {
            comparison: &comparison,
            visible: &visible,
            selected: &selected,
            cursor: 0,
            first_visible: 0,
        };
        let (buf, _) = draw(&view, 100, 5);
        let row = row_text(&buf, 2);
        assert!(row.contains("assets/"));
        assert_eq!(row.matches("<dir>").count(), 2);
        assert!(row.contains(" = "));
    }

    #[test]
    fn scrolled_list_reports_first_visible_and_clips_to_height() {
        let comparison = comparison();
        let visible = [0, 1, 2, 3];
        let selected = [false; 4];
        let view = SyncListView {
            comparison: &comparison,
            visible: &visible,
            selected: &selected,
            cursor: 2,
            first_visible: 1,
        };
        // Border, header, two list rows, border.
        let (buf, hit) = draw(&view, 100, 5);
        assert_eq!(hit.first_visible, 1);
        assert_eq!(hit.row_count, 2);
        assert!(row_text(&buf, 2).contains("index.html"));
        assert!(row_text(&buf, 3).contains("new.txt"));
        assert!(buf[(5, 3)].modifier.contains(Modifier::REVERSED));

        let map = crate::ui::hit::HitMap {
            sync_view: Some(hit),
            ..Default::default()
        };
        assert_eq!(
            map.resolve(5, 3),
            Some(crate::ui::action::ClickTarget::SyncRow(2))
        );
        assert_eq!(map.resolve(5, 1), None);
    }

    #[test]
    fn empty_list_says_so() {
        let comparison = comparison();
        let selected = [false; 4];
        let view = SyncListView {
            comparison: &comparison,
            visible: &[],
            selected: &selected,
            cursor: 0,
            first_visible: 0,
        };
        let (buf, hit) = draw(&view, 60, 6);
        assert!(row_text(&buf, 2).contains("No entries to show"));
        assert_eq!(hit.row_count, 0);
    }

    #[test]
    fn status_labels_match_the_contract() {
        assert_eq!(status_label(DiffStatus::LeftOnly), "new left");
        assert_eq!(status_label(DiffStatus::RightOnly), "new right");
        assert_eq!(status_label(DiffStatus::LeftNewer), "->");
        assert_eq!(status_label(DiffStatus::RightNewer), "<-");
        assert_eq!(status_label(DiffStatus::SizeDiffers), "size");
        assert_eq!(status_label(DiffStatus::KindDiffers), "kind");
        assert_eq!(status_label(DiffStatus::Same), "=");
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(1024 * 1024 - 1), "1.0 MiB");
        assert_eq!(format_size(13 * 1024 * 1024 * 1024), "13.0 GiB");
        assert_eq!(format_size(u64::MAX), "16.0 EiB");
    }
}
