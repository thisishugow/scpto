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
//!   (`Groups::classify` with the resolved hostname once known); without
//!   sshto groups the list has no tab row. `Local` sits in `Other`. Each
//!   row's detail is `user@hostname` (or `hostname` without a user) from
//!   [`ConnectionState::resolved`]; `HostResolved` updates the detail and
//!   the tab of an open picker's rows in place. A failed resolution leaves
//!   the row without detail (connecting reports the real problem). A pick
//!   (`App::connection_picked`) shows the endpoint (`Remote { sudo: false }`
//!   or `Local`) with `App::switch_endpoint`. Picking the endpoint the pane
//!   already shows retries it: `Effect::Connect` when it failed,
//!   `Effect::Reconnect` (retry now) while it waits for a retry.
//! - `S` reconnects the focused pane's endpoint with sudo:
//!   `App::switch_endpoint(side, Remote { host, sudo: true }, cwd)`; on a
//!   local or already-sudo pane it is a status message.
//! - Status: every `Effect::Connect` the app emits (by core or here) is
//!   reflected in [`ConnectionState::endpoints`]; the pane title shows
//!   `connecting`, `reconnecting in N s`, `authenticating`, `waiting for
//!   sudo password`, or `failed`. The reconnect countdown ticks with a
//!   one-second `Effect::ScheduleTimer` ([`ConnectionState::countdown`])
//!   while any endpoint is reconnecting. `Disconnected` shows
//!   `reconnecting` until the session announces the delay of its retry.
//! - `InteractiveAuthRequired`: emits `Effect::InteractiveAuth` right away
//!   (the runtime runs foreground effects one at a time) and shows
//!   `authenticating`.
//! - `SudoPasswordRequired`: pushes a `DialogPurpose::SudoPassword` dialog
//!   (unless one for the endpoint is already open) with a masked field
//!   (`max_len: sudo::PASSWORD_MAX_LEN`, value created with that capacity)
//!   and Ok / Cancel. Ok → `Effect::SudoPassword` with the field's
//!   `Zeroizing<String>` moved into a `Secret` (never copied); Cancel → the
//!   endpoint is `failed` and panes showing it switch back to the same host
//!   without sudo. The password never reaches any other state, status
//!   text, or message.
//! - sudo failures (`ConnectFailed` with `Error::Sudo`): core shows the
//!   `SudoError` message in the status bar. `WrongPassword` asks again with
//!   a fresh dialog that says the password was incorrect; any other sudo
//!   failure is `failed` and switches the panes back without sudo, like
//!   Cancel.
//! - `Reconnecting`: status with countdown. When `error.vpn_hint_eligible()`
//!   and `Settings::vpn_rule(alias, resolved hostname)` matches, a
//!   `DialogPurpose::Vpn` dialog (Run / Cancel, the message shows the
//!   command) opens once per endpoint until it connects. Run →
//!   `Effect::RunVpn`; `VpnFinished` Ok → `Effect::Reconnect`, Err → error
//!   status (the backoff keeps retrying). A VPN or password dialog still
//!   open when its endpoint connects or is released is closed.
//! - `Reconnected`: status cleared and
//!   `Effect::Transfer(TransferCommand::EndpointReconnected)`, so parked
//!   items resume (core refreshes the panes).
//! - Releasing endpoints (`App::connection_release_unused`): an endpoint
//!   in [`ConnectionState::endpoints`] that no pane shows, no unfinished
//!   job of `TransferState::jobs` uses (source or destination), no running
//!   sync of `SyncState::running` uses, and no edit of `EditState::active`
//!   uses gets `Effect::Disconnect` and is removed from the map. `Local` is
//!   never disconnected. The pick, `S`, and sudo switch-back paths release
//!   the endpoint they leave themselves; calling it again is a no-op.
//! - Session events for an endpoint that is neither in the map nor used
//!   are stale (it was released while they were in flight) and ignored.
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
use std::path::PathBuf;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use super::core::StatusMessage;
use super::dialog::{
    Dialog, DialogAnswer, DialogFocus, DialogPurpose, PickerItem, PickerKind, PickerList,
    PickerTarget, TextField,
};
use super::App;
use crate::error::{ConnectError, Error, SudoError};
use crate::hosts::ResolvedHost;
use crate::sudo::PASSWORD_MAX_LEN;
use crate::transfer::model::TransferCommand;
use crate::ui::action::{Action, DialogButton, Effect, PaneSide, Secret, TimerId};
use crate::vfs::Endpoint;

/// Interval of the reconnect countdown timer.
const COUNTDOWN_TICK: Duration = Duration::from_secs(1);

/// Caption of the host picker tab that shows every row.
const TAB_ALL: &str = "All";

/// Caption of the host picker tab for hosts no sshto group claims.
const TAB_OTHER: &str = "Other";

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

impl App {
    /// Claims `OpenEndpointPicker` and `ReconnectWithSudo`; hands every
    /// other action back unchanged.
    pub fn connection_action(&mut self, action: Action) -> Result<Vec<Effect>, Action> {
        match action {
            Action::OpenEndpointPicker => Ok(self.connection_open_picker(self.core.focused())),
            Action::ReconnectWithSudo => Ok(self.reconnect_with_sudo(self.core.focused())),
            other => Err(other),
        }
    }

    /// Opens the host picker for `side` (startup and `o`).
    pub fn connection_open_picker(&mut self, side: PaneSide) -> Vec<Effect> {
        let tabs = if self.groups.names.is_empty() {
            Vec::new()
        } else {
            let mut tabs = Vec::with_capacity(self.groups.names.len() + 2);
            tabs.push(TAB_ALL.to_owned());
            tabs.extend(self.groups.names.iter().cloned());
            tabs.push(TAB_OTHER.to_owned());
            tabs
        };
        let mut items = vec![PickerItem {
            label: Endpoint::Local.to_string(),
            detail: String::new(),
            tab: self.other_tab(),
            target: PickerTarget::Local,
        }];
        items.extend(
            self.hosts
                .aliases
                .iter()
                .filter(|alias| !self.groups.is_excluded(alias))
                .map(|alias| PickerItem {
                    label: alias.clone(),
                    detail: self.host_detail(alias),
                    tab: self.host_tab(alias),
                    target: PickerTarget::Host(alias.clone()),
                }),
        );
        let pane = match side {
            PaneSide::Left => "left",
            PaneSide::Right => "right",
        };
        self.dialog.picker = Some(PickerList {
            kind: PickerKind::Hosts { side },
            title: format!("Open in the {pane} pane"),
            tabs,
            active_tab: 0,
            filter: String::new(),
            items,
            cursor: 0,
            first_visible: 0,
        });
        Vec::new()
    }

    /// A row of the host picker was picked (`Answer::Picked` with
    /// `PickerKind::Hosts { side }`).
    pub fn connection_picked(&mut self, side: PaneSide, target: PickerTarget) -> Vec<Effect> {
        let endpoint = match target {
            PickerTarget::Local => Endpoint::Local,
            PickerTarget::Host(host) => Endpoint::Remote { host, sudo: false },
            // The host picker has no bookmark rows; bookmark picks are the
            // dialog feature's.
            PickerTarget::Bookmark(_) => return Vec::new(),
        };
        if self.core.pane(side).endpoint == endpoint {
            return self.retry(endpoint);
        }
        self.switch_and_release(side, endpoint, None)
    }

