//! Event loop: merges the crossterm `EventStream`, background events, and
//! signals; feeds the app; dispatches effects to executors; and redraws only
//! when state changed, coalesced to at most 60 frames per second.
//!
//! # Behavior
//!
//! - Before the first frame the runtime hands the app's image resize
//!   receiver to the resize worker, dispatches the startup effects, and
//!   sends `Action::Resize` with the terminal size (core wiring point 7).
//! - Terminal events go through `ui::input::InputState` with the `HitMap`
//!   of the last drawn frame; terminal resizes (SIGWINCH) arrive as
//!   crossterm resize events. SIGTERM, SIGINT, and SIGHUP end the loop like
//!   `Effect::Quit`.
//! - Effects are dispatched per the table in `ui::effects`. Effects that
//!   suspend the TUI (`InteractiveAuth`, `RunVpn`, `OpenShell`, `Edit`) are
//!   queued in arrival order and run one at a time through
//!   `TerminalGuard::run_foreground`; the asynchronous builders (`OpenShell`,
//!   `Edit`) are polled by the loop, so input keeps working while a remote
//!   file downloads. After a foreground program the terminal size is sent
//!   again and the frame is redrawn in full.
//! - A frame is drawn only after an action or a background event, and at
//!   most once per [`FRAME_INTERVAL`].
//! - Shutdown: input stops, the edit temp directory is removed (it lives in
//!   the session's ControlPath directory, whose teardown waits for that
//!   directory to empty), the event receiver and this runtime's engine
//!   sender are dropped, and `Sessions::shutdown` closes the masters.
//!
//! # Public API (Wave 3, integration owner)
//!
//! ```text
//! /// Runs the TUI until quit; always shuts down the executors.
//! pub async fn run(
//!     guard: &mut TerminalGuard,
//!     app: App,
//!     effects: Vec<Effect>,
//!     ctx: Ctx,
//!     events: UnboundedReceiver<BgEvent>,
//! ) -> std::io::Result<()>;
//! ```

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use ratatui::crossterm::event::{Event, EventStream};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::term::{Signals, TerminalGuard};
use crate::ui::action::{Action, BgEvent, Effect};
use crate::ui::app::App;
use crate::ui::effects::{
    edit, external, fs, preview, session, state, sync, timer, transfer, AfterForeground, Ctx,
    Foreground,
};
use crate::ui::hit::HitMap;
use crate::ui::input::InputState;
use crate::ui::render;

/// Shortest time between two frames (60 frames per second).
pub const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// A suspending effect waiting for its turn.
enum Pending {
    /// The builder still runs (a download or a master check).
    Building(Pin<Box<dyn Future<Output = Option<Foreground>>>>),
    /// Ready to run in the foreground (boxed: a `Command` is large).
    Ready(Box<Foreground>),
}

/// What woke the loop.
enum Step {
    Terminal(Option<io::Result<Event>>),
    Background(BgEvent),
    Built(Option<Foreground>),
    Signal,
    Draw,
}

/// Runs the TUI until quit; always shuts down the executors.
pub async fn run(
    guard: &mut TerminalGuard,
    mut app: App,
    effects: Vec<Effect>,
    ctx: Ctx,
    mut events: UnboundedReceiver<BgEvent>,
) -> io::Result<()> {
    if let Some(requests) = app.preview.resize_requests.take() {
        preview::spawn_resize_worker(requests, ctx.events.clone());
    }
    let result = event_loop(guard, &mut app, effects, &ctx, &mut events).await;
    // Results arriving from now on have nobody to act on them.
    drop(events);
    shutdown(ctx).await;
    result
}

