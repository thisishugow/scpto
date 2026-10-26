//! Modal dialog widget: message, optional text field, checkbox, buttons.
//!
//! Centered on the screen, cleared underneath, sized to its content (at most
//! the screen minus a 2-cell margin). Buttons show their hotkey letter
//! underlined; the focused button, field, or checkbox is highlighted. The
//! text field (rename, mkdir, chmod, sudo password) is drawn by
//! [`text_input::render`], which also masks a password. The checkbox
//! is drawn as `[x] <label>` and recorded as a clickable
//! `DialogButton::ApplyToAll` button.
//!
//! Layout inside the border (one cell of horizontal padding): the message,
//! wrapped at word boundaries (explicit newlines kept, words longer than a
//! line broken), then a blank row and the text field (one row, full content
//! width) when present, then a blank row and the checkbox when present, then
//! a blank row and the buttons as `[Label]`, centered and flowed onto as
//! many rows as needed. A hotkey that does not occur in its label is shown
//! as `[Label (k)]`. When the screen is too short, message lines are dropped
//! from the end (the last kept line ends in `...`) so the field, the
//! checkbox, and the buttons stay visible.
//!
//! # Public API (Wave 2, widgets)
//!
//! ```text
//! pub struct DialogView<'a> {
//!     pub title: &'a str,
//!     /// Wrapped to the dialog width.
//!     pub message: &'a str,
//!     pub field: Option<TextInputView<'a>>,
//!     pub checkbox: Option<CheckboxView<'a>>,
//!     pub buttons: &'a [ButtonView<'a>],
//! }
//!
//! pub struct CheckboxView<'a> {
//!     pub label: &'a str,
//!     pub checked: bool,
//!     pub focused: bool,
//! }
//!
//! pub struct ButtonView<'a> {
//!     pub button: DialogButton,
//!     pub label: &'a str,
//!     pub hotkey: Option<char>,
//!     pub focused: bool,
//! }
//!
//! /// Draws the dialog centered in `screen`; returns its hit data and the
//! /// caret position of a focused text field.
//! pub fn render(view: &DialogView<'_>, screen: Rect, buf: &mut Buffer) -> (DialogHit, Option<Position>);
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Widget};

use crate::ui::action::DialogButton;
use crate::ui::hit::DialogHit;
use crate::ui::widgets::text_input::{self, TextInputView};

/// Narrowest content width (inside border and padding) on a large screen.
pub const MIN_CONTENT_WIDTH: u16 = 30;
/// Widest content width; longer messages wrap.
pub const MAX_CONTENT_WIDTH: u16 = 64;

/// Cells kept free on every side of the dialog.
const MARGIN: u16 = 2;
/// Border and padding on the left and right together.
const FRAME_WIDTH: u16 = 4;
/// Border on the top and bottom together.
const FRAME_HEIGHT: u16 = 2;
/// Cells between buttons on one row.
const BUTTON_GAP: u16 = 2;

/// What the dialog shows.
#[derive(Debug, Clone, Copy)]
pub struct DialogView<'a> {
    /// Shown in the top border.
    pub title: &'a str,
    /// Wrapped to the dialog width.
    pub message: &'a str,
    /// The text field of the rename, mkdir, chmod, and sudo password
    /// dialogs; masked for a password.
    pub field: Option<TextInputView<'a>>,
    /// The conflict dialog's "apply to all in this job" box.
    pub checkbox: Option<CheckboxView<'a>>,
    /// Buttons in display order.
    pub buttons: &'a [ButtonView<'a>],
}

/// A checkbox, recorded as `DialogButton::ApplyToAll`.
#[derive(Debug, Clone, Copy)]
pub struct CheckboxView<'a> {
    /// Text after the box.
    pub label: &'a str,
    /// Whether the box is ticked.
    pub checked: bool,
    /// Whether keyboard focus is on the checkbox.
    pub focused: bool,
}

/// One button.
#[derive(Debug, Clone, Copy)]
pub struct ButtonView<'a> {
    /// What clicking the button reports.
    pub button: DialogButton,
    /// The caption, without brackets.
    pub label: &'a str,
    /// The key that presses the button, underlined in the caption.
    pub hotkey: Option<char>,
    /// Whether keyboard focus is on the button.
    pub focused: bool,
}

