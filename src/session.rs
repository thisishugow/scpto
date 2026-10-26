//! One connection per remote host: the per-process ControlMaster,
//! multiplexed background children, two SFTP channels per endpoint, error
//! classification, remote tool probes, exponential backoff reconnect, and teardown. `Sessions`
//! implements `connections::Connections`.
//!
//! # Master lifecycle
//!
//! - ControlPath directory: `/tmp/scpto-<uid>/<pid>/` (both levels mode
//!   0700, created by scpto; an existing `/tmp/scpto-<uid>` must be a real
//!   directory owned by the user), socket `%C`. At startup, `<pid>`
//!   directories whose process no longer exists (`kill(pid, 0)` fails with
//!   `ESRCH`) are removed.
//! - Master: `<program> [-F <file>] -o ControlPath=<dir>/%C -o
//!   ControlMaster=yes -o ControlPersist=10m -o ServerAliveInterval=<keepalive>
//!   -o ServerAliveCountMax=3 <common non-interactive options>
//!   -o BatchMode=yes -N -f -- <host>`, spawned with `process_group(0)`,
//!   stdin null, `kill_on_drop`.
//!   Classification uses the exit code first: exit 0 is success even with a
//!   non-empty stderr (OpenSSH 10 prints a post-quantum warning for old
//!   servers); stderr lines end in `\r\n`.
//! - When the BatchMode master fails with `needs_foreground_auth()`, the app
//!   receives `BgEvent::InteractiveAuthRequired`. The executor then runs
//!   `foreground_master_command` (the master command without `BatchMode`,
//!   stdio inherited, no own process group; `-f` makes ssh return once
//!   authenticated) through `term::TerminalGuard::run_foreground` and calls
//!   `connect` again, which finds the master through `ssh -O check`.
//! - Children (sftp channels, `spawn_remote`, probes, `ssh_command`):
//!   `-o ControlPath=<dir>/%C -o ControlMaster=no`, the common non-interactive
//!   options, `-o BatchMode=yes`; `ssh -O check <host>` before each spawn, and a
//!   missing master is rebuilt instead of letting the child connect itself.
//!   SFTP channel: `-s -- <host> sftp`; its stderr is drained by a dedicated
//!   task into a `StderrTail` of the last 64 lines (each line capped at
//!   4 KiB).
//! - Each endpoint keeps a Browse and a Transfer channel. For a sudo
//!   endpoint both channels share one root sftp-server started through
//!   `Backends::start_sudo` (`sudo::start`): sudo endpoints use neither tar
//!   nor rsync, and one start means one password prompt. The password is
//!   never retained: when a reconnect of a sudo endpoint needs a password
//!   again, the session sends `BgEvent::SudoPasswordRequired` again.
//!   Password replies start stage 2 only after `PasswordRequired` or
//!   `WrongPassword` (whose `ConnectFailed` event asks again in the UI).
//!   Replies in every other state are dropped and zeroized.
//! - Teardown (`shutdown`): `ssh -O exit <host>` for every master this
//!   process started, then wait up to 3 s for the sockets to disappear
//!   before removing the `<pid>` directory. `disconnect` closes a host's
//!   master the same way once no endpoint of that host remains.
//!
//! # Classification and reconnect
//!
//! - ssh exit 255 with stderr matching timeout / no route / refused / name
//!   resolution → `ConnectError::Unreachable`; `Permission denied (` /
//!   `Too many authentication failures` → `AuthFailed`; `Host key
//!   verification failed` / `REMOTE HOST IDENTIFICATION HAS CHANGED` →
//!   `HostKeyRejected`; `channel ... open failed` / `administratively
//!   prohibited` → `ChannelOpenFailed`; spawn failure → `SshUnavailable`;
//!   else `Other`. Post-quantum warning lines (`** ...`) never count. SFTP-level
//!   start failures are mapped by `vfs::sftp::SftpFs::start`.
//! - A channel whose `SftpChannel::closed` resolves ends the connection:
//!   the endpoint sends `BgEvent::Disconnected` with `ConnectionLost` and
//!   retries; each attempt runs `ssh -O check` and rebuilds the master when
//!   it is gone.
//! - Retries follow `backoff_delay` (settings `reconnect-base-ms`,
//!   `reconnect-cap-ms`), each announced as `BgEvent::Reconnecting` with the
//!   last error (an unreachable first connect is `attempt: 1`, so the VPN
//!   hint fires through `ConnectError::vpn_hint_eligible`). Only errors with
//!   `should_auto_reconnect()` are retried. Success after a drop sends
//!   `BgEvent::Reconnected`; while reconnecting, `Connections::fs` waits.
//! - VPN hint eligibility is `ConnectError::vpn_hint_eligible()`; matching
//!   the `vpn` rule is the app's job (`Settings::vpn_rule`).
//!
//! # Quoting
//!
//! Every remote command string (tar `-C`, the `t` shell, probes, sudo) is
//! built with `connections::quote` / `connections::join_command`, the one
//! quoting helper of the crate. The `t` shell's command is
//! `cd '<dir>' && exec "$SHELL" -l` built so that `$SHELL` sits inside the
//! single-quoted remote command and expands remotely.
//!
//! # Tool probe
//!
//! `Connections::tools` runs one `sh -c` probe per connection: `command -v
//! tar` and `rsync --version`; `rsync_ok` requires a first line of
//! `rsync  version 3.1` or newer from samba rsync (openrsync and 2.x do not
//! qualify). Results are cached until the endpoint reconnects; the endpoint
//! must have been connected with `connect`.
//!
//! # Other modules (injected)
//!
//! The SFTP client (`vfs::sftp`), the sudo start (`sudo`), and the local
//! file system (`vfs::local`) are reached through the [`Backends`] trait so
//! tests can substitute these dependencies. `ui::effects::session` provides
//! `ProductionBackends` and the `SftpChannel` implementation for `SftpFs`;
//! startup passes the production backends to `Sessions::new`. These adapters
//! delegate to `SftpFs::start` / `SftpFs::closed`,
//! `sudo::locate_sftp_server`, `sudo::start`, and `LocalFs::new`.
//!
//! # Public API
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
//!         backends: Arc<dyn Backends>,
//!     ) -> Result<Sessions, Error>;
//!     /// Starts connecting in the background; answers with `Connected`,
//!     /// `Reconnecting`, `ConnectFailed`, `InteractiveAuthRequired`, or
//!     /// `SudoPasswordRequired`. Connecting an already connected endpoint
//!     /// answers `Connected` again.
//!     pub fn connect(&self, endpoint: Endpoint);
//!     /// Continues a sudo start with the typed password (`sudo::start`
//!     /// stage 2) only after `PasswordRequired` or `WrongPassword`.
//!     /// The password is dropped (zeroized) afterwards or when ignored.
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
//! /// Behavior of other modules the session drives.
//! pub trait Backends: Send + Sync {
//!     fn local(&self) -> Arc<dyn Fs>;
//!     fn start_sftp(&self, endpoint: Endpoint, channel: RemoteChild)
//!         -> BoxFuture<'_, Result<Arc<dyn SftpChannel>, ConnectError>>;
//!     fn locate_sftp_server<'a>(&'a self, connections: &'a dyn Connections, endpoint: &'a Endpoint)
//!         -> BoxFuture<'a, Result<PathBuf, SudoError>>;
//!     fn start_sudo<'a>(&'a self, connections: &'a dyn Connections, endpoint: &'a Endpoint,
//!         sftp_server: &'a Path, password: Option<&'a Secret>)
//!         -> BoxFuture<'a, Result<RemoteChild, SudoError>>;
//! }
//!
//! /// An SFTP `Fs` that reports when its session ended (`SftpFs`).
//! pub trait SftpChannel: Fs {
//!     fn closed(&self) -> BoxFuture<'_, ()>;
//! }
//!
//! /// Pure helpers.
//! /// `min(base * 2^(attempt - 1), cap)` for one-based `attempt`.
//! pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration) -> Duration;
//! /// Classifies a finished, unsuccessful ssh process by exit status and
//! /// stderr tail.
//! pub fn classify_ssh_exit(status: ExitStatus, stderr: &[String]) -> ConnectError;
//! /// Parses the tool probe output.
//! pub fn parse_tools(output: &str) -> RemoteTools;
//! /// `/tmp/scpto-<uid>/<pid>`. Always under `/tmp`, never `$TMPDIR`, so
//! /// the socket path stays below the 104-byte macOS limit.
//! pub fn control_dir(uid: u32, pid: u32) -> PathBuf;
//! ```
//!
//! Every method that starts work in the background (`connect`,
//! `sudo_password`, `reconnect`, `disconnect`) must be called inside a
//! Tokio runtime.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::future::{join_all, select_all, BoxFuture};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, Command};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{watch, Notify, OnceCell};
use tokio::task::JoinHandle;

use crate::connections::{
    join_command, rsync_version_ok, Connections, FsChannel, RemoteChild, RemoteTools, SshClient,
    StderrTail,
};
use crate::error::{ConnectError, Error, ExitStatus, FsError, SudoError, UnreachableReason};
use crate::settings::{Settings, KEEPALIVE_COUNT_MAX};
use crate::ui::action::{BgEvent, Secret};
use crate::vfs::{Endpoint, Fs};

/// Common non-interactive SSH options, each passed as `-o <opt>`.
const COMMON_OPTIONS: [&str; 6] = [
    "RemoteCommand=none",
    "RequestTTY=no",
    "ClearAllForwardings=yes",
    "PermitLocalCommand=no",
    "ForwardX11=no",
    "ForwardAgent=no",
];

/// `ControlPersist` of every master: a crashed scpto leaves its masters
/// alive for at most this long.
const CONTROL_PERSIST: &str = "10m";

/// stderr lines kept per child.
const STDERR_TAIL_LINES: usize = 64;

/// Longest stderr line kept, in bytes; the rest of a longer line is dropped.
const STDERR_LINE_MAX: usize = 4096;

/// How long teardown waits for master sockets to disappear.
const TEARDOWN_WAIT: Duration = Duration::from_secs(3);

/// Poll interval while waiting for master sockets to disappear.
const TEARDOWN_POLL: Duration = Duration::from_millis(50);

/// Line separating the tar answer from the `rsync --version` output.
const TOOLS_MARKER: &str = "--scpto-rsync--";

/// The tool probe, run as `sh -c <script>` as the login user.
const TOOLS_SCRIPT: &str = "if command -v tar >/dev/null 2>&1; then echo tar=yes; \
     else echo tar=no; fi; echo --scpto-rsync--; rsync --version 2>/dev/null; exit 0";

/// The wait before retry number `attempt` (one-based):
/// `min(base * 2^(attempt - 1), cap)`. Attempt 0 counts as attempt 1.
pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration) -> Duration {
    1u32.checked_shl(attempt.saturating_sub(1))
        .map_or(cap, |factor| base.saturating_mul(factor).min(cap))
}

