//! tar acceleration between the local machine and a non-sudo remote: the
//! `tar` crate packs or unpacks locally through `tokio_util::io::SyncIoBridge`
//! in `spawn_blocking` (symlinks not followed; absolute and `..` entries
//! rejected), streaming through remote `tar -C <dir> -xf -` / `-cf -`.
//!
//! # Contract (plan 7.5 S8)
//!
//! - The remote side is `Connections::spawn_remote(remote, ["tar", "-C",
//!   dir, "-xf", "-"])` for uploads and `["tar", "-C", dir, "-cf", "-",
//!   names...]` for downloads (words are quoted by the session). Its stderr
//!   tail goes into `Error::CommandFailed { program: "tar", .. }` on a
//!   non-zero exit.
//! - Packing: `Builder::follow_symlinks(false)`, entries named relative to
//!   the local source's parent, so each source lands under its own name.
//! - Unpacking: every entry is checked first; an absolute path or a `..`
//!   component fails the job with `Error::Archive`. Extraction uses
//!   `Entry::unpack_in(local_dir)` with `set_overwrite(true)` and
//!   `set_preserve_permissions(false)` plus `mode & 0o777` applied
//!   explicitly; mtimes are kept.
//! - No `.scpto-part`, no resume: a failure leaves whatever was written.
//! - `progress` is called with the tar stream bytes moved so far.
//! - Cancel is by dropping the future (the remote child is killed on drop).
//!
//! # Public API (Wave 3)
//!
//! ```text
//! /// Packs `sources` (absolute local paths) into remote `remote_dir`.
//! pub async fn upload(
//!     connections: &dyn Connections,
//!     sources: &[PathBuf],
//!     remote: &Endpoint,
//!     remote_dir: &Path,
//!     progress: &(dyn Fn(u64) + Send + Sync),
//! ) -> Result<(), Error>;
//!
//! /// Unpacks `names` (entries of remote `remote_dir`) into `local_dir`.
//! pub async fn download(
//!     connections: &dyn Connections,
//!     remote: &Endpoint,
//!     remote_dir: &Path,
//!     names: &[OsString],
//!     local_dir: &Path,
//!     progress: &(dyn Fn(u64) + Send + Sync),
//! ) -> Result<(), Error>;
//! ```
