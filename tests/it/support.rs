//! Real sessions and engine plumbing shared by the integration scenarios.
use scpto::{
    connections::{Connections, FsChannel, SshClient},
    session::Sessions,
    settings::Settings,
    state::PartialsFile,
    transfer::{
        engine::{self, EngineConfig},
        model::*,
    },
    ui::{
        action::BgEvent,
        effects::{session::ProductionBackends, transfer::TarStreams},
    },
    vfs::{Endpoint, Fs, Location, WriteMode},
};
use std::{
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

/// Maximum wait for one observable background result.
pub(super) const LIMIT: Duration = Duration::from_secs(90);

// Sessions owns a process-scoped socket directory. Separate test processes
// preserve production semantics while allowing libtest to run cases in parallel.
/// Runs an opt-in scenario in its own process and Tokio runtime.
pub(super) fn isolated<F: Future<Output = ()>>(name: &str, body: impl FnOnce() -> F) {
    if std::env::var("SCPTO_IT").as_deref() != Ok("1") {
        eprintln!("SKIP {name}: set SCPTO_IT=1 for Docker SSH fixtures");
        return;
    }
    if std::env::var("SCPTO_IT_CHILD").as_deref() != Ok(name) {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--include-ignored", "--nocapture"])
            .env("SCPTO_IT_CHILD", name)
            .status()
            .unwrap();
        assert!(status.success(), "integration subprocess {name}: {status}");
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(240), body())
                .await
                .expect("integration scenario exceeded 240 seconds");
        });
}

/// Locates the generated fixture config without reading the user config.
pub(super) fn config() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/it/ssh_config");
    assert!(
        path.is_file(),
        "run tests/it/fixtures/up.sh first: {}",
        path.display()
    );
    path
}

/// Names one of the documented fixture users.
pub(super) fn endpoint(image: &str, user: &str, sudo: bool) -> Endpoint {
    Endpoint::Remote {
        host: format!("it-{image}-{user}"),
        sudo,
    }
}

/// Associates a path with its endpoint.
pub(super) fn location(endpoint: &Endpoint, path: &Path) -> Location {
    Location {
        endpoint: endpoint.clone(),
        path: path.into(),
    }
}

/// Owns real sessions, isolated local storage, and remote cleanup roots.
pub(super) struct Fixture {
    /// The production connection provider.
    pub sessions: Arc<Sessions>,
    /// Connection lifecycle events.
    pub events: mpsc::UnboundedReceiver<BgEvent>,
    /// Private local data, edit files, and partial records.
    pub temp: tempfile::TempDir,
    roots: Vec<(Endpoint, PathBuf)>,
    closed: bool,
}

impl Fixture {
    /// Starts production sessions using only the fixture configuration.
    pub fn new() -> Self {
        let (tx, events) = mpsc::unbounded_channel();
        let settings = Settings {
            reconnect_base: Duration::from_millis(100),
            reconnect_cap: Duration::from_millis(500),
            ..Settings::default()
        };
        let sessions = Arc::new(
            Sessions::new(
                SshClient {
                    program: "ssh".into(),
                    config_file: Some(config()),
                },
                &settings,
                tx,
                Arc::new(ProductionBackends),
            )
            .unwrap(),
        );
        Self {
            sessions,
            events,
            temp: tempfile::tempdir().unwrap(),
            roots: Vec::new(),
            closed: false,
        }
    }

    /// Waits for one session event with a deadline.
    pub async fn next(&mut self) -> BgEvent {
        tokio::time::timeout(LIMIT, self.events.recv())
            .await
            .unwrap()
            .unwrap()
    }

    /// Connects and requires a successful connection event.
    pub async fn connect(&mut self, ep: &Endpoint) {
        self.sessions.connect(ep.clone());
        match self.next().await {
            BgEvent::Connected { endpoint, .. } => assert_eq!(&endpoint, ep),
            event => panic!("connect {ep}: {event:?}"),
        }
    }

    /// Fetches the current transfer channel.
    pub async fn fs(&self, ep: &Endpoint) -> Arc<dyn Fs> {
        self.sessions.fs(ep, FsChannel::Transfer).await.unwrap()
    }

    /// Creates and registers a unique remote directory for cleanup.
    pub async fn root(&mut self, ep: &Endpoint, label: &str) -> PathBuf {
        let path = PathBuf::from(format!("/tmp/scpto-it-{label}-{}", std::process::id()));
        self.fs(ep).await.mkdir(&path).await.unwrap();
        self.roots.push((ep.clone(), path.clone()));
        path
    }