/// Classifies a finished ssh process that did not succeed, by its exit
/// status and stderr tail.
///
/// Callers check for success first: exit 0 is success whatever stderr says.
/// Only exit 255 is ssh's own failure; other codes come from the remote
/// command and are [`ConnectError::Other`]. Lines may still carry `\r\n`;
/// post-quantum warning lines (`** ...`) are ignored.
pub fn classify_ssh_exit(status: ExitStatus, stderr: &[String]) -> ConnectError {
    let lines: Vec<&str> = stderr
        .iter()
        .map(|line| line.trim_end_matches(['\r', '\n']))
        .filter(|line| !line.trim().is_empty() && !line.starts_with("** "))
        .collect();
    let all = || {
        if lines.is_empty() {
            format!("ssh ended with {status}")
        } else {
            lines.join("; ")
        }
    };
    if status != ExitStatus::Code(255) {
        return ConnectError::Other { detail: all() };
    }
    let find = |needles: &[&str]| {
        lines
            .iter()
            .find(|line| needles.iter().any(|needle| line.contains(needle)))
            .map(|line| (*line).to_string())
    };
    if let Some(detail) = find(&[
        "Host key verification failed",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
    ]) {
        return ConnectError::HostKeyRejected { detail };
    }
    if let Some(detail) = find(&["Permission denied (", "Too many authentication failures"]) {
        return ConnectError::AuthFailed { detail };
    }
    let unreachable: [(UnreachableReason, &[&str]); 4] = [
        (
            UnreachableReason::NameResolution,
            &[
                "Could not resolve hostname",
                "Name or service not known",
                "nodename nor servname provided",
                "Temporary failure in name resolution",
            ],
        ),
        (
            UnreachableReason::Timeout,
            &["timed out", "Connection timed out"],
        ),
        (
            UnreachableReason::NoRoute,
            &["No route to host", "Network is unreachable"],
        ),
        (UnreachableReason::Refused, &["Connection refused"]),
    ];
    for (reason, needles) in unreachable {
        if let Some(detail) = find(needles) {
            return ConnectError::Unreachable { reason, detail };
        }
    }
    if let Some(detail) = lines
        .iter()
        .find(|line| {
            (line.contains("channel") && line.contains("open failed"))
                || line.contains("administratively prohibited")
        })
        .map(|line| (*line).to_string())
    {
        return ConnectError::ChannelOpenFailed { detail };
    }
    ConnectError::Other { detail: all() }
}

/// Parses the output of the tool probe script: `tar=yes` before the marker
/// line, then the `rsync --version` output judged by
/// [`rsync_version_ok`]. Output without the marker reports no tools.
pub fn parse_tools(output: &str) -> RemoteTools {
    let mut lines = output.lines();
    let mut tar = false;
    for line in lines.by_ref() {
        match line.trim_end_matches('\r') {
            TOOLS_MARKER => {
                let rest: Vec<&str> = lines.collect();
                return RemoteTools {
                    tar,
                    rsync_ok: rsync_version_ok(&rest.join("\n")),
                };
            }
            "tar=yes" => tar = true,
            _ => {}
        }
    }
    RemoteTools {
        tar: false,
        rsync_ok: false,
    }
}

/// `/tmp/scpto-<uid>/<pid>`. Always under `/tmp`, never `$TMPDIR`, so the
/// socket path stays below the 104-byte macOS limit.
pub fn control_dir(uid: u32, pid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/scpto-{uid}")).join(pid.to_string())
}

/// Behavior of other modules that the session drives, injected so the
/// session does not depend on their implementations. The production
/// implementation forwards to `vfs::local::LocalFs::new`,
/// `vfs::sftp::SftpFs::start`, `sudo::locate_sftp_server`, and
/// `sudo::start`.
pub trait Backends: Send + Sync {
    /// The local file system, served for `Endpoint::Local` on both
    /// channels.
    fn local(&self) -> Arc<dyn Fs>;

    /// Runs the SFTP handshake on a started channel child
    /// (`SftpFs::start`). The returned channel owns the child.
    fn start_sftp(
        &self,
        endpoint: Endpoint,
        channel: RemoteChild,
    ) -> BoxFuture<'_, Result<Arc<dyn SftpChannel>, ConnectError>>;

    /// Finds the sftp-server binary of a sudo endpoint's host
    /// (`sudo::locate_sftp_server`).
    fn locate_sftp_server<'a>(
        &'a self,
        connections: &'a dyn Connections,
        endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<PathBuf, SudoError>>;

    /// Starts a root sftp-server through sudo, stage 1 without a password
    /// or stage 2 with one (`sudo::start`).
    fn start_sudo<'a>(
        &'a self,
        connections: &'a dyn Connections,
        endpoint: &'a Endpoint,
        sftp_server: &'a Path,
        password: Option<&'a Secret>,
    ) -> BoxFuture<'a, Result<RemoteChild, SudoError>>;
}

/// An SFTP file system that reports when its session ended
/// (`vfs::sftp::SftpFs`).
pub trait SftpChannel: Fs {
    /// Resolves when the session is unusable: the child exited or the
    /// client's background task failed (`SftpFs::closed`).
    fn closed(&self) -> BoxFuture<'_, ()>;
}

/// The SSH sessions of one scpto process. See the module documentation.
pub struct Sessions {
    shared: Arc<Shared>,
}

/// State shared by `Sessions` and its background tasks.
struct Shared {
    client: SshClient,
    control_dir: PathBuf,
    keepalive: Duration,
    reconnect_base: Duration,
    reconnect_cap: Duration,
    events: UnboundedSender<BgEvent>,
    backends: Arc<dyn Backends>,
    masters: Mutex<HashMap<String, Arc<Master>>>,
    endpoints: Mutex<HashMap<Endpoint, Slot>>,
}

/// One host's master connection.
#[derive(Default)]
struct Master {
    /// Serializes `-O check`, master start, and `-O exit` for the host.
    lock: tokio::sync::Mutex<()>,
    /// Whether this process started a master (or handed out the foreground
    /// master command) that has not been closed since.
    started: AtomicBool,
}

/// One remote endpoint known to the session.
struct Slot {
    state: Arc<SlotState>,
    /// The connect task; `None` or finished once it gave up.
    task: Option<JoinHandle<()>>,
}

/// The part of a slot its connect task updates.
struct SlotState {
    status: watch::Sender<Status>,
    /// Wakes a task waiting for its next retry.
    retry_now: Notify,
    /// A connection of this endpoint was lost and is not restored yet, so
    /// the next success is a `Reconnected`.
    lost: AtomicBool,
}

/// Connection status observed by `Connections::fs`.
#[derive(Clone)]
enum Status {
    /// Connecting or reconnecting; `fs` waits.
    Pending,
    /// Connected.
    Ready(Channels),
    /// Not connected and not retrying on its own.
    Down(Error),
}

/// The handles of a connected endpoint.
#[derive(Clone)]
struct Channels {
    browse: Arc<dyn Fs>,
    transfer: Arc<dyn Fs>,
    home: PathBuf,
    tools: Arc<OnceCell<RemoteTools>>,
}

/// A connection plus the channels whose end ends it.
struct Live {
    channels: Channels,
    watched: Vec<Arc<dyn SftpChannel>>,
}

/// Why an attempt to establish an endpoint failed.
enum Failure {
    /// Auth or host key failure under BatchMode: hand over to OpenSSH.
    Interactive(ConnectError),
    /// sudo needs a password.
    SudoPassword,
    /// Anything else; retried when `should_auto_reconnect()`.
    Error(Error),
}

impl Failure {
    fn connect(error: ConnectError) -> Failure {
        if error.needs_foreground_auth() {
            Failure::Interactive(error)
        } else {
            Failure::Error(Error::Connect(error))
        }
    }

    fn sudo(error: SudoError) -> Failure {
        match error {
            SudoError::PasswordRequired => Failure::SudoPassword,
            SudoError::Connect(error) => Failure::connect(error),
            other => Failure::Error(Error::Sudo(other)),
        }
    }
}

/// Why a background task is (re)started for an endpoint.
enum StartReason {
    Connect,
    Reconnect,
    Password(Secret),
}

impl Sessions {
    /// Creates the ControlPath directory `control_dir(uid, pid)` and removes
    /// stale ones. Status events go to `events`; other modules' behavior
    /// comes from `backends`.
    pub fn new(
        client: SshClient,
        settings: &Settings,
        events: UnboundedSender<BgEvent>,
        backends: Arc<dyn Backends>,
    ) -> Result<Sessions, Error> {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let dir = control_dir(uid, std::process::id());
        let base = dir
            .parent()
            .expect("control_dir always has a parent directory");
        Sessions::with_base(client, settings, events, backends, base, uid)
    }

    /// `new` with the per-user directory `base` (tests use a private one).
    fn with_base(
        client: SshClient,
        settings: &Settings,
        events: UnboundedSender<BgEvent>,
        backends: Arc<dyn Backends>,
        base: &Path,
        uid: u32,
    ) -> Result<Sessions, Error> {
        let control_dir = prepare_control_dir(base, uid, std::process::id())?;
        Ok(Sessions {
            shared: Arc::new(Shared {
                client,
                control_dir,
                keepalive: settings.keepalive_interval,
                reconnect_base: settings.reconnect_base,
                reconnect_cap: settings.reconnect_cap,
                events,
                backends,
                masters: Mutex::default(),
                endpoints: Mutex::default(),
            }),
        })
    }

    /// Starts connecting in the background; answers with `Connected`,
    /// `Reconnecting`, `ConnectFailed`, `InteractiveAuthRequired`, or
    /// `SudoPasswordRequired`. Connecting an already connected endpoint
    /// answers `Connected` again; connecting one that is still connecting
    /// waits for that attempt's answer.
    pub fn connect(&self, endpoint: Endpoint) {
        self.start(endpoint, StartReason::Connect);
    }

    /// Continues a sudo start with the typed password (`sudo::start`
    /// stage 2) only after `PasswordRequired` or `WrongPassword`.
    /// The password is dropped (zeroized) afterwards or when ignored.
    pub fn sudo_password(&self, endpoint: Endpoint, password: Secret) {
        self.start(endpoint, StartReason::Password(password));
    }

    /// Retries now, resetting the backoff: wakes an endpoint waiting for its
    /// next retry, or starts connecting one that gave up.
    pub fn reconnect(&self, endpoint: Endpoint) {
        self.start(endpoint, StartReason::Reconnect);
    }

    /// Closes an endpoint's channels; the master of a host closes when no
    /// endpoint of that host remains. Pending `Connections::fs` calls for
    /// the endpoint fail.
    pub fn disconnect(&self, endpoint: &Endpoint) {
        let Endpoint::Remote { host, .. } = endpoint else {
            return;
        };
        let host_unused = {
            let mut slots = lock(&self.shared.endpoints);
            if let Some(task) = slots.remove(endpoint).and_then(|slot| slot.task) {
                task.abort();
            }
            !slots.keys().any(|other| remote_host(other) == Some(host))
        };
        if host_unused {
            let shared = Arc::clone(&self.shared);
            let host = host.clone();
            tokio::spawn(async move { shared.close_master(&host).await });
        }
    }

