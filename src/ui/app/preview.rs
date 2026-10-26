//! Previews in the app: full-screen preview and quick view, debounced
//! loading with cancellation of superseded requests, scrolling, the
//! rendered/raw Markdown toggle, and the help screen (the other full-screen
//! viewer).
//!
//! # Behavior
//!
//! - `v` / F3, and `Open` on an entry that is not enterable (core hands it
//!   over through `App::preview_open`): opens [`PreviewState::full`] for
//!   the focused pane's cursor entry with `ViewerContent::Loading` and emits
//!   `Effect::LoadPreview`; an earlier load still in flight is canceled with
//!   `Effect::CancelRequest`.
//! - `i` toggles [`PreviewState::quick_view`]: the non-focused pane shows
//!   [`PreviewState::quick`], the focused pane's cursor entry. After every
//!   browse action and listing, core calls `App::preview_cursor_moved`:
//!   when quick view is on and the cursor entry changed, the in-flight load
//!   is canceled and the debounce timer ([`PreviewState::debounce`], one id
//!   reused) is scheduled for `preview::QUICK_VIEW_DEBOUNCE`; when it fires,
//!   the load starts. Directories and special files are shown as
//!   `ViewerContent::Binary` (size, type, mode) without a load.
//! - `PreviewLoaded` for the pending request fills the viewer it was for:
//!   text is highlighted with [`PreviewState::highlighter`] (created on the
//!   first text preview with `App::background`), Markdown files start
//!   rendered (`preview::render_markdown`), images become a
//!   `ThreadProtocol` built with [`PreviewState::resize`], failures become
//!   `ViewerContent::Failed`. Other request ids are dropped.
//!   `ImageResized` is handed to the viewer whose `ThreadProtocol` accepts
//!   it (`update_resized_protocol`).
//! - Viewer mode (`InputMode::Viewer`): `MoveCursor` and `Scroll { region:
//!   ScrollRegion::Preview | ScrollRegion::Help }` scroll (a page is the
//!   screen height minus the two border rows); `ToggleMarkdown` switches a
//!   Markdown viewer between rendered and raw (raw is highlighted);
//!   `Cancel` closes the help screen if open, else the full preview.
//! - `?` / F1 opens the help screen ([`PreviewState::help`], scroll 0) over
//!   whatever is shown, including the comparison view.
//! - The runtime takes [`PreviewState::resize_requests`] once at startup and
//!   hands it to `ui::effects::preview::spawn_resize_worker`.
//!
//! # Public API (Wave 3, ui/app/preview)
//!
//! ```text
//! impl App {
//!     /// Claims `Preview`, `ToggleQuickView`, `Help`, `Scroll` on
//!     /// `ScrollRegion::Preview` / `Help`, and every action in
//!     /// `InputMode::Viewer`; hands every other action back unchanged.
//!     pub fn preview_action(&mut self, action: Action) -> Result<Vec<Effect>, Action>;
//!     /// Full-screen preview of `side`'s cursor entry (`v`, and core's
//!     /// `Open` on a non-enterable entry).
//!     pub fn preview_open(&mut self, side: PaneSide) -> Vec<Effect>;
//!     /// Quick view follow-up after browse actions and listings.
//!     pub fn preview_cursor_moved(&mut self) -> Vec<Effect>;
//!     /// `BgEvent::PreviewLoaded`.
//!     pub fn preview_loaded(&mut self, request: RequestId, location: Location, result: Result<PreviewData, Error>) -> Vec<Effect>;
//!     /// `BgEvent::ImageResized`.
//!     pub fn preview_image_resized(&mut self, resized: ImageResized);
//!     /// `Some` when `id` is the debounce timer.
//!     pub fn preview_timer(&mut self, id: TimerId, at: Instant) -> Option<Vec<Effect>>;
//!     /// `Some(InputMode::Viewer)` while the help screen or the full
//!     /// preview is open.
//!     pub fn preview_input_mode(&self) -> Option<InputMode>;
//!
//!     /// View accessors for render (`&mut` because images render
//!     /// statefully). `title` is a per-frame scratch string.
//!     pub fn full_preview_view<'a>(&'a mut self, title: &'a mut String) -> Option<PreviewView<'a>>;
//!     /// The quick view and the pane it is drawn in.
//!     pub fn quick_preview_view<'a>(&'a mut self, title: &'a mut String) -> Option<(PaneSide, PreviewView<'a>)>;
//!     pub fn help_view(&self) -> Option<HelpView>;
//! }
//! ```

use std::sync::mpsc;

use ratatui::text::Text;
use ratatui_image::thread::{ResizeRequest, ThreadProtocol};

use crate::preview::Highlighter;
use crate::ui::action::{RequestId, TimerId};
use crate::vfs::{Entry, Location};

/// State owned by this feature; part of [`super::App`]. Not `Debug`: image
/// protocols are not.
pub struct PreviewState {
    /// The full-screen preview while open.
    pub full: Option<Viewer>,
    /// Whether quick view is on.
    pub quick_view: bool,
    /// The quick view's content while quick view is on and something was
    /// shown.
    pub quick: Option<Viewer>,
    /// The help screen's scroll position while it is open.
    pub help: Option<usize>,
    /// The `Effect::LoadPreview` in flight.
    pub pending: Option<PendingPreview>,
    /// The quick view debounce timer id, allocated once.
    pub debounce: Option<TimerId>,
    /// Syntax highlighting, created on the first text preview.
    pub highlighter: Option<Highlighter>,
    /// Where image protocols send their resize requests.
    pub resize: mpsc::Sender<ResizeRequest>,
    /// The receiving end of [`PreviewState::resize`] until the runtime takes
    /// it for the resize worker.
    pub resize_requests: Option<mpsc::Receiver<ResizeRequest>>,
}

impl Default for PreviewState {
    fn default() -> PreviewState {
        let (resize, requests) = mpsc::channel();
        PreviewState {
            full: None,
            quick_view: false,
            quick: None,
            help: None,
            pending: None,
            debounce: None,
            highlighter: None,
            resize,
            resize_requests: Some(requests),
        }
    }
}

/// A load in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingPreview {
    /// Its correlation id.
    pub request: RequestId,
    /// The viewer it fills.
    pub target: PreviewTarget,
}

/// Which viewer a load fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewTarget {
    /// [`PreviewState::full`].
    Full,
    /// [`PreviewState::quick`].
    Quick,
}

/// One previewed file.
pub struct Viewer {
    /// The file.
    pub location: Location,
    /// What is shown.
    pub content: ViewerContent,
    /// First line shown.
    pub scroll: usize,
    /// For Markdown: rendered (`true`) or raw.
    pub rendered: bool,
}

/// What a viewer shows.
pub enum ViewerContent {
    /// The load is in flight.
    Loading,
    /// Text, highlighted or rendered.
    Text {
        /// The bytes read, kept to switch between rendered and raw.
        bytes: Vec<u8>,
        /// Whether the file is longer than what was read.
        truncated: bool,
        /// The lines drawn.
        text: Text<'static>,
    },
    /// Attributes only (binary files, directories, special files).
    Binary(Entry),
    /// An image.
    Image(ThreadProtocol),
    /// The load failed; the message to show.
    Failed(String),
}
