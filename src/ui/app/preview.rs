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
//! - Opening the full preview on the file quick view shows takes over the
//!   quick view's loaded content (or its in-flight load) instead of loading
//!   again; closing the full preview hands it back. While the full preview
//!   is open, quick view loads wait (the debounce timer does nothing) and a
//!   quick view image is dropped, so at most one viewer awaits an image
//!   resize; closing it resumes the quick view load.
//! - `ImageResized` goes to the viewer whose image awaits a resize (full
//!   preview first): `Ok` is offered to its `ThreadProtocol`
//!   (`update_resized_protocol`), `Err` turns the viewer into
//!   `ViewerContent::Failed`. A response with no awaiting viewer belongs to
//!   a viewer that was closed or replaced and is dropped.
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

use std::ffi::OsStr;
use std::mem;
use std::sync::mpsc;
use std::time::Instant;

use ratatui::text::Text;
use ratatui_image::thread::{ResizeRequest, ThreadProtocol};

use super::App;
use crate::error::Error;
use crate::preview::{
    self, Background, Highlighter, ImageResized, PreviewData, QUICK_VIEW_DEBOUNCE,
};
use crate::ui::action::{
    Action, CursorMove, Effect, InputMode, PaneSide, RequestId, ScrollRegion, TimerId,
};
use crate::ui::widgets::help::{self, HelpView};
use crate::ui::widgets::preview::{PreviewBody, PreviewView};
use crate::vfs::{Endpoint, Entry, EntryKind, Location};

/// Rows a viewer's border takes (top and bottom).
const BORDER_ROWS: u16 = 2;

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

impl Viewer {
    /// A viewer at the top; Markdown files start rendered.
    fn new(location: Location, content: ViewerContent) -> Viewer {
        let rendered = is_markdown(&location);
        Viewer {
            location,
            content,
            scroll: 0,
            rendered,
        }
    }

    /// Whether the viewer is waiting for its image's background resize: a
    /// `ThreadProtocol` gives its protocol away while the resize runs.
    fn awaiting_resize(&self) -> bool {
        matches!(&self.content, ViewerContent::Image(protocol) if protocol.protocol_type().is_none())
    }

    /// Moves the first shown line to `target`, clamped so the last page of
    /// text stays full (only text scrolls).
    fn scroll_to(&mut self, target: usize, page: usize) {
        let lines = match &self.content {
            ViewerContent::Text { text, .. } => text.lines.len(),
            ViewerContent::Loading
            | ViewerContent::Binary(_)
            | ViewerContent::Image(_)
            | ViewerContent::Failed(_) => 0,
        };
        self.scroll = target.min(lines.saturating_sub(page));
    }

    /// The widget view, with `label` (plus the Markdown mode) as title.
    fn view<'a>(&'a mut self, title: &'a mut String, label: &str) -> PreviewView<'a> {
        title.clear();
        title.push_str(label);
        let mut truncated = false;
        let body = match &mut self.content {
            ViewerContent::Loading => PreviewBody::Loading,
            ViewerContent::Text {
                truncated: cut,
                text,
                ..
            } => {
                truncated = *cut;
                if is_markdown(&self.location) {
                    title.push_str(if self.rendered {
                        " (rendered)"
                    } else {
                        " (raw)"
                    });
                }
                PreviewBody::Text(text)
            }
            ViewerContent::Binary(entry) => PreviewBody::Binary(entry),
            ViewerContent::Image(protocol) => PreviewBody::Image(protocol),
            ViewerContent::Failed(message) => PreviewBody::Failed(message),
        };
        PreviewView {
            title,
            body,
            scroll: self.scroll,
            truncated,
        }
    }
}

impl App {
    /// Claims `Preview`, `ToggleQuickView`, `Help`, `Scroll` on
    /// `ScrollRegion::Preview` / `Help`, and every action in
    /// `InputMode::Viewer`; hands every other action back unchanged.
    pub fn preview_action(&mut self, action: Action) -> Result<Vec<Effect>, Action> {
        if self.preview_input_mode().is_some() {
            return Ok(self.viewer_action(action));
        }
        match action {
            Action::Preview => Ok(self.preview_open(self.core.focused())),
            Action::ToggleQuickView => Ok(self.toggle_quick_view()),
            Action::Help => {
                self.preview.help = Some(0);
                Ok(Vec::new())
            }
            Action::Scroll {
                region: ScrollRegion::Preview,
                lines,
            } => {
                let page = self.quick_page();
                if let Some(quick) = self.preview.quick.as_mut() {
                    quick.scroll_to(scrolled_by(quick.scroll, lines), page);
                }
                Ok(Vec::new())
            }
            // The help screen is not open, so there is nothing to scroll.
            Action::Scroll {
                region: ScrollRegion::Help,
                ..
            } => Ok(Vec::new()),
            other => Err(other),
        }
    }

    /// Full-screen preview of `side`'s cursor entry (`v`, and core's `Open`
    /// on a non-enterable entry).
    pub fn preview_open(&mut self, side: PaneSide) -> Vec<Effect> {
        let Some((location, entry)) = self.cursor_target(side) else {
            return Vec::new();
        };
        if let Some(effects) = self.take_over_quick(&location) {
            return effects;
        }
        // Only the full preview may hold an image while it is open (see
        // `preview_image_resized`); the hidden quick view reloads on close.
        if let Some(quick) = self.preview.quick.as_mut() {
            if matches!(quick.content, ViewerContent::Image(_)) {
                quick.content = ViewerContent::Loading;
                quick.scroll = 0;
            }
        }
        if needs_load(&entry) {
            self.preview.full = Some(Viewer::new(location.clone(), ViewerContent::Loading));
            self.start_load(PreviewTarget::Full, location)
        } else {
            self.preview.full = Some(Viewer::new(location, ViewerContent::Binary(entry)));
            self.cancel_pending()
        }
    }