async fn event_loop(
    guard: &mut TerminalGuard,
    app: &mut App,
    effects: Vec<Effect>,
    ctx: &Ctx,
    events: &mut UnboundedReceiver<BgEvent>,
) -> io::Result<()> {
    let mut signals = Signals::new()?;
    let mut input = InputState::new();
    let mut hits = HitMap::default();
    let mut foreground: VecDeque<Pending> = VecDeque::new();
    let mut terminal_events = EventStream::new();

    if dispatch_all(ctx, effects, &mut foreground) {
        return Ok(());
    }
    if send_size(guard, app, ctx, &mut foreground)? {
        return Ok(());
    }
    let mut dirty = true;
    let mut last_draw: Option<tokio::time::Instant> = None;

    loop {
        // Run the suspending effect at the front once it is built.
        match foreground.pop_front() {
            Some(Pending::Ready(job)) => {
                let started = Instant::now();
                let (fresh, status) = guard.run_foreground(terminal_events, job.command).await;
                terminal_events = fresh;
                finish_foreground(ctx, job.then, status, started.elapsed());
                if send_size(guard, app, ctx, &mut foreground)? {
                    return Ok(());
                }
                dirty = true;
                last_draw = None;
                continue;
            }
            Some(building) => foreground.push_front(building),
            None => {}
        }

        let draw_at = last_draw.map_or_else(tokio::time::Instant::now, |at| at + FRAME_INTERVAL);
        let step = tokio::select! {
            event = terminal_events.next() => Step::Terminal(event),
            Some(event) = events.recv() => Step::Background(event),
            built = front_built(&mut foreground) => Step::Built(built),
            _ = signals.recv() => Step::Signal,
            () = tokio::time::sleep_until(draw_at), if dirty => Step::Draw,
        };

        match step {
            Step::Terminal(None) => return Ok(()),
            Step::Terminal(Some(event)) => {
                let event = event?;
                let mode = app.input_mode();
                if let Some(action) = input.handle(&event, mode, &hits, Instant::now()) {
                    if dispatch_all(ctx, app.handle_action(action), &mut foreground) {
                        return Ok(());
                    }
                    dirty = true;
                }
            }
            Step::Background(event) => {
                if dispatch_all(ctx, app.handle_event(event), &mut foreground) {
                    return Ok(());
                }
                dirty = true;
            }
            Step::Built(built) => {
                foreground.pop_front();
                if let Some(job) = built {
                    foreground.push_front(Pending::Ready(Box::new(job)));
                }
            }
            Step::Signal => return Ok(()),
            Step::Draw => {
                guard
                    .terminal()
                    .draw(|frame| hits = render::draw(frame, app))?;
                dirty = false;
                last_draw = Some(tokio::time::Instant::now());
            }
        }
    }
}

/// Completes with the builder at the front of the queue; pending while the
/// front is not a builder.
async fn front_built(foreground: &mut VecDeque<Pending>) -> Option<Foreground> {
    match foreground.front_mut() {
        Some(Pending::Building(builder)) => builder.await,
        _ => std::future::pending().await,
    }
}

/// Sends the current terminal size to the app; `true` when that quit.
fn send_size(
    guard: &mut TerminalGuard,
    app: &mut App,
    ctx: &Ctx,
    foreground: &mut VecDeque<Pending>,
) -> io::Result<bool> {
    let size = guard.terminal().size()?;
    let effects = app.handle_action(Action::Resize {
        width: size.width,
        height: size.height,
    });
    Ok(dispatch_all(ctx, effects, foreground))
}

/// Dispatches every effect in order; `true` when one of them is
/// `Effect::Quit` (the effects after it are not run).
fn dispatch_all(ctx: &Ctx, effects: Vec<Effect>, foreground: &mut VecDeque<Pending>) -> bool {
    for effect in effects {
        if let Effect::Quit = effect {
            return true;
        }
        dispatch(ctx, effect, foreground);
    }
    false
}

