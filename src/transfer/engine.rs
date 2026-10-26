//! Transfer engine: expands jobs into items, schedules them on
//! `concurrency` slots, applies pause/cancel through `tokio::sync::watch`,
//! raises and resolves conflicts, retries, parks items as
//! `WaitingForReconnect` on lost connections, resumes `.scpto-part` files
//! whose `partials.json` record matches the source, and performs moves
//! (rename within an endpoint, copy-then-delete across endpoints).
//!
//! # Structure
//!
//! [`run`] is a single actor that owns the authoritative queue. Every piece
//! of I/O (expansion, one item, a tar stream, a source-directory removal,
//! discarding a partial) runs in its own task, fetches its handles from
//! `Connections::fs` on `FsChannel::Transfer` when it starts, and reports
//! back to the actor, which alone changes item states and emits
//! `TransferEvent`s. The actor counts running per-file items itself
//! instead of using a semaphore, so paused and conflicting items never hold
//! a slot.
//!
//! # Behavior (plan 2.3, 7.5)
//!
//! - Every queue change is reported as `TransferEvent`s per
//!   `transfer::model`; the app never sees engine internals.
//! - Expansion (on the source's `FsChannel::Transfer`) walks directories
//!   with `lstat`: directories become `ItemKind::Dir` items before their
//!   contents, symlinks `ItemKind::Symlink` (recreated, never followed),
//!   FIFOs/sockets/devices `ItemKind::Special` created `Skipped {
//!   SpecialFile }`. `JobTargets::Pairs` items are used as given, creating
//!   missing parent directories. `JobCreated` carries the mode, which needs
//!   the expanded file count, so it is sent after expansion together with
//!   `ItemsAdded`; an expansion failure is a `JobFailed` that is not
//!   preceded by `JobCreated` (the app allocated the id and correlates).
//! - Mode: `TransferMode::Tar` when the targets are `IntoDirectory`, one
//!   side is `Local`, the other a non-sudo remote whose
//!   `Connections::tools().tar` is true, the expanded file count is at
//!   least `tar-threshold`, every source path and the destination
//!   directory are valid UTF-8 (remote commands are strings), and, for a
//!   download, all sources share one parent directory (one remote `tar -C`);
//!   otherwise `PerFile`. Tar jobs check existing top-level destination
//!   names first and raise `TarConflictRaised` when any exist, then run the
//!   injected [`TarTransfer`].
//! - Directory items merge into an existing directory (or a symlink to
//!   one); a non-directory in the way fails the item. Items wait until
//!   their parent directory item is `Done`; when a directory item fails or
//!   is canceled, its waiting descendants get the same outcome, and
//!   retrying either one re-queues the other.
//! - Per-file items wait for a `concurrency` slot; conflicts use the
//!   destination's actual `lstat` (the object that would be replaced;
//!   handles case-insensitive APFS), then NFC-normalized names from one
//!   listing per destination directory per job. When only the NFC
//!   comparison matches, the item adopts the existing name
//!   (`ItemRenamed`), so Overwrite replaces that file instead of creating
//!   a second, visually identical one. The job's preset or an earlier
//!   "apply to all" answers without asking. `OverwriteIfNewer` overwrites
//!   only when the source mtime exceeds the destination's by more than
//!   `mtime-tolerance` whole seconds; an unknown mtime counts as not newer.
//!   Rename picks the first free `name (N).ext`.
//! - Resume: when a non-empty `.scpto-part` exists and the `PartialStore`
//!   record matches the source size and mtime (read fresh with `lstat`
//!   when the item starts), the conflict offers `Resume`
//!   (`Conflict::partial`) and a non-conflicting item resumes silently;
//!   otherwise the part is restarted from zero. Overwrite restarts from
//!   zero; Resume without a matching part writes from zero. The record is
//!   set before the first byte is written and removed after success or
//!   `DiscardPartial`; sources without a known size and mtime get no
//!   record and never resume.
//! - Pause/cancel: each running item selects on its `watch` control and
//!   drops its work future; the part file is kept. Tar jobs cannot be
//!   paused (tar has no resume); canceling any item of a tar job cancels
//!   the whole job.
//! - `FsError::Disconnected` parks the item as `WaitingForReconnect`; it
//!   re-queues on `TransferCommand::EndpointReconnected` for its job's
//!   source or destination, with fresh handles from `Connections::fs`.
//! - Move: same endpoint → `rename` per file and symlink (falling back to
//!   copy+delete only for `FsError::Unsupported`); across endpoints → copy,
//!   then delete the source of each file and symlink before its item is
//!   `Done`. Source directories are removed only when every item beneath
//!   them is `Done` (and their own source subdirectories were removed);
//!   a failed removal fails the directory item, and retrying it tries
//!   again. Same-endpoint copies use `copy_within` when available (inside
//!   `stream`).
//! - Tar jobs run outside the per-file slots. Their progress (tar stream
//!   bytes) is spread over the job's file items in expansion order, which
//!   approximates the job total; tar does not report per-entry progress. A
//!   failed tar stream fails every unfinished item; a tar move deletes the
//!   sources after a successful stream, and retrying after a failed delete
//!   only retries the deletes.
//! - Closing the command channel stops the engine: running tasks are
//!   dropped (parts and records are kept for the next start) and [`run`]
//!   returns. A dropped event receiver stops it the same way.
//!
//! # Public API
//!
//! ```text
//! pub struct EngineConfig {
//!     pub concurrency: usize,
//!     pub tar_threshold: usize,
//!     pub mtime_tolerance: u64,
//! }
//!
//! impl EngineConfig {
//!     pub fn from_settings(settings: &Settings) -> EngineConfig;
//! }
//!
//! /// The tar streams, implemented in Wave 3 by delegating to
//! /// `transfer::tar::{upload, download}` (same arguments).
//! pub trait TarTransfer: Send + Sync { fn upload(..); fn download(..); }
//!
//! pub async fn run(
//!     config: EngineConfig,
//!     connections: Arc<dyn Connections>,
//!     partials: Arc<dyn PartialStore>,
//!     tar: Arc<dyn TarTransfer>,
//!     commands: TransferCommandRx,
//!     events: TransferEventTx,
//! );
//! ```
//!
//! `tar` is the one addition to the signature frozen for Wave 2:
//! `transfer::tar` is a Wave 3 module, so the engine takes it injected like
//! `Connections` and `PartialStore`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;
use unicode_normalization::UnicodeNormalization;

use crate::connections::{Connections, FsChannel};
use crate::error::{Error, FsError};
use crate::settings::Settings;
use crate::state::PartialStore;
use crate::transfer::model::{
    Conflict, ConflictChoice, ConflictDecision, Item, ItemId, ItemKind, ItemState, JobId, JobKind,
    JobRequest, JobSummary, JobTargets, PartialInfo, PartialRecord, QueueTarget, SkipReason,
    TarConflictChoice, TransferCommand, TransferCommandRx, TransferEvent, TransferEventTx,
    TransferMode,
};
use crate::transfer::stream::{self, CopySpec};
use crate::vfs::{Endpoint, Entry, EntryKind, Fs, Location};

/// Engine tuning taken from the settings file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// Per-file items transferred at the same time (at least one runs).
    pub concurrency: usize,
    /// Minimum expanded file count of a job that uses tar mode.
    pub tar_threshold: usize,
    /// Whole seconds of mtime difference treated as equal.
    pub mtime_tolerance: u64,
}

impl EngineConfig {
    /// The engine-relevant subset of `settings`.
    pub fn from_settings(settings: &Settings) -> EngineConfig {
        EngineConfig {
            concurrency: settings.concurrency,
            tar_threshold: settings.tar_threshold,
            mtime_tolerance: settings.mtime_tolerance,
        }
    }
}

/// The tar streams of `transfer::tar`, injected so the engine can be tested
/// without remote processes. The product implementation delegates to
/// `transfer::tar::upload` and `transfer::tar::download`, whose signatures
/// these methods mirror.
pub trait TarTransfer: Send + Sync {
    /// Packs `sources` (absolute local paths) into remote `remote_dir`.
    fn upload<'a>(
        &'a self,
        connections: &'a dyn Connections,
        sources: &'a [PathBuf],
        remote: &'a Endpoint,
        remote_dir: &'a Path,
        progress: &'a (dyn Fn(u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), Error>>;

    /// Unpacks `names` (entries of remote `remote_dir`) into `local_dir`.
    fn download<'a>(
        &'a self,
        connections: &'a dyn Connections,
        remote: &'a Endpoint,
        remote_dir: &'a Path,
        names: &'a [OsString],
        local_dir: &'a Path,
        progress: &'a (dyn Fn(u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), Error>>;
}

/// Runs the engine until `commands` is closed (or `events` has no receiver)
/// and every task ended. Startup spawns it on the runtime.
pub async fn run(
    config: EngineConfig,
    connections: Arc<dyn Connections>,
    partials: Arc<dyn PartialStore>,
    tar: Arc<dyn TarTransfer>,
    mut commands: TransferCommandRx,
    events: TransferEventTx,
) {
    let (reports, mut inbox) = mpsc::unbounded_channel();
    let mut engine = Engine {
        config,
        shared: Shared {
            connections,
            partials,
            tar,
            reports,
        },
        events,
        jobs: BTreeMap::new(),
        slots: HashMap::new(),
        ready: VecDeque::new(),
        running: 0,
        next_item: 0,
        tasks: JoinSet::new(),
    };
    engine.drive(&mut commands, &mut inbox).await;
    engine.tasks.shutdown().await;
}

/// Interval between two tar progress reports.
const TAR_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Desired state of a running task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Run,
    Pause,
    Cancel,
}

/// Why a running task stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Pause,
    Cancel,
}

/// Resolves when the actor asks the task to stop.
async fn stopped(control: &mut watch::Receiver<Control>) -> Stop {
    let stop = match control.wait_for(|value| *value != Control::Run).await {
        Ok(value) if *value == Control::Pause => Some(Stop::Pause),
        Ok(_) => Some(Stop::Cancel),
        // The actor keeps the sender until it handled this task's report;
        // it only goes away when the engine shuts down and aborts the task.
        Err(_) => None,
    };
    match stop {
        Some(stop) => stop,
        None => std::future::pending().await,
    }
}

/// How a per-file item task ended.
#[derive(Debug)]
enum Outcome {
    Done,
    Skipped(SkipReason),
    Conflict(Box<Conflict>),
    Failed(Error),
    Stopped(Stop),
}

/// How a tar task ended.
#[derive(Debug)]
enum TarOutcome {
    /// The data was transferred (or only deletes were retried); the listed
    /// items failed to delete their source after a move.
    Streamed(Vec<(ItemId, Error)>),
    Failed(Error),
    Canceled,
}

/// One item found by expansion, before it gets an id.
#[derive(Debug)]
struct Found {
    kind: ItemKind,
    source: PathBuf,
    destination: PathBuf,
    size: u64,
    mtime: Option<i64>,
    mode: Option<u32>,
    /// Index of the parent directory in the expansion list.
    parent: Option<usize>,
}

/// Result of expanding a job.
#[derive(Debug)]
struct Expansion {
    /// Present when the job runs in tar mode.
    tar: Option<TarTargets>,
    found: Vec<Found>,
}

/// The arguments of a job's tar stream.
#[derive(Debug, Clone)]
enum TarTargets {
    /// Local sources packed into `directory` on `remote`.
    Upload {
        remote: Endpoint,
        sources: Vec<PathBuf>,
        directory: PathBuf,
    },
    /// Entries `names` of `remote_dir` on `remote` unpacked into the local
    /// `directory`.
    Download {
        remote: Endpoint,
        remote_dir: PathBuf,
        names: Vec<OsString>,
        directory: PathBuf,
    },
}

/// Messages from tasks to the actor.
#[derive(Debug)]
enum Report {
    Expanded {
        job: JobId,
        result: Result<Expansion, Error>,
    },
    Progress {
        item: ItemId,
        bytes: u64,
    },
    Renamed {
        item: ItemId,
        destination: PathBuf,
    },
    ItemEnded {
        item: ItemId,
        outcome: Outcome,
    },
    SourceDirRemoved {
        item: ItemId,
        result: Result<(), Error>,
    },
    PartialDiscarded {
        item: ItemId,
        result: Result<(), Error>,
    },
    TarChecked {
        job: JobId,
        result: Result<Vec<PathBuf>, Error>,
    },
    TarProgress {
        job: JobId,
        bytes: u64,
    },
    TarEnded {
        job: JobId,
        outcome: TarOutcome,
    },
}

type Reports = mpsc::UnboundedSender<Report>;

fn report(reports: &Reports, message: Report) {
    // The actor holds the receiver until every task has been aborted, so a
    // failed send only happens during shutdown, when nobody wants the
    // report any more.
    let _ = reports.send(message);
}

/// The event receiver is gone; the engine stops.
#[derive(Debug)]
struct Closed;

/// Handles every task needs.
#[derive(Clone)]
struct Shared {
    connections: Arc<dyn Connections>,
    partials: Arc<dyn PartialStore>,
    tar: Arc<dyn TarTransfer>,
    reports: Reports,
}

/// NFC-normalized names of destination directories, listed once per job.
#[derive(Default)]
struct NameIndex {
    dirs: Mutex<HashMap<PathBuf, Arc<HashMap<String, OsString>>>>,
}

