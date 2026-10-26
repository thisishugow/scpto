//! Modal dialog widget: message, optional text field, checkbox, buttons.
//!
//! Centered on the screen, cleared underneath, sized to its content (at most
//! the screen minus a 2-cell margin). Buttons show their hotkey letter
//! underlined; the focused button or field is highlighted. The checkbox is
//! drawn as `[x] <label>` and recorded as a clickable
//! `DialogButton::ApplyToAll` button.
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
