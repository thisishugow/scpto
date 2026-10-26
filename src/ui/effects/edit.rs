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
//!
//! # Public API (Wave 3, ui/effects/edit)
//!
//! ```text
//! pub async fn prepare(ctx: Ctx, location: Location) -> Option<Foreground>;
//! pub fn finish(ctx: &Ctx, session: EditSession, status: std::io::Result<std::process::ExitStatus>, elapsed: Duration);
//! pub fn resolve_conflict(ctx: &Ctx, location: Location, temp: PathBuf, overwrite: bool);
//! ```
//!
//! [`Foreground`]: super::Foreground
