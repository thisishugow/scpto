//! Executes `Effect::Compare`, `Effect::Sync`, and `Effect::CancelSync`
//! using `sync`, choosing rsync or an engine job of file pairs. The local
//! rsync probe (`sync::local_rsync_ok`) runs here, once per sync.
//!
//! # Behavior
//!
//! - `compare` runs under `ctx.requests`: the Browse file systems of both
//!   sides, `sync::compare(.., ctx.settings.mtime_tolerance)`, answered with
//!   `Compared`.
//! - `sync`: the remote side's `Connections::tools` and
//!   `sync::local_rsync_ok()` decide `sync::rsync_eligible` (a tools error
//!   counts as not eligible). rsync path, under `ctx.syncs` with the job id:
//!   `Connections::ssh_command(remote, false)`, then `sync::run_rsync(..,
//!   delete_extraneous)`, answered with `SyncFinished`. Engine path: when
//!   `delete_extraneous`, every path of `sync::extraneous` is removed with
//!   `vfs::remove_tree` on the destination's Transfer channel first (a
//!   failure is `SyncFinished { result: Err }` and nothing is submitted);
//!   then `TransferCommand::Submit(sync::engine_job(..))` goes to the
//!   engine, whose events report the job.
//! - `cancel`: when `ctx.syncs.abort(&job)` stopped an rsync run (which
//!   kills rsync's process group), sends `SyncFinished { result:
//!   Err(Error::Canceled) }`; otherwise the job is an engine job and gets
//!   `TransferCommand::Cancel(QueueTarget::Job(job))`.
//!
//! # Public API (Wave 3, ui/effects/sync)
//!
//! ```text
//! pub fn compare(ctx: &Ctx, request: RequestId, left: Location, right: Location);
//! pub fn sync(ctx: &Ctx, job: JobId, direction: SyncDirection, comparison: Box<Comparison>, paths: Vec<PathBuf>, delete_extraneous: bool);
//! pub fn cancel(ctx: &Ctx, job: JobId);
//! ```
//!
//! # Settling a sync
//!
//! A sync task stays registered under `ctx.syncs` from its start (the rsync
//! probe included, so a cancel during the probe stops it) until it settles:
//! it reports `SyncFinished` (rsync path, or a failed deletion) or submits
//! its engine job. Settling and `cancel` both claim the registration under
//! one lock, and only the winner acts: a sync is reported exactly once, and
//! a cancel that arrives after the engine job was submitted always reaches
//! the engine after the `Submit`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc::UnboundedSender;

use crate::connections::{Connections, FsChannel};
use crate::error::Error;
use crate::sync::{self, Comparison, SyncDirection};
use crate::transfer::model::{JobId, QueueTarget, TransferCommand, TransferCommandTx};
use crate::ui::action::{BgEvent, RequestId};
use crate::ui::effects::transfer::forward;
use crate::ui::effects::{Ctx, Tasks};
use crate::vfs::{self, Endpoint, Location};

/// Serializes settling a sync with canceling one (see "Settling a sync").
static HANDOFF: Mutex<()> = Mutex::new(());

/// Compares the trees under `left` and `right` (`Effect::Compare`),
/// answering with `Compared`.
pub fn compare(ctx: &Ctx, request: RequestId, left: Location, right: Location) {
    let connections = Arc::clone(&ctx.connections);
    let events = ctx.events.clone();
    let mtime_tolerance = ctx.settings.mtime_tolerance;
    ctx.requests.spawn(request, async move {
        let result = compare_trees(&*connections, left, right, mtime_tolerance).await;
        report(&events, BgEvent::Compared { request, result });
    });
}

/// Copies the selected `paths` of `comparison` one way (`Effect::Sync`),
/// via rsync or as engine job `job`.
pub fn sync(
    ctx: &Ctx,
    job: JobId,
    direction: SyncDirection,
    comparison: Box<Comparison>,
    paths: Vec<PathBuf>,
    delete_extraneous: bool,
) {
    Syncer::of(ctx).start(SyncRequest {
        job,
        direction,
        comparison,
        paths,
        delete_extraneous,
    });
}

