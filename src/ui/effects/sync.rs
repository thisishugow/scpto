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