    /// Answers of `DialogPurpose::{SudoPassword, Vpn}`.
    pub fn connection_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect> {
        let DialogAnswer {
            purpose,
            button,
            field,
            ..
        } = answer;
        match purpose {
            DialogPurpose::SudoPassword { endpoint } => match (button, field) {
                (DialogButton::Ok, Some(password)) => {
                    self.connection
                        .endpoints
                        .insert(endpoint.clone(), EndpointStatus::Connecting);
                    vec![Effect::SudoPassword {
                        endpoint,
                        password: Secret::new(password),
                    }]
                }
                // Cancel, Esc, or an answer without the field: the field (if
                // any) is dropped here, which zeroizes it.
                _ => {
                    self.core.set_status(StatusMessage::info(format!(
                        "sudo canceled for {}",
                        host_of(&endpoint).unwrap_or_default()
                    )));
                    self.leave_sudo(endpoint, Error::Canceled)
                }
            },
            DialogPurpose::Vpn { endpoint, command } => match button {
                DialogButton::Run => vec![Effect::RunVpn { endpoint, command }],
                // Cancel: the session's backoff keeps retrying.
                _ => Vec::new(),
            },
            // Core routes only the two purposes above to this feature.
            _ => Vec::new(),
        }
    }

    /// Records an `Effect::Connect` core emitted, so status shows
    /// `connecting`.
    pub fn connection_connecting(&mut self, endpoint: &Endpoint) {
        self.connection
            .endpoints
            .insert(endpoint.clone(), EndpointStatus::Connecting);
    }

    /// `BgEvent::Connected`: the endpoint is usable; a VPN offer may be made
    /// again after a later failure.
    pub fn connection_connected(&mut self, endpoint: &Endpoint) -> Vec<Effect> {
        if self.is_stale(endpoint) {
            return Vec::new();
        }
        self.mark_connected(endpoint);
        Vec::new()
    }

    /// `BgEvent::ConnectFailed`: core already shows the error in the status
    /// bar. A wrong sudo password asks again; other sudo failures switch the
    /// panes back to the host without sudo.
    pub fn connection_failed(&mut self, endpoint: &Endpoint, error: &Error) -> Vec<Effect> {
        if self.is_stale(endpoint) {
            return Vec::new();
        }
        match error {
            Error::Sudo(SudoError::WrongPassword) => {
                self.ask_sudo_password(endpoint.clone(), true);
                Vec::new()
            }
            Error::Sudo(_) => self.leave_sudo(endpoint.clone(), error.clone()),
            _ => {
                self.connection
                    .endpoints
                    .insert(endpoint.clone(), EndpointStatus::Failed(error.clone()));
                Vec::new()
            }
        }
    }

    /// `BgEvent::InteractiveAuthRequired`: hands the terminal to OpenSSH.
    pub fn connection_interactive_auth(
        &mut self,
        endpoint: Endpoint,
        error: ConnectError,
    ) -> Vec<Effect> {
        if self.is_stale(&endpoint) {
            return Vec::new();
        }
        self.core.set_status(StatusMessage::info(format!(
            "{endpoint}: {error}; continuing in the terminal"
        )));
        self.connection
            .endpoints
            .insert(endpoint.clone(), EndpointStatus::Authenticating);
        vec![Effect::InteractiveAuth(endpoint)]
    }

    /// `BgEvent::SudoPasswordRequired`: opens the masked password dialog.
    pub fn connection_sudo_password_required(&mut self, endpoint: Endpoint) -> Vec<Effect> {
        if self.is_stale(&endpoint) {
            return Vec::new();
        }
        self.ask_sudo_password(endpoint, false);
        Vec::new()
    }

    /// `BgEvent::Disconnected`: the session retries at once and announces
    /// the delay with `Reconnecting`; until then the pane shows
    /// `reconnecting`. Core shows the error in the status bar.
    pub fn connection_disconnected(
        &mut self,
        endpoint: &Endpoint,
        error: &ConnectError,
    ) -> Vec<Effect> {
        if self.is_stale(endpoint) {
            return Vec::new();
        }
        self.connection.endpoints.insert(
            endpoint.clone(),
            EndpointStatus::Reconnecting {
                attempt: 1,
                remaining: Duration::ZERO,
                error: error.clone(),
            },
        );
        Vec::new()
    }

    /// `BgEvent::Reconnecting`: shows the countdown and offers the VPN
    /// command of a matching rule once.
    pub fn connection_reconnecting(
        &mut self,
        endpoint: Endpoint,
        attempt: u32,
        delay: Duration,
        error: ConnectError,
    ) -> Vec<Effect> {
        if self.is_stale(&endpoint) {
            return Vec::new();
        }
        self.core.set_status(StatusMessage::error(format!(
            "{endpoint}: {error}; reconnecting (attempt {attempt})"
        )));
        self.offer_vpn(&endpoint, &error);
        self.connection.endpoints.insert(
            endpoint,
            EndpointStatus::Reconnecting {
                attempt,
                remaining: delay,
                error,
            },
        );
        // Scheduling the same id again replaces the pending tick, so the
        // newest countdown starts exactly at its announced delay.
        let id = match self.connection.countdown {
            Some(id) => id,
            None => {
                let id = self.core.next_timer();
                self.connection.countdown = Some(id);
                id
            }
        };
        vec![Effect::ScheduleTimer {
            id,
            after: COUNTDOWN_TICK,
        }]
    }

    /// `BgEvent::Reconnected`: parked transfer items resume (core refreshes
    /// the panes).
    pub fn connection_reconnected(&mut self, endpoint: &Endpoint) -> Vec<Effect> {
        if self.is_stale(endpoint) {
            return Vec::new();
        }
        self.mark_connected(endpoint);
        self.core
            .set_status(StatusMessage::info(format!("Reconnected to {endpoint}")));
        vec![Effect::Transfer(TransferCommand::EndpointReconnected(
            endpoint.clone(),
        ))]
    }

    /// `BgEvent::HostResolved`: records the answer and updates the rows of
    /// an open host picker. A failed resolution leaves the row as it is.
    pub fn connection_host_resolved(
        &mut self,
        alias: String,
        result: Result<ResolvedHost, Error>,
    ) -> Vec<Effect> {
        // Connecting to the alias reports the actual problem; the picker
        // only loses the optional detail.
        let Ok(resolved) = result else {
            return Vec::new();
        };
        self.connection.resolved.insert(alias.clone(), resolved);
        let detail = self.host_detail(&alias);
        let tab = self.host_tab(&alias);
        if let Some(picker) = self
            .dialog
            .picker
            .as_mut()
            .filter(|picker| matches!(picker.kind, PickerKind::Hosts { .. }))
        {
            for item in picker
                .items
                .iter_mut()
                .filter(|item| matches!(&item.target, PickerTarget::Host(host) if *host == alias))
            {
                item.detail = detail.clone();
                item.tab = tab;
            }
        }
        Vec::new()
    }

