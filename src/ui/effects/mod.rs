//! Effect executors: perform the I/O requested by `Effect`s and report
//! results as `BgEvent`s. One module per domain; every entry point takes the
//! shared [`Ctx`].
//!
//! # Conventions
//!
//! - Entry points are plain functions, one per effect, that return at once:
//!   they spawn a Tokio task for the work and report through
//!   [`Ctx::events`]. Each takes the effect's fields, not the `Effect`, so
//!   no executor ever sees a variant it does not handle.
//! - A spawned task clones the parts of [`Ctx`] it needs (every field is
//!   cheap to clone). Results go out with `ctx.events.send(..)`; a send
//!   error means the app has exited, so the result is dropped with
//!   `drop(result)` and a comment saying so.
//! - Requests that `Effect::CancelRequest` may abandon (`ListDir`,
//!   `LoadPreview`, `Compare`) run through [`Ctx::requests`]
//!   ([`Tasks::spawn`]); timers through [`Ctx::timers`]; rsync syncs through
//!   [`Ctx::syncs`]. Aborting a task drops its future, which kills its child
//!   processes (`kill_on_drop`, and the rsync process-group guard).
//! - Remote work uses `Ctx::connections` with `FsChannel::Browse`
//!   (listings, file operations, previews, edits, comparisons) or
//!   `FsChannel::Transfer` (the engine and engine-path sync deletions).
//!
//! # Dispatch (integration owner wires `ui/runtime.rs` from this table)
//!
//! | Effect | Entry point |
//! |---|---|
//! | `Connect(ep)` | `session::connect` (also `Local`: `Sessions::connect` answers `Connected { Local, home }`) |
//! | `SudoPassword { endpoint, password }` | `session::sudo_password` |
//! | `Reconnect(ep)` | `session::reconnect` |
//! | `Disconnect(ep)` | `session::disconnect` |
//! | `ResolveHost(alias)` | `session::resolve_host` |
//! | `InteractiveAuth(ep)` | runtime: `session::interactive_auth` → [`Foreground`] |
//! | `RunVpn { endpoint, command }` | runtime: `external::vpn` → [`Foreground`] |
//! | `OpenShell(location)` | runtime: `external::shell` (async) → [`Foreground`] |
//! | `Edit(location)` | runtime: `edit::prepare` (async) → [`Foreground`] |
//! | `ListDir { request, location }` | `fs::list_dir` |
//! | `FsOp { request, endpoint, op }` | `fs::fs_op` |
//! | `CheckDroppedPaths { text, paths }` | `external::check_dropped_paths` |
//! | `Reveal(location)` | `external::reveal` |
//! | `Transfer(command)` | `transfer::send` |
//! | `Compare { request, left, right }` | `sync::compare` |
//! | `Sync { .. }` | `sync::sync` |
//! | `CancelSync(job)` | `sync::cancel` |
//! | `LoadPreview { request, location }` | `preview::load` |
//! | `ResolveEditConflict { location, temp, overwrite }` | `edit::resolve_conflict` |
//! | `RememberDirectory(location)` | `state::remember_directory` |
//! | `SaveBookmark(location)` | `state::save_bookmark` |
//! | `RemoveBookmark(location)` | `state::remove_bookmark` |
//! | `CancelRequest(request)` | runtime: `ctx.requests.abort(&request)` |
//! | `ScheduleTimer { id, after }` | `timer::schedule` |
//! | `Quit` | runtime: leaves the loop (below) |
//!
//! # Runtime-owned effects
//!
//! The runtime runs every effect that suspends the TUI itself, one at a time
//! and in arrival order, through `term::TerminalGuard::run_foreground`:
//! `InteractiveAuth`, `RunVpn`, `OpenShell`, and `Edit`. It turns each into a
//! [`Foreground`] with the builder of the table above. A builder returns
//! `None` after reporting its own failure as that effect's `BgEvent`
//! (`ConnectFailed`, `ShellExited { result: Err }`, `EditFinished { result:
//! Err }`). The two async builders run as futures the event loop polls, so
//! a remote download or `ssh -O check` never blocks input. The runtime
//! queues each `Foreground`, runs its `command` in the foreground timing
//! it, and then calls the finisher of its [`AfterForeground`]:
//!
//! | `AfterForeground` | Finisher |
//! |---|---|
//! | `InteractiveAuth(ep)` | `session::interactive_auth_finished(ctx, ep, status)` |
//! | `Vpn(ep)` | `external::vpn_finished(ctx, ep, status)` |
//! | `Shell` | `external::shell_finished(ctx, status)` |
//! | `Edit(session)` | `edit::finish(ctx, session, status, elapsed)` |
//!
//! Other runtime duties: send `Action::Resize` with the terminal size before
//! the first frame; take `App::preview.resize_requests` and pass it to
//! `preview::spawn_resize_worker` before the loop; on `Effect::Quit` (or a
//! terminating signal) stop reading input, drop the engine command sender,
//! `Sessions::shutdown().await`, and remove [`Ctx::edit_temp`].
//!
//! # Public API (Wave 3 seam pass)
//!
//! ```text
//! #[derive(Clone)]
//! pub struct Ctx { /* the fields below, all pub */ }
//!
//! pub struct Foreground { pub command: tokio::process::Command, pub then: AfterForeground }
//! pub enum AfterForeground { InteractiveAuth(Endpoint), Vpn(Endpoint), Shell, Edit(EditSession) }
//!
//! /// Spawned tasks keyed by request, timer, or job id.
//! pub struct Tasks<K> { /* private */ }
//! impl<K: Clone + Eq + Hash + Send + 'static> Tasks<K> {
//!     pub fn new() -> Tasks<K>;
//!     /// Spawns `future` under `key`, aborting the task registered under
//!     /// the same key before. The entry is removed when the task ends.
//!     pub fn spawn(self: &Arc<Self>, key: K, future: impl Future<Output = ()> + Send + 'static);
//!     /// Aborts and forgets `key`'s task; whether one was running.
//!     pub fn abort(&self, key: &K) -> bool;
//!     /// Whether a task runs under `key`.
//!     pub fn contains(&self, key: &K) -> bool;
//! }
//! ```