/// Cancels the sync of `job` (`Effect::CancelSync`).
pub fn cancel(ctx: &Ctx, job: JobId) {
    Syncer::of(ctx).cancel(job);
}

/// Lists both sides on their Browse channels and compares them.
async fn compare_trees(
    connections: &dyn Connections,
    left: Location,
    right: Location,
    mtime_tolerance: u64,
) -> Result<Box<Comparison>, Error> {
    let (left_fs, right_fs) = tokio::try_join!(
        connections.fs(&left.endpoint, FsChannel::Browse),
        connections.fs(&right.endpoint, FsChannel::Browse),
    )?;
    sync::compare(&*left_fs, &*right_fs, left, right, mtime_tolerance)
        .await
        .map(Box::new)
}

/// Sends `event` to the app.
fn report(events: &UnboundedSender<BgEvent>, event: BgEvent) {
    if let Err(unsent) = events.send(event) {
        // The app has exited; nobody is left to show the result.
        drop(unsent);
    }
}

/// The fields of `Effect::Sync`.
struct SyncRequest {
    job: JobId,
    direction: SyncDirection,
    comparison: Box<Comparison>,
    paths: Vec<PathBuf>,
    delete_extraneous: bool,
}

impl SyncRequest {
    /// The directory the sync copies into.
    fn destination(&self) -> &Location {
        match self.direction {
            SyncDirection::LeftToRight => &self.comparison.right,
            SyncDirection::RightToLeft => &self.comparison.left,
        }
    }
}

/// The parts of [`Ctx`] a sync uses.
#[derive(Clone)]
struct Syncer {
    connections: Arc<dyn Connections>,
    events: UnboundedSender<BgEvent>,
    engine: TransferCommandTx,
    syncs: Arc<Tasks<JobId>>,
}

impl Syncer {
    fn of(ctx: &Ctx) -> Syncer {
        Syncer {
            connections: Arc::clone(&ctx.connections),
            events: ctx.events.clone(),
            engine: ctx.engine.clone(),
            syncs: Arc::clone(&ctx.syncs),
        }
    }

    /// Runs the sync as a task under `syncs`, keyed by its job id.
    fn start(self, request: SyncRequest) {
        let syncs = Arc::clone(&self.syncs);
        syncs.spawn(request.job, async move { self.run(request).await });
    }

    async fn run(&self, request: SyncRequest) {
        let job = request.job;
        if let Some(remote) = self.rsync_peer(&request.comparison).await {
            let result = self.rsync(&remote, &request).await;
            self.settle(job, || {
                report(&self.events, BgEvent::SyncFinished { job, result });
            });
            return;
        }
        if request.delete_extraneous {
            if let Err(error) = self.delete_extraneous(&request).await {
                self.settle(job, || {
                    report(
                        &self.events,
                        BgEvent::SyncFinished {
                            job,
                            result: Err(error),
                        },
                    );
                });
                return;
            }
        }
        let submit = TransferCommand::Submit(sync::engine_job(
            job,
            request.direction,
            &request.comparison,
            &request.paths,
        ));
        self.settle(job, || {
            if let Err(error) = forward(&self.engine, submit) {
                report(
                    &self.events,
                    BgEvent::SyncFinished {
                        job,
                        result: Err(error.into()),
                    },
                );
            }
        });
    }

