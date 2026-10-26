//! Executes connection effects (`Connect`, `InteractiveAuth`,
//! `SudoPassword`, `Reconnect`, `Disconnect`, `ResolveHost`) using
//! `session` and `hosts`; the session itself reports status as `BgEvent`s.
//! Also home of the production `session::Backends` and of
//! `impl SftpChannel for SftpFs`, which wire `vfs::sftp`, `sudo`, and
//! `vfs::local` into `session::Sessions`.
//!
//! # Behavior
//!
//! - `connect`, `sudo_password`, `reconnect`, `disconnect` forward to
//!   `ctx.sessions` (`Sessions` answers `Connect(Local)` with
//!   `Connected { Local, home }` itself).
//! - `resolve_host` spawns `hosts::resolve(&ctx.ssh, &alias)` and sends
//!   `HostResolved`.
//! - `interactive_auth` builds the foreground master command
//!   (`Sessions::foreground_master_command`); on `Err` it sends
//!   `ConnectFailed { endpoint, error }` and returns `None`.
//!   `interactive_auth_finished`: exit 0 → `ctx.sessions.connect(endpoint)`
//!   (the session finds the master with `ssh -O check` and answers as
//!   usual); a failed exit → `ConnectFailed` with
//!   `session::classify_ssh_exit(status.into(), &[])` (OpenSSH already
//!   explained on the terminal); a spawn error → `ConnectFailed` with
//!   `ConnectError::SshUnavailable`.
//! - `ProductionBackends`: `local` → `Arc::new(LocalFs::new())`;
//!   `start_sftp` → `SftpFs::start(endpoint, channel)` as
//!   `Arc<dyn SftpChannel>`; `locate_sftp_server` → `sudo::locate_sftp_server`;
//!   `start_sudo` → `sudo::start`. `impl SftpChannel for SftpFs` forwards
//!   `closed` to `SftpFs::closed`.
//!
//! # Public API (Wave 3, ui/effects/session)
//!
//! ```text
//! pub fn connect(ctx: &Ctx, endpoint: Endpoint);
//! pub fn sudo_password(ctx: &Ctx, endpoint: Endpoint, password: Secret);
//! pub fn reconnect(ctx: &Ctx, endpoint: Endpoint);
//! pub fn disconnect(ctx: &Ctx, endpoint: &Endpoint);
//! pub fn resolve_host(ctx: &Ctx, alias: String);
//! /// Runtime: the foreground command for `Effect::InteractiveAuth`.
//! pub fn interactive_auth(ctx: &Ctx, endpoint: Endpoint) -> Option<Foreground>;
//! /// Runtime: after the foreground master command exited.
//! pub fn interactive_auth_finished(ctx: &Ctx, endpoint: Endpoint, status: std::io::Result<std::process::ExitStatus>);
//!
//! /// Passed to `Sessions::new` by startup.
//! pub struct ProductionBackends;
//! impl Backends for ProductionBackends { /* see Behavior */ }
//! impl SftpChannel for SftpFs { /* closed() → SftpFs::closed() */ }
//! ```