    /// Terminates only this process's master for the supplied endpoint.
    pub async fn kill_master(&self, ep: &Endpoint) {
        let Endpoint::Remote { host, .. } = ep else {
            panic!("remote required")
        };
        let argv = self.sessions.ssh_command(ep, false).await.unwrap();
        let output = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .args(["-O", "exit", host])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "master exit: {output:?}");
    }

    /// Requires disconnect, retry, and successful reconnection events.
    pub async fn reconnected(&mut self, ep: &Endpoint) {
        let mut disconnected = false;
        let mut reconnecting = false;
        loop {
            match self.next().await {
                BgEvent::Disconnected { endpoint, .. } if &endpoint == ep => disconnected = true,
                BgEvent::Reconnecting { endpoint, .. } if &endpoint == ep => reconnecting = true,
                BgEvent::Reconnected(endpoint) if &endpoint == ep => break,
                event => panic!("unexpected reconnect event: {event:?}"),
            }
        }
        assert!(
            disconnected && reconnecting,
            "connection lifecycle was incomplete"
        );
    }

    /// Removes registered remote roots and shuts down every owned master.
    pub async fn finish(mut self) {
        for (ep, path) in &self.roots {
            scpto::vfs::remove_tree(self.fs(ep).await.as_ref(), path)
                .await
                .unwrap();
        }
        self.roots.clear();
        self.sessions.shutdown().await;
        self.closed = true;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // On assertion failure, remove only this test's registered roots. Normal
        // teardown uses Fs above; this path still works after a broken session.
        for (ep, path) in &self.roots {
            let Endpoint::Remote { host, sudo } = ep else {
                unreachable!()
            };
            let command = format!(
                "{}rm -rf -- {}",
                if *sudo { "sudo -n " } else { "" },
                scpto::connections::quote(path.to_str().unwrap()).unwrap()
            );
            match Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-F"])
                .arg(config())
                .args([host, &command])
                .output()
            {
                Ok(output) if output.status.success() => {}
                result => eprintln!("cleanup {} failed: {result:?}", path.display()),
            }
        }
        if !self.closed {
            // An assertion must not leave this test's masters running. Every
            // fixture is used inside isolated()'s multithreaded runtime.
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.sessions.shutdown());
            });
        }
    }
}

/// Writes and commits bytes through an Fs channel.
pub(super) async fn write(fs: &dyn Fs, path: &Path, bytes: &[u8]) {
    let mut file = fs
        .open_write(path, WriteMode::Truncate { permissions: 0o600 })
        .await
        .unwrap();
    file.write_all(bytes).await.unwrap();
    file.shutdown().await.unwrap();
}

/// Reads a file through an Fs channel.
pub(super) async fn read(fs: &dyn Fs, path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    fs.open_read(path, 0)
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .await
        .unwrap();
    bytes
}

/// Runs the real transfer actor with persisted partials and production tar.
pub(super) struct Engine {
    /// Commands sent to the production transfer actor.
    pub commands: TransferCommandTx,
    /// Observable transfer results.
    pub events: TransferEventRx,
    /// Persistent partial records in the fixture's temporary directory.
    pub partials: Arc<PartialsFile>,
    task: tokio::task::JoinHandle<()>,
}

impl Engine {
    /// Spawns an engine with the requested automatic-tar threshold.
    pub fn new(f: &Fixture, tar_threshold: usize) -> Self {
        let (commands, rx) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::channel(256);
        let partials = Arc::new(PartialsFile::open(Some(f.temp.path().join("state"))).unwrap());
        let task = tokio::spawn(engine::run(
            EngineConfig {
                concurrency: 2,
                tar_threshold,
                mtime_tolerance: 0,
            },
            f.sessions.clone(),
            partials.clone(),
            Arc::new(TarStreams),
            rx,
            tx,
        ));
        Self {
            commands,
            events,
            partials,
            task,
        }
    }

    /// Receives an engine event, failing immediately on transfer errors.
    pub async fn next(&mut self) -> TransferEvent {
        let event = tokio::time::timeout(LIMIT, self.events.recv())
            .await
            .unwrap()
            .unwrap();
        match &event {
            TransferEvent::JobFailed { .. }
            | TransferEvent::ItemChanged {
                state: ItemState::Failed { .. },
                ..
            } => panic!("engine failure: {event:?}"),
            _ => {}
        }
        event
    }

    /// Submits a job and answers its conflicts with the specified policy.
    pub async fn run(
        &mut self,
        request: JobRequest,
        choice: Option<ConflictChoice>,
    ) -> (JobSummary, Vec<TransferEvent>) {
        self.commands
            .send(TransferCommand::Submit(request))
            .unwrap();
        self.complete(choice).await
    }

    /// Collects results until completion, rejecting unexpected prompts.
    pub async fn complete(
        &mut self,
        choice: Option<ConflictChoice>,
    ) -> (JobSummary, Vec<TransferEvent>) {
        let mut seen = Vec::new();
        loop {
            let event = self.next().await;
            match &event {
                TransferEvent::ConflictRaised(conflict) => self
                    .commands
                    .send(TransferCommand::ResolveConflict {
                        item: conflict.item,
                        decision: ConflictDecision {
                            choice: choice.expect("unexpected conflict"),
                            apply_to_all: false,
                        },
                    })
                    .unwrap(),
                TransferEvent::TarConflictRaised { .. } => {
                    panic!("unexpected tar conflict: {event:?}")
                }
                TransferEvent::JobFinished { summary, .. } => {
                    assert_eq!(summary.failed, 0);
                    return (*summary, seen);
                }
                _ => {}
            }
            seen.push(event);
        }
    }

    /// Closes the command channel and joins the actor.
    pub async fn finish(self) {
        drop(self.commands);
        tokio::time::timeout(LIMIT, self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

/// Builds a copy request into an existing destination directory.
pub(super) fn job(
    id: u64,
    src: &Endpoint,
    dst: &Endpoint,
    sources: Vec<PathBuf>,
    directory: &Path,
) -> JobRequest {
    JobRequest {
        id: JobId(id),
        kind: JobKind::Copy,
        source: src.clone(),
        destination: dst.clone(),
        targets: JobTargets::IntoDirectory {
            sources,
            directory: directory.into(),
        },
        preset: None,
    }
}
