//! Executes `Effect::ScheduleTimer`, delivering `BgEvent::TimerFired`;
//! rescheduling an id replaces the pending timer.
//!
//! # Behavior
//!
//! `schedule` spawns the timer under `ctx.timers` with the timer id (which
//! aborts a pending timer of the same id): it sleeps for `after`, then sends
//! `TimerFired { id, at: Instant::now() }`.
//!
//! # Public API
//!
//! ```text
//! pub fn schedule(ctx: &Ctx, id: TimerId, after: Duration);
//! ```

use std::time::{Duration, Instant};

use super::Ctx;
use crate::ui::action::{BgEvent, TimerId};

/// Sends `BgEvent::TimerFired { id, .. }` after `after`, replacing a
/// pending timer of the same id (`Effect::ScheduleTimer`).
pub fn schedule(ctx: &Ctx, id: TimerId, after: Duration) {
    let events = ctx.events.clone();
    ctx.timers.spawn(id, async move {
        tokio::time::sleep(after).await;
        let fired = BgEvent::TimerFired {
            id,
            at: Instant::now(),
        };
        if let Err(unsent) = events.send(fired) {
            // The app has exited; nobody is left to report to.
            drop(unsent);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::session::testing::Harness;
    use super::*;
    use crate::connections::SshClient;

    async fn expect_fired(harness: &mut Harness) -> (TimerId, Instant) {
        match harness.next().await {
            BgEvent::TimerFired { id, at } => (id, at),
            other => panic!("expected TimerFired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_timer_fires_once_after_its_delay() {
        let mut harness = Harness::new(SshClient::default()).await;
        let scheduled = Instant::now();
        schedule(&harness.ctx, TimerId(1), Duration::from_millis(50));
        let (id, at) = expect_fired(&mut harness).await;
        assert_eq!(id, TimerId(1));
        assert!(at.duration_since(scheduled) >= Duration::from_millis(50));
        harness.expect_quiet(Duration::from_millis(150)).await;
        harness.finish().await;
    }

    #[tokio::test]
    async fn rescheduling_an_id_replaces_its_pending_timer() {
        let mut harness = Harness::new(SshClient::default()).await;
        schedule(&harness.ctx, TimerId(7), Duration::from_millis(300));
        schedule(&harness.ctx, TimerId(7), Duration::from_millis(20));
        let (id, _) = expect_fired(&mut harness).await;
        assert_eq!(id, TimerId(7));
        // The replaced 300 ms timer never fires.
        harness.expect_quiet(Duration::from_millis(500)).await;
        harness.finish().await;
    }

    #[tokio::test]
    async fn timers_of_different_ids_are_independent() {
        let mut harness = Harness::new(SshClient::default()).await;
        schedule(&harness.ctx, TimerId(1), Duration::from_millis(120));
        schedule(&harness.ctx, TimerId(2), Duration::from_millis(20));
        assert_eq!(expect_fired(&mut harness).await.0, TimerId(2));
        assert_eq!(expect_fired(&mut harness).await.0, TimerId(1));
        harness.expect_quiet(Duration::from_millis(150)).await;
        harness.finish().await;
    }
}