/// Hands one effect to its executor (table in `ui::effects`).
fn dispatch(ctx: &Ctx, effect: Effect, foreground: &mut VecDeque<Pending>) {
    match effect {
        Effect::Connect(endpoint) => session::connect(ctx, endpoint),
        Effect::SudoPassword { endpoint, password } => {
            session::sudo_password(ctx, endpoint, password);
        }
        Effect::Reconnect(endpoint) => session::reconnect(ctx, endpoint),
        Effect::Disconnect(endpoint) => session::disconnect(ctx, &endpoint),
        Effect::ResolveHost(alias) => session::resolve_host(ctx, alias),
        Effect::InteractiveAuth(endpoint) => {
            if let Some(job) = session::interactive_auth(ctx, endpoint) {
                foreground.push_back(Pending::Ready(Box::new(job)));
            }
        }
        Effect::RunVpn { endpoint, command } => {
            foreground.push_back(Pending::Ready(Box::new(external::vpn(endpoint, command))));
        }
        Effect::OpenShell(location) => {
            let builder = external::shell(ctx.clone(), location);
            foreground.push_back(Pending::Building(Box::pin(builder)));
        }
        Effect::Edit(location) => {
            let builder = edit::prepare(ctx.clone(), location);
            foreground.push_back(Pending::Building(Box::pin(builder)));
        }
        Effect::ListDir { request, location } => fs::list_dir(ctx, request, location),
        Effect::FsOp {
            request,
            endpoint,
            op,
        } => fs::fs_op(ctx, request, endpoint, op),
        Effect::CheckDroppedPaths { text, paths } => {
            external::check_dropped_paths(ctx, text, paths);
        }
        Effect::Reveal(location) => external::reveal(ctx, location),
        Effect::Transfer(command) => transfer::send(ctx, command),
        Effect::Compare {
            request,
            left,
            right,
        } => sync::compare(ctx, request, left, right),
        Effect::Sync {
            job,
            direction,
            comparison,
            paths,
            delete_extraneous,
        } => sync::sync(ctx, job, direction, comparison, paths, delete_extraneous),
        Effect::CancelSync(job) => sync::cancel(ctx, job),
        Effect::LoadPreview { request, location } => preview::load(ctx, request, location),
        Effect::ResolveEditConflict {
            location,
            temp,
            overwrite,
        } => edit::resolve_conflict(ctx, location, temp, overwrite),
        Effect::RememberDirectory(location) => state::remember_directory(ctx, location),
        Effect::RememberSort { endpoint, sort } => state::remember_sort(ctx, endpoint, sort),
        Effect::SaveBookmark(location) => state::save_bookmark(ctx, location),
        Effect::RemoveBookmark(location) => state::remove_bookmark(ctx, location),
        Effect::CancelRequest(request) => {
            // Whether a task was still running does not matter: a result
            // that already arrived is dropped by the app as stale.
            ctx.requests.abort(&request);
        }
        Effect::ScheduleTimer { id, after } => timer::schedule(ctx, id, after),
        // `dispatch_all` ends the loop before dispatching `Quit`.
        Effect::Quit => {}
    }
}

/// Calls the finisher of a foreground program.
fn finish_foreground(
    ctx: &Ctx,
    then: AfterForeground,
    status: io::Result<std::process::ExitStatus>,
    elapsed: Duration,
) {
    match then {
        AfterForeground::InteractiveAuth(endpoint) => {
            session::interactive_auth_finished(ctx, endpoint, status);
        }
        AfterForeground::Vpn(endpoint) => external::vpn_finished(ctx, endpoint, status),
        AfterForeground::Shell => external::shell_finished(ctx, status),
        AfterForeground::Edit(edit_session) => edit::finish(ctx, edit_session, status, elapsed),
    }
}

/// Stops the executors: removes the edit temp directory, drops this
/// runtime's engine sender (with the context), and tears the sessions down.
async fn shutdown(ctx: Ctx) {
    match tokio::fs::remove_dir_all(&ctx.edit_temp).await {
        Ok(()) => {}
        // Nothing was edited in this run.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        // Nobody is left to tell; the session's next start removes this
        // process's directory, the temp directory included.
        Err(error) => drop(error),
    }
    let sessions = std::sync::Arc::clone(&ctx.sessions);
    drop(ctx);
    sessions.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_interval_caps_at_sixty_frames_per_second() {
        assert!(FRAME_INTERVAL >= Duration::from_secs(1) / 60);
        assert!(FRAME_INTERVAL < Duration::from_millis(17));
    }
}
