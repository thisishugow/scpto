//! Button bar widget: one row of clickable buttons with their
//! keys, such as `[c Copy] [m Move] [? Help]`, for the current mode.
//!
//! Each button stands for the action its key produces, so a click is the
//! same as the key. The bar adapts to the width without cutting a label:
//! when every button does not fit with its label, the least important
//! buttons (highest `rank`) are dropped until the rest fit, keeping at
//! least [`MIN_LABELLED`]; when even those do not fit, every button is
//! shortened to its key (`[c]`) and the least important are dropped until
//! the rest fit. Buttons keep their given order. An `active` button (a
//! toggle that is on, such as quick view) is highlighted.
//!
//! # Public API
//!
//! ```text
//! pub struct BarButton<'a> {
//!     /// The key as shown, for example `c`, `Enter`, `Esc`.
//!     pub key: &'a str,
//!     /// What it does, for example `Copy`.
//!     pub label: &'a str,
//!     /// The action of the key in the current mode.
//!     pub action: Action,
//!     /// Lower ranks are kept longer when the bar is narrow.
//!     pub rank: u8,
//!     /// Drawn highlighted (a toggle that is on).
//!     pub active: bool,
//! }
//!
//! /// Draws the bar into the first row of `area`; returns each drawn
//! /// button's cells and action, for `HitMap::bar`.
//! pub fn render(buttons: &[BarButton<'_>], area: Rect, buf: &mut Buffer) -> Vec<(Rect, Action)>;
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Clear, Widget};

use crate::ui::action::Action;
use crate::ui::widgets::text_input::cell_width;

/// Labelled buttons kept before the bar switches to keys only.
pub const MIN_LABELLED: usize = 5;

const BRACKET: Style = Style::new().fg(Color::DarkGray);
const KEY: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const LABEL: Style = Style::new();
const ACTIVE: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Cyan)
    .add_modifier(Modifier::BOLD);

/// One button of the bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarButton<'a> {
    /// The key as shown, for example `c`, `Enter`, `Esc`.
    pub key: &'a str,
    /// What it does, for example `Copy`.
    pub label: &'a str,
    /// The action of the key in the current mode.
    pub action: Action,
    /// Lower ranks are kept longer when the bar is narrow.
    pub rank: u8,
    /// Drawn highlighted (a toggle that is on).
    pub active: bool,
}

impl BarButton<'_> {
    /// Cells of the button: `[key label]`, or `[key]` when short.
    fn width(&self, labelled: bool) -> u16 {
        let label = if labelled {
            1 + cell_width(self.label)
        } else {
            0
        };
        2 + cell_width(self.key) + label
    }
}