/// Draws the dialog centered in `screen`; returns its hit data and the
/// caret position of a focused text field.
pub fn render(
    view: &DialogView<'_>,
    screen: Rect,
    buf: &mut Buffer,
) -> (DialogHit, Option<Position>) {
    let captions: Vec<Line<'_>> = view.buttons.iter().map(button_caption).collect();
    let checkbox = view.checkbox.as_ref().map(checkbox_caption);

    let max_content = screen.width.saturating_sub(2 * MARGIN + FRAME_WIDTH).max(1);
    let natural = [
        view.message.split('\n').map(text_width).max().unwrap_or(0),
        flow_width(&captions),
        checkbox.as_ref().map_or(0, line_width),
        text_width(view.title) + 2,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    let content_width = natural
        .clamp(MIN_CONTENT_WIDTH, MAX_CONTENT_WIDTH)
        .min(max_content);

    let mut message = wrap(view.message, content_width);
    let button_rows = flow(&captions, content_width);
    let field_rows: u16 = if view.field.is_some() { 2 } else { 0 };
    let checkbox_rows: u16 = if checkbox.is_some() { 2 } else { 0 };
    let buttons_rows = if button_rows.is_empty() {
        0
    } else {
        1 + count_u16(button_rows.len())
    };
    let max_height = screen.height.saturating_sub(2 * MARGIN).max(FRAME_HEIGHT);
    let fixed = FRAME_HEIGHT + field_rows + checkbox_rows + buttons_rows;
    let message_rows = count_u16(message.len()).min(max_height.saturating_sub(fixed));
    if usize::from(message_rows) < message.len() {
        message.truncate(usize::from(message_rows));
        if let Some(last) = message.last_mut() {
            mark_cut(last, content_width);
        }
    }
    let height = fixed + message_rows;
    let area = screen.centered(
        Constraint::Length(content_width + FRAME_WIDTH),
        Constraint::Length(height),
    );

    crate::ui::widgets::overlay::clear(area, buf);
    let block = Block::bordered()
        .title(format!(" {} ", view.title))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    block.render(area, buf);

    let mut hit = DialogHit {
        area,
        buttons: Vec::new(),
    };
    let mut rows = inner.y..inner.bottom();
    for (line, y) in message.iter().zip(rows.by_ref()) {
        buf.set_stringn(inner.x, y, line, usize::from(inner.width), Style::new());
    }
    let mut caret = None;
    if let Some(field) = &view.field {
        if let Some(y) = rows.nth(1) {
            caret = text_input::render(field, Rect::new(inner.x, y, inner.width, 1), buf);
        }
    }
    if let Some(caption) = &checkbox {
        if let Some(y) = rows.nth(1) {
            let width = line_width(caption).min(inner.width);
            buf.set_line(inner.x, y, caption, width);
            hit.buttons
                .push((Rect::new(inner.x, y, width, 1), DialogButton::ApplyToAll));
        }
    }
    if !button_rows.is_empty() {
        rows.next();
    }
    for (row, y) in button_rows.iter().zip(rows) {
        let row_width = row_width(&captions, row);
        let mut x = inner.x + inner.width.saturating_sub(row_width) / 2;
        for &index in row {
            let caption = &captions[index];
            let width = line_width(caption).min(inner.right().saturating_sub(x));
            buf.set_line(x, y, caption, width);
            hit.buttons
                .push((Rect::new(x, y, width, 1), view.buttons[index].button));
            x += width + BUTTON_GAP;
        }
    }
    (hit, caret)
}

/// `[Label]` with the hotkey underlined; the whole caption is reversed
/// when focused.
fn button_caption<'a>(button: &ButtonView<'a>) -> Line<'a> {
    let base = if button.focused {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::new()
    };
    let underlined = base.add_modifier(Modifier::UNDERLINED);
    let label = button.label;
    let mut spans = vec![Span::styled("[", base)];
    match button.hotkey {
        Some(key) => match hotkey_position(label, key) {
            Some((start, end)) => spans.extend([
                Span::styled(&label[..start], base),
                Span::styled(&label[start..end], underlined),
                Span::styled(&label[end..], base),
            ]),
            None => spans.extend([
                Span::styled(label, base),
                Span::styled(" (", base),
                Span::styled(key.to_string(), underlined),
                Span::styled(")", base),
            ]),
        },
        None => spans.push(Span::styled(label, base)),
    }
    spans.push(Span::styled("]", base));
    Line::from(spans)
}