    /// The remote endpoint when the sync may run via rsync: exactly one side
    /// is local, and `sync::rsync_eligible` accepts the remote's tools and
    /// the local rsync.
    async fn rsync_peer(&self, comparison: &Comparison) -> Option<Endpoint> {
        let (left, right) = (&comparison.left.endpoint, &comparison.right.endpoint);
        let remote = match (left.is_local(), right.is_local()) {
            (true, false) => right,
            (false, true) => left,
            _ => return None,
        };
        let (tools, local_rsync_ok) =
            tokio::join!(self.connections.tools(remote), sync::local_rsync_ok());
        let tools = match tools {
            Ok(tools) => tools,
            Err(error) => {
                // A remote whose tools cannot be probed is no rsync peer; the
                // engine path reports any lasting connection problem through
                // the job's own events.
                drop(error);
                return None;
            }
        };
        sync::rsync_eligible(left, right, tools, local_rsync_ok).then(|| remote.clone())
    }

    async fn rsync(&self, remote: &Endpoint, request: &SyncRequest) -> Result<(), Error> {
        let ssh = self.connections.ssh_command(remote, false).await?;
        sync::run_rsync(
            &ssh,
            request.direction,
            &request.comparison.left,
            &request.comparison.right,
            &request.paths,
            request.delete_extraneous,
        )
        .await
    }

    /// Engine path: removes the selected destination-only paths on the
    /// destination's Transfer channel, stopping at the first failure.
    async fn delete_extraneous(&self, request: &SyncRequest) -> Result<(), Error> {
        let doomed = sync::extraneous(request.direction, &request.comparison, &request.paths);
        if doomed.is_empty() {
            return Ok(());
        }
        let destination = &request.destination().endpoint;
        let fs = self
            .connections
            .fs(destination, FsChannel::Transfer)
            .await?;
        for path in &doomed {
            vfs::remove_tree(&*fs, path).await?;
        }
        Ok(())
    }

    /// Ends `job`'s registration and runs `finish`, unless a cancel claimed
    /// the sync first (it has reported it then). Called by the sync task
    /// itself as its last step: `Tasks` offers only `abort` to deregister,
    /// and aborting the running task takes effect at its next await, which
    /// never comes because the task returns right after.
    fn settle(&self, job: JobId, finish: impl FnOnce()) {
        let _handoff = handoff();
        if self.syncs.abort(&job) {
            finish();
        }
    }

    /// Stops `job`'s sync task when it still runs (which kills a running
    /// rsync) and reports the sync canceled; otherwise its engine job was
    /// submitted, and the engine cancels it.
    fn cancel(&self, job: JobId) {
        let _handoff = handoff();
        if self.syncs.abort(&job) {
            report(
                &self.events,
                BgEvent::SyncFinished {
                    job,
                    result: Err(Error::Canceled),
                },
            );
        } else {
            if let Err(error) =
                forward(&self.engine, TransferCommand::Cancel(QueueTarget::Job(job)))
            {
                report(&self.events, BgEvent::TransferCommandFailed(error));
            }
        }
    }
}