impl NameIndex {
    /// The actual name of the entry in `dir` whose NFC form equals that of
    /// `name`, if any. A missing directory has no entries.
    async fn lookup(&self, fs: &dyn Fs, dir: &Path, name: &str) -> Result<Option<OsString>, Error> {
        let key: String = name.nfc().collect();
        let cached = self
            .dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(dir)
            .cloned();
        let names = match cached {
            Some(names) => names,
            None => {
                let entries = match fs.list(dir).await {
                    Ok(entries) => entries,
                    Err(FsError::NotFound { .. }) => Vec::new(),
                    Err(error) => return Err(error.into()),
                };
                let mut names = HashMap::new();
                for entry in entries {
                    if let Some(text) = entry.name.to_str() {
                        names
                            .entry(text.nfc().collect::<String>())
                            .or_insert_with(|| entry.name.clone());
                    }
                }
                self.dirs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(dir.to_path_buf())
                    .or_insert_with(|| Arc::new(names))
                    .clone()
            }
        };
        Ok(names.get(&key).cloned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Expanding,
    /// Expansion failed; the job has no items.
    Failed,
    /// Items run per file, or a tar job waits for a command.
    Idle,
    TarChecking,
    TarWaiting,
    TarRunning,
}

struct Job {
    kind: JobKind,
    source: Endpoint,
    destination: Endpoint,
    pairs: bool,
    /// Present for tar-mode jobs.
    tar: Option<TarTargets>,
    phase: Phase,
    items: Vec<ItemId>,
    sticky: Option<ConflictChoice>,
    /// Pause or cancel requested while the job was still expanding.
    arrival: Control,
    /// Job-level tasks in flight (directory removals, discards).
    pending: usize,
    finished_reported: bool,
    names: Arc<NameIndex>,
    tar_control: Option<watch::Sender<Control>>,
    tar_cancel_requested: bool,
    /// The tar stream succeeded; a retry only retries source deletes.
    tar_streamed: bool,
    /// File items fully covered by the tar progress so far.
    tar_progress_cursor: usize,
}

struct Slot {
    item: Item,
    parent: Option<ItemId>,
    children: Vec<ItemId>,
    decision: Option<ConflictChoice>,
    control: Option<watch::Sender<Control>>,
    stop_request: Option<Stop>,
    /// Move jobs: children whose source is not yet gone.
    incomplete_children: usize,
    /// Move jobs: this item's source is gone.
    source_gone: bool,
    discarding: bool,
}

/// What an incoming wake-up of the actor is about.
enum Wake {
    Command(Option<TransferCommand>),
    Report(Report),
    Joined(Result<(), JoinError>),
}

struct Engine {
    config: EngineConfig,
    shared: Shared,
    events: TransferEventTx,
    jobs: BTreeMap<JobId, Job>,
    slots: HashMap<ItemId, Slot>,
    /// Queued items that may be eligible to start, in order.
    ready: VecDeque<ItemId>,
    running: usize,
    next_item: u64,
    tasks: JoinSet<()>,
}

impl Engine {
    async fn drive(
        &mut self,
        commands: &mut TransferCommandRx,
        inbox: &mut mpsc::UnboundedReceiver<Report>,
    ) {
        loop {
            let wake = tokio::select! {
                command = commands.recv() => Wake::Command(command),
                Some(message) = inbox.recv() => Wake::Report(message),
                Some(joined) = self.tasks.join_next(), if !self.tasks.is_empty() => Wake::Joined(joined),
            };
            let step = match wake {
                Wake::Command(None) => return,
                Wake::Command(Some(command)) => self.command(command).await,
                Wake::Report(message) => self.handle_report(message).await,
                Wake::Joined(Err(error)) if error.is_panic() => {
                    std::panic::resume_unwind(error.into_panic())
                }
                // Tasks are only aborted by the shutdown after this loop.
                Wake::Joined(_) => Ok(()),
            };
            if step.is_err() || self.schedule().await.is_err() {
                return;
            }
        }
    }

    async fn emit(&self, event: TransferEvent) -> Result<(), Closed> {
        self.events.send(event).await.map_err(|_| Closed)
    }

    async fn set_state(&mut self, id: ItemId, state: ItemState) -> Result<(), Closed> {
        let Some(slot) = self.slots.get_mut(&id) else {
            return Ok(());
        };
        if slot.item.state == state {
            return Ok(());
        }
        slot.item.state = state.clone();
        self.emit(TransferEvent::ItemChanged { item: id, state })
            .await
    }

    fn state(&self, id: ItemId) -> Option<&ItemState> {
        self.slots.get(&id).map(|slot| &slot.item.state)
    }

    fn job_of(&self, id: ItemId) -> Option<JobId> {
        self.slots.get(&id).map(|slot| slot.item.job)
    }

    fn is_tar_job(&self, job: JobId) -> bool {
        self.jobs.get(&job).is_some_and(|job| job.tar.is_some())
    }

    fn targets(&self, target: QueueTarget) -> Vec<ItemId> {
        match target {
            QueueTarget::Item(id) if self.slots.contains_key(&id) => vec![id],
            QueueTarget::Item(_) => Vec::new(),
            QueueTarget::Job(job) => self
                .jobs
                .get(&job)
                .map(|job| job.items.clone())
                .unwrap_or_default(),
        }
    }

    // ----- commands -----

    async fn command(&mut self, command: TransferCommand) -> Result<(), Closed> {
        match command {
            TransferCommand::Submit(request) => {
                self.submit(request);
                Ok(())
            }
            TransferCommand::Pause(target) => self.pause(target).await,
            TransferCommand::Resume(target) => self.resume(target).await,
            TransferCommand::Cancel(target) => self.cancel(target).await,
            TransferCommand::Retry(target) => {
                let ids = self.targets(target);
                self.retry(ids, true).await
            }
            TransferCommand::RetryAllFailed => {
                let ids: Vec<ItemId> = self
                    .jobs
                    .values()
                    .flat_map(|job| job.items.clone())
                    .collect();
                self.retry(ids, false).await
            }
            TransferCommand::DiscardPartial(target) => {
                self.discard(target);
                Ok(())
            }
            TransferCommand::ResolveConflict { item, decision } => {
                self.resolve_conflict(item, decision).await
            }
            TransferCommand::ResolveTarConflict { job, choice } => {
                self.resolve_tar_conflict(job, choice).await
            }
            TransferCommand::EndpointReconnected(endpoint) => self.reconnected(&endpoint).await,
            TransferCommand::ClearFinished => self.clear_finished().await,
        }
    }

    fn submit(&mut self, request: JobRequest) {
        // Job ids are allocated by the app and unique (transfer::model).
        debug_assert!(!self.jobs.contains_key(&request.id), "duplicate job id");
        let job = Job {
            kind: request.kind,
            source: request.source.clone(),
            destination: request.destination.clone(),
            pairs: matches!(request.targets, JobTargets::Pairs(_)),
            tar: None,
            phase: Phase::Expanding,
            items: Vec::new(),
            sticky: request.preset,
            arrival: Control::Run,
            pending: 0,
            finished_reported: false,
            names: Arc::default(),
            tar_control: None,
            tar_cancel_requested: false,
            tar_streamed: false,
            tar_progress_cursor: 0,
        };
        self.jobs.insert(request.id, job);
        let shared = self.shared.clone();
        let tar_threshold = self.config.tar_threshold;
        self.tasks.spawn(async move {
            let result = expand(&*shared.connections, &request, tar_threshold).await;
            report(
                &shared.reports,
                Report::Expanded {
                    job: request.id,
                    result,
                },
            );
        });
    }

    async fn pause(&mut self, target: QueueTarget) -> Result<(), Closed> {
        if let QueueTarget::Job(job) = target {
            if let Some(job) = self.jobs.get_mut(&job) {
                if job.phase == Phase::Expanding && job.arrival == Control::Run {
                    job.arrival = Control::Pause;
                }
            }
        }
        for id in self.targets(target) {
            let Some(job) = self.job_of(id) else { continue };
            if self.is_tar_job(job) {
                continue;
            }
            match self.state(id) {
                Some(ItemState::Queued) => self.set_state(id, ItemState::Paused).await?,
                Some(ItemState::Running { .. }) => self.request_stop(id, Stop::Pause),
                _ => {}
            }
        }
        Ok(())
    }

    fn request_stop(&mut self, id: ItemId, stop: Stop) {
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        if slot.stop_request != Some(Stop::Cancel) {
            slot.stop_request = Some(stop);
        }
        if let Some(control) = &slot.control {
            control.send_replace(match slot.stop_request {
                Some(Stop::Cancel) => Control::Cancel,
                _ => Control::Pause,
            });
        }
    }

    async fn resume(&mut self, target: QueueTarget) -> Result<(), Closed> {
        if let QueueTarget::Job(job) = target {
            if let Some(job) = self.jobs.get_mut(&job) {
                if job.phase == Phase::Expanding && job.arrival == Control::Pause {
                    job.arrival = Control::Run;
                }
            }
        }
        for id in self.targets(target) {
            if self.state(id) == Some(&ItemState::Paused) {
                self.queue(id).await?;
            }
        }
        Ok(())
    }

    /// Marks an item `Queued` and offers it to the scheduler.
    async fn queue(&mut self, id: ItemId) -> Result<(), Closed> {
        self.set_state(id, ItemState::Queued).await?;
        self.ready.push_back(id);
        if let Some(job) = self.job_of(id).and_then(|job| self.jobs.get_mut(&job)) {
            job.finished_reported = false;
        }
        Ok(())
    }

    async fn cancel(&mut self, target: QueueTarget) -> Result<(), Closed> {
        if let QueueTarget::Job(job) = target {
            if let Some(job) = self.jobs.get_mut(&job) {
                if job.phase == Phase::Expanding {
                    job.arrival = Control::Cancel;
                }
            }
        }
        let ids = self.targets(target);
        let mut touched = Vec::new();
        let mut tar_jobs = Vec::new();
        for id in ids {
            let Some(job) = self.job_of(id) else { continue };
            if !touched.contains(&job) {
                touched.push(job);
            }
            if self.is_tar_job(job) {
                if !tar_jobs.contains(&job) {
                    tar_jobs.push(job);
                }
                continue;
            }
            match self.state(id) {
                Some(ItemState::Running { .. }) => self.request_stop(id, Stop::Cancel),
                Some(state) if !state.is_finished() => {
                    let canceled = ItemState::Skipped {
                        reason: SkipReason::Canceled,
                    };
                    self.set_state(id, canceled.clone()).await?;
                    self.finish_descendants(id, canceled).await?;
                }
                _ => {}
            }
        }
        for job in tar_jobs {
            self.cancel_tar_job(job).await?;
        }
        for job in touched {
            self.check_finished(job).await?;
        }
        Ok(())
    }

    async fn cancel_tar_job(&mut self, job_id: JobId) -> Result<(), Closed> {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return Ok(());
        };
        match job.phase {
            Phase::TarRunning => {
                if let Some(control) = &job.tar_control {
                    control.send_replace(Control::Cancel);
                }
            }
            Phase::TarChecking => job.tar_cancel_requested = true,
            Phase::Idle | Phase::TarWaiting => {
                job.phase = Phase::Idle;
                self.cancel_unfinished(job_id).await?;
            }
            Phase::Expanding | Phase::Failed => {}
        }
        Ok(())
    }

    async fn cancel_unfinished(&mut self, job: JobId) -> Result<(), Closed> {
        for id in self.targets(QueueTarget::Job(job)) {
            if self.state(id).is_some_and(|state| !state.is_finished()) {
                self.set_state(
                    id,
                    ItemState::Skipped {
                        reason: SkipReason::Canceled,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    fn retryable(state: &ItemState, canceled_too: bool) -> bool {
        match state {
            ItemState::Failed { .. } => true,
            ItemState::Skipped {
                reason: SkipReason::Canceled,
            } => canceled_too,
            _ => false,
        }
    }

    async fn retry(&mut self, ids: Vec<ItemId>, canceled_too: bool) -> Result<(), Closed> {
        let mut tar_jobs = Vec::new();
        let mut selected = Vec::new();
        for id in ids {
            let Some(slot) = self.slots.get(&id) else {
                continue;
            };
            if slot.discarding || !Engine::retryable(&slot.item.state, canceled_too) {
                continue;
            }
            if self.is_tar_job(slot.item.job) {
                if !tar_jobs.contains(&slot.item.job) {
                    tar_jobs.push(slot.item.job);
                }
                continue;
            }
            // Directories the item depends on, and items that depend on a
            // directory, are retried together.
            let mut ancestor = slot.parent;
            while let Some(parent) = ancestor {
                let parent_slot = &self.slots[&parent];
                if Engine::retryable(&parent_slot.item.state, true) {
                    selected.push(parent);
                }
                ancestor = parent_slot.parent;
            }
            selected.push(id);
            let mut stack = slot.children.clone();
            while let Some(child) = stack.pop() {
                let child_slot = &self.slots[&child];
                if Engine::retryable(&child_slot.item.state, true) {
                    selected.push(child);
                }
                stack.extend(child_slot.children.iter().copied());
            }
        }
        for id in selected {
            let still_retryable = self
                .state(id)
                .is_some_and(|state| Engine::retryable(state, true));
            if still_retryable {
                self.queue(id).await?;
            }
        }
        for job in tar_jobs {
            self.retry_tar_job(job, canceled_too).await?;
        }
        Ok(())
    }

    fn discard(&mut self, target: QueueTarget) {
        for id in self.targets(target) {
            let Some(slot) = self.slots.get_mut(&id) else {
                continue;
            };
            if slot.item.kind != ItemKind::File
                || slot.discarding
                || !Engine::retryable(&slot.item.state, true)
            {
                continue;
            }
            let Some(job) = self.jobs.get_mut(&slot.item.job) else {
                continue;
            };
            if job.tar.is_some() {
                continue;
            }
            slot.discarding = true;
            job.pending += 1;
            let location = Location {
                endpoint: job.destination.clone(),
                path: slot.item.destination.clone(),
            };
            let shared = self.shared.clone();
            self.tasks.spawn(async move {
                let result = discard_partial(&shared, &location).await;
                report(
                    &shared.reports,
                    Report::PartialDiscarded { item: id, result },
                );
            });
        }
    }

    async fn resolve_conflict(
        &mut self,
        item: ItemId,
        decision: ConflictDecision,
    ) -> Result<(), Closed> {
        if self.state(item) != Some(&ItemState::WaitingForConflict) {
            return Ok(());
        }
        let Some(job_id) = self.job_of(item) else {
            return Ok(());
        };
        let mut answered = vec![item];
        if decision.apply_to_all {
            if let Some(job) = self.jobs.get_mut(&job_id) {
                job.sticky = Some(decision.choice);
                for &other in &job.items {
                    if other != item
                        && self.slots[&other].item.state == ItemState::WaitingForConflict
                    {
                        answered.push(other);
                    }
                }
            }
        }
        for id in answered {
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.decision = Some(decision.choice);
            }
            self.queue(id).await?;
        }
        Ok(())
    }

    async fn resolve_tar_conflict(
        &mut self,
        job_id: JobId,
        choice: TarConflictChoice,
    ) -> Result<(), Closed> {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return Ok(());
        };
        if job.phase != Phase::TarWaiting {
            return Ok(());
        }
        match choice {
            TarConflictChoice::OverwriteAll => self.start_tar(job_id, false).await,
            TarConflictChoice::Cancel => {
                job.phase = Phase::Idle;
                self.cancel_unfinished(job_id).await?;
                self.check_finished(job_id).await
            }
        }
    }

    async fn reconnected(&mut self, endpoint: &Endpoint) -> Result<(), Closed> {
        let waiting: Vec<ItemId> = self
            .jobs
            .values()
            .filter(|job| &job.source == endpoint || &job.destination == endpoint)
            .flat_map(|job| job.items.iter().copied())
            .filter(|id| self.state(*id) == Some(&ItemState::WaitingForReconnect))
            .collect();
        for id in waiting {
            self.queue(id).await?;
        }
        Ok(())
    }

    async fn clear_finished(&mut self) -> Result<(), Closed> {
        let cleared: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.finished_reported || job.phase == Phase::Failed)
            .map(|(id, _)| *id)
            .collect();
        if cleared.is_empty() {
            return Ok(());
        }
        for id in &cleared {
            if let Some(job) = self.jobs.remove(id) {
                for item in job.items {
                    self.slots.remove(&item);
                }
            }
        }
        self.emit(TransferEvent::JobsCleared(cleared)).await
    }

    // ----- reports -----

    async fn handle_report(&mut self, message: Report) -> Result<(), Closed> {
        match message {
            Report::Expanded { job, result } => self.expanded(job, result).await,
            Report::Progress { item, bytes } => {
                if matches!(self.state(item), Some(ItemState::Running { .. })) {
                    self.set_state(item, ItemState::Running { bytes }).await?;
                }
                Ok(())
            }
            Report::Renamed { item, destination } => {
                let Some(slot) = self.slots.get_mut(&item) else {
                    return Ok(());
                };
                slot.item.destination = destination.clone();
                self.emit(TransferEvent::ItemRenamed { item, destination })
                    .await
            }
            Report::ItemEnded { item, outcome } => self.item_ended(item, outcome).await,
            Report::SourceDirRemoved { item, result } => {
                self.source_dir_removed(item, result).await
            }
            Report::PartialDiscarded { item, result } => {
                let Some(slot) = self.slots.get_mut(&item) else {
                    return Ok(());
                };
                slot.discarding = false;
                let job = slot.item.job;
                if let Some(job) = self.jobs.get_mut(&job) {
                    job.pending -= 1;
                }
                if let Err(error) = result {
                    self.set_state(item, ItemState::Failed { error }).await?;
                }
                self.check_finished(job).await
            }
            Report::TarChecked { job, result } => self.tar_checked(job, result).await,
            Report::TarProgress { job, bytes } => self.tar_progress(job, bytes).await,
            Report::TarEnded { job, outcome } => self.tar_ended(job, outcome).await,
        }
    }

    async fn expanded(
        &mut self,
        job_id: JobId,
        result: Result<Expansion, Error>,
    ) -> Result<(), Closed> {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return Ok(());
        };
        let expansion = match result {
            Ok(expansion) => expansion,
            Err(error) => {
                job.phase = Phase::Failed;
                return self
                    .emit(TransferEvent::JobFailed { job: job_id, error })
                    .await;
            }
        };
        let tar = expansion.tar.is_some();
        job.tar = expansion.tar;
        job.phase = Phase::Idle;
        let arrival = match (job.arrival, tar) {
            (Control::Cancel, _) => ItemState::Skipped {
                reason: SkipReason::Canceled,
            },
            (Control::Pause, false) => ItemState::Paused,
            _ => ItemState::Queued,
        };
        let move_job = job.kind == JobKind::Move;

        let first = self.next_item;
        self.next_item += expansion.found.len() as u64;
        let ids: Vec<ItemId> = (0..expansion.found.len() as u64)
            .map(|offset| ItemId(first + offset))
            .collect();
        let mut items = Vec::with_capacity(ids.len());
        for (index, found) in expansion.found.into_iter().enumerate() {
            let state = match found.kind {
                ItemKind::Special(_) => ItemState::Skipped {
                    reason: SkipReason::SpecialFile,
                },
                _ => arrival.clone(),
            };
            let item = Item {
                id: ids[index],
                job: job_id,
                kind: found.kind,
                source: found.source,
                destination: found.destination,
                size: found.size,
                mtime: found.mtime,
                mode: found.mode,
                state,
            };
            let parent = found.parent.map(|parent| ids[parent]);
            if let Some(parent) = parent {
                let parent_slot = self.slots.get_mut(&parent).expect("parents come first");
                parent_slot.children.push(item.id);
                if move_job {
                    parent_slot.incomplete_children += 1;
                }
            }
            items.push(item.clone());
            self.slots.insert(
                item.id,
                Slot {
                    item,
                    parent,
                    children: Vec::new(),
                    decision: None,
                    control: None,
                    stop_request: None,
                    incomplete_children: 0,
                    source_gone: false,
                    discarding: false,
                },
            );
        }
        let job = self.jobs.get_mut(&job_id).expect("checked above");
        job.items = ids.clone();
        let created = TransferEvent::JobCreated {
            job: job_id,
            kind: job.kind,
            mode: if tar {
                TransferMode::Tar
            } else {
                TransferMode::PerFile
            },
            source: job.source.clone(),
            destination: job.destination.clone(),
        };
        self.emit(created).await?;
        self.emit(TransferEvent::ItemsAdded { job: job_id, items })
            .await?;
        if tar {
            if arrival == ItemState::Queued {
                self.start_tar_check(job_id);
            }
        } else {
            let queued: Vec<ItemId> = ids
                .into_iter()
                .filter(|id| self.state(*id) == Some(&ItemState::Queued))
                .collect();
            self.ready.extend(queued);
        }
        self.check_finished(job_id).await
    }

    async fn item_ended(&mut self, id: ItemId, outcome: Outcome) -> Result<(), Closed> {
        self.running -= 1;
        let Some(slot) = self.slots.get_mut(&id) else {
            return Ok(());
        };
        slot.control = None;
        let stop_request = slot.stop_request.take();
        let job_id = slot.item.job;
        match outcome {
            Outcome::Done => {
                self.set_state(id, ItemState::Done).await?;
                self.item_done(id).await?;
            }
            Outcome::Skipped(reason) => self.set_state(id, ItemState::Skipped { reason }).await?,
            Outcome::Conflict(conflict) => {
                let sticky = self.jobs.get(&job_id).and_then(|job| job.sticky);
                match (stop_request, sticky) {
                    (Some(Stop::Cancel), _) => {
                        self.set_state(
                            id,
                            ItemState::Skipped {
                                reason: SkipReason::Canceled,
                            },
                        )
                        .await?
                    }
                    (Some(Stop::Pause), _) => self.set_state(id, ItemState::Paused).await?,
                    (None, Some(choice)) => {
                        if let Some(slot) = self.slots.get_mut(&id) {
                            slot.decision = Some(choice);
                        }
                        self.queue(id).await?;
                    }
                    (None, None) => {
                        self.set_state(id, ItemState::WaitingForConflict).await?;
                        self.emit(TransferEvent::ConflictRaised(conflict)).await?;
                    }
                }
            }
            Outcome::Failed(Error::Fs(FsError::Disconnected)) => {
                self.set_state(id, ItemState::WaitingForReconnect).await?
            }
            Outcome::Failed(error) => {
                let failed = ItemState::Failed { error };
                self.set_state(id, failed.clone()).await?;
                self.finish_descendants(id, failed).await?;
            }
            Outcome::Stopped(stop) => {
                if stop == Stop::Cancel || stop_request == Some(Stop::Cancel) {
                    let canceled = ItemState::Skipped {
                        reason: SkipReason::Canceled,
                    };
                    self.set_state(id, canceled.clone()).await?;
                    self.finish_descendants(id, canceled).await?;
                } else {
                    self.set_state(id, ItemState::Paused).await?;
                }
            }
        }
        self.check_finished(job_id).await
    }

    /// Gives every unfinished descendant of a directory that did not
    /// succeed the same final state.
    async fn finish_descendants(&mut self, id: ItemId, state: ItemState) -> Result<(), Closed> {
        let mut stack = self.slots[&id].children.clone();
        while let Some(child) = stack.pop() {
            let slot = &self.slots[&child];
            stack.extend(slot.children.iter().copied());
            if !slot.item.state.is_finished() {
                self.set_state(child, state.clone()).await?;
            }
        }
        Ok(())
    }

    /// An item reached `Done`: release its children, and for moves account
    /// for its source being gone.
    async fn item_done(&mut self, id: ItemId) -> Result<(), Closed> {
        let slot = &self.slots[&id];
        let job = &self.jobs[&slot.item.job];
        if slot.item.kind == ItemKind::Dir {
            let children: Vec<ItemId> = slot
                .children
                .iter()
                .copied()
                .filter(|child| self.slots[child].item.state == ItemState::Queued)
                .collect();
            self.ready.extend(children);
        }
        if job.kind != JobKind::Move {
            return Ok(());
        }
        if slot.item.kind == ItemKind::Dir {
            if slot.incomplete_children == 0 {
                self.remove_source_dir(id);
            }
        } else {
            self.source_gone(id);
        }
        Ok(())
    }

    /// Marks a move item's source as gone and removes parent source
    /// directories whose contents are all gone.
    fn source_gone(&mut self, id: ItemId) {
        let slot = self.slots.get_mut(&id).expect("item exists");
        if slot.source_gone {
            return;
        }
        slot.source_gone = true;
        let Some(parent) = slot.parent else { return };
        let parent_slot = self.slots.get_mut(&parent).expect("parent exists");
        parent_slot.incomplete_children -= 1;
        if parent_slot.incomplete_children == 0 && parent_slot.item.state == ItemState::Done {
            self.remove_source_dir(parent);
        }
    }

    fn remove_source_dir(&mut self, id: ItemId) {
        let slot = &self.slots[&id];
        let path = slot.item.source.clone();
        let job = self.jobs.get_mut(&slot.item.job).expect("job exists");
        job.pending += 1;
        let endpoint = job.source.clone();
        let shared = self.shared.clone();
        self.tasks.spawn(async move {
            let result = async {
                let fs = shared
                    .connections
                    .fs(&endpoint, FsChannel::Transfer)
                    .await?;
                fs.remove_dir(&path).await?;
                Ok(())
            }
            .await;
            report(
                &shared.reports,
                Report::SourceDirRemoved { item: id, result },
            );
        });
    }

    async fn source_dir_removed(
        &mut self,
        id: ItemId,
        result: Result<(), Error>,
    ) -> Result<(), Closed> {
        let Some(slot) = self.slots.get(&id) else {
            return Ok(());
        };
        let job = slot.item.job;
        if let Some(job) = self.jobs.get_mut(&job) {
            job.pending -= 1;
        }
        match result {
            Ok(()) => self.source_gone(id),
            Err(error) => self.set_state(id, ItemState::Failed { error }).await?,
        }
        self.check_finished(job).await
    }

    async fn check_finished(&mut self, job_id: JobId) -> Result<(), Closed> {
        let Some(job) = self.jobs.get(&job_id) else {
            return Ok(());
        };
        if job.finished_reported || job.phase != Phase::Idle || job.pending > 0 {
            return Ok(());
        }
        let mut summary = JobSummary::default();
        for id in &job.items {
            match &self.slots[id].item.state {
                ItemState::Done => summary.done += 1,
                ItemState::Skipped { .. } => summary.skipped += 1,
                ItemState::Failed { .. } => summary.failed += 1,
                _ => return Ok(()),
            }
        }
        self.jobs
            .get_mut(&job_id)
            .expect("checked above")
            .finished_reported = true;
        self.emit(TransferEvent::JobFinished {
            job: job_id,
            summary,
        })
        .await
    }

    // ----- per-file scheduling -----

    fn eligible(&self, id: ItemId) -> bool {
        let Some(slot) = self.slots.get(&id) else {
            return false;
        };
        slot.item.state == ItemState::Queued
            && slot
                .parent
                .is_none_or(|parent| self.slots[&parent].item.state == ItemState::Done)
            && !self.is_tar_job(slot.item.job)
    }

    async fn schedule(&mut self) -> Result<(), Closed> {
        while self.running < self.config.concurrency.max(1) {
            let Some(id) = self.ready.pop_front() else {
                break;
            };
            if self.eligible(id) {
                self.start_item(id).await?;
            }
        }
        Ok(())
    }

    async fn start_item(&mut self, id: ItemId) -> Result<(), Closed> {
        let slot = self.slots.get_mut(&id).expect("eligible item exists");
        let job = &self.jobs[&slot.item.job];
        let (control, receiver) = watch::channel(Control::Run);
        slot.control = Some(control);
        slot.stop_request = None;
        let task = ItemTask {
            item: slot.item.clone(),
            kind: job.kind,
            source: job.source.clone(),
            destination: job.destination.clone(),
            create_parents: job.pairs && slot.parent.is_none(),
            decision: slot.decision,
            names: job.names.clone(),
            mtime_tolerance: self.config.mtime_tolerance,
            shared: self.shared.clone(),
        };
        self.running += 1;
        self.tasks.spawn(task.run(receiver));
        self.set_state(id, ItemState::Running { bytes: 0 }).await
    }

    // ----- tar -----

    fn start_tar_check(&mut self, job_id: JobId) {
        let job = self.jobs.get_mut(&job_id).expect("job exists");
        job.phase = Phase::TarChecking;
        job.tar_cancel_requested = false;
        let destinations: Vec<PathBuf> = job
            .items
            .iter()
            .map(|id| &self.slots[id])
            .filter(|slot| slot.parent.is_none())
            .map(|slot| slot.item.destination.clone())
            .collect();
        let endpoint = job.destination.clone();
        let shared = self.shared.clone();
        self.tasks.spawn(async move {
            let result = existing_destinations(&shared, &endpoint, &destinations).await;
            report(
                &shared.reports,
                Report::TarChecked {
                    job: job_id,
                    result,
                },
            );
        });
    }

    async fn tar_checked(
        &mut self,
        job_id: JobId,
        result: Result<Vec<PathBuf>, Error>,
    ) -> Result<(), Closed> {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return Ok(());
        };
        job.phase = Phase::Idle;
        if job.tar_cancel_requested {
            self.cancel_unfinished(job_id).await?;
            return self.check_finished(job_id).await;
        }
        match result {
            Ok(existing) if existing.is_empty() => self.start_tar(job_id, false).await,
            Ok(existing) => {
                job.phase = Phase::TarWaiting;
                for id in self.targets(QueueTarget::Job(job_id)) {
                    if self.state(id) == Some(&ItemState::Queued) {
                        self.set_state(id, ItemState::WaitingForConflict).await?;
                    }
                }
                self.emit(TransferEvent::TarConflictRaised {
                    job: job_id,
                    existing,
                })
                .await
            }
            Err(error) => {
                self.fail_unfinished(job_id, error).await?;
                self.check_finished(job_id).await
            }
        }
    }

    async fn fail_unfinished(&mut self, job: JobId, error: Error) -> Result<(), Closed> {
        for id in self.targets(QueueTarget::Job(job)) {
            if self.state(id).is_some_and(|state| !state.is_finished()) {
                self.set_state(
                    id,
                    ItemState::Failed {
                        error: error.clone(),
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Starts the tar stream of a job (or, with `deletes_only`, only the
    /// source deletes of a move whose stream already succeeded) for its
    /// unfinished items.
    async fn start_tar(&mut self, job_id: JobId, deletes_only: bool) -> Result<(), Closed> {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return Ok(());
        };
        let Some(targets) = job.tar.clone() else {
            return Ok(());
        };
        job.phase = Phase::TarRunning;
        job.tar_progress_cursor = 0;
        let (control, mut receiver) = watch::channel(Control::Run);
        job.tar_control = Some(control);
        let members: Vec<(ItemId, bool, PathBuf)> = job
            .items
            .iter()
            .map(|id| &self.slots[id].item)
            .filter(|item| !item.state.is_finished())
            .map(|item| (item.id, item.kind == ItemKind::Dir, item.source.clone()))
            .collect();
        let plan = TarPlan {
            job: job_id,
            kind: job.kind,
            source: job.source.clone(),
            targets,
            deletes_only,
            members,
        };
        for (id, _, _) in &plan.members {
            self.set_state(*id, ItemState::Running { bytes: 0 }).await?;
        }
        let shared = self.shared.clone();
        self.tasks.spawn(async move {
            let outcome = tokio::select! {
                outcome = plan.run(&shared) => outcome,
                _ = stopped(&mut receiver) => TarOutcome::Canceled,
            };
            report(
                &shared.reports,
                Report::TarEnded {
                    job: job_id,
                    outcome,
                },
            );
        });
        Ok(())
    }

    async fn tar_progress(&mut self, job_id: JobId, bytes: u64) -> Result<(), Closed> {
        let Some(job) = self.jobs.get(&job_id) else {
            return Ok(());
        };
        if job.phase != Phase::TarRunning {
            return Ok(());
        }
        let start = job.tar_progress_cursor;
        let files: Vec<(ItemId, u64)> = job
            .items
            .iter()
            .map(|id| &self.slots[id].item)
            .filter(|item| item.kind == ItemKind::File)
            .map(|item| (item.id, item.size))
            .collect();
        let mut remaining = bytes;
        let mut cursor = 0;
        for (index, (id, size)) in files.iter().enumerate() {
            let share = remaining.min(*size);
            remaining -= share;
            if index >= start && matches!(self.state(*id), Some(ItemState::Running { .. })) {
                self.set_state(*id, ItemState::Running { bytes: share })
                    .await?;
            }
            if share == *size {
                cursor = index + 1;
            }
            if remaining == 0 {
                break;
            }
        }
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.tar_progress_cursor = cursor;
        }
        Ok(())
    }

    async fn tar_ended(&mut self, job_id: JobId, outcome: TarOutcome) -> Result<(), Closed> {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return Ok(());
        };
        job.phase = Phase::Idle;
        job.tar_control = None;
        let running: Vec<ItemId> = job
            .items
            .iter()
            .copied()
            .filter(|id| matches!(self.slots[id].item.state, ItemState::Running { .. }))
            .collect();
        match outcome {
            TarOutcome::Streamed(failures) => {
                job.tar_streamed = true;
                for id in running {
                    let state = match failures.iter().find(|(failed, _)| *failed == id) {
                        Some((_, error)) => ItemState::Failed {
                            error: error.clone(),
                        },
                        None => ItemState::Done,
                    };
                    self.set_state(id, state).await?;
                }
            }
            TarOutcome::Failed(error) => {
                for id in running {
                    self.set_state(
                        id,
                        ItemState::Failed {
                            error: error.clone(),
                        },
                    )
                    .await?;
                }
            }
            TarOutcome::Canceled => {
                for id in running {
                    self.set_state(
                        id,
                        ItemState::Skipped {
                            reason: SkipReason::Canceled,
                        },
                    )
                    .await?;
                }
            }
        }
        self.check_finished(job_id).await
    }

    async fn retry_tar_job(&mut self, job_id: JobId, canceled_too: bool) -> Result<(), Closed> {
        let Some(job) = self.jobs.get(&job_id) else {
            return Ok(());
        };
        if job.phase != Phase::Idle {
            return Ok(());
        }
        let streamed = job.tar_streamed;
        let retried: Vec<ItemId> = job
            .items
            .iter()
            .copied()
            .filter(|id| Engine::retryable(&self.slots[id].item.state, canceled_too))
            .collect();
        if retried.is_empty() {
            return Ok(());
        }
        for id in retried {
            self.queue(id).await?;
        }
        if streamed {
            // The data is at the destination; only source deletes failed.
            return self.start_tar(job_id, true).await;
        }
        self.start_tar_check(job_id);
        Ok(())
    }
}

// ----- expansion and mode -----

/// Expands a job's targets into items and picks its mode.
async fn expand(
    connections: &dyn Connections,
    request: &JobRequest,
    tar_threshold: usize,
) -> Result<Expansion, Error> {
    let fs = connections.fs(&request.source, FsChannel::Transfer).await?;
    let roots: Vec<(PathBuf, PathBuf)> = match &request.targets {
        JobTargets::IntoDirectory { sources, directory } => sources
            .iter()
            .map(|source| {
                let name = source.file_name().ok_or_else(|| FsError::Io {
                    path: source.clone(),
                    message: "has no file name".into(),
                })?;
                Ok((source.clone(), directory.join(name)))
            })
            .collect::<Result<_, Error>>()?,
        JobTargets::Pairs(pairs) => pairs.clone(),
    };
    let mut found = Vec::new();
    for (source, destination) in roots {
        let entry = fs.lstat(&source).await?;
        walk(&*fs, entry, source, destination, &mut found).await?;
    }
    let tar = choose_tar(connections, request, &found, tar_threshold).await?;
    Ok(Expansion { tar, found })
}

/// Adds `entry` and, for a directory, everything beneath it (directories
/// before their contents, names in byte order).
async fn walk(
    fs: &dyn Fs,
    entry: Entry,
    source: PathBuf,
    destination: PathBuf,
    found: &mut Vec<Found>,
) -> Result<(), Error> {
    let mut stack = vec![(entry, source, destination, None)];
    while let Some((entry, source, destination, parent)) = stack.pop() {
        let index = found.len();
        let is_dir = entry.kind == EntryKind::Dir;
        found.push(Found {
            kind: item_kind(&entry.kind),
            source: source.clone(),
            destination: destination.clone(),
            size: if entry.kind == EntryKind::File {
                entry.size.unwrap_or(0)
            } else {
                0
            },
            mtime: entry.mtime,
            mode: entry.mode,
            parent,
        });
        if is_dir {
            let mut children = fs.list(&source).await?;
            children.sort_by(|a, b| b.name.cmp(&a.name));
            for child in children {
                let child_source = source.join(&child.name);
                let child_destination = destination.join(&child.name);
                stack.push((child, child_source, child_destination, Some(index)));
            }
        }
    }
    Ok(())
}

fn item_kind(kind: &EntryKind) -> ItemKind {
    match kind {
        EntryKind::File => ItemKind::File,
        EntryKind::Dir => ItemKind::Dir,
        EntryKind::Symlink { target, .. } => ItemKind::Symlink {
            target: target.clone(),
        },
        EntryKind::Special(kind) => ItemKind::Special(*kind),
    }
}

/// The tar stream a job would use, judged from the request alone
/// (endpoints, target form, UTF-8 paths, one remote parent).
fn tar_targets(request: &JobRequest) -> Option<TarTargets> {
    let JobTargets::IntoDirectory { sources, directory } = &request.targets else {
        return None;
    };
    if directory.to_str().is_none() || sources.iter().any(|source| source.to_str().is_none()) {
        return None;
    }
    match (&request.source, &request.destination) {
        (Endpoint::Local, remote @ Endpoint::Remote { sudo: false, .. }) => {
            Some(TarTargets::Upload {
                remote: remote.clone(),
                sources: sources.clone(),
                directory: directory.clone(),
            })
        }
        (remote @ Endpoint::Remote { sudo: false, .. }, Endpoint::Local) => {
            let remote_dir = sources.first()?.parent()?;
            let mut names = Vec::with_capacity(sources.len());
            for source in sources {
                if source.parent() != Some(remote_dir) {
                    return None;
                }
                names.push(source.file_name()?.to_os_string());
            }
            Some(TarTargets::Download {
                remote: remote.clone(),
                remote_dir: remote_dir.to_path_buf(),
                names,
                directory: directory.clone(),
            })
        }
        _ => None,
    }
}

/// The tar stream of a job when tar mode applies.
async fn choose_tar(
    connections: &dyn Connections,
    request: &JobRequest,
    found: &[Found],
    tar_threshold: usize,
) -> Result<Option<TarTargets>, Error> {
    let Some(targets) = tar_targets(request) else {
        return Ok(None);
    };
    let files = found
        .iter()
        .filter(|found| found.kind == ItemKind::File)
        .count();
    if files < tar_threshold {
        return Ok(None);
    }
    let remote = match &targets {
        TarTargets::Upload { remote, .. } | TarTargets::Download { remote, .. } => remote,
    };
    if connections.tools(remote).await?.tar {
        Ok(Some(targets))
    } else {
        Ok(None)
    }
}

/// The top-level tar destinations that already exist.
async fn existing_destinations(
    shared: &Shared,
    endpoint: &Endpoint,
    destinations: &[PathBuf],
) -> Result<Vec<PathBuf>, Error> {
    let fs = shared.connections.fs(endpoint, FsChannel::Transfer).await?;
    let mut existing = Vec::new();
    for destination in destinations {
        match fs.lstat(destination).await {
            Ok(_) => existing.push(destination.clone()),
            Err(FsError::NotFound { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(existing)
}

/// What a tar task does.
struct TarPlan {
    job: JobId,
    kind: JobKind,
    source: Endpoint,
    targets: TarTargets,
    deletes_only: bool,
    /// The items the task covers: (id, is a directory, source path), in
    /// expansion order. A move deletes their sources after the stream.
    members: Vec<(ItemId, bool, PathBuf)>,
}

impl TarPlan {
    async fn run(self, shared: &Shared) -> TarOutcome {
        if !self.deletes_only {
            if let Err(error) = self.stream(shared).await {
                return TarOutcome::Failed(error);
            }
        }
        if self.kind != JobKind::Move {
            return TarOutcome::Streamed(Vec::new());
        }
        let fs = match shared
            .connections
            .fs(&self.source, FsChannel::Transfer)
            .await
        {
            Ok(fs) => fs,
            Err(error) => {
                return TarOutcome::Streamed(
                    self.members
                        .iter()
                        .map(|(id, _, _)| (*id, error.clone()))
                        .collect(),
                )
            }
        };
        let mut failures = Vec::new();
        for (id, is_dir, path) in self.members.iter().rev() {
            let result = if *is_dir {
                fs.remove_dir(path).await
            } else {
                fs.remove_file(path).await
            };
            if let Err(error) = result {
                failures.push((*id, Error::from(error)));
            }
        }
        TarOutcome::Streamed(failures)
    }

    async fn stream(&self, shared: &Shared) -> Result<(), Error> {
        let job = self.job;
        let reports = shared.reports.clone();
        let last = Mutex::new(None::<Instant>);
        let progress = move |bytes: u64| {
            let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
            if last.is_none_or(|at| at.elapsed() >= TAR_PROGRESS_INTERVAL) {
                *last = Some(Instant::now());
                report(&reports, Report::TarProgress { job, bytes });
            }
        };
        let connections = &*shared.connections;
        match &self.targets {
            TarTargets::Upload {
                remote,
                sources,
                directory,
            } => {
                shared
                    .tar
                    .upload(connections, sources, remote, directory, &progress)
                    .await
            }
            TarTargets::Download {
                remote,
                remote_dir,
                names,
                directory,
            } => {
                shared
                    .tar
                    .download(connections, remote, remote_dir, names, directory, &progress)
                    .await
            }
        }
    }
}

async fn discard_partial(shared: &Shared, destination: &Location) -> Result<(), Error> {
    let fs = shared
        .connections
        .fs(&destination.endpoint, FsChannel::Transfer)
        .await?;
    match fs.remove_file(&stream::part_path(&destination.path)).await {
        Ok(()) | Err(FsError::NotFound { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    shared.partials.remove(destination).await
}

// ----- one per-file item -----

/// Everything one item task needs.
struct ItemTask {
    item: Item,
    kind: JobKind,
    source: Endpoint,
    destination: Endpoint,
    create_parents: bool,
    decision: Option<ConflictChoice>,
    names: Arc<NameIndex>,
    mtime_tolerance: u64,
    shared: Shared,
}

/// How an existing destination is treated after the conflict decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WritePlan {
    /// The destination exists and is replaced.
    replace: bool,
    /// A matching part may be continued.
    resume: bool,
}

impl ItemTask {
    async fn run(self, mut control: watch::Receiver<Control>) {
        let outcome = tokio::select! {
            result = self.attempt() => match result {
                Ok(outcome) => outcome,
                Err(error) => Outcome::Failed(error),
            },
            stop = stopped(&mut control) => Outcome::Stopped(stop),
        };
        report(
            &self.shared.reports,
            Report::ItemEnded {
                item: self.item.id,
                outcome,
            },
        );
    }

    async fn attempt(&self) -> Result<Outcome, Error> {
        let connections = &self.shared.connections;
        let source = connections.fs(&self.source, FsChannel::Transfer).await?;
        let destination = connections
            .fs(&self.destination, FsChannel::Transfer)
            .await?;
        if self.create_parents {
            if let Some(parent) = self.item.destination.parent() {
                create_dir_all(&*destination, parent).await?;
            }
        }
        match &self.item.kind {
            ItemKind::Dir => {
                create_dir(&*destination, &self.item.destination).await?;
                Ok(Outcome::Done)
            }
            ItemKind::Special(_) => Ok(Outcome::Skipped(SkipReason::SpecialFile)),
            ItemKind::File | ItemKind::Symlink { .. } => {
                self.transfer(&*source, &*destination).await
            }
        }
    }

    fn renamed(&self, destination: &Path) {
        report(
            &self.shared.reports,
            Report::Renamed {
                item: self.item.id,
                destination: destination.to_path_buf(),
            },
        );
    }

    fn destination_location(&self, path: &Path) -> Location {
        Location {
            endpoint: self.destination.clone(),
            path: path.to_path_buf(),
        }
    }

    async fn transfer(&self, source: &dyn Fs, destination: &dyn Fs) -> Result<Outcome, Error> {
        let is_file = self.item.kind == ItemKind::File;
        let source_entry = source.lstat(&self.item.source).await?;
        if is_file && source_entry.kind != EntryKind::File {
            return Err(FsError::Io {
                path: self.item.source.clone(),
                message: "is no longer a regular file".into(),
            }
            .into());
        }
        let same_endpoint = self.source == self.destination;
        let renames = self.kind == JobKind::Move && same_endpoint;
        let record = record_of(&source_entry);

        let mut path = self.item.destination.clone();
        let mut write = WritePlan {
            replace: false,
            resume: true,
        };
        if let Some((actual, existing)) = find_existing(destination, &path, &self.names).await? {
            if actual != path {
                path = actual;
                self.renamed(&path);
            }
            let Some(choice) = self.decision else {
                let partial = if is_file && !renames {
                    self.matching_partial(destination, &path, record).await?
                } else {
                    None
                };
                return Ok(Outcome::Conflict(Box::new(Conflict {
                    item: self.item.id,
                    job: self.item.job,
                    source: source_entry,
                    destination: existing,
                    partial,
                })));
            };
            match choice {
                ConflictChoice::Skip => return Ok(Outcome::Skipped(SkipReason::UserSkipped)),
                ConflictChoice::OverwriteIfNewer => {
                    if !is_newer(source_entry.mtime, existing.mtime, self.mtime_tolerance) {
                        return Ok(Outcome::Skipped(SkipReason::NotNewer));
                    }
                    write = WritePlan {
                        replace: true,
                        resume: false,
                    };
                }
                ConflictChoice::Overwrite => {
                    write = WritePlan {
                        replace: true,
                        resume: false,
                    }
                }
                ConflictChoice::Resume => {
                    write = WritePlan {
                        replace: true,
                        resume: true,
                    }
                }
                ConflictChoice::Rename => {
                    path = free_name(destination, &path, &self.names).await?;
                    self.renamed(&path);
                }
            }
        }

        if renames {
            match source.rename(&self.item.source, &path).await {
                Ok(()) => return Ok(Outcome::Done),
                Err(FsError::Unsupported { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        match &self.item.kind {
            ItemKind::Symlink { target } => {
                if write.replace {
                    destination.remove_file(&path).await?;
                }
                destination.symlink(target, &path).await?;
            }
            _ => {
                self.copy(
                    source,
                    destination,
                    &source_entry,
                    &path,
                    record,
                    write.resume,
                )
                .await?
            }
        }
        if self.kind == JobKind::Move {
            source.remove_file(&self.item.source).await?;
        }
        Ok(Outcome::Done)
    }

    /// A non-empty part of `path` whose record matches the source.
    async fn matching_partial(
        &self,
        destination: &dyn Fs,
        path: &Path,
        record: Option<PartialRecord>,
    ) -> Result<Option<PartialInfo>, Error> {
        let Some(record) = record else {
            return Ok(None);
        };
        if self.shared.partials.get(&self.destination_location(path)) != Some(record) {
            return Ok(None);
        }
        match destination.lstat(&stream::part_path(path)).await {
            Ok(part) => Ok(match (part.kind, part.size) {
                (EntryKind::File, Some(size)) if size > 0 && size <= record.source_size => {
                    Some(PartialInfo { size })
                }
                _ => None,
            }),
            Err(FsError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn copy(
        &self,
        source: &dyn Fs,
        destination: &dyn Fs,
        source_entry: &Entry,
        path: &Path,
        record: Option<PartialRecord>,
        resume: bool,
    ) -> Result<(), Error> {
        let partials = &self.shared.partials;
        let location = self.destination_location(path);
        let resume_from = if resume {
            self.matching_partial(destination, path, record)
                .await?
                .map_or(0, |partial| partial.size)
        } else {
            0
        };
        match record {
            Some(record) if partials.get(&location) != Some(record) => {
                partials.set(location.clone(), record).await?
            }
            Some(_) => {}
            None if partials.get(&location).is_some() => partials.remove(&location).await?,
            None => {}
        }
        let id = self.item.id;
        let reports = self.shared.reports.clone();
        let progress = move |bytes: u64| report(&reports, Report::Progress { item: id, bytes });
        stream::copy_file(
            CopySpec {
                source,
                source_path: &self.item.source,
                destination,
                destination_path: path,
                size: source_entry.size.unwrap_or(0),
                mtime: source_entry.mtime,
                mode: source_entry.mode,
                resume_from,
            },
            &progress,
        )
        .await?;
        if partials.get(&location).is_some() {
            partials.remove(&location).await?;
        }
        Ok(())
    }
}

fn record_of(entry: &Entry) -> Option<PartialRecord> {
    Some(PartialRecord {
        source_size: entry.size?,
        source_mtime: entry.mtime?,
    })
}

/// Whether `source` is newer than `destination` by more than `tolerance`
/// whole seconds. Unknown times are never newer.
fn is_newer(source: Option<i64>, destination: Option<i64>, tolerance: u64) -> bool {
    match (source, destination) {
        (Some(source), Some(destination)) => {
            i128::from(source) - i128::from(destination) > i128::from(tolerance)
        }
        _ => false,
    }
}

/// The existing object at `path`: by exact name first, then by NFC name in
/// the same directory. Returns its actual path and attributes.
async fn find_existing(
    fs: &dyn Fs,
    path: &Path,
    names: &NameIndex,
) -> Result<Option<(PathBuf, Entry)>, Error> {
    match fs.lstat(path).await {
        Ok(entry) => return Ok(Some((path.to_path_buf(), entry))),
        Err(FsError::NotFound { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(OsStr::to_str))
    else {
        return Ok(None);
    };
    let Some(actual) = names.lookup(fs, parent, name).await? else {
        return Ok(None);
    };
    if actual == name {
        return Ok(None);
    }
    let actual_path = parent.join(actual);
    match fs.lstat(&actual_path).await {
        Ok(entry) => Ok(Some((actual_path, entry))),
        Err(FsError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// `name (N).ext` for N = 1, 2, ...: the stem is everything before the last
/// dot (a leading dot does not count), so `a.tar.gz` becomes
/// `a.tar (1).gz` and `.bashrc` becomes `.bashrc (1)`.
fn numbered(path: &Path, number: u64) -> PathBuf {
    let mut name = path
        .file_stem()
        .map(OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(" ({number})"));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    path.with_file_name(name)
}

/// The first `name (N).ext` that does not exist (exactly or by NFC name).
async fn free_name(fs: &dyn Fs, path: &Path, names: &NameIndex) -> Result<PathBuf, Error> {
    let mut number = 1;
    loop {
        let candidate = numbered(path, number);
        if find_existing(fs, &candidate, names).await?.is_none() {
            return Ok(candidate);
        }
        number += 1;
    }
}

/// Creates a directory unless a directory (or a symlink to one) is already
/// there.
async fn create_dir(fs: &dyn Fs, path: &Path) -> Result<(), Error> {
    match fs.stat(path).await {
        Ok(entry) if entry.kind == EntryKind::Dir => return Ok(()),
        Ok(_) => {
            return Err(FsError::AlreadyExists {
                path: path.to_path_buf(),
            }
            .into())
        }
        Err(FsError::NotFound { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    match fs.mkdir(path).await {
        Ok(()) => Ok(()),
        // Another item of the job created it in the meantime.
        Err(FsError::AlreadyExists { .. })
            if fs
                .stat(path)
                .await
                .is_ok_and(|entry| entry.kind == EntryKind::Dir) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Creates `path` and its missing ancestors.
async fn create_dir_all(fs: &dyn Fs, path: &Path) -> Result<(), Error> {
    let mut missing = Vec::new();
    let mut current = Some(path);
    while let Some(dir) = current {
        match fs.stat(dir).await {
            Ok(entry) if entry.kind == EntryKind::Dir => break,
            Ok(_) => {
                return Err(FsError::NotADirectory {
                    path: dir.to_path_buf(),
                }
                .into())
            }
            Err(FsError::NotFound { .. }) => missing.push(dir),
            Err(error) => return Err(error.into()),
        }
        current = dir.parent();
    }
    for dir in missing.into_iter().rev() {
        create_dir(fs, dir).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use tokio::sync::{mpsc, Semaphore};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::connections::{RemoteChild, RemoteTools};
    use crate::error::ConnectError;
    use crate::transfer::stream::test_support::{MemFs, Op};
    use crate::vfs::{Capabilities, SpecialKind};

    const WAIT: Duration = Duration::from_secs(5);

    fn web() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        }
    }

    fn web_sudo() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: true,
        }
    }

    struct MockConnections {
        fs: HashMap<Endpoint, Arc<MemFs>>,
        tools: RemoteTools,
    }

    fn mock_error() -> Error {
        Error::Connect(ConnectError::Other {
            detail: "not available in the mock".into(),
        })
    }

    impl Connections for MockConnections {
        fn fs<'a>(
            &'a self,
            endpoint: &'a Endpoint,
            _channel: FsChannel,
        ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
            Box::pin(async move {
                match self.fs.get(endpoint) {
                    Some(fs) => Ok(fs.clone() as Arc<dyn Fs>),
                    None => Err(Error::Connect(ConnectError::ConnectionLost)),
                }
            })
        }

        fn spawn_remote<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _argv: &'a [String],
        ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
            Box::pin(async { Err(mock_error()) })
        }

        fn ssh_command<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _interactive: bool,
        ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
            Box::pin(async { Err(mock_error()) })
        }

        fn tools<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
            Box::pin(async move { Ok(self.tools) })
        }
    }

    #[derive(Default)]
    struct MemPartials {
        records: Mutex<HashMap<Location, PartialRecord>>,
        log: Mutex<Vec<String>>,
    }

    impl MemPartials {
        fn insert(&self, endpoint: Endpoint, path: &str, record: PartialRecord) {
            self.records.lock().expect("lock").insert(
                Location {
                    endpoint,
                    path: path.into(),
                },
                record,
            );
        }

        fn has(&self, endpoint: Endpoint, path: &str) -> bool {
            self.records.lock().expect("lock").contains_key(&Location {
                endpoint,
                path: path.into(),
            })
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().expect("lock").clone()
        }
    }

    impl PartialStore for MemPartials {
        fn get(&self, destination: &Location) -> Option<PartialRecord> {
            self.records.lock().expect("lock").get(destination).copied()
        }

        fn set(
            &self,
            destination: Location,
            record: PartialRecord,
        ) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .expect("lock")
                    .push(format!("set {}", destination.path.display()));
                self.records
                    .lock()
                    .expect("lock")
                    .insert(destination, record);
                Ok(())
            })
        }

        fn remove<'a>(&'a self, destination: &'a Location) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .expect("lock")
                    .push(format!("remove {}", destination.path.display()));
                self.records.lock().expect("lock").remove(destination);
                Ok(())
            })
        }
    }

    #[derive(Default)]
    struct MockTar {
        calls: Mutex<Vec<String>>,
        failure: Mutex<Option<Error>>,
        gate: Mutex<Option<Arc<Semaphore>>>,
    }

    impl MockTar {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock").clone()
        }

        async fn finish(&self, progress: &(dyn Fn(u64) + Send + Sync)) -> Result<(), Error> {
            progress(3);
            let gate = self.gate.lock().expect("lock").clone();
            if let Some(gate) = gate {
                gate.acquire().await.expect("gate open").forget();
            }
            match self.failure.lock().expect("lock").take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    impl TarTransfer for MockTar {
        fn upload<'a>(
            &'a self,
            _connections: &'a dyn Connections,
            sources: &'a [PathBuf],
            remote: &'a Endpoint,
            remote_dir: &'a Path,
            progress: &'a (dyn Fn(u64) + Send + Sync),
        ) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                self.calls.lock().expect("lock").push(format!(
                    "upload {sources:?} to {remote}:{}",
                    remote_dir.display()
                ));
                self.finish(progress).await
            })
        }

        fn download<'a>(
            &'a self,
            _connections: &'a dyn Connections,
            remote: &'a Endpoint,
            remote_dir: &'a Path,
            names: &'a [OsString],
            local_dir: &'a Path,
            progress: &'a (dyn Fn(u64) + Send + Sync),
        ) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                self.calls.lock().expect("lock").push(format!(
                    "download {names:?} from {remote}:{} to {}",
                    remote_dir.display(),
                    local_dir.display()
                ));
                self.finish(progress).await
            })
        }
    }

    struct Harness {
        commands: mpsc::UnboundedSender<TransferCommand>,
        events: mpsc::Receiver<TransferEvent>,
        seen: Vec<TransferEvent>,
        local: Arc<MemFs>,
        web: Arc<MemFs>,
        partials: Arc<MemPartials>,
        tar: Arc<MockTar>,
        engine: JoinHandle<()>,
    }

    struct Setup {
        config: EngineConfig,
        web_capabilities: Capabilities,
        tools: RemoteTools,
    }

    impl Default for Setup {
        fn default() -> Self {
            Setup {
                config: EngineConfig {
                    concurrency: 2,
                    tar_threshold: 1000,
                    mtime_tolerance: 0,
                },
                web_capabilities: Capabilities {
                    posix_rename: true,
                    copy_within: false,
                    symlink: true,
                    trash: false,
                },
                tools: RemoteTools {
                    tar: true,
                    rsync_ok: false,
                },
            }
        }
    }

    fn start(setup: Setup) -> Harness {
        let local = MemFs::new(Endpoint::Local);
        let web_fs = MemFs::with_capabilities(web(), setup.web_capabilities);
        let sudo_fs = MemFs::new(web_sudo());
        let connections = Arc::new(MockConnections {
            fs: HashMap::from([
                (Endpoint::Local, local.clone()),
                (web(), web_fs.clone()),
                (web_sudo(), sudo_fs),
            ]),
            tools: setup.tools,
        });
        let partials = Arc::new(MemPartials::default());
        let tar = Arc::new(MockTar::default());
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::channel(1024);
        let engine = tokio::spawn(run(
            setup.config,
            connections,
            partials.clone(),
            tar.clone(),
            command_rx,
            event_tx,
        ));
        Harness {
            commands,
            events,
            seen: Vec::new(),
            local,
            web: web_fs,
            partials,
            tar,
            engine,
        }
    }

    fn into_dir(sources: &[&str], directory: &str) -> JobTargets {
        JobTargets::IntoDirectory {
            sources: sources.iter().map(PathBuf::from).collect(),
            directory: directory.into(),
        }
    }

    impl Harness {
        fn send(&self, command: TransferCommand) {
            self.commands.send(command).expect("engine running");
        }

        fn submit(
            &self,
            id: u64,
            kind: JobKind,
            source: Endpoint,
            destination: Endpoint,
            targets: JobTargets,
            preset: Option<ConflictChoice>,
        ) {
            self.send(TransferCommand::Submit(JobRequest {
                id: JobId(id),
                kind,
                source,
                destination,
                targets,
                preset,
            }));
        }

        fn copy_up(&self, id: u64, sources: &[&str], directory: &str) {
            self.submit(
                id,
                JobKind::Copy,
                Endpoint::Local,
                web(),
                into_dir(sources, directory),
                None,
            );
        }

        async fn next(&mut self) -> TransferEvent {
            let event = tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("an event in time")
                .expect("engine running");
            self.seen.push(event.clone());
            event
        }

        async fn until(&mut self, matches: impl Fn(&TransferEvent) -> bool) -> TransferEvent {
            loop {
                let event = self.next().await;
                if matches(&event) {
                    return event;
                }
            }
        }

        async fn finished(&mut self, job: u64) -> JobSummary {
            match self
                .until(|event| {
                    matches!(event, TransferEvent::JobFinished { job: done, .. } if *done == JobId(job))
                })
                .await
            {
                TransferEvent::JobFinished { summary, .. } => summary,
                _ => unreachable!("matched above"),
            }
        }

        async fn conflict(&mut self) -> Conflict {
            match self
                .until(|event| matches!(event, TransferEvent::ConflictRaised(_)))
                .await
            {
                TransferEvent::ConflictRaised(conflict) => *conflict,
                _ => unreachable!("matched above"),
            }
        }

        async fn state_of(&mut self, item: ItemId, wanted: fn(&ItemState) -> bool) -> ItemState {
            match self
                .until(|event| {
                    matches!(event, TransferEvent::ItemChanged { item: changed, state }
                        if *changed == item && wanted(state))
                })
                .await
            {
                TransferEvent::ItemChanged { state, .. } => state,
                _ => unreachable!("matched above"),
            }
        }

        fn items(&self) -> Vec<Item> {
            self.seen
                .iter()
                .filter_map(|event| match event {
                    TransferEvent::ItemsAdded { items, .. } => Some(items.clone()),
                    _ => None,
                })
                .flatten()
                .collect()
        }

        fn item(&self, source: &str) -> Item {
            self.items()
                .into_iter()
                .find(|item| item.source == Path::new(source))
                .expect("item exists")
        }

        /// The latest state of every item, from the events seen so far.
        fn states(&self) -> HashMap<PathBuf, ItemState> {
            let sources: HashMap<ItemId, PathBuf> = self
                .items()
                .into_iter()
                .map(|item| (item.id, item.source))
                .collect();
            let mut states: HashMap<PathBuf, ItemState> = self
                .items()
                .into_iter()
                .map(|item| (item.source, item.state))
                .collect();
            for event in &self.seen {
                if let TransferEvent::ItemChanged { item, state } = event {
                    states.insert(sources[item].clone(), state.clone());
                }
            }
            states
        }

        fn state(&self, source: &str) -> ItemState {
            self.states()
                .remove(Path::new(source))
                .expect("item has a state")
        }

        fn mode(&self, job: u64) -> TransferMode {
            self.seen
                .iter()
                .find_map(|event| match event {
                    TransferEvent::JobCreated {
                        job: created, mode, ..
                    } if *created == JobId(job) => Some(*mode),
                    _ => None,
                })
                .expect("job created")
        }

        fn conflicts_raised(&self) -> usize {
            self.seen
                .iter()
                .filter(|event| matches!(event, TransferEvent::ConflictRaised(_)))
                .count()
        }
    }

    /// Polls `condition` until it holds.
    async fn eventually(condition: impl Fn() -> bool) {
        tokio::time::timeout(WAIT, async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("condition reached in time");
    }

    fn summary(done: usize, skipped: usize, failed: usize) -> JobSummary {
        JobSummary {
            done,
            skipped,
            failed,
        }
    }

    fn resolve(item: ItemId, choice: ConflictChoice, apply_to_all: bool) -> TransferCommand {
        TransferCommand::ResolveConflict {
            item,
            decision: ConflictDecision {
                choice,
                apply_to_all,
            },
        }
    }

    #[test]
    fn config_comes_from_settings() {
        let settings = Settings {
            concurrency: 5,
            tar_threshold: 7,
            mtime_tolerance: 2,
            ..Settings::default()
        };
        assert_eq!(
            EngineConfig::from_settings(&settings),
            EngineConfig {
                concurrency: 5,
                tar_threshold: 7,
                mtime_tolerance: 2,
            }
        );
    }

    #[test]
    fn numbered_names_keep_the_last_extension() {
        assert_eq!(
            numbered(Path::new("/d/a.txt"), 1),
            PathBuf::from("/d/a (1).txt")
        );
        assert_eq!(
            numbered(Path::new("/d/a.tar.gz"), 2),
            PathBuf::from("/d/a.tar (2).gz")
        );
        assert_eq!(
            numbered(Path::new("/d/.bashrc"), 1),
            PathBuf::from("/d/.bashrc (1)")
        );
        assert_eq!(
            numbered(Path::new("/d/README"), 3),
            PathBuf::from("/d/README (3)")
        );
    }

    #[test]
    fn newer_compares_whole_seconds_beyond_the_tolerance() {
        assert!(is_newer(Some(10), Some(9), 0));
        assert!(!is_newer(Some(10), Some(10), 0));
        assert!(!is_newer(Some(11), Some(9), 2));
        assert!(is_newer(Some(12), Some(9), 2));
        assert!(!is_newer(None, Some(9), 0));
        assert!(!is_newer(Some(9), None, 0));
    }

    #[tokio::test]
    async fn copies_a_tree_with_symlinks_and_skips_special_files() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_dir("/src/tree");
        h.local.add_dir("/src/tree/sub");
        h.local
            .add_file("/src/tree/sub/x.txt", b"xxxx-xxxx", 100, 0o640);
        h.local.add_file("/src/tree/y.txt", b"yy", 200, 0o4755);
        h.local.add_symlink("/src/tree/link", "sub/x.txt");
        h.local.add_special("/src/tree/fifo", SpecialKind::Fifo);
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src/tree"], "/dst");

        assert_eq!(h.finished(1).await, summary(5, 1, 0));
        assert_eq!(h.mode(1), TransferMode::PerFile);
        let items = h.items();
        let position = |source: &str| {
            items
                .iter()
                .position(|item| item.source == Path::new(source))
                .expect("listed")
        };
        assert!(position("/src/tree") < position("/src/tree/sub"));
        assert!(position("/src/tree/sub") < position("/src/tree/sub/x.txt"));
        assert_eq!(
            h.item("/src/tree/fifo").state,
            ItemState::Skipped {
                reason: SkipReason::SpecialFile
            }
        );
        assert_eq!(
            h.item("/src/tree/fifo").kind,
            ItemKind::Special(SpecialKind::Fifo)
        );

        assert_eq!(
            h.web.data("/dst/tree/sub/x.txt").as_deref(),
            Some(&b"xxxx-xxxx"[..])
        );
        let y = h.web.meta("/dst/tree/y.txt").expect("copied");
        assert_eq!((y.mtime, y.mode), (200, 0o755));
        assert_eq!(
            h.web.meta("/dst/tree/link").map(|meta| meta.node),
            Some(crate::transfer::stream::test_support::Node::Symlink(
                "sub/x.txt".into()
            ))
        );
        assert!(!h.web.exists("/dst/tree/fifo"));
        assert!(!h.partials.has(web(), "/dst/tree/y.txt"));
        assert_eq!(
            h.partials
                .log()
                .iter()
                .filter(|line| line.ends_with("/dst/tree/y.txt"))
                .cloned()
                .collect::<Vec<_>>(),
            vec!["set /dst/tree/y.txt", "remove /dst/tree/y.txt"]
        );
    }

    #[tokio::test]
    async fn never_runs_more_items_than_the_concurrency() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        let names: Vec<String> = (0..5).map(|n| format!("/src/f{n}")).collect();
        for name in &names {
            h.local.add_file(name, &[1u8; 24], 1, 0o644);
        }
        h.local.set_read_delay(Duration::from_millis(2));
        h.web.add_dir("/dst");
        let sources: Vec<&str> = names.iter().map(String::as_str).collect();
        h.copy_up(1, &sources, "/dst");
        assert_eq!(h.finished(1).await, summary(5, 0, 0));

        let mut running = HashSet::new();
        let mut most = 0;
        for event in &h.seen {
            if let TransferEvent::ItemChanged { item, state } = event {
                if matches!(state, ItemState::Running { .. }) {
                    running.insert(*item);
                } else {
                    running.remove(item);
                }
                most = most.max(running.len());
            }
        }
        assert_eq!(most, 2);
    }

    #[tokio::test]
    async fn conflict_choices_skip_overwrite_and_rename() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.txt", b"new", 50, 0o644);
        h.web.add_dir("/dst");
        h.web.add_file("/dst/a.txt", b"old!", 40, 0o600);
        h.web.add_file("/dst/a (1).txt", b"taken", 40, 0o600);

        h.copy_up(1, &["/src/a.txt"], "/dst");
        let conflict = h.conflict().await;
        assert_eq!(conflict.job, JobId(1));
        assert_eq!(conflict.destination.size, Some(4));
        assert_eq!(conflict.source.size, Some(3));
        assert_eq!(conflict.partial, None);
        assert_eq!(h.state("/src/a.txt"), ItemState::WaitingForConflict);
        h.send(resolve(conflict.item, ConflictChoice::Skip, false));
        assert_eq!(h.finished(1).await, summary(0, 1, 0));
        assert_eq!(
            h.state("/src/a.txt"),
            ItemState::Skipped {
                reason: SkipReason::UserSkipped
            }
        );
        assert_eq!(h.web.data("/dst/a.txt").as_deref(), Some(&b"old!"[..]));

        h.copy_up(2, &["/src/a.txt"], "/dst");
        let conflict = h.conflict().await;
        h.send(resolve(conflict.item, ConflictChoice::Rename, false));
        h.until(|event| {
            matches!(event, TransferEvent::ItemRenamed { destination, .. }
                if destination == Path::new("/dst/a (2).txt"))
        })
        .await;
        assert_eq!(h.finished(2).await, summary(1, 0, 0));
        assert_eq!(h.web.data("/dst/a (2).txt").as_deref(), Some(&b"new"[..]));
        assert_eq!(h.web.data("/dst/a.txt").as_deref(), Some(&b"old!"[..]));

        h.copy_up(3, &["/src/a.txt"], "/dst");
        let conflict = h.conflict().await;
        h.send(resolve(conflict.item, ConflictChoice::Overwrite, false));
        assert_eq!(h.finished(3).await, summary(1, 0, 0));
        assert_eq!(h.web.data("/dst/a.txt").as_deref(), Some(&b"new"[..]));
    }

    #[tokio::test]
    async fn overwrite_if_newer_honors_the_mtime_tolerance() {
        let mut setup = Setup::default();
        setup.config.mtime_tolerance = 2;
        let mut h = start(setup);
        h.local.add_dir("/src");
        h.local.add_file("/src/close.txt", b"close", 101, 0o644);
        h.local.add_file("/src/newer.txt", b"newer", 103, 0o644);
        h.web.add_dir("/dst");
        h.web.add_file("/dst/close.txt", b"old", 100, 0o644);
        h.web.add_file("/dst/newer.txt", b"old", 100, 0o644);
        h.submit(
            1,
            JobKind::Copy,
            Endpoint::Local,
            web(),
            into_dir(&["/src/close.txt", "/src/newer.txt"], "/dst"),
            Some(ConflictChoice::OverwriteIfNewer),
        );
        assert_eq!(h.finished(1).await, summary(1, 1, 0));
        assert_eq!(h.conflicts_raised(), 0, "the preset answers");
        assert_eq!(
            h.state("/src/close.txt"),
            ItemState::Skipped {
                reason: SkipReason::NotNewer
            }
        );
        assert_eq!(h.web.data("/dst/close.txt").as_deref(), Some(&b"old"[..]));
        assert_eq!(h.web.data("/dst/newer.txt").as_deref(), Some(&b"newer"[..]));
    }

    #[tokio::test]
    async fn apply_to_all_answers_the_other_conflicts_of_the_job() {
        let mut setup = Setup::default();
        setup.config.concurrency = 1;
        let mut h = start(setup);
        h.local.add_dir("/src");
        h.web.add_dir("/dst");
        for name in ["a", "b", "c"] {
            h.local.add_file(&format!("/src/{name}"), b"new", 5, 0o644);
            h.web.add_file(&format!("/dst/{name}"), b"old", 5, 0o644);
        }
        h.copy_up(1, &["/src/a", "/src/b", "/src/c"], "/dst");
        let first = h.conflict().await;
        h.send(resolve(first.item, ConflictChoice::Overwrite, true));
        assert_eq!(h.finished(1).await, summary(3, 0, 0));
        for name in ["a", "b", "c"] {
            assert_eq!(
                h.web.data(&format!("/dst/{name}")).as_deref(),
                Some(&b"new"[..])
            );
        }
    }

    #[tokio::test]
    async fn nfc_equal_names_conflict_and_overwrite_the_existing_file() {
        let mut h = start(Setup::default());
        let nfd = "/src/cafe\u{301}.txt";
        let nfc = "/dst/caf\u{e9}.txt";
        h.local.add_dir("/src");
        h.local.add_file(nfd, b"fresh", 5, 0o644);
        h.web.add_dir("/dst");
        h.web.add_file(nfc, b"stale", 1, 0o644);
        h.copy_up(1, &[nfd], "/dst");
        let conflict = h.conflict().await;
        assert!(h.seen.iter().any(|event| matches!(event,
            TransferEvent::ItemRenamed { destination, .. } if destination == Path::new(nfc))));
        assert_eq!(conflict.destination.name, OsString::from("caf\u{e9}.txt"));
        h.send(resolve(conflict.item, ConflictChoice::Overwrite, false));
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(h.web.data(nfc).as_deref(), Some(&b"fresh"[..]));
        assert!(!h.web.exists("/dst/cafe\u{301}.txt"));
    }

    #[tokio::test]
    async fn a_matching_partial_resumes_silently() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.bin", b"0123456789", 77, 0o644);
        h.web.add_dir("/dst");
        h.web.add_file("/dst/a.bin.scpto-part", b"0123", 0, 0o644);
        let record = PartialRecord {
            source_size: 10,
            source_mtime: 77,
        };
        h.partials.insert(web(), "/dst/a.bin", record);
        h.copy_up(1, &["/src/a.bin"], "/dst");
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(h.local.read_offsets("/src/a.bin"), vec![4]);
        assert_eq!(
            h.web.data("/dst/a.bin").as_deref(),
            Some(&b"0123456789"[..])
        );
        assert!(!h.partials.has(web(), "/dst/a.bin"));
    }

    #[tokio::test]
    async fn a_stale_partial_restarts_from_zero() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.bin", b"0123456789", 78, 0o644);
        h.web.add_dir("/dst");
        h.web.add_file("/dst/a.bin.scpto-part", b"XXXX", 0, 0o644);
        h.partials.insert(
            web(),
            "/dst/a.bin",
            PartialRecord {
                source_size: 10,
                source_mtime: 77,
            },
        );
        h.copy_up(1, &["/src/a.bin"], "/dst");
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(h.local.read_offsets("/src/a.bin"), vec![0]);
        assert_eq!(
            h.web.data("/dst/a.bin").as_deref(),
            Some(&b"0123456789"[..])
        );
    }

    #[tokio::test]
    async fn a_conflict_offers_resume_for_a_matching_partial() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.bin", b"0123456789", 77, 0o644);
        h.web.add_dir("/dst");
        h.web.add_file("/dst/a.bin", b"old", 1, 0o644);
        h.web.add_file("/dst/a.bin.scpto-part", b"012345", 0, 0o644);
        h.partials.insert(
            web(),
            "/dst/a.bin",
            PartialRecord {
                source_size: 10,
                source_mtime: 77,
            },
        );
        h.copy_up(1, &["/src/a.bin"], "/dst");
        let conflict = h.conflict().await;
        assert_eq!(conflict.partial, Some(PartialInfo { size: 6 }));
        h.send(resolve(conflict.item, ConflictChoice::Resume, false));
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(h.local.read_offsets("/src/a.bin"), vec![6]);
        assert_eq!(
            h.web.data("/dst/a.bin").as_deref(),
            Some(&b"0123456789"[..])
        );
    }

    #[tokio::test]
    async fn pause_keeps_the_part_and_resume_continues_it() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local
            .add_file("/src/big.bin", b"0123456789abcdef", 9, 0o644);
        let gate = h.local.gate_reads("/src/big.bin", 4);
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src/big.bin"], "/dst");
        let item = h.items_after_added().await;
        let web_fs = h.web.clone();
        eventually(move || {
            web_fs
                .data("/dst/big.bin.scpto-part")
                .is_some_and(|d| d.len() == 4)
        })
        .await;
        assert!(
            h.partials.has(web(), "/dst/big.bin"),
            "record set before data"
        );
        h.send(TransferCommand::Pause(QueueTarget::Item(item)));
        h.state_of(item, |state| *state == ItemState::Paused).await;
        assert_eq!(
            h.web.data("/dst/big.bin.scpto-part").as_deref(),
            Some(&b"0123"[..])
        );
        assert!(h.partials.has(web(), "/dst/big.bin"));

        gate.add_permits(100);
        h.send(TransferCommand::Resume(QueueTarget::Job(JobId(1))));
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(h.local.read_offsets("/src/big.bin"), vec![0, 4]);
        assert_eq!(
            h.web.data("/dst/big.bin").as_deref(),
            Some(&b"0123456789abcdef"[..])
        );
    }

    #[tokio::test]
    async fn cancel_keeps_the_part_until_discarded_and_can_be_retried() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/big.bin", b"0123456789", 9, 0o644);
        let gate = h.local.gate_reads("/src/big.bin", 4);
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src/big.bin"], "/dst");
        let web_fs = h.web.clone();
        eventually(move || web_fs.exists("/dst/big.bin.scpto-part")).await;
        h.send(TransferCommand::Cancel(QueueTarget::Job(JobId(1))));
        assert_eq!(h.finished(1).await, summary(0, 1, 0));
        assert_eq!(
            h.state("/src/big.bin"),
            ItemState::Skipped {
                reason: SkipReason::Canceled
            }
        );
        assert!(h.web.exists("/dst/big.bin.scpto-part"));
        assert!(h.partials.has(web(), "/dst/big.bin"));

        h.send(TransferCommand::DiscardPartial(QueueTarget::Job(JobId(1))));
        let (web_fs, partials) = (h.web.clone(), h.partials.clone());
        eventually(move || {
            !web_fs.exists("/dst/big.bin.scpto-part") && !partials.has(web(), "/dst/big.bin")
        })
        .await;

        gate.add_permits(100);
        h.send(TransferCommand::Retry(QueueTarget::Job(JobId(1))));
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(
            h.web.data("/dst/big.bin").as_deref(),
            Some(&b"0123456789"[..])
        );
    }