    /// Quick view follow-up after browse actions and listings.
    pub fn preview_cursor_moved(&mut self) -> Vec<Effect> {
        if self.preview.quick_view {
            self.quick_follow(false)
        } else {
            Vec::new()
        }
    }

    /// `BgEvent::PreviewLoaded`.
    pub fn preview_loaded(
        &mut self,
        request: RequestId,
        location: Location,
        result: Result<PreviewData, Error>,
    ) -> Vec<Effect> {
        let Some(pending) = self
            .preview
            .pending
            .filter(|pending| pending.request == request)
        else {
            return Vec::new();
        };
        self.preview.pending = None;
        let background = self.background;
        let PreviewState {
            full,
            quick,
            highlighter,
            resize,
            ..
        } = &mut self.preview;
        let viewer = match pending.target {
            PreviewTarget::Full => full.as_mut(),
            PreviewTarget::Quick => quick.as_mut(),
        };
        let Some(viewer) = viewer.filter(|viewer| viewer.location == location) else {
            return Vec::new();
        };
        viewer.scroll = 0;
        viewer.content = match result {
            Ok(PreviewData::Text { bytes, truncated }) => {
                let text = text_of(
                    highlighter,
                    background,
                    &viewer.location,
                    &bytes,
                    viewer.rendered,
                );
                ViewerContent::Text {
                    bytes,
                    truncated,
                    text,
                }
            }
            Ok(PreviewData::Binary { entry }) => ViewerContent::Binary(entry),
            Ok(PreviewData::Image(image)) => {
                ViewerContent::Image(ThreadProtocol::new(resize.clone(), Some(image.protocol)))
            }
            Err(error) => ViewerContent::Failed(error.to_string()),
        };
        Vec::new()
    }

    /// `BgEvent::ImageResized`.
    pub fn preview_image_resized(&mut self, resized: ImageResized) {
        let PreviewState { full, quick, .. } = &mut self.preview;
        // `preview_open` keeps images out of the quick view while the full
        // preview is open, so at most one viewer awaits a resize. Without
        // one, the response belongs to a viewer that was closed or replaced.
        let Some(viewer) = [full.as_mut(), quick.as_mut()]
            .into_iter()
            .flatten()
            .find(|viewer| viewer.awaiting_resize())
        else {
            return;
        };
        match resized.result {
            Ok(response) => {
                if let ViewerContent::Image(protocol) = &mut viewer.content {
                    // `false` means the response answers an earlier request
                    // of another protocol; this one's own answer is still on
                    // its way, so there is nothing else to do.
                    protocol.update_resized_protocol(response);
                }
            }
            Err(error) => viewer.content = ViewerContent::Failed(error.to_string()),
        }
    }

    /// `Some` when `id` is the debounce timer.
    pub fn preview_timer(&mut self, id: TimerId, _at: Instant) -> Option<Vec<Effect>> {
        (self.preview.debounce == Some(id)).then(|| self.resume_quick())
    }

    /// `Some(InputMode::Viewer)` while the help screen or the full preview
    /// is open.
    pub fn preview_input_mode(&self) -> Option<InputMode> {
        (self.preview.help.is_some() || self.preview.full.is_some()).then_some(InputMode::Viewer)
    }

    /// The full preview's view for render (`&mut` because images render
    /// statefully). `title` is a per-frame scratch string.
    pub fn full_preview_view<'a>(&'a mut self, title: &'a mut String) -> Option<PreviewView<'a>> {
        let viewer = self.preview.full.as_mut()?;
        let label = describe(&viewer.location);
        Some(viewer.view(title, &label))
    }

