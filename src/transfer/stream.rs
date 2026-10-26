//! Single-file stream copy from any `Fs` to any `Fs`. Remote-to-remote
//! copies relay through local memory without touching disk; same-endpoint
//! remote copies use `copy_within` when the server supports it. Writes to
//! `<name>.scpto-part`, renames to the final name on completion, then sets
//! mtime and permissions (`mode & 0o777`).
//!
//! # Contract
//!
//! - The part file is the final destination with `.scpto-part` appended to
//!   its file name, in the same directory.
//! - `resume_from > 0` opens the part file with `WriteMode::Resume` and the
//!   source with `open_read(source, resume_from)`; `0` truncates. Deciding
//!   whether resuming is allowed (the `partials.json` record) is the
//!   engine's job.
//! - Data is copied in chunks with a bounded buffer; `progress` is called
//!   with the total bytes now in the part file (including the resumed
//!   prefix) at most every 100 ms and once at the end.
//! - On success the writer is shut down (committing the data), the part is
//!   renamed onto the destination (`Fs::rename` handles servers without
//!   `posix-rename`), then `set_times(mtime, mtime)` and `chmod(mode &
//!   0o777)` are applied when the source values are known.
//! - Pause and cancel are done by the engine dropping the future: the part
//!   file stays and can be resumed. No other cancellation mechanism exists.
//! - `copy_within` path: when both sides are the same endpoint and
//!   `capabilities().copy_within`, the server copies to the part path, which
//!   is then renamed as above; progress jumps to `size` at the end.
//! - Errors are returned unchanged; `FsError::Disconnected` lets the engine
//!   park the item as `WaitingForReconnect`.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! /// `<destination>.scpto-part`.
//! pub fn part_path(destination: &Path) -> PathBuf;
//!
//! pub struct CopySpec<'a> {
//!     pub source: &'a dyn Fs,
//!     pub source_path: &'a Path,
//!     pub destination: &'a dyn Fs,
//!     /// The final path (not the part path).
//!     pub destination_path: &'a Path,
//!     /// Source size, for the copy_within progress and sanity checks.
//!     pub size: u64,
//!     pub mtime: Option<i64>,
//!     pub mode: Option<u32>,
//!     /// Bytes already in a matching part file; 0 starts over.
//!     pub resume_from: u64,
//! }
//!
//! /// Copies one regular file as described above.
//! pub async fn copy_file(
//!     spec: CopySpec<'_>,
//!     progress: &(dyn Fn(u64) + Send + Sync),
//! ) -> Result<(), FsError>;
//! ```
