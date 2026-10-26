//! Transfer engine: expands jobs into items, schedules them with a
//! semaphore of `concurrency` slots, applies pause/cancel through
//! `tokio::sync::watch`, raises and resolves conflicts, retries, parks items
//! as `WaitingForReconnect` on lost connections, resumes `.scpto-part`
//! files whose `partials.json` record matches the source, and performs
//! moves (rename within an endpoint, copy-then-delete across endpoints).
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
//!   missing parent directories.
//! - Mode: `TransferMode::Tar` when the targets are `IntoDirectory`, one
//!   side is `Local`, the other a non-sudo remote whose
//!   `Connections::tools().tar` is true, and the expanded file count is at
//!   least `tar-threshold`, and every source name and the destination
//!   directory are valid UTF-8 (remote commands are strings); otherwise
//!   `PerFile`. Tar jobs check existing
//!   destination names first and raise `TarConflictRaised` when any exist,
//!   then run `transfer::tar`.
//! - Per-file items wait for a `concurrency` permit; conflicts use the
//!   destination's actual `stat` and NFC-normalized names; the job's preset
//!   or an earlier "apply to all" answers without asking.
//!   `OverwriteIfNewer` compares whole seconds with `mtime-tolerance`.
//!   Rename picks the first free `name (N).ext`.
//! - Resume: when a `.scpto-part` exists and the `PartialStore` record
//!   matches the source size and mtime, the conflict offers `Resume`
//!   (`Conflict::partial`) and a non-conflicting item resumes silently;
//!   otherwise the part is restarted from zero. The record is set before
//!   the first byte is written and removed after success or
//!   `DiscardPartial`.
//! - Pause/cancel: each running item selects on its `watch` control and
//!   drops the `stream::copy_file` future; the part file is kept.
//! - `FsError::Disconnected` parks the item as `WaitingForReconnect`; it
//!   re-queues on `TransferCommand::EndpointReconnected` with fresh handles
//!   from `Connections::fs`.
//! - Move: same endpoint → `rename` (falling back to copy+delete only for
//!   `FsError::Unsupported`); across endpoints → copy, then delete each
//!   source file after its item is `Done`, and each source directory only
//!   when every item beneath it is `Done`. Same-endpoint remote copies use
//!   `copy_within` when available (inside `stream`).
//!
//! # Public API (Wave 2; uses `transfer::stream` and `transfer::tar` from
//! Wave 3 wiring)
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
//! /// Runs the engine until `commands` is closed and every item task ended.
//! /// Startup spawns it on the runtime.
//! pub async fn run(
//!     config: EngineConfig,
//!     connections: Arc<dyn Connections>,
//!     partials: Arc<dyn PartialStore>,
//!     commands: TransferCommandRx,
//!     events: TransferEventTx,
//! );
//! ```
