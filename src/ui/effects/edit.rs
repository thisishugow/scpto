//! Executes `Effect::Edit` (with the runtime, which suspends the TUI) and
//! `Effect::ResolveEditConflict` using `edit`.
//!
//! # Behavior
//!
//! - `prepare` (async; the runtime polls it without blocking input):
//!   `Connections::fs(endpoint, Browse)`, `edit::prepare(&*fs, location,
//!   &ctx.edit_temp)`, then `edit::editor_command` with `$VISUAL` and
//!   `$EDITOR` from the environment for the session's file. It returns the
//!   [`Foreground`] with `AfterForeground::Edit(session)`; on any failure it
//!   sends `EditFinished { result: Err }` and returns `None`.
//! - `finish` (called by the runtime after the editor exited): a spawn
//!   error or an unsuccessful exit is `EditFinished { result:
//!   Err(Error::CommandFailed { .. }) }` and nothing is uploaded (the user
//!   aborted the editor); otherwise it spawns `edit::finish(session, &*fs,
//!   elapsed)` and sends `EditFinished` with its outcome.
//! - `resolve_conflict` spawns `edit::resolve_conflict` and sends
//!   `EditFinished`.
//! - No edit is lost to a dropped connection: when the file system of a
//!   remote file cannot be had for the upload (`finish`, or
//!   `resolve_conflict` with `overwrite`), the outcome is
//!   `EditOutcome::UploadFailed` with the temp file's path, as for any other
//!   upload failure.
//!
//! # Public API
//!
//! ```text
//! pub async fn prepare(ctx: Ctx, location: Location) -> Option<Foreground>;
//! pub fn finish(ctx: &Ctx, session: EditSession, status: std::io::Result<std::process::ExitStatus>, elapsed: Duration);
//! pub fn resolve_conflict(ctx: &Ctx, location: Location, temp: PathBuf, overwrite: bool);
//! ```
//!
//! [`Foreground`]: super::Foreground

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use super::{AfterForeground, Ctx, Foreground};
use crate::connections::{Connections, FsChannel};
use crate::edit::{self, EditOutcome, EditSession};
use crate::error::{self, Error};
use crate::ui::action::BgEvent;
use crate::vfs::Location;

/// The program name shown when the editor fails.
const EDITOR_PROGRAM: &str = "editor";

/// Downloads `location` (when remote) and builds the editor command the
/// runtime runs in the foreground. A failure is reported as
/// `EditFinished { result: Err }` and gives `None`.
pub async fn prepare(ctx: Ctx, location: Location) -> Option<Foreground> {
    let visual = std::env::var_os("VISUAL");
    let editor = std::env::var_os("EDITOR");
    let started = start(
        &*ctx.connections,
        &ctx.edit_temp,
        visual.as_deref(),
        editor.as_deref(),
        location.clone(),
    )
    .await;
    match started {
        Ok(foreground) => Some(foreground),
        Err(error) => {
            report(&ctx.events, location, Err(error));
            None
        }
    }
}

/// After the editor exited: reports a failed or aborted editor, else
/// compares and uploads the edits in a spawned task. Must be called inside
/// a Tokio runtime.
pub fn finish(ctx: &Ctx, session: EditSession, status: io::Result<ExitStatus>, elapsed: Duration) {
    finish_with(
        Arc::clone(&ctx.connections),
        ctx.events.clone(),
        session,
        status,
        elapsed,
    );
}

/// `Effect::ResolveEditConflict`: uploads (`overwrite`) or discards the
/// edits in `temp` in a spawned task. Must be called inside a Tokio
/// runtime.
pub fn resolve_conflict(ctx: &Ctx, location: Location, temp: PathBuf, overwrite: bool) {
    resolve_with(
        Arc::clone(&ctx.connections),
        ctx.events.clone(),
        location,
        temp,
        overwrite,
    );
}

