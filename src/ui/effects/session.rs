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
//! The sudo password only moves: `sudo_password` hands the `Secret` to the
//! session, which passes it to `sudo::start` and drops (zeroizes) it. This
//! module never formats, copies, or reports it.
//!
//! # Public API
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

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tokio::sync::mpsc::UnboundedSender;

use super::{AfterForeground, Ctx, Foreground};
use crate::connections::{Connections, RemoteChild};
use crate::error::{ConnectError, Error, SudoError};
use crate::hosts;
use crate::session::{classify_ssh_exit, Backends, SftpChannel};
use crate::sudo;
use crate::ui::action::{BgEvent, Secret};
use crate::vfs::local::LocalFs;
use crate::vfs::sftp::SftpFs;
use crate::vfs::{Endpoint, Fs};

/// Starts connecting `endpoint` (`Effect::Connect`). The session answers
/// with `Connected`, `Reconnecting`, `ConnectFailed`,
/// `InteractiveAuthRequired`, or `SudoPasswordRequired`; `Local` answers
/// `Connected { Local, home }`.
pub fn connect(ctx: &Ctx, endpoint: Endpoint) {
    ctx.sessions.connect(endpoint);
}

/// Continues a sudo start with the typed password (`Effect::SudoPassword`).
/// The session drops (zeroizes) the password once the attempt is over.
pub fn sudo_password(ctx: &Ctx, endpoint: Endpoint, password: Secret) {
    ctx.sessions.sudo_password(endpoint, password);
}

/// Retries `endpoint` now, resetting its backoff (`Effect::Reconnect`).
pub fn reconnect(ctx: &Ctx, endpoint: Endpoint) {
    ctx.sessions.reconnect(endpoint);
}

/// Closes `endpoint`'s channels (`Effect::Disconnect`); the host's master
/// closes once no endpoint of that host remains.
pub fn disconnect(ctx: &Ctx, endpoint: &Endpoint) {
    ctx.sessions.disconnect(endpoint);
}

/// Resolves `alias` with `ssh -G` in the background and sends
/// `BgEvent::HostResolved` (`Effect::ResolveHost`).
pub fn resolve_host(ctx: &Ctx, alias: String) {
    let ssh = ctx.ssh.clone();
    let events = ctx.events.clone();
    tokio::spawn(async move {
        let result = hosts::resolve(&ssh, &alias).await;
        emit(&events, BgEvent::HostResolved { alias, result });
    });
}

/// The foreground master command for `Effect::InteractiveAuth`, run by the
/// runtime with the TUI suspended. When the command cannot be built (for
/// example for `Local`), sends `ConnectFailed` and returns `None`.
pub fn interactive_auth(ctx: &Ctx, endpoint: Endpoint) -> Option<Foreground> {
    match ctx.sessions.foreground_master_command(&endpoint) {
        Ok(command) => Some(Foreground {
            command,
            then: AfterForeground::InteractiveAuth(endpoint),
        }),
        Err(error) => {
            emit(&ctx.events, BgEvent::ConnectFailed { endpoint, error });
            None
        }
    }
}

/// Continues after the foreground master command of
/// [`interactive_auth`] exited: on success the session connects through the
/// new master; otherwise sends `ConnectFailed`. OpenSSH has already
/// explained a failure on the terminal, so only the exit status is
/// classified.
pub fn interactive_auth_finished(ctx: &Ctx, endpoint: Endpoint, status: io::Result<ExitStatus>) {
    let error = match status {
        Ok(status) if status.success() => {
            ctx.sessions.connect(endpoint);
            return;
        }
        Ok(status) => classify_ssh_exit(status.into(), &[]),
        Err(error) => ConnectError::SshUnavailable {
            message: error.to_string(),
        },
    };
    emit(
        &ctx.events,
        BgEvent::ConnectFailed {
            endpoint,
            error: Error::Connect(error),
        },
    );
}

/// Sends `event` to the app.
fn emit(events: &UnboundedSender<BgEvent>, event: BgEvent) {
    if let Err(unsent) = events.send(event) {
        // The app has exited; nobody is left to report to.
        drop(unsent);
    }
}

/// The production [`Backends`]: `vfs::local`, `vfs::sftp`, and `sudo`.
/// Passed to `Sessions::new` by startup.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProductionBackends;

impl Backends for ProductionBackends {
    fn local(&self) -> Arc<dyn Fs> {
        Arc::new(LocalFs::new())
    }