    /// `BgEvent::VpnFinished`: reconnects at once after a successful VPN
    /// command; after a failed one the backoff keeps retrying.
    pub fn connection_vpn_finished(
        &mut self,
        endpoint: Endpoint,
        result: Result<(), Error>,
    ) -> Vec<Effect> {
        if let Err(error) = result {
            self.core
                .set_status(StatusMessage::error(error.to_string()));
            return Vec::new();
        }
        match self.connection.endpoints.get(&endpoint) {
            // Released while the command ran, or back on its own: nothing
            // to retry.
            None | Some(EndpointStatus::Connected) => Vec::new(),
            Some(_) => {
                self.core.set_status(StatusMessage::info(format!(
                    "VPN command finished; reconnecting to {endpoint}"
                )));
                vec![Effect::Reconnect(endpoint)]
            }
        }
    }

    /// `Some` when `id` is this feature's countdown timer. Each tick takes
    /// one second off every reconnect countdown; the timer stops once no
    /// countdown has time left. The countdown counts whole ticks, so the
    /// fire time is not needed.
    pub fn connection_timer(&mut self, id: TimerId, _at: Instant) -> Option<Vec<Effect>> {
        if self.connection.countdown != Some(id) {
            return None;
        }
        let mut running = false;
        for status in self.connection.endpoints.values_mut() {
            if let EndpointStatus::Reconnecting { remaining, .. } = status {
                *remaining = remaining.saturating_sub(COUNTDOWN_TICK);
                running |= !remaining.is_zero();
            }
        }
        if running {
            Some(vec![Effect::ScheduleTimer {
                id,
                after: COUNTDOWN_TICK,
            }])
        } else {
            self.connection.countdown = None;
            Some(Vec::new())
        }
    }

    /// `Effect::Disconnect` for endpoints nothing uses (see Behavior).
    pub fn connection_release_unused(&mut self) -> Vec<Effect> {
        let mut unused: Vec<Endpoint> = self
            .connection
            .endpoints
            .keys()
            .filter(|endpoint| !endpoint.is_local() && !self.is_in_use(endpoint))
            .cloned()
            .collect();
        unused.sort_by(|a, b| endpoint_key(a).cmp(&endpoint_key(b)));
        for endpoint in &unused {
            self.connection.endpoints.remove(endpoint);
            self.connection
                .vpn_offered
                .retain(|offered| offered != endpoint);
            self.close_dialogs(endpoint);
        }
        unused.into_iter().map(Effect::Disconnect).collect()
    }

    /// Pane title status for render, for example `reconnecting in 4 s`;
    /// `None` when connected.
    pub fn connection_status_text(&self, endpoint: &Endpoint) -> Option<String> {
        let text = match self.connection.endpoints.get(endpoint)? {
            EndpointStatus::Connected => return None,
            EndpointStatus::Connecting => "connecting".to_owned(),
            EndpointStatus::Authenticating => "authenticating".to_owned(),
            EndpointStatus::WaitingForPassword => "waiting for sudo password".to_owned(),
            EndpointStatus::Reconnecting { remaining, .. } if remaining.is_zero() => {
                "reconnecting".to_owned()
            }
            EndpointStatus::Reconnecting { remaining, .. } => {
                format!("reconnecting in {} s", whole_seconds_up(*remaining))
            }
            EndpointStatus::Failed(_) => "failed".to_owned(),
        };
        Some(text)
    }

    // Actions.

    /// `S`: shows the focused pane's host with sudo, in the same directory.
    fn reconnect_with_sudo(&mut self, side: PaneSide) -> Vec<Effect> {
        let pane = self.core.pane(side);
        let host = match &pane.endpoint {
            Endpoint::Local => {
                self.core.set_status(StatusMessage::info(
                    "sudo mode needs a remote endpoint; press o to pick a host",
                ));
                return Vec::new();
            }
            Endpoint::Remote { sudo: true, .. } => {
                let text = format!("{} is already in sudo mode", pane.endpoint);
                self.core.set_status(StatusMessage::info(text));
                return Vec::new();
            }
            Endpoint::Remote { host, sudo: false } => host.clone(),
        };
        let cwd = pane.cwd.clone();
        self.switch_and_release(side, Endpoint::Remote { host, sudo: true }, cwd)
    }

    /// Picking the endpoint a pane already shows: retries a failed endpoint
    /// and cuts a pending backoff short; otherwise nothing to do.
    fn retry(&mut self, endpoint: Endpoint) -> Vec<Effect> {
        match self.connection.endpoints.get(&endpoint) {
            Some(EndpointStatus::Failed(_)) => {
                self.connection_connecting(&endpoint);
                vec![Effect::Connect(endpoint)]
            }
            Some(EndpointStatus::Reconnecting { .. }) => vec![Effect::Reconnect(endpoint)],
            Some(
                EndpointStatus::Connecting
                | EndpointStatus::Connected
                | EndpointStatus::Authenticating
                | EndpointStatus::WaitingForPassword,
            )
            | None => Vec::new(),
        }
    }

    // Helpers.

    /// Shows `endpoint` in a pane, records the connects that causes, and
    /// releases what the pane showed before when nothing else uses it.
    fn switch_and_release(
        &mut self,
        side: PaneSide,
        endpoint: Endpoint,
        dir: Option<PathBuf>,
    ) -> Vec<Effect> {
        let mut effects = self.switch_endpoint(side, endpoint, dir);
        for effect in &effects {
            if let Effect::Connect(connecting) = effect {
                self.connection_connecting(connecting);
            }
        }
        effects.extend(self.connection_release_unused());
        effects
    }

    /// Ends sudo mode for `endpoint` after a cancel or a sudo failure: it
    /// is `failed`, and every pane showing it switches to the same host
    /// without sudo (at its directory, else the remembered one).
    fn leave_sudo(&mut self, endpoint: Endpoint, error: Error) -> Vec<Effect> {
        self.connection
            .endpoints
            .insert(endpoint.clone(), EndpointStatus::Failed(error));
        let Endpoint::Remote { host, sudo: true } = &endpoint else {
            return Vec::new();
        };
        let plain = Endpoint::Remote {
            host: host.clone(),
            sudo: false,
        };
        let mut effects = Vec::new();
        for side in [PaneSide::Left, PaneSide::Right] {
            let pane = self.core.pane(side);
            if pane.endpoint == endpoint {
                let cwd = pane.cwd.clone();
                effects.extend(self.switch_and_release(side, plain.clone(), cwd));
            }
        }
        effects
    }

