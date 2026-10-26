//! Local `Fs` implementation on `tokio::fs`. Deletion goes to the system
//! trash through the `trash` crate (`DeleteMethod::NsFileManager` on macOS,
//! so Finder automation is never prompted), run in `spawn_blocking`.
//!
//! Errors are classified with `FsError::from_io` on the path concerned.
//! `list` uses `symlink_metadata` per entry and `metadata` on symlinks only
//! to fill `target_is_dir` (a dangling link is not an error). Modes are
//! `mode & 0o7777`; mtimes are whole seconds. `home` is `$HOME`.
//! `open_write` with `WriteMode::Resume { offset }` opens the existing file
//! without truncating and seeks to `offset`. `copy_within` is
//! `FsError::Unsupported` (local copies stream like any other); `trash`
//! failures are `FsError::TrashFailed` so the app can offer a permanent
//! delete.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! pub struct LocalFs { /* private: endpoint == Endpoint::Local */ }
//!
//! impl LocalFs {
//!     pub fn new() -> LocalFs;
//! }
//!
//! impl Fs for LocalFs { /* every method */ }
//! ```
//!
//! `capabilities()`: `posix_rename: true`, `copy_within: false`,
//! `symlink: true`, `trash: true`.