    /// The quick view and the pane it is drawn in.
    pub fn quick_preview_view<'a>(
        &'a mut self,
        title: &'a mut String,
    ) -> Option<(PaneSide, PreviewView<'a>)> {
        if !self.preview.quick_view {
            return None;
        }
        let side = self.core.focused().other();
        let viewer = self.preview.quick.as_mut()?;
        let label = file_name(&viewer.location).to_string_lossy().into_owned();
        Some((side, viewer.view(title, &label)))
    }

    /// The help screen's view for render while it is open.
    pub fn help_view(&self) -> Option<HelpView> {
        self.preview.help.map(|scroll| HelpView { scroll })
    }

    // Viewer mode.

    fn viewer_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.screen_page();
        match action {
            Action::MoveCursor(movement) => match self.preview.help.as_mut() {
                Some(scroll) => *scroll = help_scroll(moved(*scroll, movement, page), page),
                None => {
                    if let Some(full) = self.preview.full.as_mut() {
                        full.scroll_to(moved(full.scroll, movement, page), page);
                    }
                }
            },
            Action::Scroll {
                region: ScrollRegion::Help,
                lines,
            } => {
                if let Some(scroll) = self.preview.help.as_mut() {
                    *scroll = help_scroll(scrolled_by(*scroll, lines), page);
                }
            }
            Action::Scroll {
                region: ScrollRegion::Preview,
                lines,
            } => {
                if let Some(full) = self.preview.full.as_mut() {
                    full.scroll_to(scrolled_by(full.scroll, lines), page);
                }
            }
            Action::ToggleMarkdown if self.preview.help.is_none() => self.toggle_markdown(),
            Action::Help => self.preview.help = Some(0),
            Action::Cancel => return self.close_viewer(),
            // Viewer mode claims every action; the rest have no meaning here.
            _ => {}
        }
        Vec::new()
    }

    /// Switches a Markdown full preview between rendered and raw.
    fn toggle_markdown(&mut self) {
        let page = self.screen_page();
        let background = self.background;
        let PreviewState {
            full, highlighter, ..
        } = &mut self.preview;
        let Some(viewer) = full.as_mut().filter(|viewer| is_markdown(&viewer.location)) else {
            return;
        };
        let ViewerContent::Text { bytes, text, .. } = &mut viewer.content else {
            return;
        };
        viewer.rendered = !viewer.rendered;
        *text = text_of(
            highlighter,
            background,
            &viewer.location,
            bytes,
            viewer.rendered,
        );
        viewer.scroll_to(viewer.scroll, page);
    }

    /// Closes the help screen if open, else the full preview.
    fn close_viewer(&mut self) -> Vec<Effect> {
        if self.preview.help.take().is_some() {
            return Vec::new();
        }
        let Some(full) = self.preview.full.take() else {
            return Vec::new();
        };
        let full_pending = self
            .preview
            .pending
            .is_some_and(|pending| pending.target == PreviewTarget::Full);
        if self.preview.quick_view {
            if let Some(quick) = self.preview.quick.as_mut().filter(|quick| {
                quick.location == full.location && matches!(quick.content, ViewerContent::Loading)
            }) {
                // Hand the content (or its load) back to the quick view.
                quick.content = full.content;
                quick.rendered = full.rendered;
                if let Some(pending) = self.preview.pending.as_mut() {
                    pending.target = PreviewTarget::Quick;
                }
                return Vec::new();
            }
        }
        let mut effects = Vec::new();
        if full_pending {
            effects.extend(self.cancel_pending());
        }
        effects.extend(self.resume_quick());
        effects
    }

    // Quick view.

    fn toggle_quick_view(&mut self) -> Vec<Effect> {
        if !self.preview.quick_view {
            self.preview.quick_view = true;
            return self.quick_follow(true);
        }
        self.preview.quick_view = false;
        self.preview.quick = None;
        self.cancel_quick_load()
    }

    /// Points the quick view at the focused pane's cursor entry when it
    /// changed: loads at once when `immediate`, else after the debounce.
    /// Loads wait while the full preview is open.
    fn quick_follow(&mut self, immediate: bool) -> Vec<Effect> {
        let target = self.cursor_target(self.core.focused());
        let shown = self.preview.quick.as_ref().map(|quick| &quick.location);
        if shown == target.as_ref().map(|(location, _)| location) {
            return Vec::new();
        }
        let mut effects = self.cancel_quick_load();
        match target {
            None => self.preview.quick = None,
            Some((location, entry)) if !needs_load(&entry) => {
                self.preview.quick = Some(Viewer::new(location, ViewerContent::Binary(entry)));
            }
            Some((location, _)) => {
                self.preview.quick = Some(Viewer::new(location.clone(), ViewerContent::Loading));
                if self.preview.full.is_some() {
                    return effects;
                }
                if immediate {
                    effects.extend(self.start_load(PreviewTarget::Quick, location));
                } else {
                    let id = *self
                        .preview
                        .debounce
                        .get_or_insert_with(|| self.core.next_timer());
                    effects.push(Effect::ScheduleTimer {
                        id,
                        after: QUICK_VIEW_DEBOUNCE,
                    });
                }
            }
        }
        effects
    }

    /// Starts the quick view's load when it waits for one and nothing else
    /// is loading or open.
    fn resume_quick(&mut self) -> Vec<Effect> {
        let preview = &self.preview;
        if !preview.quick_view || preview.full.is_some() || preview.pending.is_some() {
            return Vec::new();
        }
        match preview.quick.as_ref() {
            Some(quick) if matches!(quick.content, ViewerContent::Loading) => {
                let location = quick.location.clone();
                self.start_load(PreviewTarget::Quick, location)
            }
            _ => Vec::new(),
        }
    }

    /// Moves the quick view's content (or its in-flight load) for
    /// `location` into a new full preview. `None` when there is nothing to
    /// take over (another file, a failure to retry, or a load that has not
    /// started).
    fn take_over_quick(&mut self, location: &Location) -> Option<Vec<Effect>> {
        let PreviewState {
            full,
            quick,
            pending,
            ..
        } = &mut self.preview;
        let quick = quick.as_mut().filter(|quick| quick.location == *location)?;
        match &quick.content {
            ViewerContent::Failed(_) => return None,
            ViewerContent::Loading => {
                let pending = pending
                    .as_mut()
                    .filter(|pending| pending.target == PreviewTarget::Quick)?;
                pending.target = PreviewTarget::Full;
            }
            ViewerContent::Text { .. } | ViewerContent::Binary(_) | ViewerContent::Image(_) => {}
        }
        *full = Some(Viewer {
            location: location.clone(),
            content: mem::replace(&mut quick.content, ViewerContent::Loading),
            scroll: 0,
            rendered: quick.rendered,
        });
        quick.scroll = 0;
        Some(Vec::new())
    }

    // Loads.

    /// The location and entry of `side`'s cursor row.
    fn cursor_target(&self, side: PaneSide) -> Option<(Location, Entry)> {
        let pane = self.core.pane(side);
        let entry = pane.cursor_entry()?.clone();
        let path = pane.path_of(pane.cursor)?;
        let location = Location {
            endpoint: pane.endpoint.clone(),
            path,
        };
        Some((location, entry))
    }

    /// Emits a load for `target`, canceling the one in flight.
    fn start_load(&mut self, target: PreviewTarget, location: Location) -> Vec<Effect> {
        let mut effects = self.cancel_pending();
        let request = self.core.next_request();
        self.preview.pending = Some(PendingPreview { request, target });
        effects.push(Effect::LoadPreview { request, location });
        effects
    }

    fn cancel_pending(&mut self) -> Vec<Effect> {
        self.preview
            .pending
            .take()
            .map(|pending| Effect::CancelRequest(pending.request))
            .into_iter()
            .collect()
    }

    fn cancel_quick_load(&mut self) -> Vec<Effect> {
        let quick_loading = self
            .preview
            .pending
            .is_some_and(|pending| pending.target == PreviewTarget::Quick);
        if quick_loading {
            self.cancel_pending()
        } else {
            Vec::new()
        }
    }

    // Pages.

    /// Text rows of a full-screen viewer: the screen height minus the
    /// border.
    fn screen_page(&self) -> usize {
        let height = self.core.layout().status_bar.bottom();
        usize::from(height.saturating_sub(BORDER_ROWS))
    }

    /// Text rows of the quick view: the non-focused pane's height minus the
    /// border.
    fn quick_page(&self) -> usize {
        let layout = self.core.layout();
        let area = match self.core.focused().other() {
            PaneSide::Left => layout.left,
            PaneSide::Right => layout.right,
        };
        usize::from(area.height.saturating_sub(BORDER_ROWS))
    }
}