fn handoff() -> MutexGuard<'static, ()> {
    // The guard protects no data, so a poisoned lock is still usable.
    HANDOFF.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use tokio::sync::mpsc::{self, UnboundedReceiver};

    use super::*;
    use crate::connections::RemoteTools;
    use crate::error::{ConnectError, FsError};
    use crate::settings::Settings;
    use crate::state::PartialsFile;
    use crate::sync::DiffStatus;
    use crate::transfer::model::{JobSummary, TransferCommandRx, TransferEvent};
    use crate::transfer::stream::test_support::{MemFs, Op};
    use crate::ui::effects::transfer::start_engine;
    use crate::ui::effects::transfer::test_support::{
        next_event, remote, FixtureConnections, TestConnections, Tools,
    };
    use crate::vfs::Fs;

    const JOB: JobId = JobId(7);

    struct Harness {
        syncer: Syncer,
        connections: Arc<TestConnections>,
        events: UnboundedReceiver<BgEvent>,
        engine: TransferCommandRx,
    }

    fn harness(fs: &[(Endpoint, Arc<MemFs>)], tools: Tools) -> Harness {
        let fs = fs
            .iter()
            .map(|(endpoint, fs)| (endpoint.clone(), Arc::clone(fs) as Arc<dyn Fs>))
            .collect();
        let connections = Arc::new(TestConnections::new(fs, tools));
        let (events_tx, events) = mpsc::unbounded_channel();
        let (engine_tx, engine) = mpsc::unbounded_channel();
        Harness {
            syncer: Syncer {
                connections: Arc::clone(&connections) as Arc<dyn Connections>,
                events: events_tx,
                engine: engine_tx,
                syncs: Arc::new(Tasks::new()),
            },
            connections,
            events,
            engine,
        }
    }

    fn no_rsync() -> Tools {
        Tools::Ready(Ok(RemoteTools {
            tar: true,
            rsync_ok: false,
        }))
    }

    fn at(endpoint: &Endpoint, path: &str) -> Location {
        Location {
            endpoint: endpoint.clone(),
            path: PathBuf::from(path),
        }
    }

    /// Local `/l` and remote `/r`: `new.txt` only on the left, `same.txt`
    /// on both, `extra.txt` and `gone/inner.txt` only on the right.
    fn trees() -> Vec<(Endpoint, Arc<MemFs>)> {
        let local = MemFs::new(Endpoint::Local);
        local.add_dir("/l");
        local.add_file("/l/new.txt", b"new", 100, 0o644);
        local.add_file("/l/same.txt", b"same", 100, 0o644);
        let server = remote("box");
        let far = MemFs::new(server.clone());
        far.add_dir("/r");
        far.add_file("/r/same.txt", b"same", 100, 0o644);
        far.add_file("/r/extra.txt", b"extra", 100, 0o644);
        far.add_dir("/r/gone");
        far.add_file("/r/gone/inner.txt", b"inner", 100, 0o644);
        vec![(Endpoint::Local, local), (server, far)]
    }

    async fn comparison(harness: &Harness) -> Box<Comparison> {
        compare_trees(
            &*harness.syncer.connections,
            at(&Endpoint::Local, "/l"),
            at(&remote("box"), "/r"),
            0,
        )
        .await
        .expect("compare")
    }

    fn request(
        comparison: Box<Comparison>,
        paths: &[&str],
        delete_extraneous: bool,
    ) -> SyncRequest {
        SyncRequest {
            job: JOB,
            direction: SyncDirection::LeftToRight,
            comparison,
            paths: paths.iter().map(PathBuf::from).collect(),
            delete_extraneous,
        }
    }

    async fn next_command(engine: &mut TransferCommandRx) -> TransferCommand {
        tokio::time::timeout(Duration::from_secs(10), engine.recv())
            .await
            .expect("a command in time")
            .expect("the engine channel is open")
    }

    /// Waits until `job` is no longer registered under `syncs`.
    async fn settled(syncs: &Tasks<JobId>, job: JobId) {
        for _ in 0..1000 {
            if !syncs.contains(&job) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("the sync task did not settle");
    }

    /// The next event, which must be `SyncFinished`.
    async fn sync_finished(events: &mut UnboundedReceiver<BgEvent>) -> (JobId, Result<(), Error>) {
        match next_event(events).await {
            BgEvent::SyncFinished { job, result } => (job, result),
            other => panic!("expected SyncFinished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn compare_lists_both_sides_on_their_browse_channels() {
        let fs = trees();
        let harness = harness(&fs, no_rsync());
        let comparison = comparison(&harness).await;
        let statuses: Vec<(&Path, DiffStatus)> = comparison
            .entries
            .iter()
            .map(|entry| (entry.relative.as_path(), entry.status))
            .collect();
        assert_eq!(
            statuses,
            [
                (Path::new("extra.txt"), DiffStatus::RightOnly),
                (Path::new("gone"), DiffStatus::RightOnly),
                (Path::new("gone/inner.txt"), DiffStatus::RightOnly),
                (Path::new("new.txt"), DiffStatus::LeftOnly),
                (Path::new("same.txt"), DiffStatus::Same),
            ]
        );
    }

    #[tokio::test]
    async fn a_listing_failure_is_the_comparison_result() {
        let fs = trees();
        let denied = FsError::PermissionDenied {
            path: PathBuf::from("/r"),
        };
        fs[1].1.fail(Op::List, "/r", denied.clone(), false);
        let harness = harness(&fs, no_rsync());
        let result = compare_trees(
            &*harness.syncer.connections,
            at(&Endpoint::Local, "/l"),
            at(&remote("box"), "/r"),
            0,
        )
        .await;
        assert_eq!(result, Err(Error::Fs(denied)));
    }

    #[tokio::test]
    async fn without_rsync_the_engine_job_is_submitted() {
        let fs = trees();
        let mut harness = harness(&fs, no_rsync());
        let comparison = comparison(&harness).await;
        let expected = sync::engine_job(
            JOB,
            SyncDirection::LeftToRight,
            &comparison,
            &[PathBuf::from("new.txt"), PathBuf::from("extra.txt")],
        );
        harness
            .syncer
            .clone()
            .start(request(comparison, &["new.txt", "extra.txt"], false));
        assert_eq!(
            next_command(&mut harness.engine).await,
            TransferCommand::Submit(expected)
        );
        settled(&harness.syncer.syncs, JOB).await;
        // Without `delete_extraneous` the destination-only file stays.
        assert!(fs[1].1.exists("/r/extra.txt"));
        assert!(harness.events.try_recv().is_err(), "no SyncFinished");
        assert_eq!(harness.connections.probes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_tool_probe_takes_the_engine_path() {
        let fs = trees();
        let probe_error = Error::Connect(ConnectError::Other {
            detail: "probe failed".to_owned(),
        });
        let mut harness = harness(&fs, Tools::Ready(Err(probe_error)));
        let comparison = comparison(&harness).await;
        harness
            .syncer
            .clone()
            .start(request(comparison, &["new.txt"], false));
        assert!(matches!(
            next_command(&mut harness.engine).await,
            TransferCommand::Submit(_)
        ));
    }

    #[tokio::test]
    async fn two_local_sides_are_never_probed() {
        let local = MemFs::new(Endpoint::Local);
        local.add_dir("/a");
        local.add_dir("/b");
        local.add_file("/a/f", b"f", 1, 0o644);
        let mut harness = harness(&[(Endpoint::Local, local)], Tools::Never);
        let comparison = compare_trees(
            &*harness.syncer.connections,
            at(&Endpoint::Local, "/a"),
            at(&Endpoint::Local, "/b"),
            0,
        )
        .await
        .expect("compare");
        harness
            .syncer
            .clone()
            .start(request(comparison, &["f"], false));
        assert!(matches!(
            next_command(&mut harness.engine).await,
            TransferCommand::Submit(_)
        ));
        assert_eq!(harness.connections.probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn extraneous_paths_are_deleted_before_the_job_is_submitted() {
        let fs = trees();
        let mut harness = harness(&fs, no_rsync());
        let comparison = comparison(&harness).await;
        harness.syncer.clone().start(request(
            comparison,
            &["new.txt", "extra.txt", "gone", "gone/inner.txt"],
            true,
        ));
        let TransferCommand::Submit(job) = next_command(&mut harness.engine).await else {
            panic!("expected Submit");
        };
        assert_eq!(job.id, JOB);
        assert!(!fs[1].1.exists("/r/extra.txt"));
        assert!(!fs[1].1.exists("/r/gone/inner.txt"));
        assert!(!fs[1].1.exists("/r/gone"));
        assert!(fs[1].1.exists("/r/same.txt"));
    }

    #[tokio::test]
    async fn a_failed_deletion_finishes_the_sync_and_submits_nothing() {
        let fs = trees();
        let denied = FsError::PermissionDenied {
            path: PathBuf::from("/r/extra.txt"),
        };
        fs[1]
            .1
            .fail(Op::RemoveFile, "/r/extra.txt", denied.clone(), false);
        let mut harness = harness(&fs, no_rsync());
        let comparison = comparison(&harness).await;
        harness
            .syncer
            .clone()
            .start(request(comparison, &["new.txt", "extra.txt"], true));
        assert_eq!(
            sync_finished(&mut harness.events).await,
            (JOB, Err(Error::Fs(denied)))
        );
        settled(&harness.syncer.syncs, JOB).await;
        assert!(harness.engine.try_recv().is_err(), "nothing submitted");
    }

    #[tokio::test]
    async fn canceling_during_the_probe_stops_the_sync() {
        let fs = trees();
        let mut harness = harness(&fs, Tools::Never);
        let comparison = comparison(&harness).await;
        harness
            .syncer
            .clone()
            .start(request(comparison, &["new.txt"], false));
        while harness.connections.probes.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        harness.syncer.cancel(JOB);
        assert_eq!(
            sync_finished(&mut harness.events).await,
            (JOB, Err(Error::Canceled))
        );
        assert!(!harness.syncer.syncs.contains(&JOB));
        // The aborted task never submits.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(harness.engine.try_recv().is_err());
    }

    #[tokio::test]
    async fn canceling_a_submitted_sync_cancels_its_engine_job() {
        let fs = trees();
        let mut harness = harness(&fs, no_rsync());
        let comparison = comparison(&harness).await;
        harness
            .syncer
            .clone()
            .start(request(comparison, &["new.txt"], false));
        assert!(matches!(
            next_command(&mut harness.engine).await,
            TransferCommand::Submit(_)
        ));
        settled(&harness.syncer.syncs, JOB).await;
        harness.syncer.cancel(JOB);
        assert_eq!(
            next_command(&mut harness.engine).await,
            TransferCommand::Cancel(QueueTarget::Job(JOB))
        );
        assert!(harness.events.try_recv().is_err(), "no SyncFinished");
    }

    /// Relayed events until the engine job `job` finished.
    async fn job_summary(events: &mut UnboundedReceiver<BgEvent>, job: JobId) -> JobSummary {
        loop {
            match next_event(events).await {
                BgEvent::Transfer(TransferEvent::JobFinished { job: done, summary })
                    if done == job =>
                {
                    return summary;
                }
                BgEvent::Transfer(TransferEvent::JobFailed { job: failed, error })
                    if failed == job =>
                {
                    panic!("job failed: {error}");
                }
                BgEvent::Transfer(_) => {}
                other => panic!("unexpected event {other:?}"),
            }
        }
    }

    /// `harness`, but its syncs go to a real engine reporting to the same
    /// event channel.
    fn with_engine(mut harness: Harness) -> Harness {
        let partials = Arc::new(PartialsFile::open(None).expect("in-memory partials"));
        harness.syncer.engine = start_engine(
            &Settings::default(),
            Arc::clone(&harness.syncer.connections),
            partials,
            harness.syncer.events.clone(),
        );
        harness
    }

    #[tokio::test]
    async fn an_engine_path_sync_copies_through_the_engine() {
        let fs = trees();
        let mut harness = with_engine(harness(&fs, no_rsync()));
        let comparison = comparison(&harness).await;
        harness
            .syncer
            .clone()
            .start(request(comparison, &["new.txt", "extra.txt", "gone"], true));
        let summary = job_summary(&mut harness.events, JOB).await;
        assert_eq!(
            summary,
            JobSummary {
                done: 1,
                skipped: 0,
                failed: 0
            }
        );
        assert_eq!(fs[1].1.data("/r/new.txt").as_deref(), Some(&b"new"[..]));
        assert!(!fs[1].1.exists("/r/extra.txt"));
        assert!(!fs[1].1.exists("/r/gone"));
    }

    #[tokio::test]
    async fn a_sync_without_files_to_copy_still_finishes() {
        let fs = trees();
        let mut harness = with_engine(harness(&fs, no_rsync()));
        let comparison = comparison(&harness).await;
        harness
            .syncer
            .clone()
            .start(request(comparison, &["extra.txt"], true));
        let summary = job_summary(&mut harness.events, JOB).await;
        assert_eq!(summary, JobSummary::default());
        assert!(!fs[1].1.exists("/r/extra.txt"));
    }

    /// A sync to it-modern-plain through the executor: rsync when the local
    /// rsync qualifies (Linux with rsync 3.1+), else the engine path.
    #[tokio::test]
    #[ignore = "needs the docker sshd fixtures (tests/it/fixtures/up.sh)"]
    async fn docker_sync_to_it_modern_plain() {
        let connections = Arc::new(FixtureConnections::new());
        let endpoint = remote("it-modern-plain");
        let remote_dir = connections.run(&endpoint, &["mktemp", "-d"]).await;
        connections
            .run(
                &endpoint,
                &[
                    "sh",
                    "-c",
                    "echo extra > \"$1/extra.txt\"",
                    "sh",
                    &remote_dir,
                ],
            )
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("a.txt"), b"alpha\n").expect("write");
        std::fs::write(dir.path().join("sub/b \u{7a7a}.txt"), b"beta\n").expect("write");
        let left = Location {
            endpoint: Endpoint::Local,
            path: dir.path().to_path_buf(),
        };
        let right = Location {
            endpoint: endpoint.clone(),
            path: PathBuf::from(&remote_dir),
        };

        let (events_tx, mut events) = mpsc::unbounded_channel();
        let syncer = Syncer {
            connections: Arc::clone(&connections) as Arc<dyn Connections>,
            events: events_tx.clone(),
            engine: start_engine(
                &Settings::default(),
                Arc::clone(&connections) as Arc<dyn Connections>,
                Arc::new(PartialsFile::open(None).expect("in-memory partials")),
                events_tx,
            ),
            syncs: Arc::new(Tasks::new()),
        };
        let comparison = compare_trees(&*connections, left.clone(), right.clone(), 0)
            .await
            .expect("compare");
        let paths: Vec<PathBuf> = comparison
            .entries
            .iter()
            .filter(|entry| entry.status != DiffStatus::Same)
            .map(|entry| entry.relative.clone())
            .collect();
        let rsync = sync::local_rsync_ok().await;
        syncer.clone().start(SyncRequest {
            job: JOB,
            direction: SyncDirection::LeftToRight,
            comparison,
            paths,
            delete_extraneous: true,
        });
        loop {
            match next_event(&mut events).await {
                BgEvent::SyncFinished { job, result } if job == JOB => {
                    assert!(rsync, "only rsync syncs report SyncFinished on success");
                    result.expect("rsync sync");
                    break;
                }
                BgEvent::Transfer(TransferEvent::JobFinished { job, summary }) if job == JOB => {
                    assert!(!rsync, "an rsync sync submits no engine job");
                    assert_eq!(summary.failed, 0);
                    break;
                }
                BgEvent::Transfer(TransferEvent::JobFailed { error, .. }) => {
                    panic!("job failed: {error}");
                }
                BgEvent::Transfer(_) => {}
                other => panic!("unexpected event {other:?}"),
            }
        }

        let after = compare_trees(&*connections, left, right, 0)
            .await
            .expect("compare again");
        let differing: Vec<_> = after
            .entries
            .iter()
            .filter(|entry| entry.status != DiffStatus::Same)
            .collect();
        assert!(differing.is_empty(), "{differing:?}");
        assert_eq!(after.entries.len(), 3, "a.txt, sub, sub/b: {after:?}");
        connections
            .run(&endpoint, &["rm", "-rf", &remote_dir])
            .await;
    }
}
