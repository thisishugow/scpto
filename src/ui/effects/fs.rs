//! Executes `Effect::ListDir` and `Effect::FsOp` against the browsing
//! channel (`FsChannel::Browse`) of each endpoint, including recursive
//! permanent deletes.
//!
//! # Behavior
//!
//! - `list_dir` runs under `ctx.requests` (so `CancelRequest` abandons it):
//!   `Connections::fs(endpoint, Browse)` then `Fs::list`, answered with
//!   `Listed`; a failure to get the file system is the listing's error
//!   (`FsError::Disconnected` for connection errors).
//! - `fs_op` answers `FsOpFinished` with one `FsError` per failed path:
//!   `MakeDir` → `Fs::mkdir`; `Rename` → `Fs::lstat(to)` first and
//!   `FsError::AlreadyExists` when the target exists (a rename never
//!   replaces), else `Fs::rename`; `Delete` with `DeleteMethod::Trash` →
//!   `Fs::trash` per path (failures stay `FsError::TrashFailed` so the app
//!   can offer a permanent delete), with `DeleteMethod::Permanent` →
//!   `vfs::remove_tree` per path; `Chmod` → `Fs::chmod` per path.
//!
//! # Public API
//!
//! ```text
//! pub fn list_dir(ctx: &Ctx, request: RequestId, location: Location);
//! pub fn fs_op(ctx: &Ctx, request: RequestId, endpoint: Endpoint, op: FsOp);
//! ```

use std::path::{Path, PathBuf};

use crate::connections::{Connections, FsChannel};
use crate::error::{Error, FsError};
use crate::ui::action::{BgEvent, DeleteMethod, FsOp, RequestId};
use crate::vfs::{self, Endpoint, Fs, Location};

use super::Ctx;

/// Lists `location` on its endpoint's browsing channel and answers with
/// `BgEvent::Listed`. Runs under `ctx.requests`, so `CancelRequest`
/// abandons it and a newer listing under the same id replaces it.
pub fn list_dir(ctx: &Ctx, request: RequestId, location: Location) {
    let connections = ctx.connections.clone();
    let events = ctx.events.clone();
    ctx.requests.spawn(request, async move {
        let result = list(connections.as_ref(), &location).await;
        // A send error means the app has exited; the listing is not needed.
        drop(events.send(BgEvent::Listed {
            request,
            location,
            result,
        }));
    });
}

/// Performs `op` on `endpoint`'s browsing channel and answers with
/// `BgEvent::FsOpFinished`, listing one `FsError` per failed path.
pub fn fs_op(ctx: &Ctx, request: RequestId, endpoint: Endpoint, op: FsOp) {
    let connections = ctx.connections.clone();
    let events = ctx.events.clone();
    tokio::spawn(async move {
        let failures = perform(connections.as_ref(), &endpoint, &op).await;
        // A send error means the app has exited; nothing is left to refresh.
        drop(events.send(BgEvent::FsOpFinished {
            request,
            endpoint,
            op,
            failures,
        }));
    });
}

async fn list(
    connections: &dyn Connections,
    location: &Location,
) -> Result<Vec<vfs::Entry>, FsError> {
    let fs = connections
        .fs(&location.endpoint, FsChannel::Browse)
        .await
        .map_err(|error| unavailable(error, &location.path))?;
    fs.list(&location.path).await
}

async fn perform(connections: &dyn Connections, endpoint: &Endpoint, op: &FsOp) -> Vec<FsError> {
    let fs = match connections.fs(endpoint, FsChannel::Browse).await {
        Ok(fs) => fs,
        // Every path of the operation failed, so each gets its own entry
        // (the trash-failed prompt and the notice work per path).
        Err(error) => {
            return op_paths(op)
                .into_iter()
                .map(|path| unavailable(error.clone(), path))
                .collect()
        }
    };
    let fs = fs.as_ref();
    match op {
        FsOp::MakeDir { path } => fs.mkdir(path).await.err().into_iter().collect(),
        FsOp::Rename { from, to } => rename(fs, from, to).await.err().into_iter().collect(),
        FsOp::Delete { paths, method } => {
            let mut failures = Vec::new();
            for path in paths {
                let result = match method {
                    // `LocalFs::trash` reports a refusing backend as
                    // `TrashFailed`, which is kept as is so the app can
                    // offer a permanent delete for exactly these paths.
                    DeleteMethod::Trash => fs.trash(path).await,
                    DeleteMethod::Permanent => vfs::remove_tree(fs, path).await,
                };
                failures.extend(result.err());
            }
            failures
        }
        FsOp::Chmod { paths, mode } => {
            let mut failures = Vec::new();
            for path in paths {
                failures.extend(fs.chmod(path, *mode).await.err());
            }
            failures
        }
    }
}

