//! Single-line text input widget with caret and optional masking.
//!
//! The visible window scrolls horizontally to keep the caret in view. A
//! masked field draws a fixed run of [`MASK_LEN`] `*` once it is non-empty
//! (nothing while empty), with the caret after the run: typing shows that a
//! key registered without revealing the password's length. It never copies
//! the value (the sudo password lives only in its `Zeroizing` buffer).
//!
//! This module also holds the crate-internal text helpers the widgets share
//! (control-character sanitizing, cell widths, and ellipsis truncation).
//! They measure by grapheme with ratatui's own `CellWidth`, so measured
//! widths always agree with what `Buffer::set_stringn` draws.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct TextInputView<'a> {
//!     pub value: &'a str,
//!     /// Caret position in characters, `0..=value.chars().count()`.
//!     pub caret: usize,
//!     pub masked: bool,
//!     pub focused: bool,
//! }
//!
//! /// Draws one row; returns the caret's screen position when focused.
//! pub fn render(view: &TextInputView<'_>, area: Rect, buf: &mut Buffer) -> Option<Position>;
//! ```

use std::borrow::Cow;
use std::fmt;

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Clear, Widget};

/// Placeholder drawn for a control character (tab, newline, escape, ...).
const CONTROL_PLACEHOLDER: char = '?';

/// Appended (or prepended) when text is cut to fit.
const ELLIPSIS: &str = "\u{2026}";

/// Number of `*` a non-empty masked field shows, whatever its length.
pub const MASK_LEN: usize = 8;

/// Style of the field row while it has focus.
const FOCUSED: Style = Style::new().fg(Color::White).bg(Color::DarkGray);

/// Style of the field row without focus.
const UNFOCUSED: Style = Style::new().add_modifier(Modifier::UNDERLINED);

/// A text field to draw. `Debug` never prints a masked value.
#[derive(Clone, Copy)]
pub struct TextInputView<'a> {
    /// The field content.
    pub value: &'a str,
    /// Caret position in characters, `0..=value.chars().count()`.
    pub caret: usize,
    /// Draw [`MASK_LEN`] `*` (nothing while empty) instead of the content.
    pub masked: bool,
    /// Whether the field has focus (highlighted, caret shown).
    pub focused: bool,
}

impl fmt::Debug for TextInputView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("TextInputView");
        if self.masked {
            debug.field("value", &"<redacted>");
        } else {
            debug.field("value", &self.value);
        }
        debug
            .field("caret", &self.caret)
            .field("masked", &self.masked)
            .field("focused", &self.focused)
            .finish()
    }
}

/// Draws one row; returns the caret's screen position when focused.
pub fn render(view: &TextInputView<'_>, area: Rect, buf: &mut Buffer) -> Option<Position> {
    if area.is_empty() {
        return None;
    }
    let row = Rect { height: 1, ..area };
    let style = if view.focused { FOCUSED } else { UNFOCUSED };
    Clear.render(row, buf);
    buf.set_style(row, style);

    let (display, caret): (Cow<'_, str>, usize) = if view.masked {
        // The caret sits after the fixed run, which says nothing about where
        // it is inside the hidden value.
        let mask = if view.value.is_empty() { 0 } else { MASK_LEN };
        (Cow::Owned("*".repeat(mask)), mask)
    } else {
        // `sanitize` maps characters one to one, so the caret's character
        // index is valid in `display` as well.
        (sanitize(view.value), view.caret)
    };
    let caret_byte = display
        .char_indices()
        .nth(caret)
        .map_or(display.len(), |(offset, _)| offset);

    let clusters = clusters(&display);
    // Clusters starting before the caret are drawn left of it; a caret inside
    // a cluster (between a base and its combining mark) sits after it.
    let before = clusters
        .iter()
        .take_while(|cluster| cluster.offset < caret_byte)
        .count();
    // One column stays free for the caret itself.
    let room = row.width - 1;
    let mut start = before;
    let mut caret_column = 0u16;
    while start > 0 {
        let width = clusters[start - 1].width;
        if caret_column + width > room {
            break;
        }
        caret_column += width;
        start -= 1;
    }

    let start_byte = clusters.get(start).map_or(display.len(), |c| c.offset);
    buf.set_stringn(
        row.x,
        row.y,
        &display[start_byte..],
        usize::from(row.width),
        style,
    );
    view.focused
        .then(|| Position::new(row.x + caret_column, row.y))
}

/// One grapheme cluster of a string: byte offset and drawn width.
struct Cluster {
    offset: usize,
    width: u16,
}

