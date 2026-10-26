//! Preview widget: highlighted text, rendered Markdown, binary summary, or
//! image, full screen or as quick view.
//!
//! A bordered block titled with the file name (plus `truncated` when the
//! text was cut, and `raw` / `rendered` for Markdown). Text scrolls by
//! line; binary files show size, type, and mode; images render through the
//! `ThreadProtocol` (resized in the background).
//!
//! The caller puts `raw` / `rendered` into `title`; the widget appends
//! `(truncated)` when `truncated` is set. Text draws only the visible lines
//! (no copy of the whole text per frame), with `scroll` clamped so the last
//! page stays full, and the bottom border shows the visible line range.
//! Images fit the inner area keeping their aspect ratio; the first frame
//! after a size change draws nothing while the resize request is in flight.
//! The viewer region in the `HitMap` is recorded by render from the same
//! `area`.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct PreviewView<'a> {
//!     pub title: &'a str,
//!     pub body: PreviewBody<'a>,
//!     pub scroll: usize,
//!     pub truncated: bool,
//! }
//!
//! pub enum PreviewBody<'a> {
//!     Loading,
//!     /// Highlighted or rendered text, produced by `preview::Highlighter`
//!     /// or `preview::render_markdown`.
//!     Text(&'a Text<'static>),
//!     Binary(&'a Entry),
//!     Image(&'a mut ThreadProtocol),
//!     Failed(&'a str),
//! }
//!
//! pub fn render(view: PreviewView<'_>, area: Rect, buf: &mut Buffer);
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, StatefulWidget, Widget, Wrap};
use ratatui_image::thread::ThreadProtocol;
use ratatui_image::StatefulImage;

use super::sync_view::format_size;
use crate::vfs::{Entry, EntryKind, SpecialKind};

/// What the preview shows.
pub struct PreviewView<'a> {
    /// The file name, plus `raw` / `rendered` for Markdown.
    pub title: &'a str,
    /// The content.
    pub body: PreviewBody<'a>,
    /// First text line shown; clamped to the content.
    pub scroll: usize,
    /// Whether the text was cut at the read limit.
    pub truncated: bool,
}