/// `prepare` without the context: the file system, the download, and the
/// editor command for the given `$VISUAL` and `$EDITOR` values.
async fn start(
    connections: &dyn Connections,
    temp_root: &Path,
    visual: Option<&OsStr>,
    editor: Option<&OsStr>,
    location: Location,
) -> Result<Foreground, Error> {
    let fs = connections
        .fs(&location.endpoint, FsChannel::Browse)
        .await?;
    let session = edit::prepare(&*fs, location, temp_root).await?;
    let command = edit::editor_command(visual, editor, &session.file)?;
    Ok(Foreground {
        command,
        then: AfterForeground::Edit(session),
    })
}

/// `finish` without the context.
fn finish_with(
    connections: Arc<dyn Connections>,
    events: UnboundedSender<BgEvent>,
    session: EditSession,
    status: io::Result<ExitStatus>,
    elapsed: Duration,
) {
    let location = session.location.clone();
    if let Err(error) = editor_succeeded(status) {
        report(&events, location, Err(error));
        return;
    }
    tokio::spawn(async move {
        let result = upload_edits(&*connections, session, elapsed).await;
        report(&events, location, result);
    });
}

/// `resolve_conflict` without the context.
fn resolve_with(
    connections: Arc<dyn Connections>,
    events: UnboundedSender<BgEvent>,
    location: Location,
    temp: PathBuf,
    overwrite: bool,
) {
    tokio::spawn(async move {
        let result = resolve(&*connections, &location, temp, overwrite).await;
        report(&events, location, result);
    });
}

/// `Ok` when the editor ran and exited successfully; otherwise the
/// `CommandFailed` error the user sees.
fn editor_succeeded(status: io::Result<ExitStatus>) -> Result<(), Error> {
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(Error::CommandFailed {
            program: EDITOR_PROGRAM.into(),
            status: status.into(),
            stderr: String::new(),
        }),
        Err(spawn_error) => Err(Error::CommandFailed {
            program: EDITOR_PROGRAM.into(),
            status: error::ExitStatus::NotStarted,
            stderr: spawn_error.to_string(),
        }),
    }
}

/// Compares the edited file and uploads it over the Browse channel.
async fn upload_edits(
    connections: &dyn Connections,
    session: EditSession,
    elapsed: Duration,
) -> Result<EditOutcome, Error> {
    let fs = match connections
        .fs(&session.location.endpoint, FsChannel::Browse)
        .await
    {
        Ok(fs) => fs,
        // The editor may have saved: the edits stay in the temp file and the
        // user is told where.
        Err(error) if !session.location.endpoint.is_local() => {
            return Ok(EditOutcome::UploadFailed {
                temp: session.file,
                error,
            })
        }
        Err(error) => return Err(error),
    };
    edit::finish(session, &*fs, elapsed).await
}

/// Uploads or discards the edits in `temp` over the Browse channel.
async fn resolve(
    connections: &dyn Connections,
    location: &Location,
    temp: PathBuf,
    overwrite: bool,
) -> Result<EditOutcome, Error> {
    let fs = match connections.fs(&location.endpoint, FsChannel::Browse).await {
        Ok(fs) => fs,
        Err(error) if overwrite => return Ok(EditOutcome::UploadFailed { temp, error }),
        // Discarding failed before anything was touched; the temp file goes
        // with the temp directory when scpto exits.
        Err(error) => return Err(error),
    };
    edit::resolve_conflict(&*fs, location, &temp, overwrite).await
}

