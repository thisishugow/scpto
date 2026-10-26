//! Executes `Effect::OpenShell` and `Effect::RunVpn` (with the runtime,
//! which suspends the TUI), `Effect::Reveal`, and the Finder-drop
//! existence check `Effect::CheckDroppedPaths`.
//!
//! # Behavior
//!
//! - `shell` (async): local → `$SHELL` (fallback `/bin/sh`) with
//!   `-l` in the directory; remote → `Connections::ssh_command(endpoint,
//!   true)`, then `--`, the host alias, and one remote command word
//!   `cd <dir> && exec "$SHELL" -l` with `<dir>` quoted by
//!   `connections::quote` (so `$SHELL` expands remotely). Returns the
//!   [`Foreground`] with `AfterForeground::Shell`; a failure sends
//!   `ShellExited { result: Err }` and returns `None`. `shell_finished`
//!   sends `ShellExited { result: Ok(()) }` whatever the shell's exit
//!   status, or `Err(Error::CommandFailed { status: NotStarted, .. })` when
//!   it could not be started.
//! - `vpn`: `/bin/sh -c <command>` as `AfterForeground::Vpn(endpoint)`.
//!   `vpn_finished` sends `VpnFinished { result }`: `Ok` on exit 0, else
//!   `Error::CommandFailed { program: "VPN command", status, .. }`.
//! - `reveal`: a local file → `open -R <path>` on macOS, `xdg-open
//!   <parent directory>` on Linux (background child, `process_group(0)`,
//!   stdin null, `kill_on_drop`); a remote file is first downloaded over
//!   the Browse channel to `~/Downloads` (a taken name becomes `name
//!   (1).ext`, the first free number), then revealed. Answered with
//!   `Revealed`.
//! - `check_dropped_paths`: every path must be absolute and exist
//!   (`tokio::fs::symlink_metadata`); answered with `DroppedPathsChecked`.
//!
//! # Public API (Wave 3, ui/effects/external)
//!
//! ```text
//! pub async fn shell(ctx: Ctx, location: Location) -> Option<Foreground>;
//! pub fn shell_finished(ctx: &Ctx, status: std::io::Result<std::process::ExitStatus>);
//! pub fn vpn(endpoint: Endpoint, command: String) -> Foreground;
//! pub fn vpn_finished(ctx: &Ctx, endpoint: Endpoint, status: std::io::Result<std::process::ExitStatus>);
//! pub fn reveal(ctx: &Ctx, location: Location);
//! pub fn check_dropped_paths(ctx: &Ctx, text: String, paths: Vec<PathBuf>);
//! ```
//!
//! [`Foreground`]: super::Foreground