/// The preview content.
pub enum PreviewBody<'a> {
    /// The fetch is in flight.
    Loading,
    /// Highlighted or rendered text, produced by `preview::Highlighter`
    /// or `preview::render_markdown`.
    Text(&'a Text<'static>),
    /// A binary file's attributes.
    Binary(&'a Entry),
    /// A decoded image.
    Image(&'a mut ThreadProtocol),
    /// The load failed with this message.
    Failed(&'a str),
}

/// Draws the preview filling `area`.
pub fn render(view: PreviewView<'_>, area: Rect, buf: &mut Buffer) {
    Clear.render(area, buf);
    let title = if view.truncated {
        format!(" {} (truncated) ", view.title)
    } else {
        format!(" {} ", view.title)
    };
    let mut block = Block::bordered().title(title);
    if let PreviewBody::Text(text) = &view.body {
        let inner_height = block.inner(area).height;
        let first = clamp_scroll(view.scroll, text.lines.len(), inner_height);
        if let Some(range) = range_label(first, text.lines.len(), inner_height) {
            block = block.title_bottom(Line::raw(range).right_aligned());
        }
    }
    let inner = block.inner(area);
    block.render(area, buf);

    match view.body {
        PreviewBody::Loading => {
            buf.set_stringn(
                inner.x + 1,
                inner.y,
                "Loading...",
                usize::from(inner.width.saturating_sub(1)),
                Style::new().add_modifier(Modifier::DIM),
            );
        }
        PreviewBody::Text(text) => {
            let first = clamp_scroll(view.scroll, text.lines.len(), inner.height);
            for (line, y) in text.lines.iter().skip(first).zip(inner.y..inner.bottom()) {
                buf.set_line(inner.x, y, line, inner.width);
            }
        }
        PreviewBody::Binary(entry) => {
            Paragraph::new(binary_summary(entry))
                .wrap(Wrap { trim: false })
                .render(inner, buf);
        }
        PreviewBody::Image(protocol) => {
            StatefulImage::<ThreadProtocol>::default().render(inner, buf, protocol);
        }
        PreviewBody::Failed(message) => {
            Paragraph::new(message)
                .style(Style::new().fg(Color::Red))
                .wrap(Wrap { trim: false })
                .render(inner, buf);
        }
    }
}

/// The first line to draw so that the last page stays full.
fn clamp_scroll(scroll: usize, line_count: usize, height: u16) -> usize {
    scroll.min(line_count.saturating_sub(usize::from(height)))
}

/// ` 41-80/300 ` for the bottom border, or `None` when everything fits.
fn range_label(first: usize, line_count: usize, height: u16) -> Option<String> {
    let last = (first + usize::from(height)).min(line_count);
    (line_count > usize::from(height)).then(|| format!(" {}-{last}/{line_count} ", first + 1))
}

/// The lines shown for a binary file.
fn binary_summary(entry: &Entry) -> Text<'static> {
    let size = match entry.size {
        Some(bytes) if bytes >= 1024 => format!("{} ({bytes} bytes)", format_size(bytes)),
        Some(bytes) => format_size(bytes),
        None => "unknown".to_owned(),
    };
    let mode = entry.mode.map_or_else(
        || "unknown".to_owned(),
        |mode| format!("{} ({mode:04o})", permissions(mode)),
    );
    let label = Style::new().add_modifier(Modifier::BOLD);
    Text::from(vec![
        Line::styled(
            "Binary file, content not shown",
            Style::new().add_modifier(Modifier::DIM),
        ),
        Line::raw(""),
        Line::from(vec![Span::styled("Size  ", label), size.into()]),
        Line::from(vec![
            Span::styled("Type  ", label),
            kind_label(&entry.kind).into(),
        ]),
        Line::from(vec![Span::styled("Mode  ", label), mode.into()]),
    ])
}

fn kind_label(kind: &EntryKind) -> String {
    match kind {
        EntryKind::File => "regular file".to_owned(),
        EntryKind::Dir => "directory".to_owned(),
        EntryKind::Symlink { target, .. } => format!("symlink -> {}", target.display()),
        EntryKind::Special(special) => match special {
            SpecialKind::Fifo => "named pipe",
            SpecialKind::Socket => "socket",
            SpecialKind::BlockDevice => "block device",
            SpecialKind::CharDevice => "character device",
            SpecialKind::Unknown => "unknown type",
        }
        .to_owned(),
    }
}

/// `rwxr-xr-x`, with `s` / `S` and `t` / `T` for setuid, setgid, sticky.
fn permissions(mode: u32) -> String {
    const SPECIAL: [(u32, u32, char); 3] = [
        (0o4000, 0o100, 's'),
        (0o2000, 0o010, 's'),
        (0o1000, 0o001, 't'),
    ];
    let mut text: Vec<char> = [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ]
    .iter()
    .zip("rwxrwxrwx".chars())
    .map(|(&bit, ch)| if mode & bit != 0 { ch } else { '-' })
    .collect();
    for (slot, (flag, exec, ch)) in [2, 5, 8].into_iter().zip(SPECIAL) {
        if mode & flag != 0 {
            text[slot] = if mode & exec != 0 {
                ch
            } else {
                ch.to_ascii_uppercase()
            };
        }
    }
    text.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use image::{DynamicImage, Rgba, RgbaImage};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use ratatui_image::picker::Picker;

    use super::*;

    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn draw(terminal: &mut Terminal<TestBackend>, view: PreviewView<'_>) -> Buffer {
        let mut view = Some(view);
        terminal
            .draw(|frame| {
                let view = view.take().unwrap();
                render(view, frame.area(), frame.buffer_mut());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn numbered(count: usize) -> Text<'static> {
        Text::from(
            (1..=count)
                .map(|n| Line::styled(format!("line {n}"), Style::new().fg(Color::Green)))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn text_scrolls_and_shows_the_range() {
        let text = numbered(10);
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        let buf = draw(
            &mut terminal,
            PreviewView {
                title: "notes.txt",
                body: PreviewBody::Text(&text),
                scroll: 3,
                truncated: true,
            },
        );
        assert!(row_text(&buf, 0).contains(" notes.txt (truncated) "));
        assert!(row_text(&buf, 1).starts_with("\u{2502}line 4 "));
        assert!(row_text(&buf, 4).starts_with("\u{2502}line 7 "));
        assert!(row_text(&buf, 5).contains(" 4-7/10 "));
        // Styles from the highlighter are kept.
        assert_eq!(buf[(1, 1)].fg, Color::Green);
    }

    #[test]
    fn scroll_past_the_end_keeps_the_last_page_full() {
        let text = numbered(10);
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        let buf = draw(
            &mut terminal,
            PreviewView {
                title: "notes.txt",
                body: PreviewBody::Text(&text),
                scroll: 500,
                truncated: false,
            },
        );
        assert!(row_text(&buf, 0).contains(" notes.txt "));
        assert!(!row_text(&buf, 0).contains("truncated"));
        assert!(row_text(&buf, 1).starts_with("\u{2502}line 7 "));
        assert!(row_text(&buf, 4).starts_with("\u{2502}line 10 "));
        assert!(row_text(&buf, 5).contains(" 7-10/10 "));
    }

    #[test]
    fn short_text_has_no_range_label() {
        let text = numbered(2);
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        let buf = draw(
            &mut terminal,
            PreviewView {
                title: "README.md rendered",
                body: PreviewBody::Text(&text),
                scroll: 1,
                truncated: false,
            },
        );
        assert!(row_text(&buf, 0).contains(" README.md rendered "));
        assert!(row_text(&buf, 1).starts_with("\u{2502}line 1 "));
        assert!(!row_text(&buf, 5).contains('/'));
    }

    #[test]
    fn binary_shows_size_type_and_mode() {
        let entry = Entry {
            name: OsString::from("app"),
            kind: EntryKind::File,
            size: Some(1_048_576),
            mtime: None,
            mode: Some(0o4755),
        };
        let mut terminal = Terminal::new(TestBackend::new(50, 8)).unwrap();
        let buf = draw(
            &mut terminal,
            PreviewView {
                title: "app",
                body: PreviewBody::Binary(&entry),
                scroll: 0,
                truncated: false,
            },
        );
        assert!(row_text(&buf, 1).contains("Binary file, content not shown"));
        assert!(row_text(&buf, 3).contains("Size  1.0 MiB (1048576 bytes)"));
        assert!(row_text(&buf, 4).contains("Type  regular file"));
        assert!(row_text(&buf, 5).contains("Mode  rwsr-xr-x (4755)"));
    }

    #[test]
    fn permissions_cover_special_bits() {
        assert_eq!(permissions(0o644), "rw-r--r--");
        assert_eq!(permissions(0o755), "rwxr-xr-x");
        assert_eq!(permissions(0o2750), "rwxr-s---");
        assert_eq!(permissions(0o1777), "rwxrwxrwt");
        assert_eq!(permissions(0o1666), "rw-rw-rwT");
        assert_eq!(permissions(0o4644), "rwSr--r--");
        assert_eq!(
            kind_label(&EntryKind::Symlink {
                target: PathBuf::from("/opt/x"),
                target_is_dir: false
            }),
            "symlink -> /opt/x"
        );
    }

    #[test]
    fn loading_and_failure_messages() {
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
        let buf = draw(
            &mut terminal,
            PreviewView {
                title: "big.png",
                body: PreviewBody::Loading,
                scroll: 0,
                truncated: false,
            },
        );
        assert!(row_text(&buf, 1).contains("Loading..."));

        let buf = draw(
            &mut terminal,
            PreviewView {
                title: "big.png",
                body: PreviewBody::Failed("image larger than the 20 MiB preview limit"),
                scroll: 0,
                truncated: false,
            },
        );
        // Wrapped to the inner width.
        assert!(row_text(&buf, 1).contains("image larger than the 20 MiB"));
        assert!(row_text(&buf, 2).contains("preview limit"));
        assert_eq!(buf[(1, 1)].fg, Color::Red);
    }

    #[test]
    fn image_renders_after_the_background_resize() {
        let pixels = RgbaImage::from_fn(16, 16, |x, y| {
            Rgba([
                u8::try_from(x * 16).unwrap(),
                u8::try_from(y * 16).unwrap(),
                200,
                255,
            ])
        });
        let protocol = Picker::halfblocks().new_resize_protocol(DynamicImage::ImageRgba8(pixels));
        let (tx, rx) = mpsc::channel();
        let mut state = ThreadProtocol::new(tx, Some(protocol));
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        let painted = |buf: &Buffer| {
            let inner = Rect::new(1, 1, 18, 8);
            inner
                .positions()
                .filter(|&position| buf[position].symbol() != " ")
                .count()
        };

        // First frame: the protocol needs a resize for this area, which is
        // handed to the worker; nothing is drawn yet.
        let buf = draw(
            &mut terminal,
            PreviewView {
                title: "logo.png",
                body: PreviewBody::Image(&mut state),
                scroll: 0,
                truncated: false,
            },
        );
        assert_eq!(painted(&buf), 0);
        let request = rx.try_recv().expect("a resize request");

        // The worker's result arrives; the next frame draws the image.
        let response = request.resize_encode().unwrap();
        assert!(state.update_resized_protocol(response));
        let buf = draw(
            &mut terminal,
            PreviewView {
                title: "logo.png",
                body: PreviewBody::Image(&mut state),
                scroll: 0,
                truncated: false,
            },
        );
        assert!(row_text(&buf, 0).contains(" logo.png "));
        assert!(painted(&buf) > 0);
        assert!(
            rx.try_recv().is_err(),
            "no further resize for the same area"
        );
    }
}
