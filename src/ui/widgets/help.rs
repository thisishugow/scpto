//! Help screen listing key and mouse bindings.
//!
//! The content is static (the plan 2.4 key table as amended by plan 8.9:
//! letter keys only, no function keys; mouse gestures; and the
//! Terminal.app / iTerm2 modifier notes) and scrolls by line. Every content
//! line is one screen row (nothing wraps), so [`line_count`] is exactly the
//! number of rows the content takes.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct HelpView {
//!     /// First line shown; clamped to the content.
//!     pub scroll: usize,
//! }
//!
//! /// Number of content lines, for scroll clamping by the app.
//! pub fn line_count() -> usize;
//!
//! pub fn render(view: &HelpView, area: Rect, buf: &mut Buffer);
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};

/// Cells of the key column.
const KEY_WIDTH: usize = 24;

const BORDER: Style = Style::new().fg(Color::Cyan);
const HEADING: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
const KEY: Style = Style::new().fg(Color::Cyan);
const TEXT: Style = Style::new();

/// One line of the help content.
enum HelpLine {
    /// A section title.
    Heading(&'static str),
    /// A binding: keys and what they do.
    Key(&'static str, &'static str),
    /// A line of prose.
    Text(&'static str),
    /// An empty line between sections.
    Blank,
}

use HelpLine::{Blank, Heading, Key, Text};

const CONTENT: &[HelpLine] = &[
    Heading("Panes"),
    Key("Tab", "Switch focused pane"),
    Key("Up Down / j k", "Move the cursor"),
    Key("PgUp PgDn", "Move one page"),
    Key("Home End / g G", "First / last entry"),
    Key(
        "Enter / Right / l",
        "Enter directory (.. goes up); preview a file",
    ),
    Key("Backspace / Left / h", "Parent directory"),
    Key("Space / Insert", "Toggle selection and move down"),
    Key("Ctrl+A / *", "Select all / invert selection"),
    Key("/", "Filter (fuzzy match); Enter keeps, Esc clears"),
    Key(".", "Toggle hidden files"),
    Key("s", "Sort menu: name, extension, size, modified, reverse"),
    Key("x", "Menu of actions for the entry under the cursor"),
    Blank,
    Heading("File operations"),
    Key("c", "Copy to the other pane's directory"),
    Key("m", "Move to the other pane's directory"),
    Key("n", "Create directory"),
    Key("d / Delete", "Delete (asks first)"),
    Key("r", "Rename"),
    Key("e", "Edit with $VISUAL / $EDITOR"),
    Key("v", "Full-screen preview"),
    Key("i", "Toggle quick view in the other pane"),
    Key("p", "Change permissions (chmod)"),
    Key("R", "Reveal in Finder (remote files download first)"),
    Blank,
    Heading("Endpoints and tools"),
    Key("o", "Switch this pane's endpoint (host picker)"),
    Key("S", "Reconnect this pane with sudo"),
    Key("b / B", "Bookmark list / bookmark this directory"),
    Key("=", "Compare the two panes' directories"),
    Key("t", "Open a shell in this directory"),
    Key("Q", "Toggle the queue panel"),
    Key("?", "This help"),
    Key("q", "Quit (asks while transfers run)"),
    Blank,
    Heading("Viewers (preview, help)"),
    Key("Up Down PgUp PgDn", "Scroll"),
    Key("m", "Toggle rendered / raw Markdown"),
    Key("Esc / q", "Close"),
    Blank,
    Heading("Dialogs, menus, and lists"),
    Key("Tab / Shift+Tab", "Next / previous field or button"),
    Key("Enter", "Confirm or pick"),
    Key("Esc", "Cancel or close"),
    Key("Space", "Toggle the focused option"),
    Key("Letters", "Button and menu hotkeys; filter text in lists"),
    Key("Ctrl+W / Ctrl+U", "Delete word / clear field"),
    Blank,
    Heading("Mouse"),
    Key("Click", "Move the cursor and focus the pane"),
    Key("Ctrl+click / Cmd+click", "Toggle selection"),
    Key("Shift+click", "Select a range"),
    Key("Option/Alt+click", "Menu of actions for the clicked entry"),
    Key("Double click", "Enter directory; preview a file"),
    Key("Pane title button", "Switch this pane's endpoint"),
    Key("Column header", "Sort by it; click again to reverse"),
    Key("Bottom bar buttons", "Same as their keys"),
    Key("Drag", "Copy to the other pane or into a folder row"),
    Key(
        "Drag within a pane",
        "Move into a folder row (same endpoint)",
    ),
    Key("Esc while dragging", "Cancel the drag"),
    Key("Wheel", "Scroll"),
    Key("Queue buttons", "Pause, resume, cancel, or retry"),
    Key("Right click", "Not used (left to the terminal)"),
    Blank,
    Heading("Finder"),
    Text("Drop files from Finder onto the window to upload them to the"),
    Text("focused remote pane's directory. Dragging out to Finder is not"),
    Text("possible in a terminal: use R to reveal a file instead."),
    Blank,
    Heading("Terminal notes"),
    Text("Terminal.app does not report Cmd, and iTerm2 uses Cmd+click for"),
    Text("URLs: use Ctrl+click or Space to toggle selection."),
    Text("Option+click reaches scpto only when the terminal sends Option as"),
    Text("Meta/Alt (and Terminal.app uses it for native selection): x and"),
    Text("the bottom bar offer the same actions."),
    Text("With mouse capture on, select text natively by holding Option"),
    Text("(Terminal.app) or Shift."),
    Text("Mac keyboards have no Insert key: use Space."),
];

/// The help screen to draw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HelpView {
    /// First line shown; clamped to the content.
    pub scroll: usize,
}

/// Number of content lines, for scroll clamping by the app.
pub fn line_count() -> usize {
    CONTENT.len()
}

/// Draws the help screen into `area`, scrolled to `view.scroll` (clamped so
/// the last page stays full).
pub fn render(view: &HelpView, area: Rect, buf: &mut Buffer) {
    Clear.render(area, buf);
    let block = Block::bordered()
        .border_style(BORDER)
        .title(" Help ")
        .title_bottom(" Esc / q closes ");
    let inner = block.inner(area);
    block.render(area, buf);

    let page = usize::from(inner.height);
    let first = view.scroll.min(line_count().saturating_sub(page));
    for (offset, line) in CONTENT.iter().skip(first).take(page).enumerate() {
        // `offset < inner.height`, a u16.
        let y = inner.y + offset as u16;
        buf.set_line(inner.x + 1, y, &styled(line), inner.width.saturating_sub(1));
    }
}

fn styled(line: &HelpLine) -> Line<'static> {
    match line {
        Heading(text) => Line::styled(*text, HEADING),
        Key(keys, action) => Line::from(vec![
            Span::styled(format!("  {keys:<KEY_WIDTH$}"), KEY),
            Span::styled(*action, TEXT),
        ]),
        Text(text) => Line::styled(format!("  {text}"), TEXT),
        Blank => Line::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::widgets::text_input::testing::lines;

    fn draw(scroll: usize, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, 80, height);
        let mut buf = Buffer::empty(area);
        render(&HelpView { scroll }, area, &mut buf);
        lines(&buf)
    }