/// Whether showing `entry` needs `Effect::LoadPreview`: regular files and
/// symlinks to them. Directories and special files show their attributes.
fn needs_load(entry: &Entry) -> bool {
    match entry.kind {
        EntryKind::File => true,
        EntryKind::Symlink { target_is_dir, .. } => !target_is_dir,
        EntryKind::Dir | EntryKind::Special(_) => false,
    }
}

/// The last path component (the whole path for `/`).
fn file_name(location: &Location) -> &OsStr {
    location
        .path
        .file_name()
        .unwrap_or(location.path.as_os_str())
}

fn is_markdown(location: &Location) -> bool {
    preview::is_markdown(file_name(location))
}

/// The lines of a text preview: rendered Markdown, or highlighted (the
/// highlighter is created on first use).
fn text_of(
    highlighter: &mut Option<Highlighter>,
    background: Background,
    location: &Location,
    bytes: &[u8],
    rendered: bool,
) -> Text<'static> {
    if rendered {
        preview::render_markdown(bytes)
    } else {
        highlighter
            .get_or_insert_with(|| Highlighter::new(background))
            .highlight(file_name(location), bytes)
    }
}

/// `current` after a cursor key, unclamped (`End` is `usize::MAX`).
fn moved(current: usize, movement: CursorMove, page: usize) -> usize {
    let page = page.max(1);
    match movement {
        CursorMove::Up => current.saturating_sub(1),
        CursorMove::Down => current.saturating_add(1),
        CursorMove::PageUp => current.saturating_sub(page),
        CursorMove::PageDown => current.saturating_add(page),
        CursorMove::Home => 0,
        CursorMove::End => usize::MAX,
    }
}

/// `current` after a wheel scroll of `lines` (negative is up), unclamped.
fn scrolled_by(current: usize, lines: i32) -> usize {
    // `u32` always fits `usize` on the 32- and 64-bit unix targets.
    let distance = lines.unsigned_abs() as usize;
    if lines < 0 {
        current.saturating_sub(distance)
    } else {
        current.saturating_add(distance)
    }
}

/// `target` clamped so the help screen's last page stays full.
fn help_scroll(target: usize, page: usize) -> usize {
    target.min(help::line_count().saturating_sub(page))
}