    /// Pushes the masked sudo password dialog for `endpoint` unless one is
    /// already open. `retry` says that the last password was rejected.
    fn ask_sudo_password(&mut self, endpoint: Endpoint, retry: bool) {
        self.connection
            .endpoints
            .insert(endpoint.clone(), EndpointStatus::WaitingForPassword);
        let open = self.dialog.stack.iter().any(|dialog| {
            matches!(&dialog.purpose, DialogPurpose::SudoPassword { endpoint: open } if *open == endpoint)
        });
        if open {
            return;
        }
        let host = host_of(&endpoint).unwrap_or_default();
        let prompt = format!("Password for sudo on {host}. It is used once and never stored.");
        let message = if retry {
            format!("{}.\n{prompt}", SudoError::WrongPassword)
        } else {
            prompt
        };
        self.dialog.stack.push(Dialog {
            purpose: DialogPurpose::SudoPassword { endpoint },
            title: "sudo password".to_owned(),
            message,
            field: Some(TextField {
                // Allocated once at full size so typing never reallocates
                // (and never leaves a copy of a prefix behind).
                value: Zeroizing::new(String::with_capacity(PASSWORD_MAX_LEN)),
                caret: 0,
                masked: true,
                max_len: Some(PASSWORD_MAX_LEN),
            }),
            checkbox: None,
            buttons: vec![DialogButton::Ok, DialogButton::Cancel],
            focus: DialogFocus::Field,
        });
    }

    /// Opens the VPN dialog for an unreachable endpoint whose host matches a
    /// `vpn` rule, once until it connects.
    fn offer_vpn(&mut self, endpoint: &Endpoint, error: &ConnectError) {
        if !error.vpn_hint_eligible() || self.connection.vpn_offered.contains(endpoint) {
            return;
        }
        let Some(host) = host_of(endpoint) else {
            return;
        };
        let hostname = self
            .connection
            .resolved
            .get(host)
            .map(|resolved| resolved.hostname.as_str());
        let Some(rule) = self.settings.vpn_rule(host, hostname) else {
            return;
        };
        let command = rule.command.clone();
        let message = format!("VPN may be required for {host} ({error}).\nCommand: {command}");
        self.connection.vpn_offered.push(endpoint.clone());
        self.dialog.stack.push(Dialog {
            purpose: DialogPurpose::Vpn {
                endpoint: endpoint.clone(),
                command,
            },
            title: "VPN may be required".to_owned(),
            message,
            field: None,
            checkbox: None,
            buttons: vec![DialogButton::Run, DialogButton::Cancel],
            focus: DialogFocus::Button(0),
        });
    }

    /// The endpoint is usable: status, VPN offer reset, stale dialogs gone.
    fn mark_connected(&mut self, endpoint: &Endpoint) {
        self.connection
            .endpoints
            .insert(endpoint.clone(), EndpointStatus::Connected);
        self.connection
            .vpn_offered
            .retain(|offered| offered != endpoint);
        self.close_dialogs(endpoint);
    }

    /// Closes this feature's dialogs for `endpoint`; a dropped password
    /// field is zeroized.
    fn close_dialogs(&mut self, endpoint: &Endpoint) {
        self.dialog.stack.retain(|dialog| match &dialog.purpose {
            DialogPurpose::SudoPassword { endpoint: open }
            | DialogPurpose::Vpn { endpoint: open, .. } => open != endpoint,
            _ => true,
        });
    }

    /// Whether a pane, an unfinished job, a running sync, or an edit uses
    /// `endpoint`.
    fn is_in_use(&self, endpoint: &Endpoint) -> bool {
        [PaneSide::Left, PaneSide::Right]
            .into_iter()
            .any(|side| self.core.pane(side).endpoint == *endpoint)
            || self.transfer.jobs.iter().any(|job| {
                job.outcome.is_none() && (job.source == *endpoint || job.destination == *endpoint)
            })
            || self
                .sync
                .running
                .iter()
                .any(|sync| sync.left == *endpoint || sync.right == *endpoint)
            || self
                .edit
                .active
                .iter()
                .any(|location| location.endpoint == *endpoint)
    }

    /// An event for an endpoint released while the event was in flight.
    fn is_stale(&self, endpoint: &Endpoint) -> bool {
        !self.connection.endpoints.contains_key(endpoint) && !self.is_in_use(endpoint)
    }

    /// The picker detail of an alias: `user@hostname`, `hostname`, or empty
    /// until resolved.
    fn host_detail(&self, alias: &str) -> String {
        match self.connection.resolved.get(alias) {
            Some(ResolvedHost {
                hostname,
                user: Some(user),
            }) => format!("{user}@{hostname}"),
            Some(ResolvedHost {
                hostname,
                user: None,
            }) => hostname.clone(),
            None => String::new(),
        }
    }

    /// The picker tab of an alias: its sshto group, else `Other`.
    fn host_tab(&self, alias: &str) -> usize {
        let hostname = self
            .connection
            .resolved
            .get(alias)
            .map(|resolved| resolved.hostname.as_str());
        match self.groups.classify(alias, hostname) {
            Some(group) if !self.groups.names.is_empty() => group + 1,
            _ => self.other_tab(),
        }
    }

    /// Index of the `Other` tab; 0 (ignored) when the picker has no tabs.
    fn other_tab(&self) -> usize {
        if self.groups.names.is_empty() {
            0
        } else {
            self.groups.names.len() + 1
        }
    }
}

/// The ssh alias of a remote endpoint.
fn host_of(endpoint: &Endpoint) -> Option<&str> {
    match endpoint {
        Endpoint::Local => None,
        Endpoint::Remote { host, .. } => Some(host),
    }
}

/// A sort key that orders endpoints deterministically.
fn endpoint_key(endpoint: &Endpoint) -> (Option<&str>, bool) {
    match endpoint {
        Endpoint::Local => (None, false),
        Endpoint::Remote { host, sudo } => (Some(host), *sudo),
    }
}