    #[tokio::test]
    async fn a_lost_connection_waits_and_resumes_after_reconnect() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.bin", b"0123456789", 9, 0o644);
        h.local
            .fail_read_after("/src/a.bin", 4, FsError::Disconnected);
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src/a.bin"], "/dst");
        let item = h.items_after_added().await;
        h.state_of(item, |state| *state == ItemState::WaitingForReconnect)
            .await;
        h.send(TransferCommand::EndpointReconnected(web()));
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(h.local.read_offsets("/src/a.bin"), vec![0, 4]);
        assert_eq!(
            h.web.data("/dst/a.bin").as_deref(),
            Some(&b"0123456789"[..])
        );
    }

    impl Harness {
        /// Waits for the first `ItemsAdded` and returns its first item.
        async fn items_after_added(&mut self) -> ItemId {
            match self
                .until(|event| matches!(event, TransferEvent::ItemsAdded { .. }))
                .await
            {
                TransferEvent::ItemsAdded { items, .. } => items[0].id,
                _ => unreachable!("matched above"),
            }
        }
    }

    #[tokio::test]
    async fn failed_items_keep_their_error_and_retry_all_failed_requeues_them() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.txt", b"abc", 9, 0o644);
        h.web.add_dir("/dst");
        let denied = FsError::PermissionDenied {
            path: "/dst/a.txt.scpto-part".into(),
        };
        h.web
            .fail(Op::OpenWrite, "/dst/a.txt.scpto-part", denied.clone(), true);
        h.copy_up(1, &["/src/a.txt"], "/dst");
        assert_eq!(h.finished(1).await, summary(0, 0, 1));
        assert_eq!(
            h.state("/src/a.txt"),
            ItemState::Failed {
                error: Error::Fs(denied)
            }
        );
        h.send(TransferCommand::RetryAllFailed);
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(h.web.data("/dst/a.txt").as_deref(), Some(&b"abc"[..]));
    }

    #[tokio::test]
    async fn a_failed_directory_fails_its_contents_and_retry_covers_them() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_dir("/src/tree");
        h.local.add_file("/src/tree/a.txt", b"a", 1, 0o644);
        h.web.add_dir("/dst");
        h.web.add_file("/dst/tree", b"in the way", 1, 0o644);
        h.copy_up(1, &["/src/tree"], "/dst");
        assert_eq!(h.finished(1).await, summary(0, 0, 2));
        let blocked = ItemState::Failed {
            error: Error::Fs(FsError::AlreadyExists {
                path: "/dst/tree".into(),
            }),
        };
        assert_eq!(h.state("/src/tree"), blocked);
        assert_eq!(h.state("/src/tree/a.txt"), blocked);

        h.web.remove_node("/dst/tree");
        let child = h.item("/src/tree/a.txt").id;
        h.send(TransferCommand::Retry(QueueTarget::Item(child)));
        assert_eq!(h.finished(1).await, summary(2, 0, 0));
        assert_eq!(h.web.data("/dst/tree/a.txt").as_deref(), Some(&b"a"[..]));
    }

    #[tokio::test]
    async fn cross_endpoint_move_deletes_only_succeeded_sources() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_dir("/src/tree");
        h.local.add_file("/src/tree/a.txt", b"a", 1, 0o644);
        h.local.add_file("/src/tree/b.txt", b"b", 1, 0o644);
        let denied = FsError::PermissionDenied {
            path: "/src/tree/b.txt".into(),
        };
        h.local.fail(Op::OpenRead, "/src/tree/b.txt", denied, false);
        h.web.add_dir("/dst");
        h.submit(
            1,
            JobKind::Move,
            Endpoint::Local,
            web(),
            into_dir(&["/src/tree"], "/dst"),
            None,
        );
        assert_eq!(h.finished(1).await, summary(2, 0, 1));
        assert!(!h.local.exists("/src/tree/a.txt"));
        assert!(h.local.exists("/src/tree/b.txt"));
        assert!(h.local.exists("/src/tree"), "kept while b.txt remains");
        assert!(h.web.exists("/dst/tree/a.txt"));

        h.local.clear_failure(Op::OpenRead, "/src/tree/b.txt");
        h.send(TransferCommand::RetryAllFailed);
        assert_eq!(h.finished(1).await, summary(3, 0, 0));
        assert!(
            !h.local.exists("/src/tree"),
            "removed once everything moved"
        );
        assert_eq!(h.web.data("/dst/tree/b.txt").as_deref(), Some(&b"b"[..]));
    }

    #[tokio::test]
    async fn same_endpoint_move_renames_and_falls_back_on_unsupported() {
        let mut h = start(Setup::default());
        h.web.add_dir("/src");
        h.web.add_dir("/src/tree");
        h.web.add_file("/src/tree/a.txt", b"a", 1, 0o644);
        h.web.add_file("/src/tree/b.txt", b"b", 1, 0o644);
        h.web.add_dir("/dst");
        h.web.fail(
            Op::Rename,
            "/src/tree/b.txt",
            FsError::Unsupported {
                operation: "rename".into(),
                reason: "test".into(),
            },
            false,
        );
        h.submit(
            1,
            JobKind::Move,
            web(),
            web(),
            into_dir(&["/src/tree"], "/dst"),
            None,
        );
        assert_eq!(h.finished(1).await, summary(3, 0, 0));
        assert!(h.web.called(Op::Rename, "/src/tree/a.txt"));
        assert!(!h.web.called(Op::OpenRead, "/src/tree/a.txt"));
        assert!(h.web.called(Op::OpenRead, "/src/tree/b.txt"));
        assert_eq!(h.web.data("/dst/tree/a.txt").as_deref(), Some(&b"a"[..]));
        assert_eq!(h.web.data("/dst/tree/b.txt").as_deref(), Some(&b"b"[..]));
        assert!(!h.web.exists("/src/tree"));
    }

    #[tokio::test]
    async fn same_endpoint_copy_uses_copy_within_when_supported() {
        let mut setup = Setup::default();
        setup.web_capabilities.copy_within = true;
        let mut h = start(setup);
        h.web.add_dir("/src");
        h.web.add_file("/src/a.txt", b"abc", 1, 0o644);
        h.web.add_dir("/dst");
        h.submit(
            1,
            JobKind::Copy,
            web(),
            web(),
            into_dir(&["/src/a.txt"], "/dst"),
            None,
        );
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert!(h.web.called(Op::CopyWithin, "/src/a.txt"));
        assert_eq!(h.web.data("/dst/a.txt").as_deref(), Some(&b"abc"[..]));
        assert!(h.web.exists("/src/a.txt"));
    }

    #[tokio::test]
    async fn pairs_create_missing_parent_directories() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.txt", b"abc", 1, 0o644);
        h.submit(
            1,
            JobKind::Copy,
            Endpoint::Local,
            web(),
            JobTargets::Pairs(vec![("/src/a.txt".into(), "/deep/er/a.txt".into())]),
            Some(ConflictChoice::Overwrite),
        );
        assert_eq!(h.finished(1).await, summary(1, 0, 0));
        assert_eq!(h.web.data("/deep/er/a.txt").as_deref(), Some(&b"abc"[..]));
    }

    #[tokio::test]
    async fn expansion_failure_fails_the_job() {
        let mut h = start(Setup::default());
        h.copy_up(1, &["/missing"], "/dst");
        let event = h
            .until(|event| matches!(event, TransferEvent::JobFailed { .. }))
            .await;
        assert_eq!(
            event,
            TransferEvent::JobFailed {
                job: JobId(1),
                error: Error::Fs(FsError::NotFound {
                    path: "/missing".into()
                }),
            }
        );
        h.send(TransferCommand::ClearFinished);
        assert_eq!(h.next().await, TransferEvent::JobsCleared(vec![JobId(1)]));
    }

    fn tar_setup() -> Setup {
        let mut setup = Setup::default();
        setup.config.tar_threshold = 2;
        setup
    }

    fn add_two_files(fs: &MemFs, dir: &str) {
        fs.add_dir(dir);
        fs.add_file(&format!("{dir}/a.txt"), b"a", 1, 0o644);
        fs.add_file(&format!("{dir}/b.txt"), b"b", 1, 0o644);
    }

    #[tokio::test]
    async fn tar_mode_uploads_when_eligible() {
        let mut h = start(tar_setup());
        add_two_files(&h.local, "/src");
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src"], "/dst");
        assert_eq!(h.finished(1).await, summary(3, 0, 0));
        assert_eq!(h.mode(1), TransferMode::Tar);
        assert_eq!(h.tar.calls(), vec![r#"upload ["/src"] to web:/dst"#]);
    }

    #[tokio::test]
    async fn tar_mode_downloads_from_one_remote_parent() {
        let mut h = start(tar_setup());
        add_two_files(&h.web, "/srv");
        h.local.add_dir("/home");
        h.submit(
            1,
            JobKind::Copy,
            web(),
            Endpoint::Local,
            into_dir(&["/srv/a.txt", "/srv/b.txt"], "/home"),
            None,
        );
        assert_eq!(h.finished(1).await, summary(2, 0, 0));
        assert_eq!(
            h.tar.calls(),
            vec![r#"download ["a.txt", "b.txt"] from web:/srv to /home"#]
        );
    }

    #[tokio::test]
    async fn tar_mode_is_not_used_when_any_condition_fails() {
        // Below the threshold.
        let mut h = start(tar_setup());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.txt", b"a", 1, 0o644);
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src/a.txt"], "/dst");
        h.finished(1).await;
        assert_eq!(h.mode(1), TransferMode::PerFile);

        // A sudo remote.
        add_two_files(&h.local, "/two");
        h.submit(
            2,
            JobKind::Copy,
            Endpoint::Local,
            web_sudo(),
            into_dir(&["/two"], "/"),
            None,
        );
        h.finished(2).await;
        assert_eq!(h.mode(2), TransferMode::PerFile);

        // Downloads from different parents.
        add_two_files(&h.web, "/x");
        h.web.add_dir("/y");
        h.web.add_file("/y/c.txt", b"c", 1, 0o644);
        h.local.add_dir("/down");
        h.submit(
            3,
            JobKind::Copy,
            web(),
            Endpoint::Local,
            into_dir(&["/x/a.txt", "/x/b.txt", "/y/c.txt"], "/down"),
            None,
        );
        h.finished(3).await;
        assert_eq!(h.mode(3), TransferMode::PerFile);

        // Explicit pairs.
        h.submit(
            4,
            JobKind::Copy,
            Endpoint::Local,
            web(),
            JobTargets::Pairs(vec![
                ("/two/a.txt".into(), "/p/a.txt".into()),
                ("/two/b.txt".into(), "/p/b.txt".into()),
            ]),
            None,
        );
        h.finished(4).await;
        assert_eq!(h.mode(4), TransferMode::PerFile);
        assert!(h.tar.calls().is_empty());
    }

    #[tokio::test]
    async fn tar_mode_needs_tar_on_the_remote() {
        let mut setup = tar_setup();
        setup.tools.tar = false;
        let mut h = start(setup);
        add_two_files(&h.local, "/src");
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src"], "/dst");
        assert_eq!(h.finished(1).await, summary(3, 0, 0));
        assert_eq!(h.mode(1), TransferMode::PerFile);
        assert!(h.tar.calls().is_empty());
    }

    #[tokio::test]
    async fn tar_conflicts_ask_once_for_the_job() {
        let mut h = start(tar_setup());
        add_two_files(&h.local, "/src");
        h.web.add_dir("/dst");
        h.web.add_dir("/dst/src");
        h.copy_up(1, &["/src"], "/dst");
        let event = h
            .until(|event| matches!(event, TransferEvent::TarConflictRaised { .. }))
            .await;
        assert_eq!(
            event,
            TransferEvent::TarConflictRaised {
                job: JobId(1),
                existing: vec!["/dst/src".into()],
            }
        );
        assert!(h.tar.calls().is_empty());
        h.send(TransferCommand::ResolveTarConflict {
            job: JobId(1),
            choice: TarConflictChoice::OverwriteAll,
        });
        assert_eq!(h.finished(1).await, summary(3, 0, 0));
        assert_eq!(h.tar.calls().len(), 1);

        h.copy_up(2, &["/src"], "/dst");
        h.until(|event| matches!(event, TransferEvent::TarConflictRaised { .. }))
            .await;
        h.send(TransferCommand::ResolveTarConflict {
            job: JobId(2),
            choice: TarConflictChoice::Cancel,
        });
        assert_eq!(h.finished(2).await, summary(0, 3, 0));
        assert_eq!(h.tar.calls().len(), 1, "a canceled tar job never streams");
    }

    #[tokio::test]
    async fn tar_failure_fails_the_items_and_retry_streams_again() {
        let mut h = start(tar_setup());
        add_two_files(&h.local, "/src");
        h.web.add_dir("/dst");
        let failure = Error::CommandFailed {
            program: "tar".into(),
            status: crate::error::ExitStatus::Code(2),
            stderr: "disk full".into(),
        };
        *h.tar.failure.lock().expect("lock") = Some(failure.clone());
        h.copy_up(1, &["/src"], "/dst");
        assert_eq!(h.finished(1).await, summary(0, 0, 3));
        assert_eq!(h.state("/src/a.txt"), ItemState::Failed { error: failure });
        h.send(TransferCommand::Retry(QueueTarget::Job(JobId(1))));
        assert_eq!(h.finished(1).await, summary(3, 0, 0));
        assert_eq!(h.tar.calls().len(), 2);
    }

    #[tokio::test]
    async fn tar_cancel_stops_the_stream() {
        let mut h = start(tar_setup());
        add_two_files(&h.local, "/src");
        h.web.add_dir("/dst");
        let gate = Arc::new(Semaphore::new(0));
        *h.tar.gate.lock().expect("lock") = Some(gate.clone());
        h.copy_up(1, &["/src"], "/dst");
        let item = h.items_after_added().await;
        let tar = h.tar.clone();
        eventually(move || !tar.calls().is_empty()).await;
        h.send(TransferCommand::Cancel(QueueTarget::Item(item)));
        assert_eq!(h.finished(1).await, summary(0, 3, 0));
        drop(gate);
    }

    #[tokio::test]
    async fn tar_move_deletes_the_sources_after_the_stream() {
        let mut h = start(tar_setup());
        add_two_files(&h.local, "/src");
        h.web.add_dir("/dst");
        h.submit(
            1,
            JobKind::Move,
            Endpoint::Local,
            web(),
            into_dir(&["/src"], "/dst"),
            None,
        );
        assert_eq!(h.finished(1).await, summary(3, 0, 0));
        assert!(!h.local.exists("/src"));
        assert!(!h.local.exists("/src/a.txt"));
    }

    #[tokio::test]
    async fn tar_progress_is_spread_over_the_file_items() {
        let mut h = start(tar_setup());
        add_two_files(&h.local, "/src");
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src"], "/dst");
        h.finished(1).await;
        // The mock reports 3 bytes: one for each 1-byte file, capped.
        let a = h.item("/src/a.txt").id;
        let b = h.item("/src/b.txt").id;
        for id in [a, b] {
            assert!(h.seen.contains(&TransferEvent::ItemChanged {
                item: id,
                state: ItemState::Running { bytes: 1 }
            }));
        }
    }

    #[tokio::test]
    async fn clear_finished_drops_finished_jobs_and_closing_stops_the_engine() {
        let mut h = start(Setup::default());
        h.local.add_dir("/src");
        h.local.add_file("/src/a.txt", b"a", 1, 0o644);
        h.web.add_dir("/dst");
        h.copy_up(1, &["/src/a.txt"], "/dst");
        h.finished(1).await;
        h.send(TransferCommand::ClearFinished);
        assert_eq!(h.next().await, TransferEvent::JobsCleared(vec![JobId(1)]));
        let Harness {
            commands, engine, ..
        } = h;
        drop(commands);
        tokio::time::timeout(WAIT, engine)
            .await
            .expect("engine stops")
            .expect("engine did not panic");
    }
}
