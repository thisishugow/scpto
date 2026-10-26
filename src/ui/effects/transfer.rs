//! Executes `Effect::Transfer` by forwarding commands to the engine, starts
//! the engine, relays engine events as `BgEvent::Transfer`, and holds the
//! production `TarTransfer` adapter.
//!
//! # Behavior
//!
//! - `start_engine` creates the engine channels (events bounded at 256),
//!   spawns `transfer::engine::run` with `EngineConfig::from_settings`, the
//!   given connections and partial store, and `Arc::new(TarStreams)`, plus a
//!   relay task that sends every `TransferEvent` as `BgEvent::Transfer`
//!   until either side closes. The returned sender is `Ctx::engine`;
//!   dropping every clone stops the engine.
//! - `send` acknowledges submitted jobs after the engine channel accepts
//!   them. A closed channel reports the failure to the application.
//! - `TarStreams` implements `transfer::engine::TarTransfer` by calling
//!   `transfer::tar::upload` / `transfer::tar::download` with the same
//!   arguments.
//!
//! # Public API
//!
//! ```text
//! pub fn start_engine(
//!     settings: &Settings,
//!     connections: Arc<dyn Connections>,
//!     partials: Arc<dyn PartialStore>,
//!     events: UnboundedSender<BgEvent>,
//! ) -> TransferCommandTx;
//! pub fn send(ctx: &Ctx, command: TransferCommand);
//! pub struct TarStreams;
//! impl TarTransfer for TarStreams { /* delegates to transfer::tar */ }
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::connections::Connections;
use crate::error::{Error, FsError};
use crate::settings::Settings;
use crate::state::PartialStore;
use crate::transfer::engine::{self, EngineConfig, TarTransfer};
use crate::transfer::model::{TransferCommand, TransferCommandTx};
use crate::transfer::tar;
use crate::ui::action::BgEvent;
use crate::ui::effects::Ctx;
use crate::vfs::Endpoint;

/// Capacity of the engine's event channel. Bounded so a stalled app slows
/// the engine down instead of buffering progress without limit.
const EVENT_CAPACITY: usize = 256;

/// Starts the transfer engine and the relay of its events to `events` as
/// `BgEvent::Transfer`. Returns the command sender for `Ctx::engine`; the
/// engine stops once every clone of it is dropped, or once the app's event
/// receiver is gone. Must be called inside a Tokio runtime.
pub fn start_engine(
    settings: &Settings,
    connections: Arc<dyn Connections>,
    partials: Arc<dyn PartialStore>,
    events: UnboundedSender<BgEvent>,
) -> TransferCommandTx {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::channel(EVENT_CAPACITY);
    tokio::spawn(engine::run(
        EngineConfig::from_settings(settings),
        connections,
        partials,
        Arc::new(TarStreams),
        command_rx,
        event_tx,
    ));
    tokio::spawn(async move {
        // Ends when the engine stops (its sender is dropped) or when the app
        // exited; returning drops the receiver, which stops the engine too.
        while let Some(event) = event_rx.recv().await {
            if events.send(BgEvent::Transfer(event)).is_err() {
                break;
            }
        }
    });
    command_tx
}

/// Forwards `command` to the engine (`Effect::Transfer`).
pub fn send(ctx: &Ctx, command: TransferCommand) {
    let job = match &command {
        TransferCommand::Submit(request) => Some(request.id),
        _ => None,
    };
    let result = forward(&ctx.engine, command);
    let event = match (job, result) {
        (Some(job), result) => BgEvent::TransferSubmitted { job, result },
        (None, Err(error)) => BgEvent::TransferCommandFailed(error),
        (None, Ok(())) => return,
    };
    // A closed event receiver means the application has already exited.
    let _ = ctx.events.send(event);
}