/// Sends `EditFinished`.
fn report(
    events: &UnboundedSender<BgEvent>,
    location: Location,
    result: Result<EditOutcome, Error>,
) {
    if let Err(unsent) = events.send(BgEvent::EditFinished { location, result }) {
        // The app has exited; nobody is left to tell.
        drop(unsent);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use std::sync::Mutex;

    use futures_util::future::BoxFuture;
    use tokio::sync::mpsc::{self, UnboundedReceiver};

    use crate::connections::{RemoteChild, RemoteTools};
    use crate::error::FsError;
    use crate::vfs::local::LocalFs;
    use crate::vfs::{Endpoint, Fs};

    /// What [`FakeConnections::fs`] answers.
    pub(in crate::ui::effects) enum Serve {
        /// The local file system, for any endpoint (a tempdir stands in for
        /// the remote host).
        Local,
        /// `FsError::Disconnected`, as for a host that dropped.
        Disconnected,
        /// Never answers, like a host that is still reconnecting.
        Pending,
    }

    /// `Connections` serving file systems only, recording the channels
    /// asked for.
    pub(in crate::ui::effects) struct FakeConnections {
        serve: Serve,
        fs: Arc<dyn Fs>,
        channels: Mutex<Vec<FsChannel>>,
    }

    impl FakeConnections {
        pub(in crate::ui::effects) fn new(serve: Serve) -> Arc<FakeConnections> {
            Arc::new(FakeConnections {
                serve,
                fs: Arc::new(LocalFs::new()),
                channels: Mutex::new(Vec::new()),
            })
        }

        /// Every channel `fs` was called with, in order.
        pub(in crate::ui::effects) fn channels(&self) -> Vec<FsChannel> {
            self.channels.lock().expect("not poisoned").clone()
        }
    }

    impl Connections for FakeConnections {
        fn fs<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            channel: FsChannel,
        ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
            self.channels.lock().expect("not poisoned").push(channel);
            match self.serve {
                Serve::Local => {
                    let fs = Arc::clone(&self.fs);
                    Box::pin(async move { Ok(fs) })
                }
                Serve::Disconnected => Box::pin(async { Err(FsError::Disconnected.into()) }),
                Serve::Pending => Box::pin(std::future::pending()),
            }
        }

        fn spawn_remote<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _argv: &'a [String],
        ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
            panic!("the executors under test never start remote commands")
        }

        fn ssh_command<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _interactive: bool,
        ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
            panic!("the executors under test never build ssh commands")
        }

        fn tools<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
            panic!("the executors under test never probe tools")
        }
    }

    /// A remote location whose path is a real local file.
    pub(in crate::ui::effects) fn remote(path: &Path) -> Location {
        Location {
            endpoint: Endpoint::Remote {
                host: "fixture".into(),
                sudo: false,
            },
            path: path.to_path_buf(),
        }
    }

    /// The next `EditFinished`, failing the test after five seconds.
    async fn next_finished(
        events: &mut UnboundedReceiver<BgEvent>,
    ) -> (Location, Result<EditOutcome, Error>) {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event within five seconds")
            .expect("the channel is open");
        match event {
            BgEvent::EditFinished { location, result } => (location, result),
            other => panic!("expected EditFinished, got {other:?}"),
        }
    }

    /// A stub editor that appends a line to the file it is given (the file
    /// is the last argument, `$1` after the `sh` placeholder for `$0`).
    const APPENDING_EDITOR: &str = r#"sh -c 'printf "added\n" >> "$1"' sh"#;

    struct Fixture {
        _dir: tempfile::TempDir,
        file: PathBuf,
        temp_root: PathBuf,
    }

    fn fixture(content: &[u8]) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, content).expect("write the remote file");
        let temp_root = dir.path().join("edit-temp");
        Fixture {
            _dir: dir,
            file,
            temp_root,
        }
    }

    /// Runs `prepare` for a remote copy of `fixture.file` with `visual` as
    /// `$VISUAL`, then the editor, and returns the session and how it ran.
    async fn edit_with(
        connections: &FakeConnections,
        fixture: &Fixture,
        visual: &str,
    ) -> (EditSession, io::Result<ExitStatus>, Duration) {
        let foreground = start(
            connections,
            &fixture.temp_root,
            Some(OsStr::new(visual)),
            None,
            remote(&fixture.file),
        )
        .await
        .expect("prepared");
        let AfterForeground::Edit(session) = foreground.then else {
            panic!("an edit continues with AfterForeground::Edit");
        };
        let mut command = foreground.command;
        let started = std::time::Instant::now();
        let status = command.status().await;
        (session, status, started.elapsed())
    }

    #[tokio::test]
    async fn an_appending_editor_gets_its_change_uploaded() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Local);
        let (session, status, elapsed) = edit_with(&connections, &fixture, APPENDING_EDITOR).await;
        let temp = session.file.clone();
        assert!(
            temp.starts_with(&fixture.temp_root),
            "edited in the temp dir"
        );
        assert_eq!(std::fs::read(&fixture.file).unwrap(), b"original\n");

        let (tx, mut events) = mpsc::unbounded_channel();
        finish_with(connections.clone(), tx, session, status, elapsed);
        let (location, result) = next_finished(&mut events).await;

        assert_eq!(location, remote(&fixture.file));
        assert_eq!(result, Ok(EditOutcome::Uploaded));
        assert_eq!(std::fs::read(&fixture.file).unwrap(), b"original\nadded\n");
        assert!(!temp.exists(), "the temp copy is cleaned up");
        assert_eq!(
            connections.channels(),
            [FsChannel::Browse, FsChannel::Browse],
            "download and upload both use the browsing channel"
        );
    }

    #[tokio::test]
    async fn an_editor_that_changes_nothing_uploads_nothing() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Local);
        let (session, status, elapsed) = edit_with(&connections, &fixture, "true").await;

        let (tx, mut events) = mpsc::unbounded_channel();
        finish_with(connections, tx, session, status, elapsed);
        let (_, result) = next_finished(&mut events).await;

        assert_eq!(result, Ok(EditOutcome::Unchanged { quick_exit: true }));
        assert_eq!(std::fs::read(&fixture.file).unwrap(), b"original\n");
    }

    #[tokio::test]
    async fn a_failing_editor_is_reported_and_nothing_is_uploaded() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Local);
        let visual = r#"sh -c 'printf "half\n" >> "$1"; exit 3' sh"#;
        let (session, status, elapsed) = edit_with(&connections, &fixture, visual).await;
        let temp = session.file.clone();

        let (tx, mut events) = mpsc::unbounded_channel();
        finish_with(connections, tx, session, status, elapsed);
        let (_, result) = next_finished(&mut events).await;

        assert_eq!(
            result,
            Err(Error::CommandFailed {
                program: EDITOR_PROGRAM.into(),
                status: error::ExitStatus::Code(3),
                stderr: String::new(),
            })
        );
        assert_eq!(std::fs::read(&fixture.file).unwrap(), b"original\n");
        assert!(temp.exists(), "what the editor saved stays until exit");
    }

    #[tokio::test]
    async fn an_editor_that_cannot_start_is_a_not_started_failure() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Local);
        let (session, status, elapsed) =
            edit_with(&connections, &fixture, "/nonexistent/scpto-editor").await;
        let spawn_error = status.as_ref().expect_err("cannot spawn").to_string();

        let (tx, mut events) = mpsc::unbounded_channel();
        finish_with(connections, tx, session, status, elapsed);
        let (_, result) = next_finished(&mut events).await;

        assert_eq!(
            result,
            Err(Error::CommandFailed {
                program: EDITOR_PROGRAM.into(),
                status: error::ExitStatus::NotStarted,
                stderr: spawn_error,
            })
        );
        assert_eq!(std::fs::read(&fixture.file).unwrap(), b"original\n");
    }

    #[tokio::test]
    async fn a_connection_lost_while_editing_keeps_the_edits() {
        let fixture = fixture(b"original\n");
        let (session, status, elapsed) = edit_with(
            &FakeConnections::new(Serve::Local),
            &fixture,
            APPENDING_EDITOR,
        )
        .await;
        let temp = session.file.clone();

        let (tx, mut events) = mpsc::unbounded_channel();
        let dropped = FakeConnections::new(Serve::Disconnected);
        finish_with(dropped, tx, session, status, elapsed);
        let (_, result) = next_finished(&mut events).await;

        assert_eq!(
            result,
            Ok(EditOutcome::UploadFailed {
                temp: temp.clone(),
                error: FsError::Disconnected.into(),
            })
        );
        assert_eq!(std::fs::read(&temp).unwrap(), b"original\nadded\n");
        assert_eq!(std::fs::read(&fixture.file).unwrap(), b"original\n");
    }

    #[tokio::test]
    async fn a_local_file_is_edited_in_place() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Local);
        let location = Location {
            endpoint: Endpoint::Local,
            path: fixture.file.clone(),
        };
        let foreground = start(
            &*connections,
            &fixture.temp_root,
            None,
            Some(OsStr::new(APPENDING_EDITOR)),
            location.clone(),
        )
        .await
        .expect("prepared");
        let AfterForeground::Edit(session) = foreground.then else {
            panic!("an edit continues with AfterForeground::Edit");
        };
        assert_eq!(session.file, fixture.file);
        let mut command = foreground.command;
        let status = command.status().await;

        let (tx, mut events) = mpsc::unbounded_channel();
        finish_with(connections, tx, session, status, Duration::from_secs(2));
        let (reported, result) = next_finished(&mut events).await;

        assert_eq!(reported, location);
        assert_eq!(result, Ok(EditOutcome::LocalEdited));
        assert_eq!(std::fs::read(&fixture.file).unwrap(), b"original\nadded\n");
        assert!(!fixture.temp_root.exists(), "local edits need no temp copy");
    }

    #[tokio::test]
    async fn preparing_fails_without_a_file_system() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Disconnected);
        let result = start(
            &*connections,
            &fixture.temp_root,
            None,
            None,
            remote(&fixture.file),
        )
        .await;
        assert_eq!(result.err(), Some(FsError::Disconnected.into()));
    }

    #[tokio::test]
    async fn an_unsplittable_editor_variable_fails_preparing() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Local);
        let result = start(
            &*connections,
            &fixture.temp_root,
            Some(OsStr::new("vim 'unbalanced")),
            None,
            remote(&fixture.file),
        )
        .await;
        assert!(matches!(
            result.err(),
            Some(Error::CommandFailed {
                status: error::ExitStatus::NotStarted,
                ..
            })
        ));
    }

    /// Prepares and runs the appending editor while the "remote" file is
    /// changed behind its back, giving the conflicting temp file.
    async fn conflicting_edit(fixture: &Fixture, connections: &Arc<FakeConnections>) -> PathBuf {
        let (session, status, elapsed) = edit_with(connections, fixture, APPENDING_EDITOR).await;
        std::fs::write(&fixture.file, b"changed on the server\n").unwrap();
        let (tx, mut events) = mpsc::unbounded_channel();
        finish_with(connections.clone(), tx, session, status, elapsed);
        match next_finished(&mut events).await.1 {
            Ok(EditOutcome::RemoteChanged { temp }) => temp,
            other => panic!("expected RemoteChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolving_a_conflict_by_overwriting_uploads_the_edits() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Local);
        let temp = conflicting_edit(&fixture, &connections).await;

        let (tx, mut events) = mpsc::unbounded_channel();
        resolve_with(connections, tx, remote(&fixture.file), temp.clone(), true);
        let (location, result) = next_finished(&mut events).await;

        assert_eq!(location, remote(&fixture.file));
        assert_eq!(result, Ok(EditOutcome::Uploaded));
        assert_eq!(std::fs::read(&fixture.file).unwrap(), b"original\nadded\n");
        assert!(!temp.exists());
    }

    #[tokio::test]
    async fn resolving_a_conflict_by_discarding_keeps_the_server_copy() {
        let fixture = fixture(b"original\n");
        let connections = FakeConnections::new(Serve::Local);
        let temp = conflicting_edit(&fixture, &connections).await;

        let (tx, mut events) = mpsc::unbounded_channel();
        resolve_with(connections, tx, remote(&fixture.file), temp.clone(), false);
        let (_, result) = next_finished(&mut events).await;

        assert_eq!(result, Ok(EditOutcome::Unchanged { quick_exit: false }));
        assert_eq!(
            std::fs::read(&fixture.file).unwrap(),
            b"changed on the server\n"
        );
        assert!(!temp.exists());
    }

    #[tokio::test]
    async fn overwriting_without_a_connection_keeps_the_edits() {
        let fixture = fixture(b"original\n");
        let temp = conflicting_edit(&fixture, &FakeConnections::new(Serve::Local)).await;

        let (tx, mut events) = mpsc::unbounded_channel();
        let dropped = FakeConnections::new(Serve::Disconnected);
        resolve_with(dropped, tx, remote(&fixture.file), temp.clone(), true);
        let (_, result) = next_finished(&mut events).await;

        assert_eq!(
            result,
            Ok(EditOutcome::UploadFailed {
                temp: temp.clone(),
                error: FsError::Disconnected.into(),
            })
        );
        assert_eq!(std::fs::read(&temp).unwrap(), b"original\nadded\n");
    }
}