    fn start_sftp(
        &self,
        endpoint: Endpoint,
        channel: RemoteChild,
    ) -> BoxFuture<'_, Result<Arc<dyn SftpChannel>, ConnectError>> {
        Box::pin(async move {
            let fs = SftpFs::start(endpoint, channel).await?;
            Ok(Arc::new(fs) as Arc<dyn SftpChannel>)
        })
    }

    fn locate_sftp_server<'a>(
        &'a self,
        connections: &'a dyn Connections,
        endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<PathBuf, SudoError>> {
        Box::pin(sudo::locate_sftp_server(connections, endpoint))
    }

    fn start_sudo<'a>(
        &'a self,
        connections: &'a dyn Connections,
        endpoint: &'a Endpoint,
        sftp_server: &'a Path,
        password: Option<&'a Secret>,
    ) -> BoxFuture<'a, Result<RemoteChild, SudoError>> {
        Box::pin(sudo::start(connections, endpoint, sftp_server, password))
    }
}

impl SftpChannel for SftpFs {
    fn closed(&self) -> BoxFuture<'_, ()> {
        // The inherent `SftpFs::closed`, named by path because this trait
        // method has the same name.
        Box::pin(SftpFs::closed(self))
    }
}

/// Test support shared by the executor tests of this directory.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use ratatui_image::picker::Picker;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::sync::{Mutex, MutexGuard};

    use super::ProductionBackends;
    use crate::connections::{Connections, SshClient};
    use crate::session::Sessions;
    use crate::settings::Settings;
    use crate::state::PersistedState;
    use crate::ui::action::BgEvent;
    use crate::ui::effects::{Ctx, Tasks};

    /// How long a test waits for one event before failing.
    const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

    /// `Sessions::new` recreates this process's ControlPath directory
    /// (`/tmp/scpto-<uid>/<pid>`) and `Sessions::shutdown` removes it, so
    /// two live `Sessions` in one test process would delete each other's
    /// master sockets. Every harness holds this lock for its lifetime.
    static SESSIONS: Mutex<()> = Mutex::const_new(());

    /// A production `Ctx` (real `Sessions` with `ProductionBackends`) whose
    /// events, from executors and sessions alike, arrive in `events`.
    pub(crate) struct Harness {
        /// The context under test; its fields may be adjusted before use.
        pub(crate) ctx: Ctx,
        /// Every `BgEvent` sent through `ctx.events`.
        pub(crate) events: UnboundedReceiver<BgEvent>,
        /// Scratch space: `state/` is the initial `ctx.state_dir`.
        pub(crate) scratch: tempfile::TempDir,
        _exclusive: MutexGuard<'static, ()>,
    }

    impl Harness {
        /// A harness whose ssh invocations use `ssh`.
        pub(crate) async fn new(ssh: SshClient) -> Harness {
            let exclusive = SESSIONS.lock().await;
            let scratch = tempfile::tempdir().expect("scratch directory");
            let settings = Settings::default();
            let (sender, events) = tokio::sync::mpsc::unbounded_channel();
            let sessions = Arc::new(
                Sessions::new(
                    ssh.clone(),
                    &settings,
                    sender.clone(),
                    Arc::new(ProductionBackends),
                )
                .expect("sessions"),
            );
            // The engine is not under test; its commands go nowhere.
            let (engine, _) = tokio::sync::mpsc::unbounded_channel();
            let ctx = Ctx {
                connections: Arc::clone(&sessions) as Arc<dyn Connections>,
                sessions,
                events: sender,
                settings: Arc::new(settings),
                ssh,
                state_dir: Some(scratch.path().join("state")),
                persisted: Arc::new(tokio::sync::Mutex::new(PersistedState::default())),
                engine,
                image_picker: Arc::new(Picker::halfblocks()),
                edit_temp: scratch.path().join("edit"),
                requests: Arc::new(Tasks::new()),
                timers: Arc::new(Tasks::new()),
                syncs: Arc::new(Tasks::new()),
            };
            Harness {
                ctx,
                events,
                scratch,
                _exclusive: exclusive,
            }
        }

        /// The next event; fails the test after [`EVENT_TIMEOUT`].
        pub(crate) async fn next(&mut self) -> BgEvent {
            tokio::time::timeout(EVENT_TIMEOUT, self.events.recv())
                .await
                .expect("no event in time")
                .expect("event channel open")
        }

        /// Fails the test if an event arrives within `wait`.
        pub(crate) async fn expect_quiet(&mut self, wait: Duration) {
            if let Ok(event) = tokio::time::timeout(wait, self.events.recv()).await {
                panic!("unexpected event {event:?}");
            }
        }

        /// The ControlPath directory of this process's sessions.
        pub(crate) fn control_dir() -> PathBuf {
            // SAFETY: getuid has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            crate::session::control_dir(uid, std::process::id())
        }

        /// Tears the sessions down (closing masters, removing the
        /// ControlPath directory) before the lock is released.
        pub(crate) async fn finish(self) {
            self.ctx.sessions.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use zeroize::Zeroizing;

    use super::testing::Harness;
    use super::*;
    use crate::connections::{FsChannel, SshClient};
    use crate::error::UnreachableReason;
    use crate::hosts::ResolvedHost;

    fn remote(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.into(),
            sudo: false,
        }
    }

    /// The real `ssh` with a generated configuration file in `dir`.
    fn client_with_config(dir: &Path, config: &str) -> SshClient {
        let file = dir.join("ssh_config");
        std::fs::write(&file, config).unwrap();
        SshClient {
            program: PathBuf::from("ssh"),
            config_file: Some(file),
        }
    }

    #[tokio::test]
    async fn connecting_local_answers_connected_with_the_local_home() {
        let mut harness = Harness::new(SshClient::default()).await;
        let home = LocalFs::new().home().await.unwrap();
        connect(&harness.ctx, Endpoint::Local);
        match harness.next().await {
            BgEvent::Connected {
                endpoint,
                home: got,
            } => {
                assert_eq!(endpoint, Endpoint::Local);
                assert_eq!(got, home);
            }
            other => panic!("expected Connected, got {other:?}"),
        }
        reconnect(&harness.ctx, Endpoint::Local);
        assert!(matches!(
            harness.next().await,
            BgEvent::Connected { endpoint: Endpoint::Local, home: got } if got == home
        ));
        // Disconnecting the local endpoint has nothing to close.
        disconnect(&harness.ctx, &Endpoint::Local);
        harness.expect_quiet(Duration::from_millis(100)).await;
        harness.finish().await;
    }

    #[test]
    fn production_backends_serve_the_local_file_system() {
        assert_eq!(ProductionBackends.local().endpoint(), &Endpoint::Local);
    }

    #[tokio::test]
    async fn interactive_auth_of_local_reports_connect_failed() {
        let mut harness = Harness::new(SshClient::default()).await;
        assert!(interactive_auth(&harness.ctx, Endpoint::Local).is_none());
        match harness.next().await {
            BgEvent::ConnectFailed { endpoint, error } => {
                assert_eq!(endpoint, Endpoint::Local);
                assert!(!error.to_string().is_empty());
            }
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
        harness.finish().await;
    }

    #[tokio::test]
    async fn interactive_auth_runs_the_master_without_batch_mode() {
        let config = tempfile::tempdir().unwrap();
        let client = client_with_config(config.path(), "Host web\n  HostName 127.0.0.1\n");
        let harness = Harness::new(client).await;
        let endpoint = remote("web");
        let foreground = interactive_auth(&harness.ctx, endpoint.clone()).expect("command");
        assert!(matches!(
            &foreground.then,
            AfterForeground::InteractiveAuth(got) if *got == endpoint
        ));
        let args: Vec<&OsStr> = foreground.command.as_std().get_args().collect();
        assert!(!args.contains(&OsStr::new("BatchMode=yes")), "{args:?}");
        assert!(args.contains(&OsStr::new("ControlMaster=yes")), "{args:?}");
        assert_eq!(args.last(), Some(&OsStr::new("web")));
        harness.finish().await;
    }

    #[tokio::test]
    async fn a_failed_foreground_master_reports_its_exit_classification() {
        let mut harness = Harness::new(SshClient::default()).await;
        let endpoint = remote("web");
        let status = ExitStatus::from_raw(255 << 8);
        interactive_auth_finished(&harness.ctx, endpoint.clone(), Ok(status));
        match harness.next().await {
            BgEvent::ConnectFailed {
                endpoint: got,
                error: Error::Connect(error),
            } => {
                assert_eq!(got, endpoint);
                assert_eq!(
                    error,
                    classify_ssh_exit(crate::error::ExitStatus::Code(255), &[])
                );
            }
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
        harness.finish().await;
    }

    #[tokio::test]
    async fn a_foreground_master_that_did_not_start_reports_ssh_unavailable() {
        let mut harness = Harness::new(SshClient::default()).await;
        let endpoint = remote("web");
        let error = io::Error::from(io::ErrorKind::NotFound);
        let message = error.to_string();
        interactive_auth_finished(&harness.ctx, endpoint.clone(), Err(error));
        match harness.next().await {
            BgEvent::ConnectFailed {
                endpoint: got,
                error:
                    Error::Connect(ConnectError::SshUnavailable {
                        message: got_message,
                    }),
            } => {
                assert_eq!(got, endpoint);
                assert_eq!(got_message, message);
            }
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
        harness.finish().await;
    }

    #[tokio::test]
    async fn a_successful_foreground_master_continues_connecting() {
        let config = tempfile::tempdir().unwrap();
        let client = client_with_config(
            config.path(),
            "Host refused\n  HostName 127.0.0.1\n  Port 1\n",
        );
        let mut harness = Harness::new(client).await;
        let endpoint = remote("refused");
        interactive_auth_finished(&harness.ctx, endpoint.clone(), Ok(ExitStatus::from_raw(0)));
        // The continuation is an ordinary connect: an unreachable host is
        // announced as the first retry.
        match harness.next().await {
            BgEvent::Reconnecting {
                endpoint: got,
                attempt: 1,
                error:
                    ConnectError::Unreachable {
                        reason: UnreachableReason::Refused,
                        ..
                    },
                ..
            } => assert_eq!(got, endpoint),
            other => panic!("expected Reconnecting, got {other:?}"),
        }
        harness.finish().await;
    }

    #[tokio::test]
    async fn resolve_host_reports_the_effective_hostname_and_user() {
        let config = tempfile::tempdir().unwrap();
        let client = client_with_config(
            config.path(),
            "Host web\n  HostName web.example.test\n  User alice\n",
        );
        let mut harness = Harness::new(client).await;
        resolve_host(&harness.ctx, "web".into());
        match harness.next().await {
            BgEvent::HostResolved { alias, result } => {
                assert_eq!(alias, "web");
                assert_eq!(
                    result.unwrap(),
                    ResolvedHost {
                        hostname: "web.example.test".into(),
                        user: Some("alice".into()),
                    }
                );
            }
            other => panic!("expected HostResolved, got {other:?}"),
        }
        harness.finish().await;
    }

    #[tokio::test]
    async fn resolve_host_reports_a_failure() {
        let scratch = tempfile::tempdir().unwrap();
        let client = SshClient {
            program: scratch.path().join("no-such-ssh"),
            config_file: None,
        };
        let mut harness = Harness::new(client).await;
        resolve_host(&harness.ctx, "web".into());
        match harness.next().await {
            BgEvent::HostResolved { alias, result } => {
                assert_eq!(alias, "web");
                assert!(
                    matches!(result, Err(Error::CommandFailed { .. })),
                    "{result:?}"
                );
            }
            other => panic!("expected HostResolved, got {other:?}"),
        }
        harness.finish().await;
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

    async fn seed_names(fs: &dyn Fs, home: &Path) -> Vec<OsString> {
        let mut names: Vec<OsString> = fs
            .list(&home.join("seed"))
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        names.sort();
        names
    }

    /// Closes the master of `host` from outside, as a network drop would.
    async fn kill_master(config: &Path, host: &str) {
        let control = format!("ControlPath={}/%C", Harness::control_dir().display());
        let killed = tokio::process::Command::new("ssh")
            .arg("-F")
            .arg(config)
            .args(["-o", &control, "-O", "exit", "--", host])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(killed.status.success(), "{killed:?}");
    }

    #[tokio::test]
    async fn it_connect_yields_listing_channels_that_notice_a_drop() {
        let Some(config) = it_config() else {
            return;
        };
        let mut harness = Harness::new(it_client(&config)).await;
        let host = "it-modern-plain";
        let endpoint = remote(host);
        connect(&harness.ctx, endpoint.clone());
        let home = expect_connected(&mut harness, &endpoint).await;
        assert_eq!(home, PathBuf::from("/home/plain"));

        // Both channels are real SFTP sessions through `ProductionBackends`.
        let browse = harness
            .ctx
            .connections
            .fs(&endpoint, FsChannel::Browse)
            .await
            .unwrap();
        let transfer = harness
            .ctx
            .connections
            .fs(&endpoint, FsChannel::Transfer)
            .await
            .unwrap();
        let names = seed_names(browse.as_ref(), &home).await;
        assert!(names.contains(&OsString::from("hello.txt")), "{names:?}");
        assert_eq!(seed_names(transfer.as_ref(), &home).await, names);

        // `SftpChannel::closed` of the production channel ends the
        // connection, and the session brings it back.
        kill_master(&config, host).await;
        match harness.next().await {
            BgEvent::Disconnected {
                endpoint: got,
                error: ConnectError::ConnectionLost,
            } => assert_eq!(got, endpoint),
            other => panic!("expected Disconnected, got {other:?}"),
        }
        assert!(matches!(
            harness.next().await,
            BgEvent::Reconnecting { attempt: 1, .. }
        ));
        assert!(matches!(
            harness.next().await,
            BgEvent::Reconnected(got) if got == endpoint
        ));
        let browse = harness
            .ctx
            .connections
            .fs(&endpoint, FsChannel::Browse)
            .await
            .unwrap();
        assert_eq!(seed_names(browse.as_ref(), &home).await, names);

        disconnect(&harness.ctx, &endpoint);
        assert!(harness
            .ctx
            .connections
            .fs(&endpoint, FsChannel::Browse)
            .await
            .is_err());
        harness.finish().await;
    }

    #[tokio::test]
    async fn it_interactive_auth_continuation_connects_through_the_new_master() {
        let Some(config) = it_config() else {
            return;
        };
        let mut harness = Harness::new(it_client(&config)).await;
        let endpoint = remote("it-modern-plain");
        let mut foreground = interactive_auth(&harness.ctx, endpoint.clone()).expect("command");
        let AfterForeground::InteractiveAuth(then) = foreground.then else {
            panic!("the continuation is InteractiveAuth");
        };
        // The fixture key needs no prompt; null stdin keeps a regression
        // from waiting on the test's terminal.
        let status = foreground.command.stdin(Stdio::null()).status().await;
        interactive_auth_finished(&harness.ctx, then, status);
        let home = expect_connected(&mut harness, &endpoint).await;
        let browse = harness
            .ctx
            .connections
            .fs(&endpoint, FsChannel::Browse)
            .await
            .unwrap();
        assert!(seed_names(browse.as_ref(), &home)
            .await
            .contains(&OsString::from("hello.txt")));
        harness.finish().await;
    }

    #[tokio::test]
    async fn it_sudo_password_reaches_a_root_sftp_server() {
        let Some(config) = it_config() else {
            return;
        };
        let mut harness = Harness::new(it_client(&config)).await;
        let endpoint = Endpoint::Remote {
            host: "it-modern-sudopw".into(),
            sudo: true,
        };
        let wrong = "scpto-wrong-pw-7f3a";
        let right = "scpto-test-pw";
        let secret = |text: &str| Secret::new(Zeroizing::new(text.to_string()));
        let mut seen = Vec::new();

        connect(&harness.ctx, endpoint.clone());
        let event = harness.next().await;
        seen.push(format!("{event:?}"));
        assert!(matches!(event, BgEvent::SudoPasswordRequired(ref got) if *got == endpoint));

        sudo_password(&harness.ctx, endpoint.clone(), secret(wrong));
        let event = harness.next().await;
        seen.push(format!("{event:?}"));
        if let BgEvent::ConnectFailed { error, .. } = &event {
            seen.push(error.to_string());
        }
        assert!(
            matches!(
                event,
                BgEvent::ConnectFailed {
                    error: Error::Sudo(SudoError::WrongPassword),
                    ..
                }
            ),
            "{event:?}"
        );

        sudo_password(&harness.ctx, endpoint.clone(), secret(right));
        let event = harness.next().await;
        seen.push(format!("{event:?}"));
        let BgEvent::Connected { endpoint: got, .. } = event else {
            panic!("expected Connected, got {event:?}");
        };
        assert_eq!(got, endpoint);

        // The server runs as root: it can create a directory in a
        // root-owned one.
        let fs = harness
            .ctx
            .connections
            .fs(&endpoint, FsChannel::Browse)
            .await
            .unwrap();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/srv/rootonly/fx-session-{}-{nanos}",
            std::process::id()
        ));
        fs.mkdir(&dir).await.unwrap();
        fs.remove_dir(&dir).await.unwrap();

        for text in &seen {
            assert!(!text.contains(wrong), "password leaked into {text}");
            assert!(!text.contains(right), "password leaked into {text}");
        }
        harness.finish().await;
    }
}