/// Sends a command, reporting a closed engine channel to the caller.
pub(super) fn forward(engine: &TransferCommandTx, command: TransferCommand) -> Result<(), FsError> {
    engine.send(command).map_err(|_| FsError::Io {
        path: "transfer queue".into(),
        message: "Transfer engine stopped; the command was not submitted".into(),
    })
}

/// The product's tar streams: delegates to `transfer::tar`.
pub struct TarStreams;

impl TarTransfer for TarStreams {
    fn upload<'a>(
        &'a self,
        connections: &'a dyn Connections,
        sources: &'a [PathBuf],
        remote: &'a Endpoint,
        remote_dir: &'a Path,
        progress: &'a (dyn Fn(u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<Vec<(PathBuf, Error)>, Error>> {
        Box::pin(tar::upload(
            connections,
            sources,
            remote,
            remote_dir,
            progress,
        ))
    }

    fn download<'a>(
        &'a self,
        connections: &'a dyn Connections,
        remote: &'a Endpoint,
        remote_dir: &'a Path,
        names: &'a [OsString],
        local_dir: &'a Path,
        progress: &'a (dyn Fn(u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<Vec<(PathBuf, Error)>, Error>> {
        Box::pin(tar::download(
            connections,
            remote,
            remote_dir,
            names,
            local_dir,
            progress,
        ))
    }
}

