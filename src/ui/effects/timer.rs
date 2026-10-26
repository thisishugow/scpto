//! Executes `Effect::ScheduleTimer`, delivering `BgEvent::TimerFired`;
//! rescheduling an id replaces the pending timer.
//!
//! # Behavior
//!
//! `schedule` spawns the timer under `ctx.timers` with the timer id (which
//! aborts a pending timer of the same id): it sleeps for `after`, then sends
//! `TimerFired { id, at: Instant::now() }`.
//!
//! # Public API (Wave 3, ui/effects/timer)
//!
//! ```text
//! pub fn schedule(ctx: &Ctx, id: TimerId, after: Duration);
//! ```
