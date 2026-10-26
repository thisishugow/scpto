//! Clearing shared by popup layers, including partially covered wide glyphs.

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::widgets::{Clear, Widget};

/// Clears the popup and any glyph crossing its left edge, so the terminal
/// does not hide the popup's first cell behind the underlying wide glyph.
pub(crate) fn clear(area: Rect, buf: &mut Buffer) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    for y in area.top()..area.bottom() {
        let mut x = buf.area.left();
        while x < area.left() {
            let end = x.saturating_add(buf[(x, y)].symbol().cell_width().max(1));
            if end > area.left() {
                for covered in x..area.left() {
                    buf[(covered, y)].reset();
                }
                break;
            }
            x = end;
        }
    }
    Clear.render(area, buf);
}
