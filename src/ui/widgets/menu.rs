//! Popup menu widget (plan 8.9): the sort menu and the context menu of a
//! pane row.
//!
//! A bordered box with the title in the top border and one row per item:
//! an optional check mark (`[x]` / `[ ]`), the label, and the item's key
//! right-aligned. The box opens with its top-left corner at the anchor and
//! is moved left and up as needed to stay inside `bounds`; when `bounds` is
//! too short for every item, the rows scroll so the cursor item stays
//! visible. The cursor item is highlighted.
//!
//! # Public API (plan 8.9)
//!
//! ```text
//! pub struct MenuItemView<'a> {
//!     pub key: &'a str,
//!     pub label: &'a str,
//!     /// `Some` draws a check box (the sort menu marks the current key).
//!     pub checked: Option<bool>,
//! }
//!
//! pub struct MenuView<'a> {
//!     pub title: &'a str,
//!     pub items: &'a [MenuItemView<'a>],
//!     pub cursor: usize,
//!     /// Preferred top-left corner (column, row).
//!     pub anchor: (u16, u16),
//! }
//!
//! /// Draws the menu inside `bounds` and returns its hit data.
//! pub fn render(view: &MenuView<'_>, bounds: Rect, buf: &mut Buffer) -> MenuHit;
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Widget};

use crate::ui::hit::MenuHit;
use crate::ui::widgets::text_input::{cell_width, fit_end};

const BORDER: Style = Style::new().fg(Color::Cyan);
const ITEM: Style = Style::new();
const KEY: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const CURSOR: Style = Style::new().add_modifier(Modifier::REVERSED);

/// One item to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItemView<'a> {
    /// The key as shown, for example `n` or `Enter`.
    pub key: &'a str,
    /// What the item does.
    pub label: &'a str,
    /// `Some` draws a check box (the sort menu marks the current key).
    pub checked: Option<bool>,
}

/// The menu to draw, borrowed from the app state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuView<'a> {
    /// Title in the top border.
    pub title: &'a str,
    /// The items in order.
    pub items: &'a [MenuItemView<'a>],
    /// Index of the highlighted item.
    pub cursor: usize,
    /// Preferred top-left corner (column, row).
    pub anchor: (u16, u16),
}

/// Draws the menu inside `bounds` and returns its hit data.
pub fn render(view: &MenuView<'_>, bounds: Rect, buf: &mut Buffer) -> MenuHit {
    let label_width = view
        .items
        .iter()
        .map(|item| {
            let check = if item.checked.is_some() { 4 } else { 0 };
            check + cell_width(item.label)
        })
        .max()
        .unwrap_or(0);
    let key_width = view
        .items
        .iter()
        .map(|item| cell_width(item.key))
        .max()
        .unwrap_or(0);
    // Border, a space on both sides, and two spaces between label and key.
    let wanted_width = (label_width + key_width + 6).max(cell_width(view.title) + 4);
    let width = wanted_width.min(bounds.width);
    let item_count = u16::try_from(view.items.len()).unwrap_or(u16::MAX);
    let height = item_count.saturating_add(2).min(bounds.height);
    if width < 3 || height < 3 {
        return MenuHit {
            area: Rect::default(),
            items: Vec::new(),
        };
    }
    let x = view.anchor.0.clamp(bounds.x, bounds.right() - width);
    let y = view.anchor.1.clamp(bounds.y, bounds.bottom() - height);
    let area = Rect::new(x, y, width, height);
    crate::ui::widgets::overlay::clear(area, buf);
    Block::bordered()
        .border_style(BORDER)
        .title(format!(
            " {} ",
            fit_end(view.title, width.saturating_sub(4))
        ))
        .render(area, buf);

    let rows = usize::from(height - 2);
    let first = (view.cursor + 1).saturating_sub(rows);
    let inner_width = width - 2;
    let mut items = Vec::with_capacity(rows);
    for (offset, (index, item)) in view
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .enumerate()
    {
        // `offset < rows <= u16::MAX`.
        let row_y = y + 1 + offset as u16;
        let row = Rect::new(x + 1, row_y, inner_width, 1);
        let style = if index == view.cursor { CURSOR } else { ITEM };
        buf.set_style(row, style);
        let mut label = String::from(" ");
        match item.checked {
            Some(true) => label.push_str("[x] "),
            Some(false) => label.push_str("[ ] "),
            None => {}
        }
        label.push_str(item.label);
        let key_cells = cell_width(item.key);
        let label_room = inner_width.saturating_sub(key_cells + 1);
        buf.set_stringn(
            row.x,
            row_y,
            fit_end(&label, label_room),
            usize::from(label_room),
            style,
        );
        if key_cells < inner_width {
            buf.set_string(
                row.right() - key_cells - 1,
                row_y,
                item.key,
                KEY.patch(style),
            );
        }
        items.push((row, index));
    }
    MenuHit { area, items }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::widgets::text_input::testing::row_text;

    const ITEMS: [MenuItemView<'static>; 3] = [
        MenuItemView {
            key: "n",
            label: "Name",
            checked: Some(true),
        },
        MenuItemView {
            key: "s",
            label: "Size",
            checked: Some(false),
        },
        MenuItemView {
            key: "r",
            label: "Reverse order",
            checked: None,
        },
    ];

    fn draw(view: &MenuView<'_>, width: u16, height: u16) -> (Buffer, MenuHit) {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let hit = render(view, area, &mut buf);
        (buf, hit)
    }

    #[test]
    fn draws_items_with_checks_and_keys_at_the_anchor() {
        let view = MenuView {
            title: "Sort",
            items: &ITEMS,
            cursor: 1,
            anchor: (2, 1),
        };
        let (buf, hit) = draw(&view, 40, 10);
        assert_eq!(hit.area, Rect::new(2, 1, 20, 5));
        let v = "\u{2502}";
        assert!(row_text(&buf, 2).starts_with(&format!("  {v} [x] Name       n {v}")));
        assert!(row_text(&buf, 3).starts_with(&format!("  {v} [ ] Size       s {v}")));
        assert!(row_text(&buf, 4).starts_with(&format!("  {v} Reverse order  r {v}")));
        assert!(buf[(4, 3)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(4, 2)].modifier.contains(Modifier::REVERSED));
        assert_eq!(
            hit.items,
            [
                (Rect::new(3, 2, 18, 1), 0),
                (Rect::new(3, 3, 18, 1), 1),
                (Rect::new(3, 4, 18, 1), 2)
            ]
        );
    }

    #[test]
    fn stays_inside_the_bounds_and_scrolls_to_the_cursor() {
        let view = MenuView {
            title: "Sort",
            items: &ITEMS,
            cursor: 2,
            anchor: (35, 8),
        };
        let (_, hit) = draw(&view, 40, 10);
        assert_eq!(hit.area, Rect::new(20, 5, 20, 5));

        // Two rows tall: only one item fits, and it is the cursor's.
        let (_, hit) = draw(&view, 40, 3);
        assert_eq!(hit.items, [(Rect::new(21, 1, 18, 1), 2)]);

        let (_, hit) = draw(&view, 2, 2);
        assert!(hit.items.is_empty());
    }
}