/// Byte range of the first character of `label` equal to `key`, ignoring
/// case.
fn hotkey_position(label: &str, key: char) -> Option<(usize, usize)> {
    label
        .char_indices()
        .find(|&(_, ch)| ch.to_lowercase().eq(key.to_lowercase()))
        .map(|(start, ch)| (start, start + ch.len_utf8()))
}

fn checkbox_caption<'a>(checkbox: &CheckboxView<'a>) -> Line<'a> {
    let style = if checkbox.focused {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::new()
    };
    let mark = if checkbox.checked { "[x] " } else { "[ ] " };
    Line::from(vec![
        Span::styled(mark, style),
        Span::styled(checkbox.label, style),
    ])
}

/// Width of all captions on one row.
fn flow_width(captions: &[Line<'_>]) -> u16 {
    let all: Vec<usize> = (0..captions.len()).collect();
    row_width(captions, &all)
}

fn row_width(captions: &[Line<'_>], row: &[usize]) -> u16 {
    let widths: u16 = row.iter().map(|&index| line_width(&captions[index])).sum();
    widths + BUTTON_GAP * count_u16(row.len()).saturating_sub(1)
}

/// Splits the captions into rows no wider than `width`, keeping order. A
/// caption wider than `width` gets a row of its own.
fn flow(captions: &[Line<'_>], width: u16) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used = 0;
    for (index, caption) in captions.iter().enumerate() {
        let caption_width = line_width(caption);
        match rows.last_mut() {
            Some(row) if used + BUTTON_GAP + caption_width <= width => {
                row.push(index);
                used += BUTTON_GAP + caption_width;
            }
            _ => {
                rows.push(vec![index]);
                used = caption_width;
            }
        }
    }
    rows
}

/// Wraps `text` to lines at most `width` cells wide: at spaces where
/// possible, inside a word when it is longer than a line. Newlines start a
/// new line.
fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split(' ') {
            let word_width = text_width(word);
            let gap = u16::from(!line.is_empty());
            if used + gap + word_width <= width {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                used += gap + word_width;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            for ch in word.chars() {
                let ch_width = char_width(ch);
                if used + ch_width > width && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(ch);
                used += ch_width;
            }
        }
        lines.push(line);
    }
    lines
}

/// Ends `line` with `...` within `width` cells, marking dropped lines.
fn mark_cut(line: &mut String, width: u16) {
    while !line.is_empty() && text_width(line) + 3 > width {
        line.pop();
    }
    line.push_str("...");
}

fn text_width(text: &str) -> u16 {
    u16::try_from(Line::raw(text).width()).unwrap_or(u16::MAX)
}

fn char_width(ch: char) -> u16 {
    let mut encoded = [0; 4];
    text_width(ch.encode_utf8(&mut encoded))
}

fn line_width(line: &Line<'_>) -> u16 {
    u16::try_from(line.width()).unwrap_or(u16::MAX)
}

fn count_u16(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::ui::action::ClickTarget;
    use crate::ui::hit::HitMap;
    use crate::ui::widgets::text_input::MASK_LEN;

    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn text_at(buf: &Buffer, rect: Rect) -> String {
        (rect.x..rect.right())
            .map(|x| buf[(x, rect.y)].symbol())
            .collect()
    }

    fn draw(view: &DialogView<'_>, width: u16, height: u16) -> (Buffer, DialogHit) {
        let (buf, hit, caret) = draw_with_caret(view, width, height);
        assert_eq!(caret, None, "no focused field");
        (buf, hit)
    }

    fn draw_with_caret(
        view: &DialogView<'_>,
        width: u16,
        height: u16,
    ) -> (Buffer, DialogHit, Option<Position>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut drawn = None;
        terminal
            .draw(|frame| {
                // Something underneath that the dialog must clear.
                let area = frame.area();
                for position in area.positions() {
                    frame.buffer_mut()[position].set_symbol("#");
                }
                drawn = Some(render(view, area, frame.buffer_mut()));
            })
            .unwrap();
        let (hit, caret) = drawn.unwrap();
        (terminal.backend().buffer().clone(), hit, caret)
    }

    fn button(button: DialogButton, label: &'static str, hotkey: char) -> ButtonView<'static> {
        ButtonView {
            button,
            label,
            hotkey: Some(hotkey),
            focused: false,
        }
    }

    fn drawn_buttons(buf: &Buffer, hit: &DialogHit) -> Vec<(String, DialogButton)> {
        hit.buttons
            .iter()
            .map(|&(rect, button)| (text_at(buf, rect), button))
            .collect()
    }

    #[test]
    fn confirm_dialog_is_centered_with_hotkeys_and_focus() {
        let buttons = [
            ButtonView {
                focused: true,
                ..button(DialogButton::Yes, "Yes", 'y')
            },
            button(DialogButton::No, "No", 'n'),
        ];
        let view = DialogView {
            title: "Delete",
            message: "Delete 3 items from web:/srv?\nRemote deletion is permanent.",
            field: None,
            checkbox: None,
            buttons: &buttons,
        };
        let (buf, hit) = draw(&view, 80, 24);
        // 30 content + 4 frame wide; 2 message rows, blank, buttons, frame.
        assert_eq!(hit.area, Rect::new(23, 9, 34, 6));
        assert!(row_text(&buf, 9).contains("\u{250c} Delete \u{2500}"));
        assert!(row_text(&buf, 10).contains("\u{2502} Delete 3 items from web:/srv?  \u{2502}"));
        assert!(row_text(&buf, 11).contains("Remote deletion is permanent."));
        assert_eq!(
            drawn_buttons(&buf, &hit),
            vec![
                ("[Yes]".to_owned(), DialogButton::Yes),
                ("[No]".to_owned(), DialogButton::No),
            ]
        );
        assert!(row_text(&buf, 13).contains("[Yes]  [No]"));
        let (yes, _) = hit.buttons[0];
        let (no, _) = hit.buttons[1];
        // Hotkeys underlined; only the focused button is reversed.
        assert!(buf[(yes.x + 1, yes.y)]
            .modifier
            .contains(Modifier::UNDERLINED));
        assert!(!buf[(yes.x + 2, yes.y)]
            .modifier
            .contains(Modifier::UNDERLINED));
        assert!(buf[(yes.x, yes.y)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(no.x, no.y)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(no.x + 1, no.y)]
            .modifier
            .contains(Modifier::UNDERLINED));
        // Cleared underneath, untouched outside.
        assert_eq!(buf[(40, 12)].symbol(), " ");
        assert_eq!(buf[(0, 0)].symbol(), "#");

        let map = HitMap {
            dialog: Some(hit),
            ..HitMap::default()
        };
        assert_eq!(
            map.resolve(no.x + 1, no.y),
            Some(ClickTarget::DialogButton(DialogButton::No))
        );
        assert_eq!(map.resolve(no.x + 1, no.y - 1), None);
    }

    #[test]
    fn conflict_dialog_flows_buttons_and_records_the_checkbox() {
        let buttons = [
            button(DialogButton::Overwrite, "Overwrite", 'o'),
            button(DialogButton::Skip, "Skip", 's'),
            button(DialogButton::Rename, "Rename", 'r'),
            button(DialogButton::OverwriteIfNewer, "Overwrite if newer", 'n'),
            button(DialogButton::Resume, "Resume", 'u'),
            button(DialogButton::Cancel, "Cancel", 'c'),
        ];
        let view = DialogView {
            title: "File exists",
            message: "report.pdf already exists in web:/srv/docs.",
            field: None,
            checkbox: Some(CheckboxView {
                label: "Apply to all in this job",
                checked: true,
                focused: true,
            }),
            buttons: &buttons,
        };
        let (buf, hit) = draw(&view, 60, 24);
        // 60 - 2 * 2 margin - 4 frame = 52 content cells; the six buttons
        // need 67, so they flow onto two rows.
        assert_eq!(hit.area.width, 56);
        let drawn = drawn_buttons(&buf, &hit);
        assert_eq!(
            drawn[0],
            (
                "[x] Apply to all in this job".to_owned(),
                DialogButton::ApplyToAll
            )
        );
        let (checkbox, _) = hit.buttons[0];
        assert!(buf[(checkbox.x, checkbox.y)]
            .modifier
            .contains(Modifier::REVERSED));
        let labels: Vec<_> = drawn[1..]
            .iter()
            .map(|(text, button)| (text.as_str(), *button))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("[Overwrite]", DialogButton::Overwrite),
                ("[Skip]", DialogButton::Skip),
                ("[Rename]", DialogButton::Rename),
                ("[Overwrite if newer]", DialogButton::OverwriteIfNewer),
                ("[Resume]", DialogButton::Resume),
                ("[Cancel]", DialogButton::Cancel),
            ]
        );
        let first_row = hit.buttons[1].0.y;
        let second_row = hit.buttons[6].0.y;
        assert_eq!(second_row, first_row + 1);
        assert_eq!(checkbox.y + 2, first_row);
        // "Overwrite if newer": hotkey `n` is the `n` of "newer".
        let (newer, _) = hit.buttons[4];
        let n_column = newer.x + 1 + u16::try_from("Overwrite if ".len()).unwrap();
        assert_eq!(buf[(n_column, newer.y)].symbol(), "n");
        assert!(buf[(n_column, newer.y)]
            .modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn hotkey_missing_from_the_label_is_appended() {
        let buttons = [button(DialogButton::Run, "Run", 'x')];
        let view = DialogView {
            title: "VPN",
            message: "VPN may be required for nadi-gpu.\nCommand: ssh -t ts-dgx-dev ovpn",
            field: None,
            checkbox: None,
            buttons: &buttons,
        };
        let (buf, hit) = draw(&view, 80, 24);
        let (run, _) = hit.buttons[0];
        assert_eq!(text_at(&buf, run), "[Run (x)]");
        assert!(buf[(run.x + 6, run.y)]
            .modifier
            .contains(Modifier::UNDERLINED));
        assert!(row_text(&buf, hit.area.y + 1).contains("VPN may be required for nadi-gpu."));
        assert!(row_text(&buf, hit.area.y + 2).contains("Command: ssh -t ts-dgx-dev ovpn"));
    }

    #[test]
    fn long_error_message_wraps_to_the_maximum_width() {
        let buttons = [button(DialogButton::Ok, "OK", 'o')];
        let message = "Permission denied (publickey). The server rejected every key offered by the agent and the configured identity files.";
        let view = DialogView {
            title: "Connection failed",
            message,
            field: None,
            checkbox: None,
            buttons: &buttons,
        };
        let (buf, hit) = draw(&view, 100, 30);
        assert_eq!(hit.area.width, MAX_CONTENT_WIDTH + FRAME_WIDTH);
        let lines: Vec<String> = (hit.area.y + 1..hit.area.y + 3)
            .map(|y| row_text(&buf, y))
            .collect();
        assert!(lines[0].contains("Permission denied (publickey). The server rejected every key"));
        assert!(lines[1].contains("offered by the agent and the configured identity files."));
        assert_eq!(hit.area.height, 2 + 2 + 2);
    }

    #[test]
    fn short_screen_drops_message_lines_but_keeps_buttons() {
        let buttons = [button(DialogButton::Ok, "OK", 'o')];
        let message = "one\ntwo\nthree\nfour\nfive\nsix";
        let view = DialogView {
            title: "Error",
            message,
            field: None,
            checkbox: None,
            buttons: &buttons,
        };
        // 12 rows - 2 * 2 margin = 8: frame 2, blank + buttons 2, 4 lines.
        let (buf, hit) = draw(&view, 60, 12);
        assert_eq!(hit.area.height, 8);
        assert!(row_text(&buf, hit.area.y + 4).contains("four..."));
        assert!(!(0..12).any(|y| row_text(&buf, y).contains("five")));
        assert_eq!(
            drawn_buttons(&buf, &hit),
            vec![("[OK]".to_owned(), DialogButton::Ok)]
        );
    }

    fn ok_cancel() -> [ButtonView<'static>; 2] {
        [
            button(DialogButton::Ok, "OK", 'o'),
            button(DialogButton::Cancel, "Cancel", 'c'),
        ]
    }

    #[test]
    fn masked_password_field_sits_below_the_message_with_its_caret() {
        let buttons = ok_cancel();
        let view = DialogView {
            title: "sudo",
            message: "Password for deploy@web:",
            field: Some(TextInputView {
                value: "hunter2",
                caret: 7,
                masked: true,
                focused: true,
            }),
            checkbox: None,
            buttons: &buttons,
        };
        let (buf, hit, caret) = draw_with_caret(&view, 80, 24);
        // Frame 2, message 1, blank + field 2, blank + buttons 2.
        assert_eq!(hit.area.height, 7);
        let field_y = hit.area.y + 3;
        let field_x = hit.area.x + 2;
        assert_eq!(
            text_at(&buf, Rect::new(field_x, field_y, MIN_CONTENT_WIDTH, 1)),
            format!(
                "{}{}",
                "*".repeat(MASK_LEN),
                " ".repeat(usize::from(MIN_CONTENT_WIDTH) - MASK_LEN)
            )
        );
        assert!(!(0..24).any(|y| row_text(&buf, y).contains("hunter2")));
        assert_eq!(caret, Some(Position::new(field_x + 8, field_y)));
        // The field spans the content width and nothing else.
        assert_eq!(buf[(field_x, field_y)].bg, ratatui::style::Color::DarkGray);
        assert_eq!(
            buf[(field_x + MIN_CONTENT_WIDTH - 1, field_y)].bg,
            ratatui::style::Color::DarkGray
        );
        assert_ne!(
            buf[(field_x + MIN_CONTENT_WIDTH, field_y)].bg,
            ratatui::style::Color::DarkGray
        );
        assert_eq!(hit.buttons[0].0.y, field_y + 2);
    }

    #[test]
    fn unfocused_field_returns_no_caret_and_stacks_above_the_checkbox() {
        let buttons = ok_cancel();
        let view = DialogView {
            title: "Rename",
            message: "New name:",
            field: Some(TextInputView {
                value: "report.pdf",
                caret: 3,
                masked: false,
                focused: false,
            }),
            checkbox: Some(CheckboxView {
                label: "Keep extension",
                checked: false,
                focused: true,
            }),
            buttons: &buttons,
        };
        let (buf, hit, caret) = draw_with_caret(&view, 80, 24);
        assert_eq!(caret, None);
        let field_y = hit.area.y + 3;
        assert!(row_text(&buf, field_y).contains("report.pdf"));
        let (checkbox, button) = hit.buttons[0];
        assert_eq!(button, DialogButton::ApplyToAll);
        assert_eq!(checkbox.y, field_y + 2);
        assert_eq!(hit.buttons[1].0.y, field_y + 4);
        assert_eq!(hit.area.height, 2 + 1 + 2 + 2 + 2);
    }

    #[test]
    fn short_screen_drops_message_lines_but_keeps_the_field() {
        let buttons = ok_cancel();
        let view = DialogView {
            title: "Create directory",
            message: "one\ntwo\nthree\nfour",
            field: Some(TextInputView {
                value: "new",
                caret: 3,
                masked: false,
                focused: true,
            }),
            checkbox: None,
            buttons: &buttons,
        };
        // 11 rows - 2 * 2 margin = 7: frame 2, field 2, buttons 2, 1 line.
        let (buf, hit, caret) = draw_with_caret(&view, 60, 11);
        assert_eq!(hit.area.height, 7);
        assert!(row_text(&buf, hit.area.y + 1).contains("one..."));
        let field_y = hit.area.y + 3;
        assert!(row_text(&buf, field_y).contains("new"));
        assert_eq!(caret, Some(Position::new(hit.area.x + 2 + 3, field_y)));
        assert_eq!(hit.buttons[0].0.y, field_y + 2);
    }

    #[test]
    fn wrap_breaks_at_spaces_and_inside_long_words() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("x abcdefgh", 4), vec!["x", "abcd", "efgh"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("", 10), vec![""]);
        // Wide characters count two cells.
        assert_eq!(
            wrap("\u{4f60}\u{597d}\u{4e16}", 4),
            vec!["\u{4f60}\u{597d}", "\u{4e16}"]
        );
    }

    #[test]
    fn hotkeys_match_case_insensitively() {
        assert_eq!(hotkey_position("Overwrite", 'o'), Some((0, 1)));
        assert_eq!(hotkey_position("Skip", 'K'), Some((1, 2)));
        assert_eq!(hotkey_position("Skip", 'z'), None);
    }
}