pub mod edit;
pub mod external;
pub mod fs;
pub mod preview;
pub mod session;
pub mod state;
pub mod sync;
pub mod timer;
pub mod transfer;

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ratatui_image::picker::Picker;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use crate::connections::{Connections, SshClient};
use crate::edit::EditSession;
use crate::session::Sessions;
use crate::settings::Settings;
use crate::state::PersistedState;
use crate::transfer::model::{JobId, TransferCommandTx};
use crate::ui::action::{BgEvent, RequestId, TimerId};
use crate::vfs::Endpoint;

/// Everything the executors share. Built once by startup; cloning is cheap.
#[derive(Clone)]
pub struct Ctx {
    /// The SSH sessions: connect, sudo password, reconnect, disconnect, the
    /// foreground master command, and shutdown.
    pub sessions: Arc<Sessions>,
    /// The same sessions as `Connections`, for file systems, remote
    /// commands, `ssh_command`, and tool probes.
    pub connections: Arc<dyn Connections>,
    /// Where results go.
    pub events: UnboundedSender<BgEvent>,
    /// Effective settings (`mtime-tolerance` for comparisons).
    pub settings: Arc<Settings>,
    /// How `ssh` is invoked, for `hosts::resolve`.
    pub ssh: SshClient,
    /// `state::state_dir`; `None` when nothing can be persisted.
    pub state_dir: Option<PathBuf>,
    /// The executor's copy of `state.json`, changed and saved under the
    /// lock so saves never overtake each other.
    pub persisted: Arc<tokio::sync::Mutex<PersistedState>>,
    /// Commands to the transfer engine (`transfer::start_engine`).
    pub engine: TransferCommandTx,
    /// The terminal's image protocol picker, queried once at startup.
    pub image_picker: Arc<Picker>,
    /// The per-process temp directory for edits (mode 0700, created on
    /// first use, removed on exit).
    pub edit_temp: PathBuf,
    /// Requests `Effect::CancelRequest` may abandon.
    pub requests: Arc<Tasks<RequestId>>,
    /// Pending timers; rescheduling an id replaces its timer.
    pub timers: Arc<Tasks<TimerId>>,
    /// Running rsync syncs by job id (engine-path syncs are engine jobs).
    pub syncs: Arc<Tasks<JobId>>,
}

