//! Endpoint connections in the app: per-endpoint status (connecting,
//! connected, reconnecting with countdown), endpoint switching from the host
//! picker, interactive auth hand-off, the masked sudo password prompt, the
//! VPN hint dialog, and closing endpoints nothing uses any more.
//!
//! # Behavior
//!
//! - Host picker (`o`, and at startup on the right pane when no right
//!   endpoint was given): `App::connection_open_picker` sets
//!   `DialogState::picker` to a `PickerKind::Hosts` list: `Local` first,
//!   then every alias of `App::hosts` that `Groups::is_excluded` does not
//!   exclude. Tabs are `All`, the sshto group names, and `Other`
//!   (`Groups::classify` with the resolved hostname once known). Each
//!   row's detail is `user@hostname` from [`ConnectionState::resolved`];
//!   `HostResolved` updates the detail and the tab of an open picker's
//!   rows in place. A pick (`App::connection_picked`) shows the endpoint
//!   (`Remote { sudo: false }` or `Local`) with `App::switch_endpoint`.
//! - `S` reconnects the focused pane's endpoint with sudo:
//!   `App::switch_endpoint(side, Remote { host, sudo: true }, cwd)`; on a
//!   local or already-sudo pane it is a status message.
//! - Status: every `Effect::Connect` the app emits (by core or here) is
//!   reflected in [`ConnectionState::endpoints`]; the pane title shows
//!   `connecting`, `reconnecting in N s`, `authenticating`, `waiting for
//!   sudo password`, or `failed`. The reconnect countdown ticks with a
//!   one-second `Effect::ScheduleTimer` ([`ConnectionState::countdown`])
//!   while any endpoint is reconnecting.
//! - `InteractiveAuthRequired`: emits `Effect::InteractiveAuth` right away
//!   (the runtime runs foreground effects one at a time) and shows
//!   `authenticating`.
//! - `SudoPasswordRequired`: pushes a `DialogPurpose::SudoPassword` dialog
//!   with a masked field (`max_len: sudo::PASSWORD_MAX_LEN`, value created
//!   with that capacity) and Ok / Cancel. Ok → `Effect::SudoPassword` with
//!   the field's `Zeroizing<String>` moved into a `Secret` (never copied);
//!   Cancel → the endpoint is `failed` and panes showing it switch back to
//!   the same host without sudo.
//! - `Reconnecting`: status with countdown. When `error.vpn_hint_eligible()`
//!   and `Settings::vpn_rule(alias, resolved hostname)` matches, a
//!   `DialogPurpose::Vpn` dialog (Run / Cancel, the message shows the
//!   command) opens once per endpoint until it connects. Run →
//!   `Effect::RunVpn`; `VpnFinished` Ok → `Effect::Reconnect`, Err → error
//!   status (the backoff keeps retrying).
//! - `Reconnected`: status cleared and
//!   `Effect::Transfer(TransferCommand::EndpointReconnected)`, so parked
//!   items resume (core refreshes the panes).
//! - Releasing endpoints (`App::connection_release_unused`): an endpoint
//!   in [`ConnectionState::endpoints`] that no pane shows, no unfinished
//!   job of `TransferState::jobs` uses (source or destination), no running
//!   sync of `SyncState::running` uses, and no edit of `EditState::active`
//!   uses gets `Effect::Disconnect` and is removed from the map. `Local` is
//!   never disconnected.
//!
//! # Public API (Wave 3, ui/app/connection)
//!
//! ```text
//! impl App {
//!     /// Claims `OpenEndpointPicker` and `ReconnectWithSudo`; hands every
//!     /// other action back unchanged.
//!     pub fn connection_action(&mut self, action: Action) -> Result<Vec<Effect>, Action>;
//!     /// Opens the host picker for `side` (startup and `o`).
//!     pub fn connection_open_picker(&mut self, side: PaneSide) -> Vec<Effect>;
//!     /// A row of the host picker was picked (`Answer::Picked` with
//!     /// `PickerKind::Hosts { side }`).
//!     pub fn connection_picked(&mut self, side: PaneSide, target: PickerTarget) -> Vec<Effect>;
//!     /// Answers of `DialogPurpose::{SudoPassword, Vpn}`.
//!     pub fn connection_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect>;
//!     /// Records an `Effect::Connect` core emitted, so status shows
//!     /// `connecting`.
//!     pub fn connection_connecting(&mut self, endpoint: &Endpoint);
//!
//!     /// Background events, one method per kind.
//!     pub fn connection_connected(&mut self, endpoint: &Endpoint) -> Vec<Effect>;
//!     pub fn connection_failed(&mut self, endpoint: &Endpoint, error: &Error) -> Vec<Effect>;
//!     pub fn connection_interactive_auth(&mut self, endpoint: Endpoint, error: ConnectError) -> Vec<Effect>;
//!     pub fn connection_sudo_password_required(&mut self, endpoint: Endpoint) -> Vec<Effect>;
//!     pub fn connection_disconnected(&mut self, endpoint: &Endpoint, error: &ConnectError) -> Vec<Effect>;
//!     pub fn connection_reconnecting(&mut self, endpoint: Endpoint, attempt: u32, delay: Duration, error: ConnectError) -> Vec<Effect>;
//!     pub fn connection_reconnected(&mut self, endpoint: &Endpoint) -> Vec<Effect>;
//!     pub fn connection_host_resolved(&mut self, alias: String, result: Result<ResolvedHost, Error>) -> Vec<Effect>;
//!     pub fn connection_vpn_finished(&mut self, endpoint: Endpoint, result: Result<(), Error>) -> Vec<Effect>;
//!     /// `Some` when `id` is this feature's countdown timer.
//!     pub fn connection_timer(&mut self, id: TimerId, at: Instant) -> Option<Vec<Effect>>;
//!
//!     /// `Effect::Disconnect` for endpoints nothing uses (see Behavior).
//!     pub fn connection_release_unused(&mut self) -> Vec<Effect>;
//!
//!     /// Pane title status for render, for example `reconnecting in 4 s`;
//!     /// `None` when connected.
//!     pub fn connection_status_text(&self, endpoint: &Endpoint) -> Option<String>;
//! }
//! ```

use std::collections::HashMap;
use std::time::Duration;

use crate::error::{ConnectError, Error};
use crate::hosts::ResolvedHost;
use crate::ui::action::TimerId;
use crate::vfs::Endpoint;

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct ConnectionState {
    /// Every endpoint the app asked to connect and has not released, with
    /// its status.
    pub endpoints: HashMap<Endpoint, EndpointStatus>,
    /// `ssh -G` answers by alias.
    pub resolved: HashMap<String, ResolvedHost>,
    /// The one-second countdown timer while any endpoint is reconnecting.
    pub countdown: Option<TimerId>,
    /// Endpoints whose VPN dialog was offered since they last connected.
    pub vpn_offered: Vec<Endpoint>,
}

/// Connection status of one endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointStatus {
    /// `Effect::Connect` is in flight.
    Connecting,
    /// Connected and usable.
    Connected,
    /// `Effect::InteractiveAuth` is pending or running.
    Authenticating,
    /// The sudo password dialog is open.
    WaitingForPassword,
    /// Waiting to retry after a retryable failure.
    Reconnecting {
        /// One-based number of the next attempt.
        attempt: u32,
        /// Time left before it, counted down by the countdown timer.
        remaining: Duration,
        /// Why the last attempt failed.
        error: ConnectError,
    },
    /// Failed for good (no automatic retry).
    Failed(Error),
}