/// Renames without ever replacing: an existing `to` (any kind, including a
/// dangling symlink) is `FsError::AlreadyExists`.
async fn rename(fs: &dyn Fs, from: &Path, to: &Path) -> Result<(), FsError> {
    match fs.lstat(to).await {
        Ok(_) => Err(FsError::AlreadyExists {
            path: to.to_path_buf(),
        }),
        Err(FsError::NotFound { .. }) => fs.rename(from, to).await,
        Err(error) => Err(error),
    }
}

/// The paths an operation works on, in request order.
fn op_paths(op: &FsOp) -> Vec<&PathBuf> {
    match op {
        FsOp::MakeDir { path } => vec![path],
        FsOp::Rename { from, .. } => vec![from],
        FsOp::Delete { paths, .. } | FsOp::Chmod { paths, .. } => paths.iter().collect(),
    }
}

/// The `FsError` for a file system that could not be obtained for work on
/// `path`: a connection failure is `Disconnected` (the endpoint is gone or
/// reconnecting), a file system error stays itself, and anything else
/// keeps its user-facing text.
fn unavailable(error: Error, path: &Path) -> FsError {
    match error {
        Error::Fs(error) => error,
        Error::Connect(_) => FsError::Disconnected,
        other => FsError::Io {
            path: path.to_path_buf(),
            message: other.to_string(),
        },
    }
}