/// A program the runtime runs with the TUI suspended, and what follows.
pub struct Foreground {
    /// The command, with inherited stdio.
    pub command: Command,
    /// The finisher called with the exit status.
    pub then: AfterForeground,
}

/// What the runtime does after a [`Foreground`] command exited.
pub enum AfterForeground {
    /// Continue connecting `Endpoint` (`session::interactive_auth_finished`).
    InteractiveAuth(Endpoint),
    /// Report the VPN command and let the app reconnect
    /// (`external::vpn_finished`).
    Vpn(Endpoint),
    /// Report the `t` shell (`external::shell_finished`).
    Shell,
    /// Compare and upload the edited file (`edit::finish`), timed for the
    /// quick-exit hint.
    Edit(EditSession),
}

/// Spawned tasks registered under a key, so they can be aborted or
/// replaced. A task's entry is removed when it ends.
pub struct Tasks<K> {
    tasks: Mutex<HashMap<K, AbortHandle>>,
}

impl<K: Clone + Eq + Hash + Send + 'static> Tasks<K> {
    /// No tasks.
    pub fn new() -> Tasks<K> {
        Tasks {
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// Spawns `future` under `key`, aborting the task registered under the
    /// same key before. Must be called inside a Tokio runtime.
    pub fn spawn<F>(self: &Arc<Self>, key: K, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // The lock is held until the handle is stored, so a task that ends
        // at once still finds its own entry to remove.
        let mut tasks = self.lock();
        let registry = Arc::clone(self);
        let own_key = key.clone();
        let handle = tokio::spawn(async move {
            future.await;
            registry.finished(&own_key, tokio::task::id());
        });
        if let Some(previous) = tasks.insert(key, handle.abort_handle()) {
            previous.abort();
        }
    }

    /// Aborts and forgets `key`'s task; returns whether one was registered.
    pub fn abort(&self, key: &K) -> bool {
        match self.lock().remove(key) {
            Some(task) => {
                task.abort();
                true
            }
            None => false,
        }
    }

    /// Whether a task is registered under `key`.
    pub fn contains(&self, key: &K) -> bool {
        self.lock().contains_key(key)
    }

    /// Removes `key`'s entry if it still belongs to task `id` (a replacement
    /// registered under the same key stays).
    fn finished(&self, key: &K, id: tokio::task::Id) {
        let mut tasks = self.lock();
        if tasks.get(key).is_some_and(|task| task.id() == id) {
            tasks.remove(key);
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, AbortHandle>> {
        // The map stays consistent even if a holder panicked.
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<K: Clone + Eq + Hash + Send + 'static> Default for Tasks<K> {
    fn default() -> Tasks<K> {
        Tasks::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[test]
    fn the_context_can_move_into_spawned_tasks() {
        // Compiles only while every executor can clone `Ctx` into a task.
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Ctx>();
        assert_send_sync::<Foreground>();
    }

    #[tokio::test]
    async fn finished_tasks_deregister_themselves() {
        let tasks = Arc::new(Tasks::new());
        let (done, finished) = oneshot::channel();
        tasks.spawn(1, async move {
            done.send(()).expect("receiver alive");
        });
        finished.await.expect("ran");
        // The wrapper removes the entry right after the future.
        for _ in 0..100 {
            if !tasks.contains(&1) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!tasks.contains(&1));
        assert!(!tasks.abort(&1));
    }

    #[tokio::test]
    async fn spawning_under_a_key_replaces_and_abort_cancels() {
        let tasks = Arc::new(Tasks::new());
        // Each task holds a sender; an aborted task drops it unsent.
        let (first_tx, first_rx) = oneshot::channel::<()>();
        tasks.spawn(7, async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(first_tx);
        });
        let (second_tx, second_rx) = oneshot::channel::<()>();
        tasks.spawn(7, async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(second_tx);
        });
        assert!(first_rx.await.is_err(), "the replaced task was aborted");
        assert!(tasks.contains(&7));
        assert!(tasks.abort(&7));
        assert!(second_rx.await.is_err(), "aborted");
        assert!(!tasks.contains(&7));
    }
}
