//! Floating drag label (for example "⇢ 3 items") drawn next to the pointer.
//!
//! Drawn one cell right of the pointer, moved left or up as needed to stay
//! inside the screen; "1 item" in the singular. When moving left would put
//! the label over the pointer cell, it goes one row up instead (one row down
//! on the top row), so the pointer's own cell stays visible.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct DragLabelView {
//!     pub count: usize,
//!     pub column: u16,
//!     pub row: u16,
//! }
//!
//! pub fn render(view: &DragLabelView, screen: Rect, buf: &mut Buffer);
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::ui::widgets::text_input::{cell_width, fit_end};

const LABEL: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Yellow)
    .add_modifier(Modifier::BOLD);

/// The drag label to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragLabelView {
    /// Number of dragged items.
    pub count: usize,
    /// Pointer column.
    pub column: u16,
    /// Pointer row.
    pub row: u16,
}

/// Draws the label next to the pointer, kept inside `screen`.
pub fn render(view: &DragLabelView, screen: Rect, buf: &mut Buffer) {
    if screen.is_empty() {
        return;
    }
    let noun = if view.count == 1 { "item" } else { "items" };
    let label = format!(" \u{21e2} {} {noun} ", view.count);
    let label = fit_end(&label, screen.width);
    let width = cell_width(&label);

    let column = view.column.clamp(screen.x, screen.right() - 1);
    let row = view.row.clamp(screen.y, screen.bottom() - 1);
    let x = column.saturating_add(1).min(screen.right() - width);
    let y = if x > column || screen.height == 1 {
        row
    } else if row > screen.y {
        row - 1
    } else {
        row + 1
    };
    buf.set_stringn(x, y, &label, usize::from(width), LABEL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::widgets::text_input::testing::row_text;

    /// The label for three items, 11 cells.
    const LABEL_3: &str = " \u{21e2} 3 items ";

    fn draw(count: usize, column: u16, row: u16) -> Buffer {
        let screen = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(screen);
        render(&DragLabelView { count, column, row }, screen, &mut buf);
        buf
    }

    #[test]
    fn label_sits_right_of_the_pointer() {
        let buf = draw(3, 2, 1);
        assert_eq!(row_text(&buf, 1), format!("   {LABEL_3}{}", " ".repeat(6)));
        assert_eq!(buf[(3, 1)].bg, Color::Yellow);
        assert_eq!(buf[(2, 1)].bg, Color::Reset);
    }

    #[test]
    fn singular_for_one_item() {
        let buf = draw(1, 0, 0);
        assert_eq!(
            row_text(&buf, 0),
            format!("  \u{21e2} 1 item {}", " ".repeat(9))
        );
    }

    #[test]
    fn near_the_right_edge_the_label_moves_left_and_up() {
        // Label is 11 cells; at column 15 it cannot sit right of the
        // pointer, so it ends at the screen edge one row up.
        let buf = draw(3, 15, 2);
        assert_eq!(row_text(&buf, 1), format!("{}{LABEL_3}", " ".repeat(9)));
        assert_eq!(row_text(&buf, 2).trim(), "");

        // Still right of the pointer when it fits exactly.
        let buf = draw(3, 8, 2);
        assert_eq!(row_text(&buf, 2), format!("{}{LABEL_3}", " ".repeat(9)));
    }

    #[test]
    fn on_the_top_row_it_moves_down_instead() {
        let buf = draw(3, 19, 0);
        assert_eq!(row_text(&buf, 0).trim(), "");
        assert_eq!(row_text(&buf, 1), format!("{}{LABEL_3}", " ".repeat(9)));
    }

    #[test]
    fn narrow_screen_cuts_the_label() {
        let screen = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(screen);
        let view = DragLabelView {
            count: 120,
            column: 5,
            row: 3,
        };
        render(&view, screen, &mut buf);
        assert_eq!(row_text(&buf, 0), " \u{21e2} 12\u{2026}");
    }
}