    /// The foreground master command for interactive auth: the master
    /// command without `BatchMode`, with inherited stdio and without its own
    /// process group, so OpenSSH can prompt on the terminal.
    pub fn foreground_master_command(&self, endpoint: &Endpoint) -> Result<Command, Error> {
        let host = remote_host(endpoint).ok_or_else(|| local_has_no_ssh(endpoint))?;
        self.shared
            .master(host)
            .started
            .store(true, Ordering::SeqCst);
        let mut command = Command::new(&self.shared.client.program);
        command.args(self.shared.master_args(host, false));
        Ok(command)
    }

    /// Teardown: stops every endpoint, sends `ssh -O exit` to every master
    /// this process started, waits up to 3 s for their sockets to
    /// disappear, and removes the ControlPath directory.
    pub async fn shutdown(&self) {
        let slots: Vec<Slot> = lock(&self.shared.endpoints)
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        for task in slots.into_iter().filter_map(|slot| slot.task) {
            task.abort();
        }
        let hosts: Vec<String> = lock(&self.shared.masters).keys().cloned().collect();
        join_all(hosts.iter().map(|host| self.shared.close_master(host))).await;
        let deadline = tokio::time::Instant::now() + TEARDOWN_WAIT;
        while dir_has_entries(&self.shared.control_dir) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(TEARDOWN_POLL).await;
        }
        // Teardown has nobody left to report to; a directory that cannot be
        // removed now is removed by the next start's stale cleanup, since
        // this process is gone by then.
        if let Err(error) = std::fs::remove_dir_all(&self.shared.control_dir) {
            drop(error);
        }
    }

    fn start(&self, endpoint: Endpoint, reason: StartReason) {
        if endpoint.is_local() {
            if matches!(reason, StartReason::Connect | StartReason::Reconnect) {
                let shared = Arc::clone(&self.shared);
                tokio::spawn(async move { shared.connect_local().await });
            }
            return;
        }
        let mut slots = lock(&self.shared.endpoints);
        let slot = slots.entry(endpoint.clone()).or_insert_with(|| Slot {
            state: Arc::new(SlotState {
                status: watch::Sender::new(Status::Pending),
                retry_now: Notify::new(),
                lost: AtomicBool::new(false),
            }),
            task: None,
        });
        if matches!(reason, StartReason::Password(_))
            && !matches!(
                &*slot.state.status.borrow(),
                Status::Down(Error::Sudo(
                    SudoError::PasswordRequired | SudoError::WrongPassword
                ))
            )
        {
            return;
        }
        // A terminal task publishes Down before its event and finishes afterward.
        // Accept the reply immediately; that task cannot write the state again.
        if slot.task.as_ref().is_some_and(|task| !task.is_finished())
            && !matches!(&*slot.state.status.borrow(), Status::Down(_))
        {
            match reason {
                StartReason::Connect => {
                    if let Status::Ready(channels) = &*slot.state.status.borrow() {
                        self.shared.emit(BgEvent::Connected {
                            endpoint,
                            home: channels.home.clone(),
                        });
                    }
                }
                StartReason::Reconnect => slot.state.retry_now.notify_waiters(),
                // A running task never waits for a password; the password
                // is dropped (zeroized) here.
                StartReason::Password(_) => {}
            }
            return;
        }
        let password = match reason {
            StartReason::Password(password) => Some(password),
            StartReason::Connect | StartReason::Reconnect => None,
        };
        slot.state.status.send_replace(Status::Pending);
        let shared = Arc::clone(&self.shared);
        let state = Arc::clone(&slot.state);
        slot.task = Some(tokio::spawn(async move {
            shared.run_endpoint(endpoint, state, password).await;
        }));
    }
}

impl Shared {
    fn emit(&self, event: BgEvent) {
        // A closed receiver means the app is shutting down; nobody is left
        // to inform.
        if let Err(closed) = self.events.send(event) {
            drop(closed);
        }
    }

    async fn connect_local(&self) {
        let event = match self.backends.local().home().await {
            Ok(home) => BgEvent::Connected {
                endpoint: Endpoint::Local,
                home,
            },
            Err(error) => BgEvent::ConnectFailed {
                endpoint: Endpoint::Local,
                error: Error::Fs(error),
            },
        };
        self.emit(event);
    }

    /// The connect task of one remote endpoint: establishes it, watches it,
    /// and retries with backoff until it succeeds or meets an error that is
    /// not retried.
    async fn run_endpoint(
        &self,
        endpoint: Endpoint,
        state: Arc<SlotState>,
        mut password: Option<Secret>,
    ) {
        // Failed attempts in a row; the next announced retry is `attempt + 1`.
        let mut failures: u32 = 0;
        loop {
            let error = match self.establish(&endpoint, password.take()).await {
                Ok(live) => {
                    failures = 0;
                    let home = live.channels.home.clone();
                    state.status.send_replace(Status::Ready(live.channels));
                    self.emit(if state.lost.swap(false, Ordering::SeqCst) {
                        BgEvent::Reconnected(endpoint.clone())
                    } else {
                        BgEvent::Connected {
                            endpoint: endpoint.clone(),
                            home,
                        }
                    });
                    select_all(live.watched.iter().map(|channel| channel.closed())).await;
                    state.status.send_replace(Status::Pending);
                    drop(live.watched);
                    state.lost.store(true, Ordering::SeqCst);
                    self.emit(BgEvent::Disconnected {
                        endpoint: endpoint.clone(),
                        error: ConnectError::ConnectionLost,
                    });
                    ConnectError::ConnectionLost
                }
                Err(Failure::Interactive(error)) => {
                    state
                        .status
                        .send_replace(Status::Down(Error::Connect(error.clone())));
                    self.emit(BgEvent::InteractiveAuthRequired { endpoint, error });
                    return;
                }
                Err(Failure::SudoPassword) => {
                    state
                        .status
                        .send_replace(Status::Down(Error::Sudo(SudoError::PasswordRequired)));
                    self.emit(BgEvent::SudoPasswordRequired(endpoint));
                    return;
                }
                Err(Failure::Error(Error::Connect(error))) if error.should_auto_reconnect() => {
                    error
                }
                Err(Failure::Error(error)) => {
                    state.status.send_replace(Status::Down(error.clone()));
                    self.emit(BgEvent::ConnectFailed { endpoint, error });
                    return;
                }
            };
            failures += 1;
            let delay = backoff_delay(failures, self.reconnect_base, self.reconnect_cap);
            // Registered before the event goes out, so a `reconnect` sent in
            // answer to it is never missed.
            let retry_now = state.retry_now.notified();
            self.emit(BgEvent::Reconnecting {
                endpoint: endpoint.clone(),
                attempt: failures,
                delay,
                error,
            });
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = retry_now => failures = 0,
            }
        }
    }

    /// One attempt: master, channels, home directory.
    async fn establish(
        &self,
        endpoint: &Endpoint,
        password: Option<Secret>,
    ) -> Result<Live, Failure> {
        let Endpoint::Remote { host, sudo } = endpoint else {
            return Err(Failure::Error(local_has_no_ssh(endpoint)));
        };
        self.ensure_master(host).await.map_err(Failure::connect)?;
        let (browse, transfer, watched) = if *sudo {
            let connections: &dyn Connections = self;
            let server = self
                .backends
                .locate_sftp_server(connections, endpoint)
                .await
                .map_err(Failure::sudo)?;
            let child = self
                .backends
                .start_sudo(connections, endpoint, &server, password.as_ref())
                .await
                .map_err(Failure::sudo)?;
            drop(password);
            let channel = self
                .backends
                .start_sftp(endpoint.clone(), child)
                .await
                .map_err(Failure::connect)?;
            let fs: Arc<dyn Fs> = channel.clone();
            (Arc::clone(&fs), fs, vec![channel])
        } else {
            drop(password);
            let (browse, transfer) = tokio::try_join!(
                self.open_sftp(endpoint, host),
                self.open_sftp(endpoint, host)
            )?;
            let browse_fs: Arc<dyn Fs> = browse.clone();
            let transfer_fs: Arc<dyn Fs> = transfer.clone();
            (browse_fs, transfer_fs, vec![browse, transfer])
        };
        let home = browse.home().await.map_err(|error| match error {
            FsError::Disconnected => Failure::Error(Error::Connect(ConnectError::ConnectionLost)),
            other => Failure::Error(Error::Fs(other)),
        })?;
        Ok(Live {
            channels: Channels {
                browse,
                transfer,
                home,
                tools: Arc::default(),
            },
            watched,
        })
    }

    async fn open_sftp(
        &self,
        endpoint: &Endpoint,
        host: &str,
    ) -> Result<Arc<dyn SftpChannel>, Failure> {
        let mut command = self.child_command(host).await.map_err(Failure::connect)?;
        command.args(["-s", "--", host, "sftp"]);
        let child = spawn_piped(command).map_err(Failure::connect)?;
        self.backends
            .start_sftp(endpoint.clone(), child)
            .await
            .map_err(Failure::connect)
    }

    fn master(&self, host: &str) -> Arc<Master> {
        Arc::clone(lock(&self.masters).entry(host.to_string()).or_default())
    }

    /// Makes sure the host's master is running: `ssh -O check`, and a
    /// BatchMode master start when it is not.
    async fn ensure_master(&self, host: &str) -> Result<(), ConnectError> {
        let master = self.master(host);
        let _serialized = master.lock.lock().await;
        if self.control(host, "check").await?.status.success() {
            return Ok(());
        }
        master.started.store(true, Ordering::SeqCst);
        let mut command = Command::new(&self.client.program);
        command
            .args(self.master_args(host, true))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let output = command.output().await.map_err(ssh_unavailable)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(classify_ssh_exit(
                output.status.into(),
                &stderr_lines(&output.stderr),
            ))
        }
    }

    /// Sends `ssh -O exit` to a master this process started.
    async fn close_master(&self, host: &str) {
        let master = self.master(host);
        let _serialized = master.lock.lock().await;
        if !master.started.swap(false, Ordering::SeqCst) {
            return;
        }
        // A failing `-O exit` means the master is already gone (or ssh
        // cannot run at all); either way no master of ours remains to close.
        if let Err(error) = self.control(host, "exit").await {
            drop(error);
        }
    }

    /// Runs `ssh -O <operation> <host>` on the ControlPath.
    async fn control(
        &self,
        host: &str,
        operation: &str,
    ) -> Result<std::process::Output, ConnectError> {
        let mut command = Command::new(&self.client.program);
        command
            .args(self.base_args())
            .args(["-O", operation, "--", host])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        command.output().await.map_err(ssh_unavailable)
    }

    /// `-F <file>` (when set) and `-o ControlPath=<dir>/%C`.
    fn base_args(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        if let Some(file) = &self.client.config_file {
            args.push(OsString::from("-F"));
            args.push(file.clone().into_os_string());
        }
        let mut control_path = OsString::from("ControlPath=");
        control_path.push(&self.control_dir);
        control_path.push("/%C");
        args.push(OsString::from("-o"));
        args.push(control_path);
        args
    }

    /// Master arguments after the program, up to and including the host.
    fn master_args(&self, host: &str, batch: bool) -> Vec<OsString> {
        let mut args = self.base_args();
        let mut push_option = |option: String| {
            args.push(OsString::from("-o"));
            args.push(OsString::from(option));
        };
        push_option("ControlMaster=yes".into());
        push_option(format!("ControlPersist={CONTROL_PERSIST}"));
        push_option(format!("ServerAliveInterval={}", self.keepalive.as_secs()));
        push_option(format!("ServerAliveCountMax={KEEPALIVE_COUNT_MAX}"));
        for option in COMMON_OPTIONS {
            push_option(option.into());
        }
        if batch {
            push_option("BatchMode=yes".into());
        }
        args.extend(["-N", "-f", "--", host].map(OsString::from));
        args
    }

    /// The multiplexed child argv after the program, in the order
    /// `Connections::ssh_command` documents.
    fn mux_args(&self, interactive: bool) -> Vec<OsString> {
        let mut args = self.base_args();
        let mut options: Vec<&str> = vec!["ControlMaster=no"];
        if interactive {
            options.push("RemoteCommand=none");
        } else {
            options.extend(COMMON_OPTIONS);
            options.push("BatchMode=yes");
        }
        for option in options {
            args.push(OsString::from("-o"));
            args.push(OsString::from(option));
        }
        if interactive {
            args.push(OsString::from("-t"));
        }
        args
    }

    /// A non-interactive child command up to (excluding) the destination,
    /// after making sure the master runs.
    async fn child_command(&self, host: &str) -> Result<Command, ConnectError> {
        self.ensure_master(host).await?;
        let mut command = Command::new(&self.client.program);
        command.args(self.mux_args(false));
        Ok(command)
    }

    /// The handles of a connected endpoint, waiting while it (re)connects.
    async fn ready_channels(&self, endpoint: &Endpoint) -> Result<Channels, Error> {
        let mut status = lock(&self.endpoints)
            .get(endpoint)
            .map(|slot| slot.state.status.subscribe())
            .ok_or_else(|| not_connected(endpoint))?;
        loop {
            let current = status.borrow_and_update().clone();
            match current {
                Status::Ready(channels) => return Ok(channels),
                Status::Down(error) => return Err(error),
                Status::Pending => status
                    .changed()
                    .await
                    .map_err(|_| not_connected(endpoint))?,
            }
        }
    }

    async fn probe_tools(&self, endpoint: &Endpoint) -> Result<RemoteTools, Error> {
        let argv = ["sh", "-c", TOOLS_SCRIPT].map(String::from);
        let RemoteChild {
            mut child,
            mut stdin,
            mut stdout,
            stderr,
        } = self.spawn_remote(endpoint, &argv).await?;
        let program = || "sh".to_string();
        let io_failure = |error: io::Error| Error::CommandFailed {
            program: program(),
            status: ExitStatus::NotStarted,
            stderr: error.to_string(),
        };
        stdin.shutdown().await.map_err(io_failure)?;
        drop(stdin);
        let mut output = String::new();
        stdout
            .read_to_string(&mut output)
            .await
            .map_err(io_failure)?;
        let status = child.wait().await.map_err(io_failure)?;
        if status.success() {
            Ok(parse_tools(&output))
        } else {
            Err(child_failure(&program(), status.into(), &stderr))
        }
    }
}

