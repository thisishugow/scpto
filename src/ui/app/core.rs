//! Core of the app state machine: construction, dispatch of every `Action`
//! and `BgEvent` to the feature modules, the two panes and which one has
//! focus, the terminal size and the layout derived from it, the input mode
//! reported to `ui/input.rs`, status messages, id allocation, and quit.
//!
//! Per-pane list behavior (cursor, selection, sort, filter, hidden files,
//! loading) is implemented by `ui::pane::Pane`; core only decides which pane
//! an action applies to, passes the page height, and turns pane navigation
//! into `Effect::ListDir` and `Effect::RememberDirectory`. Core does not
//! implement features: transfers, dialogs, connections, sync, edit, and
//! preview handle their own actions and events in their modules, and core
//! routes to them by input mode and event kind.
//!
//! Startup: the left pane lists its directory at once (a local path is
//! always absolute, a remote one waits for `BgEvent::Connected`). When the
//! right pane is not given it starts as `Local` in the same directory with
//! the host picker open on it. A remote pane given without a path opens the
//! endpoint's remembered last directory when one exists, else its home.
//!
//! # Public API (Wave 2, ui/app/core)
//!
//! ```text
//! impl App {
//!     /// Builds the app and the effects of the first frame (connects,
//!     /// listings, `ResolveHost` for the picker's aliases). `left` and
//!     /// `right` are an endpoint and an optional absolute start directory.
//!     pub fn new(
//!         settings: Settings,
//!         hosts: HostList,
//!         groups: Groups,
//!         state: PersistedState,
//!         background: Background,
//!         left: (Endpoint, Option<PathBuf>),
//!         right: Option<(Endpoint, Option<PathBuf>)>,
//!     ) -> (App, Vec<Effect>);
//!
//!     /// Handles one user action. Pure; returns the effects to execute.
//!     pub fn handle_action(&mut self, action: Action) -> Vec<Effect>;
//!     /// Handles one background result. Pure; returns follow-up effects.
//!     pub fn handle_event(&mut self, event: BgEvent) -> Vec<Effect>;
//!     /// How `ui/input.rs` must interpret the next key.
//!     pub fn input_mode(&self) -> InputMode;
//! }
//!
//! impl CoreState {
//!     pub fn pane(&self, side: PaneSide) -> &Pane;
//!     pub fn pane_mut(&mut self, side: PaneSide) -> &mut Pane;
//!     pub fn focused(&self) -> PaneSide;
//!     /// Screen areas for the current size and panel visibility; render
//!     /// draws into exactly these so key paging and the screen agree.
//!     pub fn layout(&self) -> AppLayout;
//!     /// List rows of a pane: its layout height minus
//!     /// `widgets::pane::CHROME_ROWS`.
//!     pub fn page_height(&self, side: PaneSide) -> usize;
//!     /// Allocators shared by every feature; ids are never reused.
//!     pub fn next_request(&mut self) -> RequestId;
//!     pub fn next_job(&mut self) -> JobId;
//!     pub fn next_timer(&mut self) -> TimerId;
//!     /// The status bar message.
//!     pub fn set_status(&mut self, message: StatusMessage);
//!     pub fn status(&self) -> Option<&StatusMessage>;
//! }
//!
//! pub struct AppLayout {
//!     pub left: Rect,
//!     pub right: Rect,
//!     /// The queue panel when visible.
//!     pub queue: Option<Rect>,
//!     pub status_bar: Rect,
//! }
//!
//! pub struct StatusMessage {
//!     pub text: String,
//!     /// Drawn in the error style (failures and configuration warnings).
//!     pub error: bool,
//! }
//! ```

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct CoreState {}
