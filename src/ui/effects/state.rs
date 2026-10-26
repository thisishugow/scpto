//! Executes `Effect::RememberDirectory`, `Effect::SaveBookmark`, and
//! `Effect::RemoveBookmark` using `state`.
//!
//! # Behavior
//!
//! Each entry point spawns a task that locks `ctx.persisted`, applies the
//! change (`PersistedState::remember_directory`, `add_bookmark`,
//! `remove_bookmark`), and, when `ctx.state_dir` is set, saves the whole
//! value with `state::save_state` in `spawn_blocking` while still holding
//! the lock, so saves never overtake each other. A failed save is sent as
//! `StateWriteFailed`. Without a state directory nothing is saved (startup
//! already said so in the status bar).
//!
//! # Public API (Wave 3, ui/effects/state)
//!
//! ```text
//! pub fn remember_directory(ctx: &Ctx, location: Location);
//! pub fn save_bookmark(ctx: &Ctx, location: Location);
//! pub fn remove_bookmark(ctx: &Ctx, location: Location);
//! ```