    /// Every key of the plan 2.4 table as amended by plan 8.9, as listed.
    const PLAN_KEYS: &[&str] = &[
        "Tab",
        "Up Down / j k",
        "PgUp PgDn",
        "Home End / g G",
        "Enter / Right / l",
        "Backspace / Left / h",
        "Space / Insert",
        "Ctrl+A / *",
        "c",
        "m",
        "n",
        "d / Delete",
        "r",
        "e",
        "v",
        "i",
        "p",
        "R",
        "/",
        ".",
        "s",
        "x",
        "o",
        "S",
        "b / B",
        "=",
        "t",
        "Q",
        "q",
        "?",
    ];

    #[test]
    fn content_covers_the_plan_key_table() {
        for keys in PLAN_KEYS {
            assert!(
                CONTENT
                    .iter()
                    .any(|line| matches!(line, Key(k, _) if k == keys)),
                "missing {keys}"
            );
        }
    }

    #[test]
    fn no_function_keys_are_listed() {
        for line in CONTENT {
            if let Key(keys, _) = line {
                assert!(
                    !keys.split([' ', '/']).any(|word| word.len() > 1
                        && word.starts_with('F')
                        && word[1..].bytes().all(|b| b.is_ascii_digit())),
                    "{keys}"
                );
            }
        }
    }

    #[test]
    fn content_lines_fit_an_80_column_screen() {
        let area = Rect::new(0, 0, 80, 1);
        for line in CONTENT {
            // Border, left padding, and the line itself.
            assert!(styled(line).width() + 3 <= usize::from(area.width));
        }
    }

    #[test]
    fn first_page_shows_the_title_and_first_section() {
        let rows = draw(0, 6);
        assert!(rows[0].starts_with("\u{250c} Help \u{2500}"));
        assert_eq!(
            rows[1],
            "\u{2502} Panes".to_owned() + &" ".repeat(72) + "\u{2502}"
        );
        assert!(rows[2].starts_with(&format!(
            "\u{2502}   Tab{}Switch focused pane",
            " ".repeat(21)
        )));
        assert!(rows[5].starts_with("\u{2514} Esc / q closes \u{2500}"));
    }

    #[test]
    fn scroll_is_clamped_so_the_last_page_stays_full() {
        let page = 10;
        let last = "  Mac keyboards have no Insert key: use Space.";
        let rows = draw(line_count() - 1, page + 2);
        assert!(rows[usize::from(page)].starts_with(&format!("\u{2502} {last}")));
        assert_eq!(draw(usize::MAX, page + 2), rows);
        assert_eq!(draw(line_count() - usize::from(page), page + 2), rows);
    }

    #[test]
    fn tall_area_shows_everything_from_the_top() {
        let height = u16::try_from(line_count()).expect("small") + 2;
        let rows = draw(5, height);
        assert!(rows[1].starts_with("\u{2502} Panes"));
    }
}