/// Test doubles shared by the executor tests of this module and
/// `external`: a `Connections` serving fixed file systems and a `Ctx`
/// around it.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};

    use futures_util::future::BoxFuture;
    use ratatui_image::picker::Picker;
    use tokio::sync::mpsc::{self, UnboundedReceiver};

    use crate::connections::{Connections, FsChannel, RemoteChild, RemoteTools, SshClient};
    use crate::error::{ConnectError, Error, SudoError};
    use crate::session::{Backends, Sessions, SftpChannel};
    use crate::settings::Settings;
    use crate::ui::action::{BgEvent, Secret};
    use crate::ui::effects::{Ctx, Tasks};
    use crate::vfs::local::LocalFs;
    use crate::vfs::{Endpoint, Fs};

    /// Serves the file systems in `fs` (any channel); an endpoint without
    /// one fails like an endpoint that is not connected. `ssh_command`
    /// answers `ssh_argv` for remote endpoints.
    pub struct MapConnections {
        /// The file system of each served endpoint.
        pub fs: HashMap<Endpoint, Arc<dyn Fs>>,
        /// The answer of `ssh_command` for remote endpoints.
        pub ssh_argv: Vec<String>,
    }

    impl MapConnections {
        /// Only the local file system.
        pub fn local() -> MapConnections {
            let mut fs: HashMap<Endpoint, Arc<dyn Fs>> = HashMap::new();
            fs.insert(Endpoint::Local, Arc::new(LocalFs::new()));
            MapConnections {
                fs,
                ssh_argv: Vec::new(),
            }
        }
    }

    /// The error of an endpoint these doubles do not serve.
    pub fn not_connected() -> Error {
        Error::Connect(ConnectError::Other {
            detail: "not connected in this test".into(),
        })
    }

    impl Connections for MapConnections {
        fn fs<'a>(
            &'a self,
            endpoint: &'a Endpoint,
            _channel: FsChannel,
        ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
            Box::pin(async move { self.fs.get(endpoint).cloned().ok_or_else(not_connected) })
        }

        fn spawn_remote<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _argv: &'a [String],
        ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
            Box::pin(async { Err(not_connected()) })
        }

        fn ssh_command<'a>(
            &'a self,
            endpoint: &'a Endpoint,
            _interactive: bool,
        ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
            Box::pin(async move {
                if endpoint.is_local() {
                    Err(Error::Connect(ConnectError::Other {
                        detail: "the local machine is not reached through ssh".into(),
                    }))
                } else {
                    Ok(self.ssh_argv.clone())
                }
            })
        }

        fn tools<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
            Box::pin(async { Err(not_connected()) })
        }
    }

    /// Backends of a `Sessions` that is never connected: the executors
    /// under test reach endpoints through `Ctx::connections` only.
    struct UnusedBackends;

    fn unused() -> ConnectError {
        ConnectError::Other {
            detail: "the session is not used in this test".into(),
        }
    }

    impl Backends for UnusedBackends {
        fn local(&self) -> Arc<dyn Fs> {
            Arc::new(LocalFs::new())
        }

        fn start_sftp(
            &self,
            _endpoint: Endpoint,
            _channel: RemoteChild,
        ) -> BoxFuture<'_, Result<Arc<dyn SftpChannel>, ConnectError>> {
            Box::pin(async { Err(unused()) })
        }

        fn locate_sftp_server<'a>(
            &'a self,
            _connections: &'a dyn Connections,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<PathBuf, SudoError>> {
            Box::pin(async { Err(SudoError::Connect(unused())) })
        }

        fn start_sudo<'a>(
            &'a self,
            _connections: &'a dyn Connections,
            _endpoint: &'a Endpoint,
            _sftp_server: &'a Path,
            _password: Option<&'a Secret>,
        ) -> BoxFuture<'a, Result<RemoteChild, SudoError>> {
            Box::pin(async { Err(SudoError::Connect(unused())) })
        }
    }

    /// One `Sessions` per test process: each `Sessions::new` resets the
    /// per-process ControlPath directory, which parallel tests must not
    /// race on.
    fn sessions() -> Arc<Sessions> {
        static SESSIONS: OnceLock<Arc<Sessions>> = OnceLock::new();
        Arc::clone(SESSIONS.get_or_init(|| {
            // The receiver is dropped: a session that never connects sends
            // nothing.
            let (events, _) = mpsc::unbounded_channel();
            Arc::new(
                Sessions::new(
                    SshClient::default(),
                    &Settings::default(),
                    events,
                    Arc::new(UnusedBackends),
                )
                .expect("sessions"),
            )
        }))
    }

    /// A `Ctx` reaching endpoints through `connections`, and the receiver of
    /// its events. `edit_temp` is `temp`.
    pub fn ctx(
        connections: Arc<dyn Connections>,
        temp: &Path,
    ) -> (Ctx, UnboundedReceiver<BgEvent>) {
        let (events, received) = mpsc::unbounded_channel();
        // The engine is not running; nothing here sends it commands.
        let (engine, _) = mpsc::unbounded_channel();
        let ctx = Ctx {
            sessions: sessions(),
            connections,
            events,
            settings: Arc::new(Settings::default()),
            ssh: SshClient::default(),
            state_dir: None,
            persisted: Arc::default(),
            engine,
            image_picker: Arc::new(Picker::halfblocks()),
            edit_temp: temp.to_path_buf(),
            requests: Arc::new(Tasks::new()),
            timers: Arc::new(Tasks::new()),
            syncs: Arc::new(Tasks::new()),
        };
        (ctx, received)
    }

    /// The fixture ssh_config, or `None` (test skipped) unless `SCPTO_IT=1`.
    /// `SCPTO_IT_SSH_CONFIG` overrides the default `target/it/ssh_config`,
    /// for worktrees that share the fixtures of another checkout.
    pub fn it_config() -> Option<PathBuf> {
        if std::env::var_os("SCPTO_IT").as_deref() != Some(std::ffi::OsStr::new("1")) {
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

    /// An SFTP session over a plain `ssh -s <host> sftp`, standing in for
    /// the session layer's browsing channel.
    pub async fn sftp(config: &Path, endpoint: &Endpoint) -> Arc<dyn Fs> {
        use std::collections::VecDeque;
        use std::process::Stdio;
        use std::sync::Mutex;

        use crate::connections::StderrTail;
        use crate::vfs::sftp::SftpFs;

        let Endpoint::Remote { host, .. } = endpoint else {
            panic!("sftp needs a remote endpoint");
        };
        let mut child = tokio::process::Command::new("ssh")
            .arg("-F")
            .arg(config)
            .args(["-o", "BatchMode=yes", "-s", host.as_str(), "sftp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn ssh");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
        let channel = RemoteChild {
            child,
            stdin,
            stdout,
            stderr,
        };
        Arc::new(
            SftpFs::start(endpoint.clone(), channel)
                .await
                .expect("sftp handshake"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{ctx, it_config, not_connected, sftp, MapConnections};
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use futures_util::future::BoxFuture;
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::connections::{RemoteChild, RemoteTools};
    use crate::error::{ConnectError, SudoError};
    use crate::transfer::stream::test_support::{MemFs, Op};
    use crate::vfs::EntryKind;

    fn local(path: &Path) -> Location {
        Location {
            endpoint: Endpoint::Local,
            path: path.to_path_buf(),
        }
    }

    fn remote(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.into(),
            sudo: false,
        }
    }

    fn serving(endpoint: Endpoint, fs: &Arc<MemFs>) -> MapConnections {
        let mut connections = MapConnections::local();
        connections
            .fs
            .insert(endpoint, Arc::clone(fs) as Arc<dyn Fs>);
        connections
    }

    async fn finished(
        events: &mut UnboundedReceiver<BgEvent>,
    ) -> (RequestId, Endpoint, FsOp, Vec<FsError>) {
        match events.recv().await.expect("answered") {
            BgEvent::FsOpFinished {
                request,
                endpoint,
                op,
                failures,
            } => (request, endpoint, op, failures),
            other => panic!("expected FsOpFinished, got {other:?}"),
        }
    }

    async fn run_op(connections: MapConnections, endpoint: Endpoint, op: FsOp) -> Vec<FsError> {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, mut events) = ctx(Arc::new(connections), temp.path());
        fs_op(&ctx, RequestId(9), endpoint.clone(), op.clone());
        let (request, got_endpoint, got_op, failures) = finished(&mut events).await;
        assert_eq!(
            (request, got_endpoint, got_op),
            (RequestId(9), endpoint, op)
        );
        failures
    }

    async fn listed(
        events: &mut UnboundedReceiver<BgEvent>,
    ) -> (RequestId, Location, Result<Vec<vfs::Entry>, FsError>) {
        match events.recv().await.expect("answered") {
            BgEvent::Listed {
                request,
                location,
                result,
            } => (request, location, result),
            other => panic!("expected Listed, got {other:?}"),
        }
    }

    fn sorted_names(entries: Vec<vfs::Entry>) -> Vec<OsString> {
        let mut names: Vec<OsString> = entries.into_iter().map(|entry| entry.name).collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn lists_a_local_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let (ctx, mut events) = ctx(Arc::new(MapConnections::local()), dir.path());
        list_dir(&ctx, RequestId(1), local(dir.path()));
        let (request, location, result) = listed(&mut events).await;
        assert_eq!(request, RequestId(1));
        assert_eq!(location, local(dir.path()));
        assert_eq!(sorted_names(result.unwrap()), ["a.txt", "sub"]);
    }

    #[tokio::test]
    async fn listing_failures_are_classified() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, mut events) = ctx(Arc::new(MapConnections::local()), dir.path());
        let missing = dir.path().join("missing");
        list_dir(&ctx, RequestId(1), local(&missing));
        let (_, _, result) = listed(&mut events).await;
        assert_eq!(result, Err(FsError::NotFound { path: missing }));

        // An endpoint whose file system cannot be had is disconnected.
        let gone = Location {
            endpoint: remote("gone"),
            path: PathBuf::from("/srv"),
        };
        list_dir(&ctx, RequestId(2), gone.clone());
        let (request, location, result) = listed(&mut events).await;
        assert_eq!((request, location), (RequestId(2), gone));
        assert_eq!(result, Err(FsError::Disconnected));
    }

    #[test]
    fn unavailable_file_systems_keep_their_class_and_text() {
        let path = Path::new("/p");
        assert_eq!(
            unavailable(
                Error::Fs(FsError::PermissionDenied { path: path.into() }),
                path
            ),
            FsError::PermissionDenied { path: path.into() }
        );
        assert_eq!(
            unavailable(Error::Connect(ConnectError::ConnectionLost), path),
            FsError::Disconnected
        );
        assert_eq!(
            unavailable(Error::Sudo(SudoError::WrongPassword), path),
            FsError::Io {
                path: path.into(),
                message: "Incorrect sudo password".into()
            }
        );
    }

    /// Serves one file system after taking a permit from `gate`.
    struct GatedConnections {
        fs: Arc<MemFs>,
        gate: Arc<tokio::sync::Semaphore>,
    }

    impl Connections for GatedConnections {
        fn fs<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _channel: FsChannel,
        ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
            Box::pin(async move {
                self.gate.acquire().await.expect("gate open").forget();
                Ok(Arc::clone(&self.fs) as Arc<dyn Fs>)
            })
        }

        fn spawn_remote<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _argv: &'a [String],
        ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
            Box::pin(async { Err(not_connected()) })
        }

        fn ssh_command<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _interactive: bool,
        ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
            Box::pin(async { Err(not_connected()) })
        }

        fn tools<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
            Box::pin(async { Err(not_connected()) })
        }
    }

    #[tokio::test]
    async fn a_canceled_listing_never_answers() {
        let memfs = MemFs::new(remote("slow"));
        memfs.add_dir("/d");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let connections = GatedConnections {
            fs: memfs,
            gate: Arc::clone(&gate),
        };
        let temp = tempfile::tempdir().unwrap();
        let (ctx, mut events) = ctx(Arc::new(connections), temp.path());
        let location = Location {
            endpoint: remote("slow"),
            path: PathBuf::from("/d"),
        };
        // The first listing waits on the closed gate until it is canceled.
        list_dir(&ctx, RequestId(5), location.clone());
        assert!(ctx.requests.contains(&RequestId(5)));
        assert!(ctx.requests.abort(&RequestId(5)));
        assert!(!ctx.requests.contains(&RequestId(5)));
        gate.add_permits(1);
        // Only the second listing answers; the canceled one never does.
        list_dir(&ctx, RequestId(6), location);
        let (request, _, result) = listed(&mut events).await;
        assert_eq!(request, RequestId(6));
        assert_eq!(result.unwrap(), []);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(events.try_recv().is_err(), "the canceled listing answered");
    }

    #[tokio::test]
    async fn make_dir_creates_and_reports_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("new");
        let op = FsOp::MakeDir { path: new.clone() };
        assert_eq!(
            run_op(MapConnections::local(), Endpoint::Local, op.clone()).await,
            []
        );
        assert!(new.is_dir());
        assert_eq!(
            run_op(MapConnections::local(), Endpoint::Local, op).await,
            [FsError::AlreadyExists { path: new }]
        );
    }

    #[tokio::test]
    async fn rename_never_replaces_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        std::fs::write(&from, b"from").unwrap();
        std::fs::write(&to, b"to").unwrap();
        let op = FsOp::Rename {
            from: from.clone(),
            to: to.clone(),
        };
        assert_eq!(
            run_op(MapConnections::local(), Endpoint::Local, op.clone()).await,
            [FsError::AlreadyExists { path: to.clone() }]
        );
        assert_eq!(std::fs::read(&to).unwrap(), b"to");
        assert_eq!(std::fs::read(&from).unwrap(), b"from");

        // A dangling symlink is an existing target too.
        std::fs::remove_file(&to).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &to).unwrap();
        assert_eq!(
            run_op(MapConnections::local(), Endpoint::Local, op.clone()).await,
            [FsError::AlreadyExists { path: to.clone() }]
        );

        std::fs::remove_file(&to).unwrap();
        assert_eq!(
            run_op(MapConnections::local(), Endpoint::Local, op).await,
            []
        );
        assert_eq!(std::fs::read(&to).unwrap(), b"from");
        assert!(!from.exists());
    }

    #[tokio::test]
    async fn rename_reports_a_failing_target_check_without_renaming() {
        let memfs = MemFs::new(remote("h"));
        memfs.add_file("/a", b"a", 0, 0o644);
        memfs.fail(Op::Lstat, "/b", FsError::Disconnected, false);
        let op = FsOp::Rename {
            from: PathBuf::from("/a"),
            to: PathBuf::from("/b"),
        };
        assert_eq!(
            run_op(serving(remote("h"), &memfs), remote("h"), op).await,
            [FsError::Disconnected]
        );
        assert!(!memfs.called(Op::Rename, "/a"));
        assert!(memfs.exists("/a"));
    }

    #[tokio::test]
    async fn permanent_delete_removes_trees_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"k").unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("a/b")).unwrap();
        std::fs::write(tree.join("a/b/f"), b"f").unwrap();
        std::fs::write(tree.join("top"), b"t").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("a/link-out")).unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let op = FsOp::Delete {
            paths: vec![tree.clone(), file.clone()],
            method: DeleteMethod::Permanent,
        };
        assert_eq!(
            run_op(MapConnections::local(), Endpoint::Local, op).await,
            []
        );
        assert!(!tree.exists());
        assert!(!file.exists());
        assert!(outside.join("keep").is_file(), "the link was followed");
    }

    #[tokio::test]
    async fn permanent_delete_reports_each_failed_path() {
        let memfs = MemFs::new(remote("h"));
        memfs.add_dir("/x");
        memfs.add_file("/x/f", b"f", 0, 0o644);
        memfs.add_file("/y", b"y", 0, 0o644);
        memfs.add_file("/z", b"z", 0, 0o644);
        let denied = FsError::PermissionDenied {
            path: PathBuf::from("/x/f"),
        };
        memfs.fail(Op::RemoveFile, "/x/f", denied.clone(), false);
        memfs.fail(Op::RemoveFile, "/z", FsError::Disconnected, false);
        let op = FsOp::Delete {
            paths: vec!["/x".into(), "/y".into(), "/z".into()],
            method: DeleteMethod::Permanent,
        };
        assert_eq!(
            run_op(serving(remote("h"), &memfs), remote("h"), op).await,
            [denied, FsError::Disconnected]
        );
        assert!(memfs.exists("/x"));
        assert!(!memfs.exists("/y"));
        assert!(memfs.exists("/z"));
    }

    #[tokio::test]
    async fn trash_errors_are_reported_per_path_unchanged() {
        // A remote file system has no trash: its error passes through.
        let memfs = MemFs::new(remote("h"));
        memfs.add_file("/f", b"f", 0, 0o644);
        let op = FsOp::Delete {
            paths: vec!["/f".into(), "/g".into()],
            method: DeleteMethod::Trash,
        };
        let unsupported = FsError::Unsupported {
            operation: "trash".into(),
            reason: "mock".into(),
        };
        assert_eq!(
            run_op(serving(remote("h"), &memfs), remote("h"), op).await,
            [unsupported.clone(), unsupported]
        );
        assert!(memfs.exists("/f"));

        // A missing local path is reported as missing before the trash
        // backend is involved, so no file reaches the user's trash.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let op = FsOp::Delete {
            paths: vec![missing.clone()],
            method: DeleteMethod::Trash,
        };
        assert_eq!(
            run_op(MapConnections::local(), Endpoint::Local, op).await,
            [FsError::NotFound { path: missing }]
        );
    }

    #[tokio::test]
    async fn chmod_sets_bits_and_reports_each_failure() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let missing = dir.path().join("missing");
        let op = FsOp::Chmod {
            paths: vec![a.clone(), missing.clone(), b.clone()],
            mode: 0o640,
        };
        assert_eq!(
            run_op(MapConnections::local(), Endpoint::Local, op).await,
            [FsError::NotFound { path: missing }]
        );
        for path in [a, b] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o7777, 0o640);
        }
    }

    #[tokio::test]
    async fn an_unavailable_endpoint_fails_every_path() {
        let op = FsOp::Delete {
            paths: vec!["/a".into(), "/b".into()],
            method: DeleteMethod::Permanent,
        };
        assert_eq!(
            run_op(MapConnections::local(), remote("gone"), op).await,
            [FsError::Disconnected, FsError::Disconnected]
        );
        let op = FsOp::MakeDir { path: "/a".into() };
        assert_eq!(
            run_op(MapConnections::local(), remote("gone"), op).await,
            [FsError::Disconnected]
        );
    }

    /// Lists the fixture seed and runs every operation over real SFTP.
    #[tokio::test]
    async fn docker_lists_and_changes_a_remote_directory() {
        let Some(config) = it_config() else {
            return;
        };
        for host in ["it-modern-plain", "it-legacy-plain"] {
            let endpoint = remote(host);
            let fs = sftp(&config, &endpoint).await;
            let mut connections = MapConnections::local();
            connections.fs.insert(endpoint.clone(), Arc::clone(&fs));
            let temp = tempfile::tempdir().unwrap();
            let (ctx, mut events) = ctx(Arc::new(connections), temp.path());

            let seed = Location {
                endpoint: endpoint.clone(),
                path: PathBuf::from("/home/plain/seed"),
            };
            list_dir(&ctx, RequestId(1), seed.clone());
            let (request, location, result) = listed(&mut events).await;
            assert_eq!((request, location), (RequestId(1), seed));
            let entries = result.expect("listing");
            let link_to_dir = entries
                .iter()
                .find(|entry| entry.name == "link-to-dir")
                .expect("link-to-dir");
            assert!(matches!(
                link_to_dir.kind,
                EntryKind::Symlink {
                    target_is_dir: true,
                    ..
                }
            ));
            let names = sorted_names(entries);
            for expected in [
                "hello.txt",
                "link-to-file",
                "nested",
                "pixel.png",
                "\u{7a7a}\u{767d} \u{6a94}\u{6848} \u{6e2c}\u{8a66}.txt",
            ] {
                assert!(
                    names.contains(&OsString::from(expected)),
                    "{host}: {names:?}"
                );
            }

            // mkdir, rename (refusing to replace), chmod, recursive delete.
            let base = PathBuf::from(format!("/tmp/scpto-fx-fs-{}", std::process::id()));
            let ops = [
                (FsOp::MakeDir { path: base.clone() }, vec![]),
                (
                    FsOp::MakeDir {
                        path: base.join("a"),
                    },
                    vec![],
                ),
                (
                    FsOp::MakeDir {
                        path: base.join("a/inner"),
                    },
                    vec![],
                ),
                (
                    FsOp::MakeDir {
                        path: base.join("b"),
                    },
                    vec![],
                ),
                (
                    FsOp::Rename {
                        from: base.join("a"),
                        to: base.join("b"),
                    },
                    vec![FsError::AlreadyExists {
                        path: base.join("b"),
                    }],
                ),
                (
                    FsOp::Rename {
                        from: base.join("a"),
                        to: base.join("c"),
                    },
                    vec![],
                ),
                (
                    FsOp::Chmod {
                        paths: vec![base.join("c")],
                        mode: 0o700,
                    },
                    vec![],
                ),
            ];
            for (op, expected) in ops {
                fs_op(&ctx, RequestId(2), endpoint.clone(), op.clone());
                let (_, _, got_op, failures) = finished(&mut events).await;
                assert_eq!(got_op, op);
                assert_eq!(failures, expected, "{host}: {op:?}");
            }
            assert_eq!(fs.lstat(&base.join("c")).await.unwrap().mode, Some(0o700));
            assert!(fs.lstat(&base.join("c/inner")).await.is_ok());

            let delete = FsOp::Delete {
                paths: vec![base.clone()],
                method: DeleteMethod::Permanent,
            };
            fs_op(&ctx, RequestId(3), endpoint.clone(), delete);
            let (_, _, _, failures) = finished(&mut events).await;
            assert_eq!(failures, []);
            assert!(matches!(
                fs.lstat(&base).await,
                Err(FsError::NotFound { .. })
            ));
        }
    }
}