impl Connections for Shared {
    fn fs<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        channel: FsChannel,
    ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
        Box::pin(async move {
            if endpoint.is_local() {
                return Ok(self.backends.local());
            }
            let channels = self.ready_channels(endpoint).await?;
            Ok(match channel {
                FsChannel::Browse => channels.browse,
                FsChannel::Transfer => channels.transfer,
            })
        })
    }

    fn spawn_remote<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        argv: &'a [String],
    ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
        Box::pin(async move {
            let host = remote_host(endpoint).ok_or_else(|| local_has_no_ssh(endpoint))?;
            let remote_command = join_command(argv)?;
            let mut command = self.child_command(host).await?;
            command.args(["--", host, &remote_command]);
            Ok(spawn_piped(command)?)
        })
    }

    fn ssh_command<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        interactive: bool,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(async move {
            let host = remote_host(endpoint).ok_or_else(|| local_has_no_ssh(endpoint))?;
            self.ensure_master(host).await?;
            std::iter::once(self.client.program.as_os_str().to_os_string())
                .chain(self.mux_args(interactive))
                .map(|word| {
                    word.into_string().map_err(|word| {
                        Error::Connect(ConnectError::SshUnavailable {
                            message: format!(
                                "{} is not valid UTF-8 and cannot be passed to rsync",
                                Path::new(&word).display()
                            ),
                        })
                    })
                })
                .collect()
        })
    }

    fn tools<'a>(&'a self, endpoint: &'a Endpoint) -> BoxFuture<'a, Result<RemoteTools, Error>> {
        Box::pin(async move {
            if endpoint.is_local() {
                return Err(local_has_no_ssh(endpoint));
            }
            let channels = self.ready_channels(endpoint).await?;
            channels
                .tools
                .get_or_try_init(|| self.probe_tools(endpoint))
                .await
                .copied()
        })
    }
}

impl Connections for Sessions {
    fn fs<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        channel: FsChannel,
    ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
        self.shared.fs(endpoint, channel)
    }

    fn spawn_remote<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        argv: &'a [String],
    ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
        self.shared.spawn_remote(endpoint, argv)
    }

    fn ssh_command<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        interactive: bool,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        self.shared.ssh_command(endpoint, interactive)
    }

    fn tools<'a>(&'a self, endpoint: &'a Endpoint) -> BoxFuture<'a, Result<RemoteTools, Error>> {
        self.shared.tools(endpoint)
    }
}

/// Locks a std mutex. The guarded maps stay consistent even if a holder
/// panicked (every update is a single insert or remove), so a poisoned lock
/// is used as is.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn remote_host(endpoint: &Endpoint) -> Option<&str> {
    match endpoint {
        Endpoint::Remote { host, .. } => Some(host),
        Endpoint::Local => None,
    }
}

fn local_has_no_ssh(endpoint: &Endpoint) -> Error {
    Error::Connect(ConnectError::Other {
        detail: format!("{endpoint} is not reached over ssh"),
    })
}

fn not_connected(endpoint: &Endpoint) -> Error {
    Error::Connect(ConnectError::Other {
        detail: format!("{endpoint} is not connected"),
    })
}

fn ssh_unavailable(error: io::Error) -> ConnectError {
    ConnectError::SshUnavailable {
        message: error.to_string(),
    }
}

/// The failure of a remote child: ssh's own exit 255 is a classified
/// connection failure, any other status is the remote program's.
fn child_failure(program: &str, status: ExitStatus, stderr: &StderrTail) -> Error {
    let lines: Vec<String> = lock(stderr).iter().cloned().collect();
    if status == ExitStatus::Code(255) {
        Error::Connect(classify_ssh_exit(status, &lines))
    } else {
        Error::CommandFailed {
            program: program.to_string(),
            status,
            stderr: lines.join("\n"),
        }
    }
}

/// Splits captured stderr into lines without `\r\n` / `\n`.
fn stderr_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

/// Spawns a background child with piped stdio in its own process group,
/// killed on drop, with stderr drained into a bounded tail.
fn spawn_piped(mut command: Command) -> Result<RemoteChild, ConnectError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(ssh_unavailable)?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = drain_stderr(child.stderr.take().expect("stderr is piped"));
    Ok(RemoteChild {
        child,
        stdin,
        stdout,
        stderr,
    })
}

/// Reads `stderr` until it ends, keeping the last [`STDERR_TAIL_LINES`]
/// lines.
fn drain_stderr(mut stderr: ChildStderr) -> StderrTail {
    let tail = StderrTail::default();
    let sink = Arc::clone(&tail);
    tokio::spawn(async move {
        let mut line = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => {
                    for &byte in &chunk[..read] {
                        if byte == b'\n' {
                            push_line(&sink, &line);
                            line.clear();
                        } else if line.len() < STDERR_LINE_MAX {
                            line.push(byte);
                        }
                    }
                }
                Err(error) => {
                    push_line(
                        &sink,
                        format!("scpto: reading stderr failed: {error}").as_bytes(),
                    );
                    break;
                }
            }
        }
        if !line.is_empty() {
            push_line(&sink, &line);
        }
    });
    tail
}

fn push_line(tail: &StderrTail, line: &[u8]) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut tail = lock(tail);
    tail.push_back(String::from_utf8_lossy(line).into_owned());
    while tail.len() > STDERR_TAIL_LINES {
        tail.pop_front();
    }
}

fn dir_has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

fn fs_error(error: &io::Error, path: &Path) -> Error {
    Error::Fs(FsError::from_io(error, path))
}

/// Creates `base` (`/tmp/scpto-<uid>`) and `base/<pid>`, both mode 0700, and
/// removes `<pid>` directories of processes that no longer exist. An
/// existing `base` must be a directory (not a symlink) owned by `uid`; a
/// leftover `base/<pid>` of an earlier process with the same pid is
/// replaced.
fn prepare_control_dir(base: &Path, uid: u32, pid: u32) -> Result<PathBuf, Error> {
    match std::fs::DirBuilder::new().mode(0o700).create(base) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata =
                std::fs::symlink_metadata(base).map_err(|error| fs_error(&error, base))?;
            if !metadata.is_dir() || metadata.uid() != uid {
                return Err(Error::Fs(FsError::Io {
                    path: base.to_path_buf(),
                    message: "exists but is not a directory owned by the current user".into(),
                }));
            }
            if metadata.mode() & 0o077 != 0 {
                std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700))
                    .map_err(|error| fs_error(&error, base))?;
            }
        }
        Err(error) => return Err(fs_error(&error, base)),
    }
    remove_stale_dirs(base, pid)?;
    let dir = base.join(pid.to_string());
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(fs_error(&error, &dir)),
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|error| fs_error(&error, &dir))?;
    Ok(dir)
}

/// Removes `base/<pid>` directories whose process no longer exists, except
/// `own_pid`'s. Entries that are not positive numbers are left alone.
fn remove_stale_dirs(base: &Path, own_pid: u32) -> Result<(), Error> {
    let entries = std::fs::read_dir(base).map_err(|error| fs_error(&error, base))?;
    for entry in entries {
        let entry = entry.map_err(|error| fs_error(&error, base))?;
        let name = entry.file_name();
        let Some(pid) = pid_of(&name) else {
            continue;
        };
        if pid == own_pid || process_exists(pid) {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            // Another scpto starting at the same time removed it first.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(fs_error(&error, &path)),
        }
    }
    Ok(())
}

fn pid_of(name: &OsStr) -> Option<u32> {
    name.to_str()?.parse::<u32>().ok().filter(|&pid| pid > 0)
}

