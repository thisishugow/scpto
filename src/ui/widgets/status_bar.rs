//! Status bar widget: messages, selection summary, transfer summary. The
//! keys of the current mode are the button bar's (`widgets::button_bar`).
//!
//! One row: the message (error style when `message_is_error`) on the left,
//! then the selection and transfer summaries. Empty parts are left out; the
//! parts drawn are
//! separated by a dimmed bar, and each is cut with an ellipsis when it does
//! not fit, so the message always wins the space.
//!
//! # Public API
//!
//! ```text
//! pub struct StatusBarView<'a> {
//!     pub message: &'a str,
//!     pub message_is_error: bool,
//!     /// For example `3 selected, 12.4 MiB`.
//!     pub selection: &'a str,
//!     /// For example `2 jobs running, 1 job failed`.
//!     pub transfers: &'a str,
//! }
//!
//! pub fn render(view: &StatusBarView<'_>, area: Rect, buf: &mut Buffer);
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Clear, Widget};

use crate::ui::widgets::text_input::{cell_width, fit_end, sanitize};

/// Drawn between two parts.
const SEPARATOR: &str = " \u{2502} ";

const MESSAGE: Style = Style::new();
const MESSAGE_ERROR: Style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
const SUMMARY: Style = Style::new().fg(Color::Cyan);
const SEPARATOR_STYLE: Style = Style::new().fg(Color::DarkGray);

/// The status bar to draw, borrowed from the app state.
#[derive(Debug, Clone, Copy)]
pub struct StatusBarView<'a> {
    /// The current status message; empty for none.
    pub message: &'a str,
    /// Draw the message in the error style.
    pub message_is_error: bool,
    /// For example `3 selected, 12.4 MiB`.
    pub selection: &'a str,
    /// For example `2 jobs running, 1 job failed`.
    pub transfers: &'a str,
}

/// Draws the status bar into the first row of `area`.
pub fn render(view: &StatusBarView<'_>, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let row = Rect { height: 1, ..area };
    Clear.render(row, buf);

    let message_style = if view.message_is_error {
        MESSAGE_ERROR
    } else {
        MESSAGE
    };
    let parts = [
        (view.message, message_style),
        (view.selection, SUMMARY),
        (view.transfers, SUMMARY),
    ];
    let separator_width = cell_width(SEPARATOR);
    let mut x = row.x;
    let mut first = true;
    for (text, style) in parts.into_iter().filter(|(text, _)| !text.is_empty()) {
        let mut remaining = row.right() - x;
        if !first {
            // A part needs at least one cell after the separator.
            if remaining <= separator_width {
                break;
            }
            x = buf
                .set_stringn(x, row.y, SEPARATOR, usize::from(remaining), SEPARATOR_STYLE)
                .0;
            remaining -= separator_width;
        }
        if remaining == 0 {
            break;
        }
        let text = sanitize(text);
        x = buf
            .set_stringn(
                x,
                row.y,
                fit_end(&text, remaining),
                usize::from(remaining),
                style,
            )
            .0;
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::widgets::text_input::testing::row_text;

    fn draw(view: &StatusBarView<'_>, width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        render(view, area, &mut buf);
        buf
    }

    fn full() -> StatusBarView<'static> {
        StatusBarView {
            message: "Copied 3 items",
            message_is_error: false,
            selection: "3 selected, 12.4 MiB",
            transfers: "2 jobs running, 1 job failed",
        }
    }

    #[test]
    fn draws_all_parts_in_order() {
        let buf = draw(&full(), 90);
        assert_eq!(
            row_text(&buf, 0).trim_end(),
            "Copied 3 items \u{2502} 3 selected, 12.4 MiB \u{2502} 2 jobs running, 1 job failed"
        );
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
        assert_eq!(buf[(17, 0)].fg, Color::Cyan);
        assert_eq!(buf[(15, 0)].fg, Color::DarkGray);
    }

    #[test]
    fn later_parts_are_cut_to_the_remaining_width() {
        let buf = draw(&full(), 50);
        assert_eq!(
            row_text(&buf, 0),
            "Copied 3 items \u{2502} 3 selected, 12.4 MiB \u{2502} 2 jobs ru\u{2026}"
        );
        // No room after the separator: the part is left out entirely.
        let buf = draw(&full(), 40);
        assert_eq!(
            row_text(&buf, 0),
            "Copied 3 items \u{2502} 3 selected, 12.4 MiB   "
        );
    }

    #[test]
    fn long_error_message_takes_the_row() {
        let view = StatusBarView {
            message: "Permission denied: /etc/shadow",
            message_is_error: true,
            ..full()
        };
        let buf = draw(&view, 20);
        assert_eq!(row_text(&buf, 0), "Permission denied: \u{2026}");
        assert_eq!(buf[(0, 0)].fg, Color::Red);
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn empty_parts_are_skipped_without_separators() {
        let view = StatusBarView {
            message: "",
            message_is_error: false,
            selection: "",
            transfers: "1 job running",
        };
        let buf = draw(&view, 30);
        assert_eq!(row_text(&buf, 0).trim_end(), "1 job running");
    }

    #[test]
    fn redraw_clears_the_previous_content() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        render(&full(), area, &mut buf);
        let view = StatusBarView {
            message: "ok",
            message_is_error: false,
            selection: "",
            transfers: "",
        };
        render(&view, area, &mut buf);
        assert_eq!(row_text(&buf, 0).trim_end(), "ok");
    }

    #[test]
    fn control_characters_in_messages_are_neutralized() {
        let view = StatusBarView {
            message: "bad\u{1b}[2Jname\n",
            message_is_error: true,
            selection: "",
            transfers: "",
        };
        let buf = draw(&view, 20);
        assert_eq!(row_text(&buf, 0).trim_end(), "bad?[2Jname?");
    }
}
