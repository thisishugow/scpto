//! One connection per remote host: the per-process ControlMaster,
//! multiplexed background children, two SFTP channels per endpoint, error
//! classification, the single remote-command quoting helper, remote tool
//! probes, exponential backoff reconnect, and teardown. `Sessions`
//! implements `connections::Connections`.
//!
//! # Master lifecycle (plan 2.5, 7.3, 8.5)
//!
//! - ControlPath directory: `/tmp/scpto-<uid>/<pid>/` (both levels mode
//!   0700, created by scpto), socket `%C`. At startup, `<pid>` directories
//!   whose process no longer exists (`kill(pid, 0)` fails with `ESRCH`) are
//!   removed.
//! - Master: `<program> [-F <file>] -o ControlMaster=yes -o
//!   ControlPath=<dir>/%C -o ControlPersist=10m -o ServerAliveInterval=<keepalive>
//!   -o ServerAliveCountMax=3 <7.3 common options> -o BatchMode=yes -N -f
//!   <host>`, spawned with `process_group(0)`, stdin null, `kill_on_drop`.
//!   Classification uses the exit code first: exit 0 is success even with a
//!   non-empty stderr (OpenSSH 10 prints a post-quantum warning for old
//!   servers); stderr lines end in `\r\n`.
//! - When the BatchMode master fails with `needs_foreground_auth()`, the app
//!   receives `BgEvent::InteractiveAuthRequired`. The executor then runs
//!   `foreground_master_command` (the master command without `BatchMode`,
//!   stdio inherited; `-f` makes ssh return once authenticated) through
//!   `term::TerminalGuard::run_foreground` and calls `connect` again, which
//!   finds the master through `ssh -O check`.
//! - Children (sftp channels, `spawn_remote`, probes, `ssh_command`):
//!   `-o ControlPath=<dir>/%C -o ControlMaster=no`, the 7.3 common options,
//!   `-o BatchMode=yes`; `ssh -O check <host>` before each spawn, and a
//!   missing master is rebuilt instead of letting the child connect itself.
//!   SFTP channel: `-s <host> sftp`; its stderr is drained by a dedicated
//!   task into a `StderrTail` of the last 64 lines.
//! - Each endpoint keeps a Browse and a Transfer channel (S17). For a sudo
//!   endpoint each channel is a `sudo::start` sftp-server; this module
//!   decides whether both channels share one sudo sftp-server, and the
//!   password is never retained: a second sudo start that needs a password
//!   sends `BgEvent::SudoPasswordRequired` again.
//! - Teardown (`shutdown`): `ssh -O exit <host>` for every master this
//!   process created, then wait up to 3 s for each socket to disappear
//!   before removing the `<pid>` directory.
//!
//! # Classification and reconnect (S1, S10)
//!
//! - ssh exit 255 with stderr matching timeout / no route / refused / name
//!   resolution → `ConnectError::Unreachable`; `Permission denied` /
//!   `Too many authentication failures` → `AuthFailed`; `Host key
//!   verification failed` / `REMOTE HOST IDENTIFICATION HAS CHANGED` →
//!   `HostKeyRejected`; `channel ... open failed` / `administratively
//!   prohibited` → `ChannelOpenFailed`; spawn failure → `SshUnavailable`;
//!   else `Other`. SFTP-level start failures are mapped by
//!   `vfs::sftp::SftpFs::start`.
//! - A channel that reports `ConnectionLost` (from `SftpFs::closed` or
//!   `FsError::Disconnected`) is confirmed with `ssh -O check`; the endpoint
//!   then sends `BgEvent::Disconnected` and retries.
//! - Retries follow `backoff_delay` (settings `reconnect-base-ms`,
//!   `reconnect-cap-ms`), each announced as `BgEvent::Reconnecting` with the
//!   last error (an unreachable first connect is `attempt: 1`, so the VPN
//!   hint fires through `ConnectError::vpn_hint_eligible`). Only errors with
//!   `should_auto_reconnect()` are retried. Success after a drop sends
//!   `BgEvent::Reconnected`; while reconnecting, `Connections::fs` waits.
//! - VPN hint eligibility is `ConnectError::vpn_hint_eligible()`; matching
//!   the `vpn` rule is the app's job (`Settings::vpn_rule`).
//!
//! # Quoting helper (7.3)
//!
//! Every remote command string (tar `-C`, rsync `-e`, the `t` shell, probes,
//! sudo) is built with `quote` / `join_command`: each word is wrapped in
//! single quotes with `'` written as `'\''`; a word containing `\n` fails
//! with `Error::NewlineInPath`. The `t` shell's command is
//! `cd '<dir>' && exec "$SHELL" -l` built so that `$SHELL` sits inside the
//! single-quoted remote command and expands remotely.
//!
//! # Tool probe
//!
//! `Connections::tools` runs one `sh -c` probe per connection: `command -v
//! tar` and `rsync --version`; `rsync_ok` requires a first line of
//! `rsync  version 3.1` or newer from samba rsync (openrsync and 2.x do not
//! qualify). Results are cached until the endpoint reconnects.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! pub struct Sessions { /* private */ }
//!
//! impl Sessions {
//!     /// Creates the ControlPath directory and removes stale ones. Status
//!     /// events go to `events`.
//!     pub fn new(
//!         client: SshClient,
//!         settings: &Settings,
//!         events: tokio::sync::mpsc::UnboundedSender<BgEvent>,
//!     ) -> Result<Sessions, Error>;
//!     /// Starts connecting in the background; answers with `Connected`,
//!     /// `Reconnecting`, `ConnectFailed`, `InteractiveAuthRequired`, or
//!     /// `SudoPasswordRequired`. Connecting an already connected endpoint
//!     /// answers `Connected` again.
//!     pub fn connect(&self, endpoint: Endpoint);
//!     /// Continues a sudo start with the typed password (`sudo::start`
//!     /// stage 2); the password is dropped (zeroized) afterwards.
//!     pub fn sudo_password(&self, endpoint: Endpoint, password: Secret);
//!     /// Retries now, resetting the backoff.
//!     pub fn reconnect(&self, endpoint: Endpoint);
//!     /// Closes an endpoint's channels; the master of a host closes when no
//!     /// endpoint of that host remains.
//!     pub fn disconnect(&self, endpoint: &Endpoint);
//!     /// The foreground master command for interactive auth (the master
//!     /// command without `BatchMode`), as a ready `Command`.
//!     pub fn foreground_master_command(&self, endpoint: &Endpoint) -> Result<tokio::process::Command, Error>;
//!     /// Teardown described above.
//!     pub async fn shutdown(&self);
//! }
//!
//! impl Connections for Sessions { /* fs, spawn_remote, ssh_command, tools */ }
//!
//! /// Pure helpers.
//! pub fn quote(word: &str) -> Result<String, Error>;
//! /// Quotes every word and joins them with single spaces.
//! pub fn join_command(words: &[String]) -> Result<String, Error>;
//! /// `min(base * 2^(attempt - 1), cap)` for one-based `attempt`.
//! pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration) -> Duration;
//! /// Classifies a finished ssh process by exit status and stderr tail.
//! pub fn classify_ssh_exit(status: ExitStatus, stderr: &[String]) -> ConnectError;
//! /// Parses the tool probe output.
//! pub fn parse_tools(output: &str) -> RemoteTools;
//! /// `/tmp/scpto-<uid>/<pid>`. Always under `/tmp`, never `$TMPDIR`, so
//! /// the socket path stays below the 104-byte macOS limit.
//! pub fn control_dir(uid: u32, pid: u32) -> PathBuf;
//! ```
