//! SFTP `Fs` implementation on `openssh-sftp-client`, over an `Sftp` built
//! from the stdin/stdout of an `ssh -s <host> sftp` child (or a sudo
//! sftp-server child). Maps protocol and background-task failures to
//! `FsError`, with a lost connection reported as `FsError::Disconnected`.
//!
//! An `SftpFs` owns its child for as long as its `Sftp` lives (plan 7.3
//! S10): dropping it closes the session and kills the child
//! (`kill_on_drop`). Errors from `Sftp::close()` after a disconnect are
//! logged, never shown.
//!
//! Facts from the Wave 1 spike that this module must honor: `readdir`
//! returns `.` and `..` (filtered here); OpenSSH 7.9 lacks `limits`,
//! `copy-data`, and `expand-path`, so every extension is feature-detected
//! (`posix_rename` from `posix-rename@openssh.com`, `copy_within` from
//! `copy-data`, `home` through `realpath(".")`); without
//! `posix-rename@openssh.com`, `rename` onto an existing non-directory
//! target removes the target first (non-atomic, plan 7.5 S14).
//!
//! # Error mapping
//!
//! - `BackgroundTaskFailure` (master or child gone; child stderr is empty)
//!   → `FsError::Disconnected` for operations, `ConnectError::ConnectionLost`
//!   at start.
//! - At start only: `SftpServerHelloMsgTooLong` → `ConnectError::ProtocolCorrupted`
//!   (shell startup output); `UnexpectedEof` with stderr line
//!   `Cannot execute command-line and remote command.` →
//!   `ConnectError::ProtocolCorrupted` naming the `RemoteCommand` conflict;
//!   other `UnexpectedEof` → `ConnectError::SftpServerMissing` with the
//!   stderr tail; any other start failure → `ConnectError::UnsupportedServer`.
//! - SFTP status codes: no such file → `NotFound`, permission denied →
//!   `PermissionDenied`, op unsupported → `Unsupported`, failure on
//!   `mkdir`/`rename` of an existing target → `AlreadyExists` after a stat
//!   confirms it, everything else → `FsError::Io` with the server message.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! pub struct SftpFs { /* private: endpoint, Sftp, RemoteChild, extensions */ }
//!
//! impl SftpFs {
//!     /// Runs the SFTP handshake on the child's stdin/stdout and queries
//!     /// the extensions. `channel.stderr` is consulted for classification.
//!     pub async fn start(endpoint: Endpoint, channel: RemoteChild) -> Result<SftpFs, ConnectError>;
//!     /// Resolves when the session is unusable (child exited or the
//!     /// client's background task failed); the session awaits it to notice
//!     /// a lost connection.
//!     pub async fn closed(&self);
//! }
//!
//! impl Fs for SftpFs { /* every method */ }
//! ```
//!
//! `capabilities()`: `posix_rename` and `copy_within` as advertised,
//! `symlink: true`, `trash: false`.