/// Draws the bar into the first row of `area`; returns each drawn button's
/// cells and action, for `HitMap::bar`.
pub fn render(buttons: &[BarButton<'_>], area: Rect, buf: &mut Buffer) -> Vec<(Rect, Action)> {
    if area.is_empty() {
        return Vec::new();
    }
    let row = Rect { height: 1, ..area };
    Clear.render(row, buf);
    let (shown, labelled) = fit(buttons, row.width);
    let mut x = row.x;
    let mut hits = Vec::with_capacity(shown.len());
    for button in shown {
        let width = button.width(labelled);
        let (bracket, key, label) = if button.active {
            (ACTIVE, ACTIVE, ACTIVE)
        } else {
            (BRACKET, KEY, LABEL)
        };
        let mut cursor = buf.set_stringn(x, row.y, "[", usize::MAX, bracket).0;
        cursor = buf
            .set_stringn(cursor, row.y, button.key, usize::MAX, key)
            .0;
        if labelled {
            cursor = buf.set_stringn(cursor, row.y, " ", usize::MAX, label).0;
            cursor = buf
                .set_stringn(cursor, row.y, button.label, usize::MAX, label)
                .0;
        }
        buf.set_stringn(cursor, row.y, "]", usize::MAX, bracket);
        hits.push((Rect::new(x, row.y, width, 1), button.action.clone()));
        x += width + 1;
    }
    hits
}

/// The buttons that fit in `width` cells, in their order, and whether they
/// keep their labels.
fn fit<'b, 'a>(buttons: &'b [BarButton<'a>], width: u16) -> (Vec<&'b BarButton<'a>>, bool) {
    // Most important first; ties keep the display order.
    let mut by_rank: Vec<usize> = (0..buttons.len()).collect();
    by_rank.sort_by_key(|&index| buttons[index].rank);
    let total = |count: usize, labelled: bool| -> u32 {
        let cells: u32 = by_rank[..count]
            .iter()
            .map(|&index| u32::from(buttons[index].width(labelled)))
            .sum();
        // One space between buttons.
        cells + u32::try_from(count.saturating_sub(1)).unwrap_or(u32::MAX)
    };
    let width = u32::from(width);
    let labelled_floor = MIN_LABELLED.min(buttons.len());
    let choice = (labelled_floor..=buttons.len())
        .rev()
        .find(|&count| count > 0 && total(count, true) <= width)
        .map(|count| (count, true))
        .or_else(|| {
            (1..=buttons.len())
                .rev()
                .find(|&count| total(count, false) <= width)
                .map(|count| (count, false))
        });
    let Some((count, labelled)) = choice else {
        return (Vec::new(), false);
    };
    let mut kept = by_rank[..count].to_vec();
    kept.sort_unstable();
    (
        kept.into_iter().map(|index| &buttons[index]).collect(),
        labelled,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::widgets::text_input::testing::row_text;

    fn buttons() -> Vec<BarButton<'static>> {
        [
            ("c", "Copy", Action::Copy, 0),
            ("m", "Move", Action::Move, 1),
            ("n", "Mkdir", Action::MakeDir, 6),
            ("d", "Delete", Action::Delete, 2),
            ("i", "Quick view", Action::ToggleQuickView, 7),
            ("?", "Help", Action::Help, 3),
            ("q", "Quit", Action::Quit, 4),
        ]
        .into_iter()
        .map(|(key, label, action, rank)| BarButton {
            key,
            label,
            action,
            rank,
            active: false,
        })
        .collect()
    }

    fn draw(buttons: &[BarButton<'_>], width: u16) -> (Buffer, Vec<(Rect, Action)>) {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        let hits = render(buttons, area, &mut buf);
        (buf, hits)
    }

    #[test]
    fn wide_bars_show_every_button_with_its_label() {
        let (buf, hits) = draw(&buttons(), 80);
        assert_eq!(
            row_text(&buf, 0).trim_end(),
            "[c Copy] [m Move] [n Mkdir] [d Delete] [i Quick view] [? Help] [q Quit]"
        );
        assert_eq!(hits.len(), 7);
        assert_eq!(hits[0], (Rect::new(0, 0, 8, 1), Action::Copy));
        assert_eq!(hits[1], (Rect::new(9, 0, 8, 1), Action::Move));
        assert_eq!(buf[(1, 0)].fg, Color::Cyan);
    }

    #[test]
    fn narrower_bars_drop_the_least_important_buttons_first() {
        // 71 cells needed; without Quick view (rank 7) 56.
        let (buf, hits) = draw(&buttons(), 60);
        assert_eq!(
            row_text(&buf, 0).trim_end(),
            "[c Copy] [m Move] [n Mkdir] [d Delete] [? Help] [q Quit]"
        );
        assert_eq!(hits.len(), 6);
        // Never cut inside a label: every drawn button is whole.
        let (buf, hits) = draw(&buttons(), 47);
        assert_eq!(
            row_text(&buf, 0).trim_end(),
            "[c Copy] [m Move] [d Delete] [? Help] [q Quit]"
        );
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn very_narrow_bars_shorten_to_keys() {
        let (buf, hits) = draw(&buttons(), 30);
        assert_eq!(row_text(&buf, 0).trim_end(), "[c] [m] [n] [d] [i] [?] [q]");
        assert_eq!(hits[6], (Rect::new(24, 0, 3, 1), Action::Quit));
        let (buf, _) = draw(&buttons(), 10);
        assert_eq!(row_text(&buf, 0).trim_end(), "[c] [m]");
        let (_, hits) = draw(&buttons(), 2);
        assert!(hits.is_empty());
    }

    #[test]
    fn active_buttons_are_highlighted() {
        let mut buttons = buttons();
        buttons[4].active = true;
        let (buf, hits) = draw(&buttons, 80);
        let (quick, _) = hits[4];
        assert_eq!(buf[(quick.x, 0)].bg, Color::Cyan);
        assert_eq!(buf[(0, 0)].bg, Color::Reset);
    }
}