/// Seconds rounded up, so a countdown never shows 0 before it is over.
fn whole_seconds_up(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::error::UnreachableReason;
    use crate::groups::{self, Groups};
    use crate::hosts::HostList;
    use crate::preview::Background;
    use crate::settings::{Settings, VpnRule};
    use crate::state::PersistedState;
    use crate::transfer::model::{JobId, JobKind, JobSummary};
    use crate::ui::action::BgEvent;
    use crate::ui::app::sync::RunningSync;
    use crate::ui::app::transfer::{JobOutcome, QueueJob};
    use crate::vfs::Location;

    /// A distinctive password that must never leak.
    const PASSWORD: &str = "pw-\u{e9}-LEAKCHECK-7f3a";

    fn remote(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.into(),
            sudo: false,
        }
    }

    fn sudo(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.into(),
            sudo: true,
        }
    }

    fn unreachable() -> ConnectError {
        ConnectError::Unreachable {
            reason: UnreachableReason::Timeout,
            detail: "ssh: connect to host 10.0.0.1 port 22: Operation timed out".into(),
        }
    }

    fn vpn_settings(pattern: &str) -> Settings {
        Settings {
            vpn_rules: vec![VpnRule {
                pattern: pattern.into(),
                command: "ssh -t jump ovpn".into(),
                line: 1,
            }],
            ..Settings::default()
        }
    }

    fn hosts(aliases: &[&str]) -> HostList {
        HostList {
            aliases: aliases.iter().map(|alias| alias.to_string()).collect(),
            warnings: Vec::new(),
        }
    }

    fn build(
        settings: Settings,
        hosts: HostList,
        groups: Groups,
        right: Option<(Endpoint, Option<PathBuf>)>,
    ) -> App {
        let (app, _) = App::new(
            settings,
            hosts,
            groups,
            PersistedState::default(),
            Background::Dark,
            (Endpoint::Local, Some("/l".into())),
            right,
        );
        app
    }

    /// Left pane local at `/l`, right pane on `web` at `/srv`, with the
    /// connect core emits recorded as core will (wiring).
    fn with_web(settings: Settings) -> App {
        let mut app = build(
            settings,
            hosts(&["db", "web"]),
            Groups::default(),
            Some((remote("web"), Some("/srv".into()))),
        );
        app.connection_connecting(&remote("web"));
        app
    }

    /// Left pane on `web [sudo]` after `S` (from `web`, listed at `/srv`),
    /// right pane local at `/r`.
    fn on_sudo_web() -> App {
        let mut app = build(
            Settings::default(),
            hosts(&["web"]),
            Groups::default(),
            Some((Endpoint::Local, Some("/r".into()))),
        );
        let picked = app.connection_picked(PaneSide::Left, PickerTarget::Host("web".into()));
        assert_eq!(picked, [Effect::Connect(remote("web"))]);
        // Core's part of `Connected` (wiring), then this feature's.
        let listing = app.handle_event(BgEvent::Connected {
            endpoint: remote("web"),
            home: "/srv".into(),
        });
        app.connection_connected(&remote("web"));
        let [Effect::ListDir { request, location }] = listing.as_slice() else {
            panic!("expected one ListDir, got {listing:?}");
        };
        app.handle_event(BgEvent::Listed {
            request: *request,
            location: location.clone(),
            result: Ok(Vec::new()),
        });
        assert_eq!(app.core.pane(PaneSide::Left).cwd, Some("/srv".into()));
        let effects = app
            .connection_action(Action::ReconnectWithSudo)
            .expect("claimed");
        assert_eq!(
            effects,
            [
                Effect::Connect(sudo("web")),
                Effect::Disconnect(remote("web"))
            ]
        );
        app
    }

    /// After leaving sudo: the pane shows `web` again and the sudo endpoint
    /// is released. Whether showing `web` lists at once or connects first is
    /// core's decision (it forgets disconnected endpoints, wiring point 3),
    /// so only this feature's part is checked.
    fn assert_switched_back(app: &App, effects: &[Effect]) {
        assert_eq!(app.core.pane(PaneSide::Left).endpoint, remote("web"));
        assert_eq!(effects.last(), Some(&Effect::Disconnect(sudo("web"))));
        assert!(!effects.contains(&Effect::Disconnect(remote("web"))));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::SudoPassword { .. })));
        assert!(!app.connection.endpoints.contains_key(&sudo("web")));
        assert!(sudo_dialogs(app).is_empty());
    }

    fn status_text(app: &App) -> String {
        app.core
            .status()
            .map(|status| status.text.clone())
            .unwrap_or_default()
    }

    fn sudo_dialogs(app: &App) -> Vec<&Dialog> {
        app.dialog
            .stack
            .iter()
            .filter(|dialog| matches!(dialog.purpose, DialogPurpose::SudoPassword { .. }))
            .collect()
    }

    fn answer(purpose: DialogPurpose, button: DialogButton, field: Option<&str>) -> DialogAnswer {
        DialogAnswer {
            purpose,
            button,
            field: field.map(|text| Zeroizing::new(text.to_owned())),
            checked: false,
        }
    }

    fn job(source: Endpoint, destination: Endpoint, outcome: Option<JobOutcome>) -> QueueJob {
        QueueJob {
            id: JobId(1),
            kind: JobKind::Copy,
            mode: None,
            source,
            destination,
            label: "copy".into(),
            sync: false,
            items: Vec::new(),
            outcome,
        }
    }

    #[test]
    fn action_claims_picker_and_sudo_and_hands_back_the_rest() {
        let mut app = with_web(Settings::default());
        assert_eq!(app.connection_action(Action::Copy), Err(Action::Copy));
        assert_eq!(app.connection_action(Action::Quit), Err(Action::Quit));
        assert_eq!(
            app.connection_action(Action::OpenEndpointPicker),
            Ok(Vec::new())
        );
        let picker = app.dialog.picker.as_ref().expect("picker");
        // The left pane has focus.
        assert_eq!(
            picker.kind,
            PickerKind::Hosts {
                side: PaneSide::Left
            }
        );
        assert!(app.connection_action(Action::ReconnectWithSudo).is_ok());
    }

    #[test]
    fn picker_lists_local_then_unexcluded_aliases_without_tabs() {
        let groups = groups::parse(Path::new("g"), "exclude|bastion*\n").expect("groups");
        let mut app = build(
            Settings::default(),
            hosts(&["bastion-1", "db", "web"]),
            groups,
            None,
        );
        assert!(app.connection_open_picker(PaneSide::Right).is_empty());
        let picker = app.dialog.picker.as_ref().expect("picker");
        assert_eq!(
            picker.kind,
            PickerKind::Hosts {
                side: PaneSide::Right
            }
        );
        assert_eq!(picker.title, "Open in the right pane");
        assert!(picker.tabs.is_empty());
        let labels: Vec<&str> = picker
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(labels, ["Local", "db", "web"]);
        assert_eq!(picker.items[0].target, PickerTarget::Local);
        assert_eq!(picker.items[1].target, PickerTarget::Host("db".into()));
        assert!(picker.items.iter().all(|item| item.detail.is_empty()));
        assert_eq!((picker.cursor, picker.first_visible), (0, 0));
        assert!(picker.filter.is_empty());
    }

    #[test]
    fn picker_tabs_follow_groups_and_resolution() {
        let groups = groups::parse(
            Path::new("g"),
            "group|prod\ngroup|lab\nmatch|prod|alias|web*\nmatch|lab|hostname|*.lab.example\n",
        )
        .expect("groups");
        let mut app = build(
            Settings::default(),
            hosts(&["box", "gpu", "web1"]),
            groups,
            None,
        );
        app.connection_host_resolved(
            "box".into(),
            Ok(ResolvedHost {
                hostname: "box.example".into(),
                user: None,
            }),
        );
        app.connection_open_picker(PaneSide::Right);
        let picker = app.dialog.picker.as_ref().expect("picker");
        assert_eq!(picker.tabs, ["All", "prod", "lab", "Other"]);
        let rows: Vec<(&str, &str, usize)> = picker
            .items
            .iter()
            .map(|item| (item.label.as_str(), item.detail.as_str(), item.tab))
            .collect();
        assert_eq!(
            rows,
            [
                ("Local", "", 3),
                ("box", "box.example", 3),
                ("gpu", "", 3),
                ("web1", "", 1)
            ]
        );
        // Resolution moves an open picker's row to its hostname group and
        // fills in the detail; other rows stay.
        app.connection_host_resolved(
            "gpu".into(),
            Ok(ResolvedHost {
                hostname: "GPU.lab.example".into(),
                user: Some("ann".into()),
            }),
        );
        let picker = app.dialog.picker.as_ref().expect("picker");
        assert_eq!(picker.items[2].detail, "ann@GPU.lab.example");
        assert_eq!(picker.items[2].tab, 2);
        assert_eq!(picker.items[1].detail, "box.example");
        // A failed resolution changes nothing.
        let before = picker.clone();
        app.connection_host_resolved("web1".into(), Err(Error::Canceled));
        assert_eq!(app.dialog.picker.as_ref(), Some(&before));
        assert!(!app.connection.resolved.contains_key("web1"));
    }

    #[test]
    fn host_resolved_leaves_a_bookmark_picker_alone() {
        let mut app = with_web(Settings::default());
        let bookmarks = PickerList {
            kind: PickerKind::Bookmarks {
                side: PaneSide::Left,
            },
            title: "Bookmarks".into(),
            tabs: Vec::new(),
            active_tab: 0,
            filter: String::new(),
            items: vec![PickerItem {
                label: "web".into(),
                detail: String::new(),
                tab: 0,
                target: PickerTarget::Host("web".into()),
            }],
            cursor: 0,
            first_visible: 0,
        };
        app.dialog.picker = Some(bookmarks.clone());
        app.connection_host_resolved(
            "web".into(),
            Ok(ResolvedHost {
                hostname: "w".into(),
                user: None,
            }),
        );
        assert_eq!(app.dialog.picker, Some(bookmarks));
    }

    #[test]
    fn picking_a_host_connects_it_and_releases_the_old_one() {
        let mut app = with_web(Settings::default());
        app.connection_connected(&remote("web"));
        let effects = app.connection_picked(PaneSide::Right, PickerTarget::Host("db".into()));
        assert_eq!(
            effects,
            [
                Effect::Connect(remote("db")),
                Effect::Disconnect(remote("web"))
            ]
        );
        assert_eq!(app.core.pane(PaneSide::Right).endpoint, remote("db"));
        assert_eq!(
            app.connection.endpoints.get(&remote("db")),
            Some(&EndpointStatus::Connecting)
        );
        assert!(!app.connection.endpoints.contains_key(&remote("web")));
        assert_eq!(
            app.connection_status_text(&remote("db")).as_deref(),
            Some("connecting")
        );
    }

    #[test]
    fn picking_local_switches_the_pane_and_bookmarks_are_ignored() {
        let mut app = with_web(Settings::default());
        let effects = app.connection_picked(PaneSide::Right, PickerTarget::Local);
        // No directory is remembered for Local, so core connects it to
        // learn its home; the connect shows as `connecting`.
        assert_eq!(
            effects,
            [
                Effect::Connect(Endpoint::Local),
                Effect::Disconnect(remote("web"))
            ]
        );
        assert_eq!(
            app.connection_status_text(&Endpoint::Local).as_deref(),
            Some("connecting")
        );
        let bookmark = PickerTarget::Bookmark(Location {
            endpoint: Endpoint::Local,
            path: "/x".into(),
        });
        assert!(app.connection_picked(PaneSide::Right, bookmark).is_empty());
    }

    #[test]
    fn picking_the_shown_endpoint_retries_it() {
        let mut app = with_web(Settings::default());
        let pick = |app: &mut App| {
            app.connection_picked(PaneSide::Right, PickerTarget::Host("web".into()))
        };
        // Still connecting: nothing to do.
        assert!(pick(&mut app).is_empty());
        app.connection_failed(
            &remote("web"),
            &Error::Connect(ConnectError::Other {
                detail: "boom".into(),
            }),
        );
        assert_eq!(
            app.connection_status_text(&remote("web")).as_deref(),
            Some("failed")
        );
        assert_eq!(pick(&mut app), [Effect::Connect(remote("web"))]);
        assert_eq!(
            app.connection.endpoints.get(&remote("web")),
            Some(&EndpointStatus::Connecting)
        );
        app.connection_reconnecting(remote("web"), 2, Duration::from_secs(4), unreachable());
        assert_eq!(pick(&mut app), [Effect::Reconnect(remote("web"))]);
    }

    #[test]
    fn sudo_key_on_local_or_sudo_pane_is_a_status_message() {
        let mut app = with_web(Settings::default());
        // The focused left pane is local.
        assert_eq!(
            app.connection_action(Action::ReconnectWithSudo),
            Ok(Vec::new())
        );
        assert!(status_text(&app).contains("sudo mode needs a remote endpoint"));
        let mut app = build(
            Settings::default(),
            hosts(&["web"]),
            Groups::default(),
            None,
        );
        app.connection_picked(PaneSide::Left, PickerTarget::Host("web".into()));
        app.connection_action(Action::ReconnectWithSudo)
            .expect("claimed");
        assert_eq!(app.core.pane(PaneSide::Left).endpoint, sudo("web"));
        assert_eq!(
            app.connection_action(Action::ReconnectWithSudo),
            Ok(Vec::new())
        );
        assert_eq!(status_text(&app), "web [sudo] is already in sudo mode");
    }

    #[test]
    fn sudo_key_switches_to_the_sudo_endpoint_and_releases_the_plain_one() {
        let mut app = build(
            Settings::default(),
            hosts(&["web"]),
            Groups::default(),
            Some((Endpoint::Local, Some("/r".into()))),
        );
        app.connection_picked(PaneSide::Left, PickerTarget::Host("web".into()));
        let effects = app
            .connection_action(Action::ReconnectWithSudo)
            .expect("claimed");
        assert_eq!(
            effects,
            [
                Effect::Connect(sudo("web")),
                Effect::Disconnect(remote("web"))
            ]
        );
        assert_eq!(
            app.connection_status_text(&sudo("web")).as_deref(),
            Some("connecting")
        );
    }

    #[test]
    fn status_text_covers_every_state() {
        let mut app = with_web(Settings::default());
        let web = remote("web");
        let text = |app: &App| app.connection_status_text(&remote("web"));
        assert_eq!(text(&app).as_deref(), Some("connecting"));
        app.connection_interactive_auth(
            web.clone(),
            ConnectError::AuthFailed {
                detail: "Permission denied (publickey).".into(),
            },
        );
        assert_eq!(text(&app).as_deref(), Some("authenticating"));
        app.connection
            .endpoints
            .insert(web.clone(), EndpointStatus::WaitingForPassword);
        assert_eq!(text(&app).as_deref(), Some("waiting for sudo password"));
        app.connection_reconnecting(web.clone(), 1, Duration::from_millis(1500), unreachable());
        assert_eq!(text(&app).as_deref(), Some("reconnecting in 2 s"));
        app.connection_connected(&web);
        assert_eq!(text(&app), None);
        app.connection_disconnected(&web, &ConnectError::ConnectionLost);
        assert_eq!(text(&app).as_deref(), Some("reconnecting"));
        app.connection_failed(&web, &Error::Canceled);
        assert_eq!(text(&app).as_deref(), Some("failed"));
        assert_eq!(app.connection_status_text(&remote("unknown")), None);
    }

    #[test]
    fn countdown_ticks_once_a_second_and_stops() {
        let mut app = with_web(Settings::default());
        let effects =
            app.connection_reconnecting(remote("web"), 3, Duration::from_secs(2), unreachable());
        let id = app.connection.countdown.expect("countdown");
        assert_eq!(
            effects,
            [Effect::ScheduleTimer {
                id,
                after: Duration::from_secs(1)
            }]
        );
        assert!(status_text(&app).contains("reconnecting (attempt 3)"));
        assert_eq!(
            app.connection_status_text(&remote("web")).as_deref(),
            Some("reconnecting in 2 s")
        );
        let now = Instant::now();
        assert_eq!(app.connection_timer(TimerId(id.0 + 100), now), None);
        assert_eq!(
            app.connection_timer(id, now),
            Some(vec![Effect::ScheduleTimer {
                id,
                after: Duration::from_secs(1)
            }])
        );
        assert_eq!(
            app.connection_status_text(&remote("web")).as_deref(),
            Some("reconnecting in 1 s")
        );
        assert_eq!(app.connection_timer(id, now), Some(Vec::new()));
        assert_eq!(app.connection.countdown, None);
        assert_eq!(
            app.connection_status_text(&remote("web")).as_deref(),
            Some("reconnecting")
        );
        // The old id is no longer ours; a new retry starts a new timer that
        // reuses itself while running.
        assert_eq!(app.connection_timer(id, now), None);
        app.connection_reconnecting(remote("web"), 4, Duration::from_secs(8), unreachable());
        let next = app.connection.countdown.expect("countdown");
        assert_ne!(next, id);
        let again =
            app.connection_reconnecting(remote("web"), 5, Duration::from_secs(16), unreachable());
        assert_eq!(
            again,
            [Effect::ScheduleTimer {
                id: next,
                after: Duration::from_secs(1)
            }]
        );
    }

    #[test]
    fn interactive_auth_hands_over_at_once() {
        let mut app = with_web(Settings::default());
        let effects = app.connection_interactive_auth(
            remote("web"),
            ConnectError::HostKeyRejected {
                detail: "Host key verification failed.".into(),
            },
        );
        assert_eq!(effects, [Effect::InteractiveAuth(remote("web"))]);
        assert!(status_text(&app).starts_with("web: Host key verification failed"));
    }

    #[test]
    fn sudo_prompt_is_a_masked_preallocated_field_opened_once() {
        let mut app = on_sudo_web();
        assert!(app
            .connection_sudo_password_required(sudo("web"))
            .is_empty());
        assert!(app
            .connection_sudo_password_required(sudo("web"))
            .is_empty());
        let dialogs = sudo_dialogs(&app);
        assert_eq!(dialogs.len(), 1);
        let dialog = dialogs[0];
        assert_eq!(
            dialog.purpose,
            DialogPurpose::SudoPassword {
                endpoint: sudo("web")
            }
        );
        assert_eq!(dialog.buttons, [DialogButton::Ok, DialogButton::Cancel]);
        assert_eq!(dialog.focus, DialogFocus::Field);
        assert!(dialog.message.contains("sudo on web"));
        let field = dialog.field.as_ref().expect("field");
        assert!(field.masked);
        assert_eq!(field.max_len, Some(PASSWORD_MAX_LEN));
        assert!(field.value.is_empty());
        assert!(field.value.capacity() >= PASSWORD_MAX_LEN);
        assert_eq!(
            app.connection_status_text(&sudo("web")).as_deref(),
            Some("waiting for sudo password")
        );
    }

    #[test]
    fn sudo_ok_moves_the_password_into_the_effect_and_never_leaks_it() {
        let mut app = on_sudo_web();
        app.connection_sudo_password_required(sudo("web"));
        // The dialog feature pops the dialog and hands over its field.
        app.dialog.stack.clear();
        let effects = app.connection_dialog_answer(answer(
            DialogPurpose::SudoPassword {
                endpoint: sudo("web"),
            },
            DialogButton::Ok,
            Some(PASSWORD),
        ));
        assert_eq!(effects.len(), 1);
        let Effect::SudoPassword { endpoint, password } = &effects[0] else {
            panic!("expected SudoPassword, got {effects:?}");
        };
        assert_eq!(*endpoint, sudo("web"));
        assert_eq!(password.expose(), PASSWORD);
        assert_eq!(
            app.connection_status_text(&sudo("web")).as_deref(),
            Some("connecting")
        );
        // Wrong password: a fresh prompt says so, the status bar shows the
        // error text, and nothing anywhere holds the password.
        let wrong = Error::Sudo(SudoError::WrongPassword);
        app.core
            .set_status(StatusMessage::error(format!("{}: {wrong}", sudo("web"))));
        assert!(app.connection_failed(&sudo("web"), &wrong).is_empty());
        let dialogs = sudo_dialogs(&app);
        assert_eq!(dialogs.len(), 1);
        assert!(dialogs[0].message.starts_with("Incorrect sudo password."));
        assert!(dialogs[0].field.as_ref().expect("field").value.is_empty());
        assert_eq!(status_text(&app), "web [sudo]: Incorrect sudo password");
        let mut seen = vec![
            format!("{effects:?}"),
            format!("{:?}", app.connection),
            status_text(&app),
            app.connection_status_text(&sudo("web")).unwrap_or_default(),
        ];
        seen.extend(
            app.dialog
                .stack
                .iter()
                .map(|dialog| format!("{} {}", dialog.title, dialog.message)),
        );
        for text in &seen {
            assert!(!text.contains(PASSWORD), "password leaked into {text}");
            assert!(!text.contains("LEAKCHECK"), "password leaked into {text}");
        }
    }

    #[test]
    fn sudo_cancel_fails_the_endpoint_and_switches_back() {
        let mut app = on_sudo_web();
        app.connection_sudo_password_required(sudo("web"));
        // The dialog feature pops the dialog and hands over its field.
        app.dialog.stack.clear();
        let effects = app.connection_dialog_answer(answer(
            DialogPurpose::SudoPassword {
                endpoint: sudo("web"),
            },
            DialogButton::Cancel,
            Some(PASSWORD),
        ));
        assert_switched_back(&app, &effects);
        assert_eq!(status_text(&app), "sudo canceled for web");
        assert!(!format!("{effects:?}").contains(PASSWORD));
    }

    #[test]
    fn other_sudo_failures_switch_back_without_asking() {
        let mut app = on_sudo_web();
        let effects = app.connection_failed(&sudo("web"), &Error::Sudo(SudoError::NotInSudoers));
        assert_switched_back(&app, &effects);
    }

    #[test]
    fn vpn_dialog_is_offered_once_for_a_matching_unreachable_host() {
        let mut app = with_web(vpn_settings("WEB*"));
        app.connection_reconnecting(remote("web"), 1, Duration::from_secs(1), unreachable());
        assert_eq!(app.dialog.stack.len(), 1);
        let dialog = &app.dialog.stack[0];
        assert_eq!(
            dialog.purpose,
            DialogPurpose::Vpn {
                endpoint: remote("web"),
                command: "ssh -t jump ovpn".into()
            }
        );
        assert_eq!(dialog.buttons, [DialogButton::Run, DialogButton::Cancel]);
        assert_eq!(dialog.focus, DialogFocus::Button(0));
        assert!(dialog.message.contains("Command: ssh -t jump ovpn"));
        // Not again while it keeps failing.
        app.connection_reconnecting(remote("web"), 2, Duration::from_secs(2), unreachable());
        assert_eq!(app.dialog.stack.len(), 1);
        // Connecting closes the stale dialog and allows a new offer later.
        app.connection_connected(&remote("web"));
        assert!(app.dialog.stack.is_empty());
        assert!(app.connection.vpn_offered.is_empty());
        app.connection_reconnecting(remote("web"), 1, Duration::from_secs(1), unreachable());
        assert_eq!(app.dialog.stack.len(), 1);
    }

    #[test]
    fn vpn_dialog_needs_an_eligible_error_and_a_rule() {
        let mut app = with_web(vpn_settings("db"));
        app.connection_reconnecting(remote("web"), 1, Duration::from_secs(1), unreachable());
        assert!(app.dialog.stack.is_empty());
        let mut app = with_web(vpn_settings("web"));
        app.connection_reconnecting(
            remote("web"),
            1,
            Duration::from_secs(1),
            ConnectError::ConnectionLost,
        );
        assert!(app.dialog.stack.is_empty());
        // A hostname rule matches once the alias is resolved.
        let mut app = with_web(vpn_settings("*.corp.example"));
        app.connection_reconnecting(remote("web"), 1, Duration::from_secs(1), unreachable());
        assert!(app.dialog.stack.is_empty());
        app.connection_host_resolved(
            "web".into(),
            Ok(ResolvedHost {
                hostname: "Web.Corp.Example".into(),
                user: None,
            }),
        );
        app.connection_reconnecting(remote("web"), 2, Duration::from_secs(2), unreachable());
        assert_eq!(app.dialog.stack.len(), 1);
    }

    #[test]
    fn vpn_run_reconnects_after_the_command() {
        let mut app = with_web(vpn_settings("web"));
        app.connection_reconnecting(remote("web"), 1, Duration::from_secs(1), unreachable());
        let purpose = app.dialog.stack.pop().expect("dialog").purpose;
        assert!(app
            .connection_dialog_answer(answer(purpose.clone(), DialogButton::Cancel, None))
            .is_empty());
        assert_eq!(
            app.connection_dialog_answer(answer(purpose, DialogButton::Run, None)),
            [Effect::RunVpn {
                endpoint: remote("web"),
                command: "ssh -t jump ovpn".into()
            }]
        );
        let failed = Error::CommandFailed {
            program: "VPN command".into(),
            status: crate::error::ExitStatus::Code(1),
            stderr: String::new(),
        };
        assert!(app
            .connection_vpn_finished(remote("web"), Err(failed.clone()))
            .is_empty());
        assert_eq!(status_text(&app), failed.to_string());
        assert!(app.core.status().expect("status").error);
        assert_eq!(
            app.connection_vpn_finished(remote("web"), Ok(())),
            [Effect::Reconnect(remote("web"))]
        );
        // Nothing to retry once connected or released.
        app.connection_connected(&remote("web"));
        assert!(app
            .connection_vpn_finished(remote("web"), Ok(()))
            .is_empty());
        assert!(app
            .connection_vpn_finished(remote("gone"), Ok(()))
            .is_empty());
    }

    #[test]
    fn reconnected_resumes_transfers() {
        let mut app = with_web(Settings::default());
        app.connection_connected(&remote("web"));
        app.connection_disconnected(&remote("web"), &ConnectError::ConnectionLost);
        app.connection_reconnecting(
            remote("web"),
            1,
            Duration::from_secs(1),
            ConnectError::ConnectionLost,
        );
        let effects = app.connection_reconnected(&remote("web"));
        assert_eq!(
            effects,
            [Effect::Transfer(TransferCommand::EndpointReconnected(
                remote("web")
            ))]
        );
        assert_eq!(app.connection_status_text(&remote("web")), None);
        assert_eq!(status_text(&app), "Reconnected to web");
    }

    #[test]
    fn release_keeps_endpoints_in_use_and_never_local() {
        let mut app = with_web(Settings::default());
        app.connection_connecting(&Endpoint::Local);
        for host in ["job", "done", "sync", "edit", "idle"] {
            app.connection_connecting(&remote(host));
        }
        app.transfer
            .jobs
            .push(job(Endpoint::Local, remote("job"), None));
        app.transfer.jobs.push(job(
            remote("done"),
            Endpoint::Local,
            Some(JobOutcome::Finished(JobSummary {
                done: 1,
                skipped: 0,
                failed: 0,
            })),
        ));
        app.sync.running.push(RunningSync {
            job: JobId(2),
            left: Endpoint::Local,
            right: remote("sync"),
        });
        app.edit.active.push(Location {
            endpoint: remote("edit"),
            path: "/etc/hosts".into(),
        });
        assert_eq!(
            app.connection_release_unused(),
            [
                Effect::Disconnect(remote("done")),
                Effect::Disconnect(remote("idle"))
            ]
        );
        assert!(app.connection_release_unused().is_empty());
        let mut kept: Vec<String> = app
            .connection
            .endpoints
            .keys()
            .map(ToString::to_string)
            .collect();
        kept.sort();
        assert_eq!(kept, ["Local", "edit", "job", "sync", "web"]);
        // Once the job finishes its endpoint goes too.
        app.transfer.jobs[0].outcome = Some(JobOutcome::Failed(Error::Canceled));
        assert_eq!(
            app.connection_release_unused(),
            [Effect::Disconnect(remote("job"))]
        );
    }

    #[test]
    fn release_closes_dialogs_of_released_endpoints() {
        let mut app = with_web(vpn_settings("*"));
        app.connection_connecting(&remote("batch"));
        app.transfer
            .jobs
            .push(job(Endpoint::Local, remote("batch"), None));
        app.connection_reconnecting(remote("batch"), 1, Duration::from_secs(1), unreachable());
        assert_eq!(app.dialog.stack.len(), 1);
        app.transfer.jobs.clear();
        assert_eq!(
            app.connection_release_unused(),
            [Effect::Disconnect(remote("batch"))]
        );
        assert!(app.dialog.stack.is_empty());
        assert!(app.connection.vpn_offered.is_empty());
    }

    #[test]
    fn events_for_released_endpoints_are_ignored() {
        let mut app = with_web(vpn_settings("*"));
        let gone = remote("gone");
        assert!(app.connection_connected(&gone).is_empty());
        assert!(app
            .connection_interactive_auth(
                gone.clone(),
                ConnectError::AuthFailed { detail: "x".into() }
            )
            .is_empty());
        assert!(app
            .connection_sudo_password_required(sudo("gone"))
            .is_empty());
        assert!(app
            .connection_reconnecting(gone.clone(), 1, Duration::from_secs(1), unreachable())
            .is_empty());
        assert!(app
            .connection_disconnected(&gone, &ConnectError::ConnectionLost)
            .is_empty());
        assert!(app.connection_reconnected(&gone).is_empty());
        assert!(app.connection_failed(&gone, &Error::Canceled).is_empty());
        assert!(app.dialog.stack.is_empty());
        assert!(!app.connection.endpoints.contains_key(&gone));
        assert_eq!(app.connection.countdown, None);
    }

    #[test]
    fn seconds_round_up() {
        assert_eq!(whole_seconds_up(Duration::from_secs(4)), 4);
        assert_eq!(whole_seconds_up(Duration::from_millis(4001)), 5);
        assert_eq!(whole_seconds_up(Duration::from_millis(1)), 1);
    }
}