/// The drawn grapheme clusters of `text`, which must hold no control
/// characters (see [`sanitize`]). Zero-width clusters are dropped, exactly
/// as `Buffer::set_stringn` drops them.
fn clusters(text: &str) -> Vec<Cluster> {
    let span = Span::raw(text);
    let mut offset = 0;
    let mut clusters = Vec::new();
    for grapheme in span.styled_graphemes(Style::new()) {
        let width = grapheme.symbol.cell_width();
        if width > 0 {
            clusters.push(Cluster { offset, width });
        }
        offset += grapheme.symbol.len();
    }
    clusters
}

/// Replaces control characters with a visible one-cell placeholder, so file
/// names and messages can neither inject terminal sequences nor shift the
/// columns that widths were measured for.
pub(crate) fn sanitize(text: &str) -> Cow<'_, str> {
    if text.contains(char::is_control) {
        Cow::Owned(
            text.chars()
                .map(|c| {
                    if c.is_control() {
                        CONTROL_PLACEHOLDER
                    } else {
                        c
                    }
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

/// Width of `text` in terminal cells as `Buffer::set_stringn` draws it.
/// `text` must already be sanitized.
pub(crate) fn cell_width(text: &str) -> u16 {
    clusters(text).iter().map(|c| c.width).sum()
}

/// `text` cut to at most `width` cells, ending in an ellipsis when cut.
/// `text` must already be sanitized.
pub(crate) fn fit_end(text: &str, width: u16) -> Cow<'_, str> {
    if cell_width(text) <= width {
        return Cow::Borrowed(text);
    }
    let Some(room) = width.checked_sub(1) else {
        return Cow::Borrowed("");
    };
    let clusters = clusters(text);
    let mut used = 0;
    let mut end = 0;
    for (index, cluster) in clusters.iter().enumerate() {
        if used + cluster.width > room {
            end = cluster.offset;
            break;
        }
        used += cluster.width;
        end = clusters
            .get(index + 1)
            .map_or(text.len(), |next| next.offset);
    }
    Cow::Owned(format!("{}{ELLIPSIS}", &text[..end]))
}

/// `text` cut to at most `width` cells from the left, starting with an
/// ellipsis when cut (keeps the end of a long path visible). `text` must
/// already be sanitized.
pub(crate) fn fit_start(text: &str, width: u16) -> Cow<'_, str> {
    if cell_width(text) <= width {
        return Cow::Borrowed(text);
    }
    let Some(room) = width.checked_sub(1) else {
        return Cow::Borrowed("");
    };
    let mut used = 0;
    let mut start = text.len();
    for cluster in clusters(text).iter().rev() {
        if used + cluster.width > room {
            break;
        }
        used += cluster.width;
        start = cluster.offset;
    }
    Cow::Owned(format!("{ELLIPSIS}{}", &text[start..]))
}

/// Test helpers shared by the widget tests.
#[cfg(test)]
pub(crate) mod testing {
    use ratatui::buffer::{Buffer, CellWidth};

    /// The text of buffer row `y`, skipping the cells hidden behind wide
    /// graphemes.
    pub(crate) fn row_text(buf: &Buffer, y: u16) -> String {
        let area = buf.area;
        let mut text = String::new();
        let mut x = area.x;
        while x < area.right() {
            let symbol = buf[(x, y)].symbol();
            text.push_str(symbol);
            x += symbol.cell_width().max(1);
        }
        text
    }

    /// Every row of the buffer, trailing spaces removed.
    pub(crate) fn lines(buf: &Buffer) -> Vec<String> {
        (buf.area.y..buf.area.bottom())
            .map(|y| row_text(buf, y).trim_end().to_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::row_text;
    use super::*;

    fn draw(view: &TextInputView<'_>, width: u16) -> (Buffer, Option<Position>) {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        let caret = render(view, area, &mut buf);
        (buf, caret)
    }

    fn field(value: &str, caret: usize) -> TextInputView<'_> {
        TextInputView {
            value,
            caret,
            masked: false,
            focused: true,
        }
    }

    #[test]
    fn short_value_starts_at_left_with_caret_after_text() {
        let (buf, caret) = draw(&field("abc", 3), 10);
        assert_eq!(row_text(&buf, 0), "abc       ");
        assert_eq!(caret, Some(Position::new(3, 0)));
        assert_eq!(buf[(0, 0)].bg, Color::DarkGray);
    }

    #[test]
    fn long_value_scrolls_to_keep_caret_visible() {
        // Width 5: four characters before the caret plus the caret column.
        let (buf, caret) = draw(&field("abcdefghij", 10), 5);
        assert_eq!(row_text(&buf, 0), "ghij ");
        assert_eq!(caret, Some(Position::new(4, 0)));

        let (buf, caret) = draw(&field("abcdefghij", 0), 5);
        assert_eq!(row_text(&buf, 0), "abcde");
        assert_eq!(caret, Some(Position::new(0, 0)));

        let (buf, caret) = draw(&field("abcdefghij", 6), 5);
        assert_eq!(row_text(&buf, 0), "cdefg");
        assert_eq!(caret, Some(Position::new(4, 0)));
    }

    #[test]
    fn wide_characters_move_the_caret_by_two_cells() {
        let value = "\u{4e2d}\u{6587}";
        let (buf, caret) = draw(&field(value, 2), 10);
        assert_eq!(row_text(&buf, 0), format!("{value}      "));
        assert_eq!(caret, Some(Position::new(4, 0)));

        let (_, caret) = draw(&field(value, 1), 10);
        assert_eq!(caret, Some(Position::new(2, 0)));

        // Width 4 leaves 3 cells before the caret: only one wide character.
        let (buf, caret) = draw(&field(value, 2), 4);
        assert_eq!(row_text(&buf, 0), "\u{6587}  ");
        assert_eq!(caret, Some(Position::new(2, 0)));
    }

    #[test]
    fn combining_marks_share_the_base_cell() {
        let value = "e\u{301}x";
        let (_, caret) = draw(&field(value, 2), 10);
        assert_eq!(caret, Some(Position::new(1, 0)));
        let (_, caret) = draw(&field(value, 3), 10);
        assert_eq!(caret, Some(Position::new(2, 0)));
    }

    #[test]
    fn masked_value_draws_a_fixed_run_and_debug_redacts() {
        let view = TextInputView {
            value: "hunter2",
            caret: 3,
            masked: true,
            focused: true,
        };
        let (buf, caret) = draw(&view, 10);
        assert_eq!(row_text(&buf, 0), "********  ");
        assert_eq!(caret, Some(Position::new(8, 0)));
        let debug = format!("{view:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("<redacted>"));

        // The run does not depend on the length.
        let long = "x".repeat(40);
        let (buf, _) = draw(
            &TextInputView {
                value: &long,
                caret: 40,
                ..view
            },
            10,
        );
        assert_eq!(row_text(&buf, 0), "********  ");
    }

    #[test]
    fn empty_masked_value_draws_nothing() {
        let view = TextInputView {
            value: "",
            caret: 0,
            masked: true,
            focused: true,
        };
        let (buf, caret) = draw(&view, 10);
        assert_eq!(row_text(&buf, 0), "          ");
        assert_eq!(caret, Some(Position::new(0, 0)));
    }

    #[test]
    fn unfocused_field_has_no_caret() {
        let view = TextInputView {
            focused: false,
            ..field("abc", 1)
        };
        let (buf, caret) = draw(&view, 6);
        assert_eq!(caret, None);
        assert_eq!(row_text(&buf, 0), "abc   ");
        assert!(buf[(0, 0)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn control_characters_are_drawn_as_placeholders() {
        let (buf, caret) = draw(&field("a\tb\u{1b}", 4), 6);
        assert_eq!(row_text(&buf, 0), "a?b?  ");
        assert_eq!(caret, Some(Position::new(4, 0)));
    }

    #[test]
    fn empty_area_draws_nothing() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        assert_eq!(
            render(&field("abc", 0), Rect::new(0, 0, 0, 1), &mut buf),
            None
        );
    }

    #[test]
    fn fit_end_cuts_with_ellipsis_by_cells() {
        assert_eq!(fit_end("abcdef", 6), "abcdef");
        assert_eq!(fit_end("abcdef", 4), "abc\u{2026}");
        assert_eq!(fit_end("abcdef", 1), "\u{2026}");
        assert_eq!(fit_end("abcdef", 0), "");
        // A wide character that does not fit before the ellipsis is dropped.
        assert_eq!(fit_end("a\u{4e2d}\u{6587}", 4), "a\u{4e2d}\u{2026}");
        assert_eq!(fit_end("a\u{4e2d}\u{6587}", 3), "a\u{2026}");
    }

    #[test]
    fn fit_start_keeps_the_end() {
        assert_eq!(fit_start("/srv/www/site", 13), "/srv/www/site");
        assert_eq!(fit_start("/srv/www/site", 6), "\u{2026}/site");
        assert_eq!(fit_start("/\u{4e2d}\u{6587}", 4), "\u{2026}\u{6587}");
        assert_eq!(fit_start("abc", 0), "");
    }

    #[test]
    fn sanitize_borrows_clean_text() {
        assert!(matches!(sanitize("plain"), Cow::Borrowed("plain")));
        assert_eq!(sanitize("a\nb"), "a?b");
        assert_eq!(cell_width("a\u{4e2d}"), 3);
    }
}