/// Whether a process with this pid exists: `kill(pid, 0)` succeeds or fails
/// with anything but `ESRCH` (`EPERM` means it exists under another user).
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs only the existence and permission check.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_string()).collect()
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        let delays: Vec<u64> = (1..=7)
            .map(|attempt| backoff_delay(attempt, base, cap).as_secs())
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(backoff_delay(0, base, cap), base);
        assert_eq!(backoff_delay(32, base, cap), cap);
        assert_eq!(backoff_delay(u32::MAX, base, cap), cap);
        assert_eq!(
            backoff_delay(3, Duration::from_millis(20), Duration::from_millis(50)),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn classification_of_ssh_failures() {
        let pq = [
            "** WARNING: connection is not using a post-quantum key exchange algorithm.\r",
            "** This session may be vulnerable to \"store now, decrypt later\" attacks.\r",
        ];
        let cases: Vec<(&[&str], ConnectError)> = vec![
            (
                &["ssh: connect to host 127.0.0.1 port 1: Connection refused\r"],
                ConnectError::Unreachable {
                    reason: UnreachableReason::Refused,
                    detail: "ssh: connect to host 127.0.0.1 port 1: Connection refused".into(),
                },
            ),
            (
                &["ssh: connect to host 10.255.255.1 port 22: Operation timed out"],
                ConnectError::Unreachable {
                    reason: UnreachableReason::Timeout,
                    detail: "ssh: connect to host 10.255.255.1 port 22: Operation timed out"
                        .into(),
                },
            ),
            (
                &["ssh: connect to host h port 22: Connection timed out"],
                ConnectError::Unreachable {
                    reason: UnreachableReason::Timeout,
                    detail: "ssh: connect to host h port 22: Connection timed out".into(),
                },
            ),
            (
                &["Connection timed out during banner exchange"],
                ConnectError::Unreachable {
                    reason: UnreachableReason::Timeout,
                    detail: "Connection timed out during banner exchange".into(),
                },
            ),
            (
                &["ssh: connect to host h port 22: No route to host"],
                ConnectError::Unreachable {
                    reason: UnreachableReason::NoRoute,
                    detail: "ssh: connect to host h port 22: No route to host".into(),
                },
            ),
            (
                &["ssh: connect to host h port 22: Network is unreachable"],
                ConnectError::Unreachable {
                    reason: UnreachableReason::NoRoute,
                    detail: "ssh: connect to host h port 22: Network is unreachable".into(),
                },
            ),
            (
                &["ssh: Could not resolve hostname x.invalid: nodename nor servname provided, or not known"],
                ConnectError::Unreachable {
                    reason: UnreachableReason::NameResolution,
                    detail: "ssh: Could not resolve hostname x.invalid: nodename nor servname provided, or not known".into(),
                },
            ),
            (
                &["ssh: Could not resolve hostname x: Temporary failure in name resolution"],
                ConnectError::Unreachable {
                    reason: UnreachableReason::NameResolution,
                    detail: "ssh: Could not resolve hostname x: Temporary failure in name resolution".into(),
                },
            ),
            (
                &["plain@127.0.0.1: Permission denied (publickey).\r"],
                ConnectError::AuthFailed {
                    detail: "plain@127.0.0.1: Permission denied (publickey).".into(),
                },
            ),
            (
                &["Received disconnect from 1.2.3.4 port 22:2: Too many authentication failures"],
                ConnectError::AuthFailed {
                    detail: "Received disconnect from 1.2.3.4 port 22:2: Too many authentication failures".into(),
                },
            ),
            (
                &[
                    "No ED25519 host key is known for [127.0.0.1]:22221 and you have requested strict checking.\r",
                    "Host key verification failed.\r",
                ],
                ConnectError::HostKeyRejected {
                    detail: "Host key verification failed.".into(),
                },
            ),
            (
                &[
                    "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@",
                    "@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @",
                    "Host key verification failed.",
                ],
                ConnectError::HostKeyRejected {
                    detail: "@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @".into(),
                },
            ),
            (
                &["channel 0: open failed: administratively prohibited: open failed"],
                ConnectError::ChannelOpenFailed {
                    detail: "channel 0: open failed: administratively prohibited: open failed"
                        .into(),
                },
            ),
            (
                &["mux_client_request_session: session request failed: Session open refused by peer"],
                ConnectError::Other {
                    detail: "mux_client_request_session: session request failed: Session open refused by peer".into(),
                },
            ),
            (
                &["kex_exchange_identification: Connection closed by remote host"],
                ConnectError::Other {
                    detail: "kex_exchange_identification: Connection closed by remote host".into(),
                },
            ),
            (
                &[],
                ConnectError::Other {
                    detail: "ssh ended with exit status 255".into(),
                },
            ),
        ];
        for (stderr, expected) in cases {
            let mut with_warning = lines(&pq);
            with_warning.extend(lines(stderr));
            assert_eq!(
                classify_ssh_exit(ExitStatus::Code(255), &with_warning),
                expected,
                "stderr: {stderr:?}"
            );
        }
    }

    #[test]
    fn classification_of_non_ssh_exits_is_other() {
        assert_eq!(
            classify_ssh_exit(
                ExitStatus::Code(1),
                &lines(&["Permission denied (publickey)."])
            ),
            ConnectError::Other {
                detail: "Permission denied (publickey).".into()
            }
        );
        assert_eq!(
            classify_ssh_exit(ExitStatus::Signal(9), &[]),
            ConnectError::Other {
                detail: "ssh ended with killed by signal 9".into()
            }
        );
    }

    #[test]
    fn tool_probe_output_is_parsed() {
        let modern =
            "tar=yes\n--scpto-rsync--\nrsync  version 3.4.1  protocol version 32\nCopyright\n";
        assert_eq!(
            parse_tools(modern),
            RemoteTools {
                tar: true,
                rsync_ok: true
            }
        );
        let no_rsync = "tar=yes\n--scpto-rsync--\n";
        assert_eq!(
            parse_tools(no_rsync),
            RemoteTools {
                tar: true,
                rsync_ok: false
            }
        );
        let openrsync = "tar=no\r\n--scpto-rsync--\r\nopenrsync: protocol version 29\r\n";
        assert_eq!(
            parse_tools(openrsync),
            RemoteTools {
                tar: false,
                rsync_ok: false
            }
        );
        // Without the marker nothing is trusted.
        assert_eq!(
            parse_tools("tar=yes\nrsync  version 3.2.7"),
            RemoteTools {
                tar: false,
                rsync_ok: false
            }
        );
    }

    #[test]
    fn tool_probe_script_runs_in_a_real_shell() {
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(TOOLS_SCRIPT)
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(&format!("\n{TOOLS_MARKER}\n")), "{text}");
        let tar_exists = std::process::Command::new("/bin/sh")
            .args(["-c", "command -v tar"])
            .output()
            .unwrap()
            .status
            .success();
        assert_eq!(parse_tools(&text).tar, tar_exists);
    }

    #[test]
    fn control_dir_is_short_and_under_tmp() {
        let dir = control_dir(501, 44396);
        assert_eq!(dir, PathBuf::from("/tmp/scpto-501/44396"));
        // `%C` expands to 40 hex digits; the socket path must fit sun_path.
        let socket = dir.join("0".repeat(40));
        assert!(socket.as_os_str().len() < 104);
        let worst = control_dir(u32::MAX, u32::MAX).join("0".repeat(40));
        assert!(worst.as_os_str().len() < 104);
    }

    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn control_dir_is_private_and_stale_dirs_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("scpto-test");
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let own = std::process::id();

        let dir = prepare_control_dir(&base, uid, own).unwrap();
        assert_eq!(dir, base.join(own.to_string()));
        assert_eq!(mode_of(&base), 0o700);
        assert_eq!(mode_of(&dir), 0o700);

        let dead = base.join(dead_pid().to_string());
        let alive = base.join(std::os::unix::process::parent_id().to_string());
        let other = base.join("not-a-pid");
        let zero = base.join("0");
        for path in [&dead, &alive, &other, &zero] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(dead.join("socket"), b"").unwrap();
        std::fs::write(dir.join("leftover"), b"").unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

        let again = prepare_control_dir(&base, uid, own).unwrap();
        assert_eq!(again, dir);
        assert!(!dead.exists());
        assert!(alive.exists() && other.exists() && zero.exists());
        assert!(!dir.join("leftover").exists(), "own leftover is replaced");
        assert_eq!(mode_of(&base), 0o700, "loose permissions are tightened");
    }

    #[test]
    fn control_dir_refuses_foreign_or_symlinked_base() {
        let root = tempfile::tempdir().unwrap();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            prepare_control_dir(&link, uid, 1),
            Err(Error::Fs(FsError::Io { .. }))
        ));
        let file = root.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(prepare_control_dir(&file, uid, 1).is_err());
        let foreign = root.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        assert!(prepare_control_dir(&foreign, uid.wrapping_add(1), 1).is_err());
    }

    #[test]
    fn process_existence_check() {
        assert!(process_exists(std::process::id()));
        assert!(!process_exists(dead_pid()));
        assert!(!process_exists(u32::MAX));
        assert_eq!(pid_of(OsStr::new("123")), Some(123));
        assert_eq!(pid_of(OsStr::new("0")), None);
        assert_eq!(pid_of(OsStr::new("-5")), None);
        assert_eq!(pid_of(OsStr::new("12a")), None);
    }

    #[tokio::test]
    async fn stderr_tail_keeps_the_last_lines_without_terminators() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "i=0; while [ $i -lt 70 ]; do printf 'line %s\\r\\n' $i >&2; i=$((i+1)); done; \
             printf 'unterminated' >&2",
        ]);
        let mut child = spawn_piped(command).unwrap();
        child.child.wait().await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let tail: Vec<String> = lock(&child.stderr).iter().cloned().collect();
            if tail.last().map(String::as_str) == Some("unterminated") {
                assert_eq!(tail.len(), STDERR_TAIL_LINES);
                assert_eq!(tail[0], "line 7");
                assert_eq!(tail[62], "line 69");
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "tail: {tail:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn stderr_lines_are_capped_in_length() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "head -c 10000 /dev/zero | tr '\\0' x >&2; echo >&2; echo end >&2",
        ]);
        let mut child = spawn_piped(command).unwrap();
        child.child.wait().await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let tail: Vec<String> = lock(&child.stderr).iter().cloned().collect();
            if tail.last().map(String::as_str) == Some("end") {
                assert_eq!(tail[0].len(), STDERR_LINE_MAX);
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "tail: {tail:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // ----- Session behavior with the real ssh client -----------------------

    /// Stands in for `vfs::local`, `vfs::sftp`, and `sudo`, which are
    /// separate modules. The SFTP side runs the real handshake.
    struct TestBackends {
        local: Arc<dyn Fs>,
        sudo_password: &'static str,
    }

    impl TestBackends {
        fn new() -> Arc<TestBackends> {
            Arc::new(TestBackends {
                local: Arc::new(DoubleFs {
                    endpoint: Endpoint::Local,
                    session: None,
                }),
                sudo_password: "scpto-test-pw",
            })
        }
    }

    impl Backends for TestBackends {
        fn local(&self) -> Arc<dyn Fs> {
            Arc::clone(&self.local)
        }

        fn start_sftp(
            &self,
            endpoint: Endpoint,
            channel: RemoteChild,
        ) -> BoxFuture<'_, Result<Arc<dyn SftpChannel>, ConnectError>> {
            Box::pin(async move {
                let RemoteChild {
                    child,
                    stdin,
                    stdout,
                    stderr,
                } = channel;
                match openssh_sftp_client::Sftp::new(
                    stdin,
                    stdout,
                    openssh_sftp_client::SftpOptions::new(),
                )
                .await
                {
                    Ok(sftp) => Ok(Arc::new(DoubleFs {
                        endpoint,
                        session: Some(DoubleSession {
                            sftp,
                            child: tokio::sync::Mutex::new(child),
                        }),
                    }) as Arc<dyn SftpChannel>),
                    Err(error) => Err(ConnectError::UnsupportedServer {
                        detail: format!("{error}; stderr: {:?}", lock(&stderr)),
                    }),
                }
            })
        }

        fn locate_sftp_server<'a>(
            &'a self,
            _connections: &'a dyn Connections,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<PathBuf, SudoError>> {
            Box::pin(async { Ok(PathBuf::from("/usr/lib/ssh/sftp-server")) })
        }

        /// Checks the password protocol only; the "root" sftp-server is the
        /// login user's, which is enough to exercise the session.
        fn start_sudo<'a>(
            &'a self,
            connections: &'a dyn Connections,
            endpoint: &'a Endpoint,
            sftp_server: &'a Path,
            password: Option<&'a Secret>,
        ) -> BoxFuture<'a, Result<RemoteChild, SudoError>> {
            Box::pin(async move {
                match password {
                    None => return Err(SudoError::PasswordRequired),
                    Some(password) if password.expose() != self.sudo_password => {
                        return Err(SudoError::WrongPassword)
                    }
                    Some(_) => {}
                }
                let argv = vec![sftp_server.display().to_string()];
                connections
                    .spawn_remote(endpoint, &argv)
                    .await
                    .map_err(|error| match error {
                        Error::Connect(error) => SudoError::Connect(error),
                        other => SudoError::Other {
                            detail: other.to_string(),
                        },
                    })
            })
        }
    }

    struct DoubleSession {
        sftp: openssh_sftp_client::Sftp,
        child: tokio::sync::Mutex<tokio::process::Child>,
    }

    /// `home` and `list` work for real; other operations are outside these
    /// tests and report `Unsupported`.
    struct DoubleFs {
        endpoint: Endpoint,
        session: Option<DoubleSession>,
    }

    fn unsupported<T: Send + 'static>(
        operation: &str,
    ) -> BoxFuture<'static, crate::vfs::FsResult<T>> {
        let error = FsError::Unsupported {
            operation: operation.into(),
            reason: "not used by the session tests".into(),
        };
        Box::pin(async move { Err(error) })
    }

    fn sftp_error(error: openssh_sftp_client::Error) -> FsError {
        match error {
            openssh_sftp_client::Error::BackgroundTaskFailure(_) => FsError::Disconnected,
            other => FsError::Io {
                path: PathBuf::new(),
                message: other.to_string(),
            },
        }
    }

    impl Fs for DoubleFs {
        fn endpoint(&self) -> &Endpoint {
            &self.endpoint
        }

        fn capabilities(&self) -> crate::vfs::Capabilities {
            crate::vfs::Capabilities {
                posix_rename: false,
                copy_within: false,
                symlink: false,
                trash: false,
            }
        }

        fn home(&self) -> BoxFuture<'_, crate::vfs::FsResult<PathBuf>> {
            Box::pin(async move {
                match &self.session {
                    Some(session) => session
                        .sftp
                        .fs()
                        .canonicalize(".")
                        .await
                        .map_err(sftp_error),
                    None => Ok(PathBuf::from("/local-home")),
                }
            })
        }

        fn canonicalize<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, crate::vfs::FsResult<PathBuf>> {
            unsupported("canonicalize")
        }

        fn list<'a>(
            &'a self,
            dir: &'a Path,
        ) -> BoxFuture<'a, crate::vfs::FsResult<Vec<crate::vfs::Entry>>> {
            Box::pin(async move {
                use futures_util::TryStreamExt;
                let Some(session) = &self.session else {
                    return Err(FsError::NotFound { path: dir.into() });
                };
                let entries: Vec<_> = session
                    .sftp
                    .fs()
                    .open_dir(dir)
                    .await
                    .map_err(sftp_error)?
                    .read_dir()
                    .try_collect()
                    .await
                    .map_err(sftp_error)?;
                Ok(entries
                    .into_iter()
                    .filter(|entry| {
                        entry.filename() != Path::new(".") && entry.filename() != Path::new("..")
                    })
                    .map(|entry| crate::vfs::Entry {
                        name: entry.filename().as_os_str().to_os_string(),
                        kind: crate::vfs::EntryKind::File,
                        size: None,
                        mtime: None,
                        mode: None,
                    })
                    .collect())
            })
        }

        fn stat<'a>(
            &'a self,
            _: &'a Path,
        ) -> BoxFuture<'a, crate::vfs::FsResult<crate::vfs::Entry>> {
            unsupported("stat")
        }

        fn lstat<'a>(
            &'a self,
            _: &'a Path,
        ) -> BoxFuture<'a, crate::vfs::FsResult<crate::vfs::Entry>> {
            unsupported("lstat")
        }

        fn mkdir<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("mkdir")
        }

        fn rename<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
        ) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("rename")
        }

        fn remove_file<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("remove_file")
        }

        fn remove_dir<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("remove_dir")
        }

        fn trash<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("trash")
        }

        fn chmod<'a>(&'a self, _: &'a Path, _: u32) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("chmod")
        }

        fn set_times<'a>(
            &'a self,
            _: &'a Path,
            _: i64,
            _: i64,
        ) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("set_times")
        }

        fn read_link<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, crate::vfs::FsResult<PathBuf>> {
            unsupported("read_link")
        }

        fn symlink<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
        ) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("symlink")
        }

        fn open_read<'a>(
            &'a self,
            _: &'a Path,
            _: u64,
        ) -> BoxFuture<'a, crate::vfs::FsResult<crate::vfs::FsReader>> {
            unsupported("open_read")
        }

        fn open_write<'a>(
            &'a self,
            _: &'a Path,
            _: crate::vfs::WriteMode,
        ) -> BoxFuture<'a, crate::vfs::FsResult<crate::vfs::FsWriter>> {
            unsupported("open_write")
        }

        fn copy_within<'a>(
            &'a self,
            _: &'a Path,
            _: &'a Path,
        ) -> BoxFuture<'a, crate::vfs::FsResult<()>> {
            unsupported("copy_within")
        }
    }

    impl SftpChannel for DoubleFs {
        fn closed(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                if let Some(session) = &self.session {
                    // Either outcome of `wait` means the child is gone.
                    let ended = session.child.lock().await.wait().await;
                    drop(ended);
                }
            })
        }
    }

    const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

    struct Harness {
        sessions: Sessions,
        events: tokio::sync::mpsc::UnboundedReceiver<BgEvent>,
        _root: tempfile::TempDir,
    }

    impl Harness {
        fn new(client: SshClient) -> Harness {
            let settings = Settings {
                reconnect_base: Duration::from_millis(20),
                reconnect_cap: Duration::from_millis(50),
                ..Settings::default()
            };
            Harness::with_settings(client, &settings)
        }

        fn with_settings(client: SshClient, settings: &Settings) -> Harness {
            // Short and under /tmp so the socket path fits sun_path.
            let root = tempfile::Builder::new()
                .prefix("scpto-t")
                .tempdir_in("/tmp")
                .unwrap();
            let (sender, events) = tokio::sync::mpsc::unbounded_channel();
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc::getuid() };
            let sessions = Sessions::with_base(
                client,
                settings,
                sender,
                TestBackends::new(),
                &root.path().join("u"),
                uid,
            )
            .unwrap();
            Harness {
                sessions,
                events,
                _root: root,
            }
        }

        async fn next(&mut self) -> BgEvent {
            tokio::time::timeout(EVENT_TIMEOUT, self.events.recv())
                .await
                .expect("no event in time")
                .expect("event channel open")
        }

        fn control_dir(&self) -> PathBuf {
            self.sessions.shared.control_dir.clone()
        }
    }

    fn remote(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.into(),
            sudo: false,
        }
    }

    fn client_with_config(dir: &Path, config: &str) -> SshClient {
        let file = dir.join("ssh_config");
        std::fs::write(&file, config).unwrap();
        SshClient {
            program: PathBuf::from("ssh"),
            config_file: Some(file),
        }
    }

    #[tokio::test]
    async fn sudo_password_is_dropped_unless_the_endpoint_asked_for_it() {
        let local = TestBackends::new().local();
        let statuses = [
            Status::Down(Error::Connect(ConnectError::AuthFailed {
                detail: "fixture authentication failed".into(),
            })),
            Status::Down(Error::Connect(ConnectError::ConnectionLost)),
            Status::Down(Error::Sudo(SudoError::NotInSudoers)),
            Status::Down(Error::Sudo(SudoError::Timeout)),
            Status::Pending,
            Status::Ready(Channels {
                browse: Arc::clone(&local),
                transfer: local,
                home: PathBuf::from("/"),
                tools: Arc::default(),
            }),
        ];
        for status in statuses {
            // A missing, finished, or still-running task must not authorize
            // a password reply when the status did not ask for one.
            for task_state in [None, Some(false), Some(true)] {
                let mut harness = Harness::new(SshClient {
                    program: PathBuf::from("true"),
                    config_file: None,
                });
                let endpoint = Endpoint::Remote {
                    host: "unsolicited-password".into(),
                    sudo: true,
                };
                let task = match task_state {
                    None => None,
                    Some(false) => Some(tokio::spawn(std::future::pending())),
                    Some(true) => {
                        let mut task = tokio::spawn(async {});
                        (&mut task).await.unwrap();
                        Some(task)
                    }
                };
                let task_id = task.as_ref().map(JoinHandle::id);
                lock(&harness.sessions.shared.endpoints).insert(
                    endpoint.clone(),
                    Slot {
                        state: Arc::new(SlotState {
                            status: watch::Sender::new(status.clone()),
                            retry_now: Notify::new(),
                            lost: AtomicBool::new(false),
                        }),
                        task,
                    },
                );
                harness.sessions.sudo_password(
                    endpoint.clone(),
                    Secret::new(zeroize::Zeroizing::new("unsolicited".into())),
                );
                {
                    let slots = lock(&harness.sessions.shared.endpoints);
                    let slot = &slots[&endpoint];
                    assert_eq!(slot.task.as_ref().map(JoinHandle::id), task_id);
                    let current = slot.state.status.borrow();
                    assert_eq!(
                        std::mem::discriminant(&*current),
                        std::mem::discriminant(&status)
                    );
                    if let (Status::Down(actual), Status::Down(expected)) = (&*current, &status) {
                        assert_eq!(actual, expected);
                    }
                }
                assert!(lock(&harness.sessions.shared.masters).is_empty());
                assert!(matches!(
                    harness.events.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ));
                harness.sessions.shutdown().await;
            }
        }
    }

    #[tokio::test]
    async fn sudo_password_replies_continue_after_required_and_wrong_password() {
        let mut harness = Harness::new(SshClient {
            program: PathBuf::from("true"),
            config_file: None,
        });
        let endpoint = Endpoint::Remote {
            host: "password-retry".into(),
            sudo: true,
        };
        harness.sessions.connect(endpoint.clone());
        assert!(matches!(
            harness.next().await,
            BgEvent::SudoPasswordRequired(got) if got == endpoint
        ));
        for _ in 0..2 {
            harness.sessions.sudo_password(
                endpoint.clone(),
                Secret::new(zeroize::Zeroizing::new("wrong".into())),
            );
            assert!(matches!(
                harness.next().await,
                BgEvent::ConnectFailed {
                    endpoint: got,
                    error: Error::Sudo(SudoError::WrongPassword),
                } if got == endpoint
            ));
        }
        harness.sessions.shutdown().await;
    }

    async fn reply_before_terminal_task_finishes(reason: StartReason) {
        // The existing backend exercises sudo's password exchange. `true`
        // makes the master check succeed without contacting an SSH host.
        let mut harness = Harness::new(SshClient {
            program: PathBuf::from("true"),
            config_file: None,
        });
        let endpoint = Endpoint::Remote {
            host: "terminal-reply".into(),
            sudo: true,
        };
        let state = Arc::new(SlotState {
            status: watch::Sender::new(Status::Pending),
            retry_now: Notify::new(),
            lost: AtomicBool::new(false),
        });
        let expects_password_failure = matches!(reason, StartReason::Password(_));
        let initial_password = if expects_password_failure {
            None
        } else {
            Some(Secret::new(zeroize::Zeroizing::new("wrong".into())))
        };
        let (finish, finishing) = tokio::sync::oneshot::channel();
        let task = {
            let shared = Arc::clone(&harness.sessions.shared);
            let endpoint = endpoint.clone();
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                shared.run_endpoint(endpoint, state, initial_password).await;
                // Hold open the real terminal-event/JoinHandle completion
                // window without depending on executor scheduling.
                finishing.await.unwrap();
            })
        };
        lock(&harness.sessions.shared.endpoints).insert(
            endpoint.clone(),
            Slot {
                state,
                task: Some(task),
            },
        );
        match harness.next().await {
            BgEvent::SudoPasswordRequired(got) if expects_password_failure => {
                assert_eq!(got, endpoint);
            }
            BgEvent::ConnectFailed {
                endpoint: got,
                error: Error::Sudo(SudoError::WrongPassword),
            } if !expects_password_failure => assert_eq!(got, endpoint),
            other => panic!("unexpected first event {other:?}"),
        }
        assert!(!lock(&harness.sessions.shared.endpoints)[&endpoint]
            .task
            .as_ref()
            .unwrap()
            .is_finished());
        match reason {
            StartReason::Connect => harness.sessions.connect(endpoint.clone()),
            StartReason::Reconnect => harness.sessions.reconnect(endpoint.clone()),
            StartReason::Password(password) => {
                harness.sessions.sudo_password(endpoint.clone(), password);
            }
        }
        match harness.next().await {
            BgEvent::ConnectFailed {
                endpoint: got,
                error: Error::Sudo(SudoError::WrongPassword),
            } if expects_password_failure => assert_eq!(got, endpoint),
            BgEvent::SudoPasswordRequired(got) if !expects_password_failure => {
                assert_eq!(got, endpoint);
            }
            other => panic!("unexpected reply event {other:?}"),
        }
        finish.send(()).unwrap();
        harness.sessions.shutdown().await;
    }

    #[tokio::test]
    async fn sudo_password_reply_precedes_terminal_task_completion() {
        reply_before_terminal_task_finishes(StartReason::Password(Secret::new(
            zeroize::Zeroizing::new("wrong".into()),
        )))
        .await;
    }

    #[tokio::test]
    async fn connect_reply_precedes_terminal_task_completion() {
        reply_before_terminal_task_finishes(StartReason::Connect).await;
    }

    #[tokio::test]
    async fn reconnect_reply_precedes_terminal_task_completion() {
        reply_before_terminal_task_finishes(StartReason::Reconnect).await;
    }

    #[tokio::test]
    async fn unreachable_host_is_retried_with_backoff_and_reconnect_resets_it() {
        let config_dir = tempfile::tempdir().unwrap();
        let client = client_with_config(
            config_dir.path(),
            "Host refused\n  HostName 127.0.0.1\n  Port 1\n",
        );
        let mut harness = Harness::new(client);
        let endpoint = remote("refused");
        harness.sessions.connect(endpoint.clone());
        let mut seen = Vec::new();
        for _ in 0..3 {
            match harness.next().await {
                BgEvent::Reconnecting {
                    endpoint: got,
                    attempt,
                    delay,
                    error,
                } => {
                    assert_eq!(got, endpoint);
                    assert!(error.vpn_hint_eligible());
                    assert!(matches!(
                        error,
                        ConnectError::Unreachable {
                            reason: UnreachableReason::Refused,
                            ..
                        }
                    ));
                    seen.push((attempt, delay.as_millis()));
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(seen, [(1, 20), (2, 40), (3, 50)]);

        // `fs` waits while the endpoint retries.
        let waiting = {
            let shared = Arc::clone(&harness.sessions.shared);
            let endpoint = endpoint.clone();
            tokio::spawn(async move { shared.fs(&endpoint, FsChannel::Browse).await.map(drop) })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished());

        // Disconnecting fails the waiting `fs` call.
        harness.sessions.disconnect(&endpoint);
        let result = tokio::time::timeout(EVENT_TIMEOUT, waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, Err(not_connected(&endpoint)));
        assert!(matches!(
            harness.sessions.fs(&endpoint, FsChannel::Transfer).await,
            Err(Error::Connect(ConnectError::Other { .. }))
        ));
        let dir = harness.control_dir();
        harness.sessions.shutdown().await;
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn reconnect_retries_at_once_and_resets_the_backoff() {
        let config_dir = tempfile::tempdir().unwrap();
        let client = client_with_config(
            config_dir.path(),
            "Host refused\n  HostName 127.0.0.1\n  Port 1\n",
        );
        let slow = Duration::from_secs(5);
        let settings = Settings {
            reconnect_base: slow,
            reconnect_cap: slow,
            ..Settings::default()
        };
        let mut harness = Harness::with_settings(client, &settings);
        let endpoint = remote("refused");
        harness.sessions.connect(endpoint.clone());
        assert!(matches!(
            harness.next().await,
            BgEvent::Reconnecting { attempt: 1, .. }
        ));
        // Without `reconnect` the next attempt would come after 5 s and be
        // announced as attempt 2.
        let started = tokio::time::Instant::now();
        harness.sessions.reconnect(endpoint.clone());
        assert!(matches!(
            harness.next().await,
            BgEvent::Reconnecting { attempt: 1, .. }
        ));
        assert!(started.elapsed() < slow);
        // `connect` while retrying does not start a second task.
        harness.sessions.connect(endpoint.clone());
        assert!(
            tokio::time::timeout(Duration::from_millis(300), harness.events.recv())
                .await
                .is_err()
        );
        harness.sessions.shutdown().await;
    }

    #[tokio::test]
    async fn missing_ssh_program_fails_without_retry() {
        let mut harness = Harness::new(SshClient {
            program: PathBuf::from("/nonexistent/ssh"),
            config_file: None,
        });
        let endpoint = remote("anything");
        harness.sessions.connect(endpoint.clone());
        match harness.next().await {
            BgEvent::ConnectFailed {
                endpoint: got,
                error: Error::Connect(ConnectError::SshUnavailable { .. }),
            } => assert_eq!(got, endpoint),
            other => panic!("unexpected event {other:?}"),
        }
        assert!(matches!(
            harness.sessions.fs(&endpoint, FsChannel::Browse).await,
            Err(Error::Connect(ConnectError::SshUnavailable { .. }))
        ));
    }

    #[tokio::test]
    async fn local_endpoint_uses_the_local_backend_and_has_no_ssh() {
        let mut harness = Harness::new(SshClient::default());
        harness.sessions.connect(Endpoint::Local);
        match harness.next().await {
            BgEvent::Connected { endpoint, home } => {
                assert_eq!(endpoint, Endpoint::Local);
                assert_eq!(home, PathBuf::from("/local-home"));
            }
            other => panic!("unexpected event {other:?}"),
        }
        let browse = harness
            .sessions
            .fs(&Endpoint::Local, FsChannel::Browse)
            .await
            .unwrap();
        let transfer = harness
            .sessions
            .fs(&Endpoint::Local, FsChannel::Transfer)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&browse, &transfer));
        assert!(harness
            .sessions
            .spawn_remote(&Endpoint::Local, &lines(&["true"]))
            .await
            .is_err());
        assert!(harness
            .sessions
            .ssh_command(&Endpoint::Local, false)
            .await
            .is_err());
        assert!(harness.sessions.tools(&Endpoint::Local).await.is_err());
        assert!(harness
            .sessions
            .foreground_master_command(&Endpoint::Local)
            .is_err());
    }

    #[test]
    fn commands_set_master_multiplexing_and_auth_options() {
        let root = tempfile::Builder::new()
            .prefix("scpto-t")
            .tempdir_in("/tmp")
            .unwrap();
        let (sender, _events) = tokio::sync::mpsc::unbounded_channel();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let sessions = Sessions::with_base(
            SshClient {
                program: PathBuf::from("/usr/bin/ssh"),
                config_file: Some(PathBuf::from("/cfg")),
            },
            &Settings::default(),
            sender,
            TestBackends::new(),
            &root.path().join("u"),
            uid,
        )
        .unwrap();
        let shared = &sessions.shared;
        let control = format!("ControlPath={}/%C", shared.control_dir.display());
        let strings = |args: Vec<OsString>| -> Vec<String> {
            args.into_iter()
                .map(|arg| arg.into_string().unwrap())
                .collect()
        };
        let common: Vec<String> = COMMON_OPTIONS
            .iter()
            .flat_map(|option| ["-o".to_string(), (*option).to_string()])
            .collect();

        let mut expected = lines(&["-F", "/cfg", "-o", &control, "-o", "ControlMaster=no"]);
        expected.extend(common.iter().cloned());
        expected.extend(lines(&["-o", "BatchMode=yes"]));
        assert_eq!(strings(shared.mux_args(false)), expected);

        let interactive = lines(&[
            "-F",
            "/cfg",
            "-o",
            &control,
            "-o",
            "ControlMaster=no",
            "-o",
            "RemoteCommand=none",
            "-t",
        ]);
        assert_eq!(strings(shared.mux_args(true)), interactive);

        let mut master = lines(&[
            "-F",
            "/cfg",
            "-o",
            &control,
            "-o",
            "ControlMaster=yes",
            "-o",
            "ControlPersist=10m",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        master.extend(common.iter().cloned());
        let mut background = master.clone();
        background.extend(lines(&["-o", "BatchMode=yes", "-N", "-f", "--", "web"]));
        assert_eq!(strings(shared.master_args("web", true)), background);

        let mut foreground = master;
        foreground.extend(lines(&["-N", "-f", "--", "web"]));
        let command = sessions.foreground_master_command(&remote("web")).unwrap();
        let std_command = command.as_std();
        assert_eq!(std_command.get_program(), "/usr/bin/ssh");
        let args: Vec<String> = std_command
            .get_args()
            .map(|arg| arg.to_str().unwrap().to_string())
            .collect();
        assert_eq!(args, foreground);
        assert!(shared.master("web").started.load(Ordering::SeqCst));
    }

    // ----- Docker sshd fixtures (SCPTO_IT=1) -------------------------------

    /// The fixture ssh_config, or `None` (test skipped) unless `SCPTO_IT=1`.
    /// `SCPTO_IT_SSH_CONFIG` overrides the default `target/it/ssh_config`,
    /// for worktrees that share the fixtures of another checkout.
    fn it_config() -> Option<PathBuf> {
        if std::env::var_os("SCPTO_IT").as_deref() != Some(OsStr::new("1")) {
            eprintln!("skipped: set SCPTO_IT=1 to run against the docker sshd fixtures");
            return None;
        }
        let path = std::env::var_os("SCPTO_IT_SSH_CONFIG").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("target/it/ssh_config"),
            PathBuf::from,
        );
        assert!(
            path.is_file(),
            "{} missing: run tests/it/fixtures/up.sh",
            path.display()
        );
        Some(path)
    }

    fn it_client(config: &Path) -> SshClient {
        SshClient {
            program: PathBuf::from("ssh"),
            config_file: Some(config.to_path_buf()),
        }
    }

    async fn expect_connected(harness: &mut Harness, endpoint: &Endpoint) -> PathBuf {
        match harness.next().await {
            BgEvent::Connected {
                endpoint: got,
                home,
            } if &got == endpoint => home,
            other => panic!("expected Connected for {endpoint}, got {other:?}"),
        }
    }

    async fn master_survives_kill_and_both_channels_reconnect(image: &str) {
        let Some(config) = it_config() else {
            return;
        };
        let mut harness = Harness::new(it_client(&config));
        let host = format!("it-{image}-plain");
        let endpoint = remote(&host);
        harness.sessions.connect(endpoint.clone());
        let home = expect_connected(&mut harness, &endpoint).await;
        assert_eq!(home, PathBuf::from("/home/plain"));

        // Connecting again answers Connected again.
        harness.sessions.connect(endpoint.clone());
        assert_eq!(expect_connected(&mut harness, &endpoint).await, home);

        let browse = harness
            .sessions
            .fs(&endpoint, FsChannel::Browse)
            .await
            .unwrap();
        let transfer = harness
            .sessions
            .fs(&endpoint, FsChannel::Transfer)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&browse, &transfer), "two separate channels");
        let seed = home.join("seed");
        let (a, b) = tokio::join!(browse.list(&seed), transfer.list(&seed));
        let names = |entries: Vec<crate::vfs::Entry>| {
            let mut names: Vec<OsString> = entries.into_iter().map(|entry| entry.name).collect();
            names.sort();
            names
        };
        let a = names(a.unwrap());
        assert!(a.contains(&OsString::from("hello.txt")), "{a:?}");
        assert_eq!(a, names(b.unwrap()));

        // Tools are probed once per connection.
        let tools = harness.sessions.tools(&endpoint).await.unwrap();
        assert_eq!(
            tools,
            RemoteTools {
                tar: true,
                rsync_ok: true
            }
        );

        // Remote commands keep every byte of their words.
        let argv = lines(&["printf", "%s|", "a b", "it's", "$HOME"]);
        let mut child = harness
            .sessions
            .spawn_remote(&endpoint, &argv)
            .await
            .unwrap();
        child.stdin.shutdown().await.unwrap();
        let mut output = String::new();
        child.stdout.read_to_string(&mut output).await.unwrap();
        assert!(child.child.wait().await.unwrap().success());
        assert_eq!(output, "a b|it's|$HOME|");

        // The rsync `-e` argv reaches the host through the master.
        let mut argv = harness
            .sessions
            .ssh_command(&endpoint, false)
            .await
            .unwrap();
        argv.extend([host.clone(), "echo mux-ok".to_string()]);
        let output = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "mux-ok\n");

        // Kill the master from outside, as a network drop would.
        let control = format!("ControlPath={}/%C", harness.control_dir().display());
        let killed = tokio::process::Command::new("ssh")
            .arg("-F")
            .arg(&config)
            .args(["-o", &control, "-O", "exit", "--", &host])
            .output()
            .await
            .unwrap();
        assert!(killed.status.success(), "{killed:?}");
        match harness.next().await {
            BgEvent::Disconnected {
                endpoint: got,
                error: ConnectError::ConnectionLost,
            } => assert_eq!(got, endpoint),
            other => panic!("expected Disconnected, got {other:?}"),
        }
        match harness.next().await {
            BgEvent::Reconnecting {
                attempt: 1,
                error: ConnectError::ConnectionLost,
                ..
            } => {}
            other => panic!("expected Reconnecting, got {other:?}"),
        }
        match harness.next().await {
            BgEvent::Reconnected(got) => assert_eq!(got, endpoint),
            other => panic!("expected Reconnected, got {other:?}"),
        }
        assert!(matches!(browse.home().await, Err(FsError::Disconnected)));
        let browse_again = harness
            .sessions
            .fs(&endpoint, FsChannel::Browse)
            .await
            .unwrap();
        let transfer_again = harness
            .sessions
            .fs(&endpoint, FsChannel::Transfer)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&browse, &browse_again));
        assert_eq!(browse_again.home().await.unwrap(), home);
        assert!(transfer_again.list(&seed).await.is_ok());
        assert_eq!(harness.sessions.tools(&endpoint).await.unwrap(), tools);

        let dir = harness.control_dir();
        harness.sessions.shutdown().await;
        assert!(!dir.exists(), "control dir removed after teardown");
    }

    #[tokio::test]
    async fn it_modern_master_two_channels_and_reconnect() {
        master_survives_kill_and_both_channels_reconnect("modern").await;
    }

    #[tokio::test]
    async fn it_legacy_master_two_channels_and_reconnect() {
        master_survives_kill_and_both_channels_reconnect("legacy").await;
    }

    #[tokio::test]
    async fn it_side_effect_config_is_neutralized() {
        let Some(config) = it_config() else {
            return;
        };
        let mut harness = Harness::new(it_client(&config));
        let endpoint = remote("it-modern-sideeffects");
        harness.sessions.connect(endpoint.clone());
        expect_connected(&mut harness, &endpoint).await;
        harness.sessions.shutdown().await;
    }

    #[tokio::test]
    async fn it_auth_and_host_key_failures_need_the_foreground() {
        let Some(config) = it_config() else {
            return;
        };
        let fixtures = config.parent().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let key = scratch.path().join("stranger");
        let keygen = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(keygen.success());
        // The legacy fixture (OpenSSH 7.9) predates sshd's PerSourcePenalties
        // (9.8). Failing logins against the modern fixture would get
        // 127.0.0.1 refused for a while, breaking concurrent tests.
        let text = format!(
            "Host badkey\n  HostName 127.0.0.1\n  Port 22222\n  User plain\n  IdentityFile {key}\n  \
             IdentitiesOnly yes\n  UserKnownHostsFile {known}\n  StrictHostKeyChecking yes\n\n\
             Host unknownkey\n  HostName 127.0.0.1\n  Port 22222\n  User plain\n  IdentityFile {fixture_key}\n  \
             IdentitiesOnly yes\n  UserKnownHostsFile /dev/null\n  StrictHostKeyChecking yes\n",
            key = key.display(),
            known = fixtures.join("known_hosts").display(),
            fixture_key = fixtures.join("id_ed25519").display(),
        );
        let mut harness = Harness::new(client_with_config(scratch.path(), &text));
        for (host, auth) in [("badkey", true), ("unknownkey", false)] {
            let endpoint = remote(host);
            harness.sessions.connect(endpoint.clone());
            match harness.next().await {
                BgEvent::InteractiveAuthRequired {
                    endpoint: got,
                    error,
                } => {
                    assert_eq!(got, endpoint);
                    assert!(error.needs_foreground_auth());
                    if auth {
                        assert!(
                            matches!(error, ConnectError::AuthFailed { .. }),
                            "{error:?}"
                        );
                    } else {
                        assert!(
                            matches!(error, ConnectError::HostKeyRejected { .. }),
                            "{error:?}"
                        );
                    }
                }
                other => panic!("unexpected event {other:?}"),
            }
            assert!(matches!(
                harness.sessions.fs(&endpoint, FsChannel::Browse).await,
                Err(Error::Connect(error)) if error.needs_foreground_auth()
            ));
        }
        harness.sessions.shutdown().await;
    }

    #[tokio::test]
    async fn it_sudo_endpoint_asks_for_the_password_and_shares_one_server() {
        let Some(config) = it_config() else {
            return;
        };
        let mut harness = Harness::new(it_client(&config));
        let endpoint = Endpoint::Remote {
            host: "it-modern-sudopw".into(),
            sudo: true,
        };
        let secret = |text: &str| Secret::new(zeroize::Zeroizing::new(text.to_string()));
        harness.sessions.connect(endpoint.clone());
        assert!(matches!(
            harness.next().await,
            BgEvent::SudoPasswordRequired(got) if got == endpoint
        ));
        harness
            .sessions
            .sudo_password(endpoint.clone(), secret("wrong"));
        assert!(matches!(
            harness.next().await,
            BgEvent::ConnectFailed {
                error: Error::Sudo(SudoError::WrongPassword),
                ..
            }
        ));
        harness
            .sessions
            .sudo_password(endpoint.clone(), secret("scpto-test-pw"));
        let home = expect_connected(&mut harness, &endpoint).await;
        assert_eq!(home, PathBuf::from("/home/sudopw"));
        let browse = harness
            .sessions
            .fs(&endpoint, FsChannel::Browse)
            .await
            .unwrap();
        let transfer = harness
            .sessions
            .fs(&endpoint, FsChannel::Transfer)
            .await
            .unwrap();
        assert!(
            Arc::ptr_eq(&browse, &transfer),
            "one sudo sftp-server per endpoint"
        );

        // After a drop the password is not retained: it is asked again.
        let control = format!("ControlPath={}/%C", harness.control_dir().display());
        let killed = tokio::process::Command::new("ssh")
            .arg("-F")
            .arg(&config)
            .args(["-o", &control, "-O", "exit", "--", "it-modern-sudopw"])
            .output()
            .await
            .unwrap();
        assert!(killed.status.success(), "{killed:?}");
        assert!(matches!(harness.next().await, BgEvent::Disconnected { .. }));
        assert!(matches!(
            harness.next().await,
            BgEvent::Reconnecting { attempt: 1, .. }
        ));
        assert!(matches!(
            harness.next().await,
            BgEvent::SudoPasswordRequired(got) if got == endpoint
        ));
        harness
            .sessions
            .sudo_password(endpoint.clone(), secret("scpto-test-pw"));
        assert!(matches!(
            harness.next().await,
            BgEvent::Reconnected(got) if got == endpoint
        ));
        harness.sessions.shutdown().await;
    }
}