/// Connections shared by the executor tests of `transfer` and `sync`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use tokio::io::AsyncBufReadExt;
    use tokio::process::Command;
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::connections::{
        join_command, rsync_version_ok, Connections, FsChannel, RemoteChild, RemoteTools,
        StderrTail,
    };
    use crate::error::{ConnectError, Error};
    use crate::ui::action::BgEvent;
    use crate::vfs::local::LocalFs;
    use crate::vfs::sftp::SftpFs;
    use crate::vfs::{Endpoint, Fs};

    /// How long a test waits for one event before failing.
    const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

    /// A non-sudo remote endpoint.
    pub fn remote(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.to_owned(),
            sudo: false,
        }
    }

    /// The next event, failing the test when none arrives in time.
    pub async fn next_event(events: &mut UnboundedReceiver<BgEvent>) -> BgEvent {
        tokio::time::timeout(EVENT_TIMEOUT, events.recv())
            .await
            .expect("an event in time")
            .expect("the event channel is open")
    }

    /// Spawns `command` as a background child with piped stdio and a
    /// drained stderr, like the session does.
    fn spawn_child(mut command: Command) -> RemoteChild {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let mut lines = tokio::io::BufReader::new(child.stderr.take().expect("stderr")).lines();
        let stderr: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
        let tail = Arc::clone(&stderr);
        tokio::spawn(async move {
            while let Some(line) = lines.next_line().await.expect("read stderr") {
                tail.lock().expect("tail lock").push_back(line);
            }
        });
        RemoteChild {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    /// What [`TestConnections::tools`] answers.
    pub enum Tools {
        /// This answer, at once.
        Ready(Result<RemoteTools, Error>),
        /// Never answers (a probe that hangs).
        Never,
    }

    /// Hermetic connections: a fixed file system per endpoint, remote
    /// commands run on this machine (the local `tar` stands in for the
    /// remote one), and scripted tool probes.
    pub struct TestConnections {
        fs: HashMap<Endpoint, Arc<dyn Fs>>,
        tools: Tools,
        /// Tool probes so far.
        pub probes: AtomicUsize,
    }

    impl TestConnections {
        /// Connections serving `fs` with `tools`.
        pub fn new(fs: Vec<(Endpoint, Arc<dyn Fs>)>, tools: Tools) -> TestConnections {
            TestConnections {
                fs: fs.into_iter().collect(),
                tools,
                probes: AtomicUsize::new(0),
            }
        }
    }

    impl Connections for TestConnections {
        fn fs<'a>(
            &'a self,
            endpoint: &'a Endpoint,
            _channel: FsChannel,
        ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
            let fs = self
                .fs
                .get(endpoint)
                .unwrap_or_else(|| panic!("no file system for {endpoint}"));
            let fs = Arc::clone(fs);
            Box::pin(async move { Ok(fs) })
        }

        fn spawn_remote<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            argv: &'a [String],
        ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
            let mut command = Command::new(&argv[0]);
            // Keeps macOS bsdtar from adding AppleDouble entries.
            command.args(&argv[1..]).env("COPYFILE_DISABLE", "1");
            Box::pin(async move { Ok(spawn_child(command)) })
        }

        fn ssh_command<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _interactive: bool,
        ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
            Box::pin(async {
                Err(Error::Connect(ConnectError::Other {
                    detail: "hermetic tests have no ssh".to_owned(),
                }))
            })
        }

        fn tools<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match &self.tools {
                    Tools::Ready(answer) => answer.clone(),
                    Tools::Never => std::future::pending().await,
                }
            })
        }
    }

    /// The docker fixtures' ssh_config: `SCPTO_IT_SSH_CONFIG`, else
    /// `target/it/ssh_config` written by `tests/it/fixtures/up.sh`.
    fn fixture_config() -> PathBuf {
        let path = std::env::var_os("SCPTO_IT_SSH_CONFIG").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("target/it/ssh_config"),
            PathBuf::from,
        );
        assert!(
            path.is_file(),
            "{} missing: run tests/it/fixtures/up.sh",
            path.display()
        );
        path
    }

    /// Connections to the docker fixtures over plain `ssh -F <config>`:
    /// real SFTP channels (one per endpoint and channel), remote commands,
    /// and `ssh` for rsync's `-e`. The local endpoint is the local file
    /// system.
    pub struct FixtureConnections {
        config: PathBuf,
        channels: tokio::sync::Mutex<HashMap<(Endpoint, FsChannel), Arc<dyn Fs>>>,
    }

    impl FixtureConnections {
        /// Connections through the fixtures' ssh_config.
        pub fn new() -> FixtureConnections {
            FixtureConnections {
                config: fixture_config(),
                channels: tokio::sync::Mutex::new(HashMap::new()),
            }
        }

        fn ssh(&self) -> Vec<String> {
            vec![
                "ssh".to_owned(),
                "-F".to_owned(),
                self.config.to_str().expect("utf8 config path").to_owned(),
                "-o".to_owned(),
                "BatchMode=yes".to_owned(),
            ]
        }

        /// `ssh <options> [extra] <host>`.
        fn ssh_to(&self, endpoint: &Endpoint, extra: &[&str]) -> Command {
            let Endpoint::Remote { host, .. } = endpoint else {
                panic!("the fixtures are remote endpoints");
            };
            let ssh = self.ssh();
            let mut command = Command::new(&ssh[0]);
            command.args(&ssh[1..]).args(extra).arg(host);
            command
        }

        /// Runs `argv` remotely and returns its trimmed standard output.
        pub async fn run(&self, endpoint: &Endpoint, argv: &[&str]) -> String {
            let words: Vec<String> = argv.iter().map(|word| (*word).to_owned()).collect();
            let output = self
                .ssh_to(endpoint, &[])
                .arg(join_command(&words).expect("quotable"))
                .stdin(Stdio::null())
                .output()
                .await
                .expect("run ssh");
            assert!(output.status.success(), "{argv:?}: {output:?}");
            String::from_utf8(output.stdout)
                .expect("utf8 output")
                .trim_end()
                .to_owned()
        }
    }

    impl Connections for FixtureConnections {
        fn fs<'a>(
            &'a self,
            endpoint: &'a Endpoint,
            channel: FsChannel,
        ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
            Box::pin(async move {
                if endpoint.is_local() {
                    return Ok(Arc::new(LocalFs::new()) as Arc<dyn Fs>);
                }
                let mut channels = self.channels.lock().await;
                let key = (endpoint.clone(), channel);
                if let Some(fs) = channels.get(&key) {
                    return Ok(Arc::clone(fs));
                }
                let mut command = self.ssh_to(endpoint, &["-s"]);
                command.arg("sftp");
                let sftp = SftpFs::start(endpoint.clone(), spawn_child(command))
                    .await
                    .map_err(Error::Connect)?;
                let fs: Arc<dyn Fs> = Arc::new(sftp);
                channels.insert(key, Arc::clone(&fs));
                Ok(fs)
            })
        }

        fn spawn_remote<'a>(
            &'a self,
            endpoint: &'a Endpoint,
            argv: &'a [String],
        ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
            Box::pin(async move {
                let mut command = self.ssh_to(endpoint, &[]);
                command.arg(join_command(argv)?);
                Ok(spawn_child(command))
            })
        }

        fn ssh_command<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _interactive: bool,
        ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
            Box::pin(async move { Ok(self.ssh()) })
        }

        fn tools<'a>(
            &'a self,
            endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
            Box::pin(async move {
                let version = self.run(endpoint, &["rsync", "--version"]).await;
                Ok(RemoteTools {
                    // Both fixture images ship tar (tests/it/fixtures).
                    tar: true,
                    rsync_ok: rsync_version_ok(&version),
                })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use tokio::sync::mpsc::{self, UnboundedReceiver};

    use super::test_support::{next_event, remote, FixtureConnections, TestConnections, Tools};
    use super::*;
    use crate::connections::RemoteTools;
    use crate::state::PartialsFile;
    use crate::transfer::model::{
        JobId, JobKind, JobRequest, JobSummary, JobTargets, TransferEvent, TransferMode,
    };
    use crate::vfs::local::LocalFs;
    use crate::vfs::Fs;

    fn partials() -> Arc<dyn PartialStore> {
        Arc::new(PartialsFile::open(None).expect("in-memory partials"))
    }

    /// A "remote" endpoint that is this machine's file system, with `tar`.
    fn loopback(host: &str) -> Arc<dyn Connections> {
        let local: Arc<dyn Fs> = Arc::new(LocalFs::new());
        Arc::new(TestConnections::new(
            vec![(Endpoint::Local, Arc::clone(&local)), (remote(host), local)],
            Tools::Ready(Ok(RemoteTools {
                tar: true,
                rsync_ok: false,
            })),
        ))
    }

    fn copy_into(
        id: u64,
        source: Endpoint,
        destination: Endpoint,
        sources: &[&Path],
        directory: &Path,
    ) -> TransferCommand {
        TransferCommand::Submit(JobRequest {
            id: JobId(id),
            kind: JobKind::Copy,
            source,
            destination,
            targets: JobTargets::IntoDirectory {
                sources: sources.iter().map(|path| path.to_path_buf()).collect(),
                directory: directory.to_path_buf(),
            },
            preset: None,
        })
    }

    /// Relayed engine events until `job` ends: its mode and summary.
    async fn finish(
        events: &mut UnboundedReceiver<BgEvent>,
        job: JobId,
    ) -> (TransferMode, JobSummary) {
        let mut mode = None;
        loop {
            match next_event(events).await {
                BgEvent::Transfer(TransferEvent::JobCreated {
                    job: created,
                    mode: chosen,
                    ..
                }) if created == job => {
                    mode = Some(chosen);
                }
                BgEvent::Transfer(TransferEvent::JobFinished {
                    job: finished,
                    summary,
                }) if finished == job => {
                    return (mode.expect("JobCreated before JobFinished"), summary);
                }
                BgEvent::Transfer(TransferEvent::JobFailed { job: failed, error })
                    if failed == job =>
                {
                    panic!("job {job:?} failed: {error}");
                }
                BgEvent::Transfer(_) => {}
                other => panic!("unexpected event {other:?}"),
            }
        }
    }

    /// `root/tree` with two files in nested directories and a symlink.
    fn make_tree(root: &Path) -> std::path::PathBuf {
        let tree = root.join("tree");
        fs::create_dir_all(tree.join("sub")).expect("mkdir");
        fs::write(tree.join("a.txt"), b"alpha\n").expect("write");
        fs::write(
            tree.join("sub/b.bin"),
            (0..=255u8).cycle().take(70_000).collect::<Vec<u8>>(),
        )
        .expect("write");
        std::os::unix::fs::symlink("a.txt", tree.join("link")).expect("symlink");
        tree
    }

    fn assert_same_tree(expected: &Path, actual: &Path) {
        assert_eq!(
            fs::read(expected.join("a.txt")).expect("a"),
            fs::read(actual.join("a.txt")).expect("a")
        );
        assert_eq!(
            fs::read(expected.join("sub/b.bin")).expect("b"),
            fs::read(actual.join("sub/b.bin")).expect("b")
        );
        assert_eq!(
            fs::read_link(actual.join("link")).expect("link"),
            Path::new("a.txt")
        );
    }

    fn summary(done: usize) -> JobSummary {
        JobSummary {
            done,
            skipped: 0,
            failed: 0,
        }
    }

    #[tokio::test]
    async fn submitting_reports_acceptance_and_closed_channel_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut ctx, mut events) =
            crate::ui::effects::fs::test_support::ctx(loopback("box"), dir.path());
        let (engine, mut received) = mpsc::unbounded_channel();
        ctx.engine = engine;
        let command = copy_into(7, Endpoint::Local, remote("box"), &[dir.path()], dir.path());
        send(&ctx, command.clone());
        assert_eq!(received.recv().await, Some(command.clone()));
        assert!(matches!(
            next_event(&mut events).await,
            BgEvent::TransferSubmitted {
                job: JobId(7),
                result: Ok(())
            }
        ));
        drop(received);
        send(&ctx, command);
        assert!(matches!(
            next_event(&mut events).await,
            BgEvent::TransferSubmitted {
                job: JobId(7),
                result: Err(_)
            }
        ));
        send(&ctx, TransferCommand::ClearFinished);
        assert!(matches!(
            next_event(&mut events).await,
            BgEvent::TransferCommandFailed(_)
        ));
    }

    #[tokio::test]
    async fn per_file_jobs_run_and_their_events_are_relayed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = make_tree(dir.path());
        let target = dir.path().join("target");
        fs::create_dir(&target).expect("mkdir");
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let engine = start_engine(&Settings::default(), loopback("box"), partials(), events_tx);

        forward(
            &engine,
            copy_into(1, Endpoint::Local, remote("box"), &[&tree], &target),
        )
        .expect("engine accepted command");
        // The directories tree and sub, the files a.txt and sub/b.bin, the link.
        assert_eq!(
            finish(&mut events, JobId(1)).await,
            (TransferMode::PerFile, summary(5))
        );
        assert_same_tree(&tree, &target.join("tree"));
    }

    #[tokio::test]
    async fn tar_jobs_stream_through_tar_streams_both_ways() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = make_tree(dir.path());
        let uploaded = dir.path().join("uploaded");
        let downloaded = dir.path().join("downloaded");
        fs::create_dir(&uploaded).expect("mkdir");
        fs::create_dir(&downloaded).expect("mkdir");
        let settings = Settings {
            tar_threshold: 1,
            ..Settings::default()
        };
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let engine = start_engine(&settings, loopback("box"), partials(), events_tx);

        forward(
            &engine,
            copy_into(1, Endpoint::Local, remote("box"), &[&tree], &uploaded),
        )
        .expect("engine accepted command");
        let (mode, summary_up) = finish(&mut events, JobId(1)).await;
        assert_eq!(mode, TransferMode::Tar);
        assert_eq!(summary_up.failed, 0);
        assert_same_tree(&tree, &uploaded.join("tree"));

        forward(
            &engine,
            copy_into(
                2,
                remote("box"),
                Endpoint::Local,
                &[&uploaded.join("tree")],
                &downloaded,
            ),
        )
        .expect("engine accepted command");
        let (mode, summary_down) = finish(&mut events, JobId(2)).await;
        assert_eq!(mode, TransferMode::Tar);
        assert_eq!(summary_down.failed, 0);
        assert_same_tree(&tree, &downloaded.join("tree"));
    }

    #[tokio::test]
    async fn dropping_the_command_sender_stops_the_engine_and_the_relay() {
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let engine = start_engine(&Settings::default(), loopback("box"), partials(), events_tx);
        drop(engine);
        // The relay held the only event sender; it is gone once both ended.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv()).await;
        assert!(matches!(closed, Ok(None)), "{closed:?}");
    }

    #[tokio::test]
    async fn a_gone_app_stops_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = make_tree(dir.path());
        let (events_tx, events) = mpsc::unbounded_channel();
        let engine = start_engine(&Settings::default(), loopback("box"), partials(), events_tx);
        drop(events);
        // The job's first event ends the relay; the next one stops the engine,
        // which drops its command receiver.
        forward(
            &engine,
            copy_into(1, Endpoint::Local, remote("box"), &[&tree], dir.path()),
        )
        .expect("engine accepted command");
        tokio::time::timeout(std::time::Duration::from_secs(10), engine.closed())
            .await
            .expect("the engine stopped");
        // A stopped engine reports that the command was rejected.
        assert!(forward(&engine, TransferCommand::ClearFinished).is_err());
    }

    /// Upload and download round trips through the executor against
    /// it-modern-plain, in tar mode and per file.
    #[tokio::test]
    #[ignore = "needs the docker sshd fixtures (tests/it/fixtures/up.sh)"]
    async fn docker_round_trip_through_the_engine() {
        let connections = Arc::new(FixtureConnections::new());
        let endpoint = remote("it-modern-plain");
        let remote_dir =
            std::path::PathBuf::from(connections.run(&endpoint, &["mktemp", "-d"]).await);
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = make_tree(dir.path());
        for (index, tar_threshold) in [1, usize::MAX].into_iter().enumerate() {
            let expected = if tar_threshold == 1 {
                TransferMode::Tar
            } else {
                TransferMode::PerFile
            };
            let up = remote_dir.join(format!("up-{index}"));
            connections
                .run(&endpoint, &["mkdir", up.to_str().expect("utf8")])
                .await;
            let down = dir.path().join(format!("down-{index}"));
            fs::create_dir(&down).expect("mkdir");
            let settings = Settings {
                tar_threshold,
                ..Settings::default()
            };
            let (events_tx, mut events) = mpsc::unbounded_channel();
            let engine = start_engine(
                &settings,
                Arc::clone(&connections) as Arc<dyn Connections>,
                partials(),
                events_tx,
            );

            let upload = JobId(1);
            forward(
                &engine,
                copy_into(upload.0, Endpoint::Local, endpoint.clone(), &[&tree], &up),
            )
            .expect("engine accepted command");
            let (mode, summary_up) = finish(&mut events, upload).await;
            assert_eq!((mode, summary_up.failed), (expected, 0));

            let download = JobId(2);
            forward(
                &engine,
                copy_into(
                    download.0,
                    endpoint.clone(),
                    Endpoint::Local,
                    &[&up.join("tree")],
                    &down,
                ),
            )
            .expect("engine accepted command");
            let (mode, summary_down) = finish(&mut events, download).await;
            assert_eq!((mode, summary_down.failed), (expected, 0));
            assert_same_tree(&tree, &down.join("tree"));
        }
        connections
            .run(
                &endpoint,
                &["rm", "-rf", remote_dir.to_str().expect("utf8")],
            )
            .await;
    }
}
