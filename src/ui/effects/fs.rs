//! Executes `Effect::ListDir` and `Effect::FsOp` against the browsing
//! channel (`FsChannel::Browse`) of each endpoint, including recursive
//! permanent deletes.
//!
//! # Behavior
//!
//! - `list_dir` runs under `ctx.requests` (so `CancelRequest` abandons it):
//!   `Connections::fs(endpoint, Browse)` then `Fs::list`, answered with
//!   `Listed`; a failure to get the file system is the listing's error
//!   (`FsError::Disconnected` for connection errors).
//! - `fs_op` answers `FsOpFinished` with one `FsError` per failed path:
//!   `MakeDir` → `Fs::mkdir`; `Rename` → `Fs::lstat(to)` first and
//!   `FsError::AlreadyExists` when the target exists (a rename never
//!   replaces), else `Fs::rename`; `Delete` with `DeleteMethod::Trash` →
//!   `Fs::trash` per path (failures stay `FsError::TrashFailed` so the app
//!   can offer a permanent delete), with `DeleteMethod::Permanent` →
//!   `vfs::remove_tree` per path; `Chmod` → `Fs::chmod` per path.
//!
//! # Public API (Wave 3, ui/effects/fs)
//!
//! ```text
//! pub fn list_dir(ctx: &Ctx, request: RequestId, location: Location);
//! pub fn fs_op(ctx: &Ctx, request: RequestId, endpoint: Endpoint, op: FsOp);
//! ```