/// A location as shown in the full preview's title: `/path` locally,
/// `host:/path` remotely.
fn describe(location: &Location) -> String {
    let path = location.path.display();
    match &location.endpoint {
        Endpoint::Local => path.to_string(),
        Endpoint::Remote { host, sudo: false } => format!("{host}:{path}"),
        Endpoint::Remote { host, sudo: true } => format!("{host}:{path} [sudo]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use image::{DynamicImage, Rgba, RgbaImage};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui_image::picker::Picker;

    use crate::groups::Groups;
    use crate::hosts::HostList;
    use crate::preview::ImagePreview;
    use crate::settings::Settings;
    use crate::state::PersistedState;
    use crate::ui::action::BgEvent;
    use crate::ui::widgets;
    use crate::vfs::SpecialKind;

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.into(),
            kind,
            size: Some(42),
            mtime: Some(1),
            mode: Some(0o644),
        }
    }

    fn local(path: &str) -> Location {
        Location {
            endpoint: Endpoint::Local,
            path: path.into(),
        }
    }

    /// Two local panes: `/l` with a directory, text, Markdown, image, and
    /// FIFO entries, `/r` with one file; both listed, on an 80x24 screen.
    fn app() -> App {
        let (mut app, effects) = App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (Endpoint::Local, Some("/l".into())),
            Some((Endpoint::Local, Some("/r".into()))),
        );
        let listings = [
            vec![
                entry("sub", EntryKind::Dir),
                entry("a.rs", EntryKind::File),
                entry("b.md", EntryKind::File),
                entry("c.png", EntryKind::File),
                entry("pipe", EntryKind::Special(SpecialKind::Fifo)),
            ],
            vec![entry("x.txt", EntryKind::File)],
        ];
        for (effect, entries) in effects.iter().zip(listings) {
            let Effect::ListDir { request, location } = effect else {
                panic!("expected ListDir, got {effect:?}");
            };
            app.handle_event(BgEvent::Listed {
                request: *request,
                location: location.clone(),
                result: Ok(entries),
            });
        }
        app.handle_action(Action::Resize {
            width: 80,
            height: 24,
        });
        app
    }

    /// Puts the left pane's cursor on `name`.
    fn select(app: &mut App, name: &str) {
        let pane = app.core.pane(PaneSide::Left);
        let index = (0..pane.visible_len())
            .find(|&index| pane.visible_entry(index).is_some_and(|e| e.name == name))
            .expect("entry is listed");
        let page = app.core.page_height(PaneSide::Left);
        app.core.pane_mut(PaneSide::Left).set_cursor(index, page);
    }

    /// The request id of a `LoadPreview` for `path`.
    fn load_of(effect: &Effect, path: &str) -> RequestId {
        match effect {
            Effect::LoadPreview { request, location } if *location == local(path) => *request,
            other => panic!("expected LoadPreview of {path}, got {other:?}"),
        }
    }

    fn text(bytes: &[u8]) -> Result<PreviewData, Error> {
        Ok(PreviewData::Text {
            bytes: bytes.to_vec(),
            truncated: false,
        })
    }

    fn image() -> Result<PreviewData, Error> {
        let pixels = RgbaImage::from_pixel(8, 8, Rgba([200, 40, 40, 255]));
        let protocol = Picker::halfblocks().new_resize_protocol(DynamicImage::ImageRgba8(pixels));
        Ok(PreviewData::Image(Box::new(ImagePreview {
            protocol,
            width: 8,
            height: 8,
        })))
    }

    fn plain(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn content(viewer: Option<&Viewer>) -> &ViewerContent {
        &viewer.expect("viewer is open").content
    }

    fn full_lines(app: &App) -> Vec<String> {
        match content(app.preview.full.as_ref()) {
            ViewerContent::Text { text, .. } => plain(text),
            _ => panic!("expected text in the full preview"),
        }
    }

    /// Draws the full preview into a buffer, as render does.
    fn draw_full(app: &mut App) {
        let mut title = String::new();
        let view = app.full_preview_view(&mut title).expect("full preview");
        let area = Rect::new(0, 0, 40, 12);
        let mut buffer = Buffer::empty(area);
        widgets::preview::render(view, area, &mut buffer);
    }

    #[test]
    fn full_preview_loads_highlights_and_drops_stale_answers() {
        let mut app = app();
        select(&mut app, "a.rs");
        let effects = app.preview_action(Action::Preview).expect("claimed");
        assert_eq!(effects.len(), 1);
        let request = load_of(&effects[0], "/l/a.rs");
        assert_eq!(app.preview_input_mode(), Some(InputMode::Viewer));
        assert!(matches!(
            content(app.preview.full.as_ref()),
            ViewerContent::Loading
        ));

        // An answer for another request leaves the viewer loading.
        let stale = RequestId(request.0 + 100);
        assert!(app
            .preview_loaded(stale, local("/l/a.rs"), text(b"x"))
            .is_empty());
        assert!(matches!(
            content(app.preview.full.as_ref()),
            ViewerContent::Loading
        ));

        let source = b"fn main() {}\nlet x = 1;\n";
        assert!(app
            .preview_loaded(request, local("/l/a.rs"), text(source))
            .is_empty());
        assert_eq!(app.preview.pending, None);
        assert_eq!(full_lines(&app), ["fn main() {}", "let x = 1;"]);
        let expected = Highlighter::new(Background::Dark).highlight(OsStr::new("a.rs"), source);
        let ViewerContent::Text { text, .. } = content(app.preview.full.as_ref()) else {
            panic!("expected text");
        };
        assert_eq!(*text, expected);
        assert!(app.preview.highlighter.is_some());

        let mut title = String::new();
        let view = app.full_preview_view(&mut title).expect("view");
        assert_eq!(view.title, "/l/a.rs");
        assert!(matches!(view.body, PreviewBody::Text(_)));
    }

    #[test]
    fn opening_again_cancels_the_load_in_flight() {
        let mut app = app();
        select(&mut app, "a.rs");
        let first = load_of(&app.preview_open(PaneSide::Left)[0], "/l/a.rs");
        app.preview.full = None;
        select(&mut app, "b.md");
        let effects = app.preview_open(PaneSide::Left);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0], Effect::CancelRequest(first));
        let second = load_of(&effects[1], "/l/b.md");
        assert_ne!(first, second);
        // The canceled answer is dropped if it arrives anyway.
        app.preview_loaded(first, local("/l/a.rs"), text(b"old"));
        assert!(matches!(
            content(app.preview.full.as_ref()),
            ViewerContent::Loading
        ));
    }

    #[test]
    fn markdown_starts_rendered_and_toggles_raw() {
        let mut app = app();
        select(&mut app, "b.md");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/b.md");
        let source = b"# Title\n\nSome *text*.\n";
        app.preview_loaded(request, local("/l/b.md"), text(source));
        let rendered = preview::render_markdown(source);
        let raw = Highlighter::new(Background::Dark).highlight(OsStr::new("b.md"), source);
        assert_ne!(plain(&rendered), plain(&raw));
        assert_eq!(full_lines(&app), plain(&rendered));
        let mut title = String::new();
        assert_eq!(
            app.full_preview_view(&mut title).expect("view").title,
            "/l/b.md (rendered)"
        );

        assert_eq!(app.preview_action(Action::ToggleMarkdown), Ok(Vec::new()));
        assert!(!app.preview.full.as_ref().expect("full").rendered);
        assert_eq!(full_lines(&app), plain(&raw));
        assert_eq!(
            app.full_preview_view(&mut title).expect("view").title,
            "/l/b.md (raw)"
        );

        app.preview_action(Action::ToggleMarkdown).expect("claimed");
        assert_eq!(full_lines(&app), plain(&rendered));
    }

    #[test]
    fn toggle_markdown_ignores_other_files() {
        let mut app = app();
        select(&mut app, "a.rs");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/a.rs");
        app.preview_loaded(request, local("/l/a.rs"), text(b"# not markdown\n"));
        let before = full_lines(&app);
        app.preview_action(Action::ToggleMarkdown).expect("claimed");
        assert!(!app.preview.full.as_ref().expect("full").rendered);
        assert_eq!(full_lines(&app), before);
    }

    #[test]
    fn directories_and_special_files_show_attributes_without_a_load() {
        let mut app = app();
        for name in ["sub", "pipe"] {
            select(&mut app, name);
            assert!(app.preview_open(PaneSide::Left).is_empty());
            match content(app.preview.full.as_ref()) {
                ViewerContent::Binary(entry) => assert_eq!(entry.name, name),
                _ => panic!("expected attributes for {name}"),
            }
            assert_eq!(app.preview.pending, None);
            assert_eq!(app.preview_action(Action::Cancel), Ok(Vec::new()));
            assert_eq!(app.preview_input_mode(), None);
        }
    }

    #[test]
    fn binary_answers_and_failures_are_shown() {
        let mut app = app();
        select(&mut app, "a.rs");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/a.rs");
        let attributes = entry("a.rs", EntryKind::File);
        app.preview_loaded(
            request,
            local("/l/a.rs"),
            Ok(PreviewData::Binary {
                entry: attributes.clone(),
            }),
        );
        assert!(
            matches!(content(app.preview.full.as_ref()), ViewerContent::Binary(e) if *e == attributes)
        );

        app.preview_action(Action::Cancel).expect("claimed");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/a.rs");
        let error = Error::Preview {
            message: "image is larger than 20 MiB".into(),
        };
        let message = error.to_string();
        app.preview_loaded(request, local("/l/a.rs"), Err(error));
        assert!(
            matches!(content(app.preview.full.as_ref()), ViewerContent::Failed(m) if *m == message)
        );
        let mut title = String::new();
        let view = app.full_preview_view(&mut title).expect("view");
        assert!(matches!(view.body, PreviewBody::Failed(m) if m == message));
    }

    #[test]
    fn viewer_scrolls_by_line_page_and_wheel_within_the_text() {
        let mut app = app();
        select(&mut app, "a.rs");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/a.rs");
        let source: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        app.preview_loaded(request, local("/l/a.rs"), text(source.as_bytes()));
        // 24 rows minus the border: a page is 22 lines, the last start 78.
        let scroll = |app: &mut App, action: Action| {
            app.preview_action(action).expect("claimed");
            app.preview.full.as_ref().expect("full").scroll
        };
        assert_eq!(scroll(&mut app, Action::MoveCursor(CursorMove::Down)), 1);
        assert_eq!(
            scroll(&mut app, Action::MoveCursor(CursorMove::PageDown)),
            23
        );
        assert_eq!(scroll(&mut app, Action::MoveCursor(CursorMove::End)), 78);
        assert_eq!(scroll(&mut app, Action::MoveCursor(CursorMove::Down)), 78);
        assert_eq!(scroll(&mut app, Action::MoveCursor(CursorMove::PageUp)), 56);
        assert_eq!(scroll(&mut app, Action::MoveCursor(CursorMove::Up)), 55);
        let wheel = |lines| Action::Scroll {
            region: ScrollRegion::Preview,
            lines,
        };
        assert_eq!(scroll(&mut app, wheel(3)), 58);
        assert_eq!(scroll(&mut app, wheel(-100)), 0);
        assert_eq!(scroll(&mut app, wheel(1000)), 78);
        assert_eq!(scroll(&mut app, Action::MoveCursor(CursorMove::Home)), 0);
        // Other actions are claimed and do nothing.
        assert_eq!(app.preview_action(Action::Copy), Ok(Vec::new()));
        assert_eq!(app.preview_input_mode(), Some(InputMode::Viewer));
    }

    #[test]
    fn help_opens_over_the_preview_scrolls_and_closes_first() {
        let mut app = app();
        assert_eq!(app.preview_action(Action::Help), Ok(Vec::new()));
        assert_eq!(app.help_view(), Some(HelpView { scroll: 0 }));
        assert_eq!(app.preview_input_mode(), Some(InputMode::Viewer));
        app.preview_action(Action::MoveCursor(CursorMove::End))
            .expect("claimed");
        let last = help::line_count().saturating_sub(22);
        assert_eq!(app.help_view(), Some(HelpView { scroll: last }));
        app.preview_action(Action::Scroll {
            region: ScrollRegion::Help,
            lines: -1,
        })
        .expect("claimed");
        assert_eq!(
            app.help_view(),
            Some(HelpView {
                scroll: last.saturating_sub(1)
            })
        );
        app.preview_action(Action::Cancel).expect("claimed");
        assert_eq!(app.help_view(), None);
        assert_eq!(app.preview_input_mode(), None);

        // Over the full preview: Esc closes help, then the preview, and `m`
        // does not reach the preview while help is shown.
        select(&mut app, "b.md");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/b.md");
        app.preview_loaded(request, local("/l/b.md"), text(b"# T\n"));
        app.preview_action(Action::Help).expect("claimed");
        app.preview_action(Action::ToggleMarkdown).expect("claimed");
        assert!(app.preview.full.as_ref().expect("full").rendered);
        app.preview_action(Action::Cancel).expect("claimed");
        assert!(app.help_view().is_none());
        assert!(app.preview.full.is_some());
        app.preview_action(Action::Cancel).expect("claimed");
        assert!(app.preview.full.is_none());
    }

    #[test]
    fn closing_a_loading_preview_cancels_its_load() {
        let mut app = app();
        select(&mut app, "a.rs");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/a.rs");
        assert_eq!(
            app.preview_action(Action::Cancel),
            Ok(vec![Effect::CancelRequest(request)])
        );
        assert_eq!(app.preview.pending, None);
    }

    #[test]
    fn unrelated_actions_are_handed_back_outside_viewer_mode() {
        let mut app = app();
        assert_eq!(app.preview_action(Action::Copy), Err(Action::Copy));
        assert_eq!(app.preview_action(Action::Cancel), Err(Action::Cancel));
        assert_eq!(
            app.preview_action(Action::ToggleMarkdown),
            Err(Action::ToggleMarkdown)
        );
        let pane_wheel = Action::Scroll {
            region: ScrollRegion::Pane(PaneSide::Left),
            lines: 1,
        };
        assert_eq!(app.preview_action(pane_wheel.clone()), Err(pane_wheel));
    }

    #[test]
    fn quick_view_follows_the_cursor_with_debounce_and_cancellation() {
        let mut app = app();
        select(&mut app, "a.rs");
        let effects = app
            .preview_action(Action::ToggleQuickView)
            .expect("claimed");
        assert_eq!(effects.len(), 1);
        let first = load_of(&effects[0], "/l/a.rs");
        let mut title = String::new();
        let (side, view) = app.quick_preview_view(&mut title).expect("quick view");
        assert_eq!(side, PaneSide::Right);
        assert_eq!(view.title, "a.rs");
        // Core calls this after the toggle too; nothing changed.
        assert!(app.preview_cursor_moved().is_empty());

        select(&mut app, "b.md");
        let effects = app.preview_cursor_moved();
        let Effect::ScheduleTimer { id, after } = effects[1] else {
            panic!("expected ScheduleTimer, got {effects:?}");
        };
        assert_eq!(effects[0], Effect::CancelRequest(first));
        assert_eq!(after, QUICK_VIEW_DEBOUNCE);
        assert!(matches!(
            content(app.preview.quick.as_ref()),
            ViewerContent::Loading
        ));

        // Moving again reuses the timer id; nothing is in flight to cancel.
        select(&mut app, "a.rs");
        assert_eq!(
            app.preview_cursor_moved(),
            [Effect::ScheduleTimer {
                id,
                after: QUICK_VIEW_DEBOUNCE
            }]
        );

        let now = Instant::now();
        assert_eq!(app.preview_timer(TimerId(id.0 + 1), now), None);
        let effects = app.preview_timer(id, now).expect("debounce timer");
        let request = load_of(&effects[0], "/l/a.rs");
        // A late duplicate fire starts nothing.
        assert_eq!(app.preview_timer(id, now), Some(Vec::new()));

        app.preview_loaded(request, local("/l/a.rs"), text(b"one\n"));
        assert!(matches!(
            content(app.preview.quick.as_ref()),
            ViewerContent::Text { .. }
        ));

        // A directory shows its attributes at once.
        select(&mut app, "sub");
        assert!(app.preview_cursor_moved().is_empty());
        assert!(matches!(
            content(app.preview.quick.as_ref()),
            ViewerContent::Binary(_)
        ));
    }

    #[test]
    fn quick_view_toggles_off_and_follows_focus() {
        let mut app = app();
        select(&mut app, "a.rs");
        let request = load_of(
            &app.preview_action(Action::ToggleQuickView)
                .expect("claimed")[0],
            "/l/a.rs",
        );
        assert_eq!(
            app.preview_action(Action::ToggleQuickView),
            Ok(vec![Effect::CancelRequest(request)])
        );
        assert!(!app.preview.quick_view);
        assert!(app.preview.quick.is_none());
        let mut title = String::new();
        assert!(app.quick_preview_view(&mut title).is_none());
        assert!(app.preview_cursor_moved().is_empty());

        // With focus on the right pane, quick view shows its cursor entry on
        // the left.
        app.preview_action(Action::ToggleQuickView)
            .expect("claimed");
        // Core runs the quick view follow-up after every browse action.
        let effects = app.handle_action(Action::SwitchFocus);
        assert!(matches!(effects.last(), Some(Effect::ScheduleTimer { .. })));
        let (side, view) = app.quick_preview_view(&mut title).expect("quick view");
        assert_eq!(side, PaneSide::Left);
        assert_eq!(view.title, "x.txt");
    }

    #[test]
    fn quick_view_scrolls_with_the_wheel() {
        let mut app = app();
        select(&mut app, "a.rs");
        let request = load_of(
            &app.preview_action(Action::ToggleQuickView)
                .expect("claimed")[0],
            "/l/a.rs",
        );
        let source: String = (1..=50).map(|n| format!("{n}\n")).collect();
        app.preview_loaded(request, local("/l/a.rs"), text(source.as_bytes()));
        let wheel = |lines| Action::Scroll {
            region: ScrollRegion::Preview,
            lines,
        };
        // The right pane is 23 rows: 21 text rows, the last start 29.
        app.preview_action(wheel(5)).expect("claimed");
        assert_eq!(app.preview.quick.as_ref().expect("quick").scroll, 5);
        app.preview_action(wheel(100)).expect("claimed");
        assert_eq!(app.preview.quick.as_ref().expect("quick").scroll, 29);
    }

    #[test]
    fn full_preview_takes_over_the_quick_view_and_hands_it_back() {
        let mut app = app();
        select(&mut app, "a.rs");
        let request = load_of(
            &app.preview_action(Action::ToggleQuickView)
                .expect("claimed")[0],
            "/l/a.rs",
        );
        // The quick view's load in flight now fills the full preview.
        assert!(app.preview_open(PaneSide::Left).is_empty());
        app.preview_loaded(request, local("/l/a.rs"), text(b"shared\n"));
        assert_eq!(full_lines(&app), ["shared"]);
        assert!(matches!(
            content(app.preview.quick.as_ref()),
            ViewerContent::Loading
        ));

        // Closing hands the content back without loading again.
        assert_eq!(app.preview_action(Action::Cancel), Ok(Vec::new()));
        match content(app.preview.quick.as_ref()) {
            ViewerContent::Text { text, .. } => assert_eq!(plain(text), ["shared"]),
            _ => panic!("expected the text back in the quick view"),
        }

        // Loaded content moves over without a load as well.
        assert!(app.preview_open(PaneSide::Left).is_empty());
        assert_eq!(full_lines(&app), ["shared"]);
    }

    #[test]
    fn closing_hands_an_unfinished_load_back_to_the_quick_view() {
        let mut app = app();
        select(&mut app, "a.rs");
        let request = load_of(
            &app.preview_action(Action::ToggleQuickView)
                .expect("claimed")[0],
            "/l/a.rs",
        );
        app.preview_open(PaneSide::Left);
        assert_eq!(app.preview_action(Action::Cancel), Ok(Vec::new()));
        app.preview_loaded(request, local("/l/a.rs"), text(b"late\n"));
        assert!(matches!(
            content(app.preview.quick.as_ref()),
            ViewerContent::Text { .. }
        ));
    }

    #[test]
    fn quick_view_waits_while_the_full_preview_is_open() {
        let mut app = app();
        select(&mut app, "sub");
        app.preview_action(Action::ToggleQuickView)
            .expect("claimed");
        select(&mut app, "a.rs");
        let effects = app.preview_open(PaneSide::Left);
        let full = load_of(&effects[0], "/l/a.rs");

        // A listing moves the cursor while the full preview loads: the quick
        // view follows without canceling the full preview's load.
        select(&mut app, "b.md");
        assert!(app.preview_cursor_moved().is_empty());
        assert_eq!(
            app.preview.pending,
            Some(PendingPreview {
                request: full,
                target: PreviewTarget::Full
            })
        );
        let id = app.preview.debounce.unwrap_or(TimerId(u64::MAX));
        assert_eq!(app.preview_timer(id, Instant::now()), None);

        // Closing cancels the full load and starts the quick view's.
        let effects = app.preview_action(Action::Cancel).expect("claimed");
        assert_eq!(effects[0], Effect::CancelRequest(full));
        load_of(&effects[1], "/l/b.md");
        assert_eq!(
            app.preview.pending.map(|pending| pending.target),
            Some(PreviewTarget::Quick)
        );
    }

    #[test]
    fn debounce_firing_while_the_full_preview_is_open_starts_nothing() {
        let mut app = app();
        select(&mut app, "a.rs");
        app.preview_action(Action::ToggleQuickView)
            .expect("claimed");
        select(&mut app, "b.md");
        let effects = app.preview_cursor_moved();
        let Some(&Effect::ScheduleTimer { id, .. }) = effects.last() else {
            panic!("expected ScheduleTimer, got {effects:?}");
        };
        select(&mut app, "c.png");
        load_of(&app.preview_open(PaneSide::Left)[0], "/l/c.png");
        assert_eq!(app.preview_timer(id, Instant::now()), Some(Vec::new()));
    }

    #[test]
    fn image_resizes_reach_the_awaiting_viewer() {
        let mut app = app();
        let requests = app.preview.resize_requests.take().expect("receiver");
        select(&mut app, "c.png");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/c.png");
        app.preview_loaded(request, local("/l/c.png"), image());
        let awaiting = |app: &App| app.preview.full.as_ref().expect("full").awaiting_resize();
        assert!(!awaiting(&app));

        // The first draw sends a resize request and gives the protocol away.
        draw_full(&mut app);
        assert!(awaiting(&app));
        let resize = requests.try_recv().expect("resize request");
        app.preview_image_resized(preview::resize(resize));
        assert!(!awaiting(&app));
        // Drawing at the same size needs no new resize.
        draw_full(&mut app);
        assert!(requests.try_recv().is_err());

        // A response nobody awaits is dropped.
        app.preview_image_resized(ImageResized {
            result: Err(Error::Preview {
                message: "stale".into(),
            }),
        });
        assert!(matches!(
            content(app.preview.full.as_ref()),
            ViewerContent::Image(_)
        ));
    }

    #[test]
    fn a_failed_image_resize_becomes_a_message() {
        let mut app = app();
        let requests = app.preview.resize_requests.take().expect("receiver");
        select(&mut app, "c.png");
        let request = load_of(&app.preview_open(PaneSide::Left)[0], "/l/c.png");
        app.preview_loaded(request, local("/l/c.png"), image());
        draw_full(&mut app);
        assert!(requests.try_recv().is_ok());
        let error = Error::Preview {
            message: "encoder failed".into(),
        };
        let message = error.to_string();
        app.preview_image_resized(ImageResized { result: Err(error) });
        assert!(
            matches!(content(app.preview.full.as_ref()), ViewerContent::Failed(m) if *m == message)
        );
    }

    #[test]
    fn opening_another_file_drops_the_quick_view_image() {
        let mut app = app();
        select(&mut app, "c.png");
        let request = load_of(
            &app.preview_action(Action::ToggleQuickView)
                .expect("claimed")[0],
            "/l/c.png",
        );
        app.preview_loaded(request, local("/l/c.png"), image());
        assert!(matches!(
            content(app.preview.quick.as_ref()),
            ViewerContent::Image(_)
        ));

        // A double click on the right pane: core focuses it and opens the
        // preview before calling `preview_cursor_moved`.
        app.handle_action(Action::SwitchFocus);
        let effects = app.preview_open(PaneSide::Right);
        load_of(&effects[0], "/r/x.txt");
        assert!(matches!(
            content(app.preview.quick.as_ref()),
            ViewerContent::Loading
        ));
        // The quick view follows the new focus without loading, then gets
        // the full preview's content on close.
        assert!(app.preview_cursor_moved().is_empty());
        let request = app.preview.pending.expect("full load").request;
        app.preview_loaded(request, local("/r/x.txt"), text(b"right\n"));
        assert_eq!(app.preview_action(Action::Cancel), Ok(Vec::new()));
        let quick = app.preview.quick.as_ref().expect("quick");
        assert_eq!(quick.location, local("/r/x.txt"));
        assert!(matches!(quick.content, ViewerContent::Text { .. }));
    }

    #[test]
    fn remote_titles_name_the_host() {
        assert_eq!(
            describe(&Location {
                endpoint: Endpoint::Remote {
                    host: "web".into(),
                    sudo: true,
                },
                path: PathBuf::from("/etc/hosts"),
            }),
            "web:/etc/hosts [sudo]"
        );
        assert_eq!(file_name(&local("/")), OsStr::new("/"));
    }
}
