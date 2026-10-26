//! sudo mode: starting a root sftp-server over the master connection
//! (plan 8.5, verified by the Wave 1 sudo spike on OpenSSH 7.9 and current
//! OpenSSH with sudo 1.8 and 1.9).
//!
//! # Flow
//!
//! 1. Locate sftp-server: run `cat /etc/ssh/sshd_config` and take the first
//!    `Subsystem sftp <path>` line before any `Match` (skipping comments,
//!    `internal-sftp`, and relative paths; extra arguments dropped). The
//!    candidates are that path followed by [`SFTP_SERVER_CANDIDATES`],
//!    deduplicated. One remote `sh` loop prints the first candidate that is a
//!    regular executable file (`[ -f ] && [ -x ]`); only output exactly equal
//!    to a candidate is accepted. None → `SudoError::SftpServerNotFound`.
//! 2. Stage 1, no stdin write: `env LC_ALL=C sudo -n sh -c '<WRAPPER>'
//!    '<sftp-server>'`. Succeeds on NOPASSWD or a global timestamp.
//! 3. On `a password is required`: kill stage 1, run `env LC_ALL=C sudo -n
//!    -v` once; `may not run sudo on` → `SudoError::NotInSudoers` without
//!    asking. Otherwise report `SudoError::PasswordRequired` so the user is
//!    asked.
//! 4. Stage 2 with the password: `env LC_ALL=C sudo -k -S -p '' sh -c
//!    '<WRAPPER>' '<sftp-server>'`; write `password\n` exactly once and
//!    nothing else until the marker. The first failure line kills the child
//!    at once; sudo is never retried on the same child.
//!
//! Each start spawns its own child (stdin, stdout, and stderr piped,
//! `process_group(0)`, `kill_on_drop(true)`) from the argv of
//! `Connections::ssh_command(endpoint, false)` plus the host alias and the
//! remote command joined with the session's quoting helper. The wrapper is a
//! fixed string without single quotes; `$0` is the quoted sftp-server path.
//!
//! # Reading stderr
//!
//! A dedicated task reads stderr line by line (stripping `\r\n` / `\n`) for
//! the child's whole life. Before the marker each line is checked in this
//! order:
//!
//! 1. The whole line equals [`MARKER_OK`]: ready. stdin and stdout are
//!    handed to `Sftp::new` (through `vfs::sftp::SftpFs`) only now; the task
//!    keeps draining into a `StderrTail` of the last [`STDERR_TAIL_LINES`]
//!    lines. The marker must be matched as a whole line because sudo's
//!    "not allowed" message echoes the wrapper, which contains it.
//! 2. The whole line equals [`MARKER_NOSFTP`] → `SudoError::SftpServerNotFound`.
//! 3. Contains `a password is required` → `PasswordRequired` in stage 1,
//!    `WrongPassword` in stage 2.
//! 4. Starts with `Sorry, try again.` or contains `incorrect password
//!    attempt` → `WrongPassword`.
//! 5. Contains `is not in the sudoers file` or `may not run sudo on` →
//!    `NotInSudoers`.
//! 6. Contains `must have a tty` → `RequiresTty`.
//! 7. Contains `is not allowed to execute` → `NotAllowedToExecute`.
//! 8. Anything else (the sudo lecture, OpenSSH warnings) is ignored.
//!
//! No marker within [`START_TIMEOUT`] → `SudoError::Timeout`; more than
//! [`PRE_MARKER_LIMIT`] bytes before the marker → `SudoError::UnexpectedOutput`;
//! end of stderr before the marker → `SudoError::Other` with the last
//! non-ignored lines (never a line echoing the wrapper). On every failure
//! the child is killed.
//!
//! # Password handling
//!
//! The password arrives as `ui::action::Secret`. The line written to stdin
//! is built in a `Zeroizing<Vec<u8>>` with capacity `len + 1`, written with
//! `write_all` plus `flush`, and dropped at once. Nothing logs or formats the
//! password; only its byte count may be logged. The masked input box builds
//! its `Zeroizing<String>` with capacity [`PASSWORD_MAX_LEN`] and rejects
//! input beyond it, so the buffer never reallocates.
//!
//! # Public API (Wave 3, security-executor)
//!
//! ```text
//! /// Finds the sftp-server binary on the host (step 1) through
//! /// `Connections::spawn_remote` on `endpoint` (runs as the login user).
//! pub async fn locate_sftp_server(
//!     connections: &dyn Connections,
//!     endpoint: &Endpoint,
//! ) -> Result<PathBuf, SudoError>;
//!
//! /// Runs stage 1, or stage 2 when `password` is given, and returns the
//! /// running root sftp-server once the marker arrived. Stage 1 answers
//! /// `Err(SudoError::PasswordRequired)` when a password is needed (after the
//! /// `sudo -n -v` sudoers check of step 3).
//! pub async fn start(
//!     connections: &dyn Connections,
//!     endpoint: &Endpoint,
//!     sftp_server: &Path,
//!     password: Option<&Secret>,
//! ) -> Result<RemoteChild, SudoError>;
//!
//! /// Pure: the parsed `Subsystem sftp` path of an sshd_config text.
//! pub fn parse_subsystem_sftp(sshd_config: &str) -> Option<PathBuf>;
//!
//! /// Pure: the classification of one pre-marker stderr line (rules above).
//! pub fn classify_line(line: &str, with_password: bool) -> LineClass;
//!
//! /// What one pre-marker stderr line means.
//! pub enum LineClass {
//!     /// The whole line is `MARKER_OK`.
//!     Ready,
//!     /// A known failure.
//!     Failed(SudoError),
//!     /// Lecture, warnings, anything else.
//!     Ignored,
//! }
//! ```
//!
//! The session calls `start` from its `Connections::fs` implementation for
//! sudo endpoints and maps `PasswordRequired` to
//! `BgEvent::SudoPasswordRequired`; every other `SudoError` becomes
//! `BgEvent::ConnectFailed` with `Error::Sudo`.

/// The root `sh` wrapper run by sudo. `$0` is the sftp-server path. It prints
/// [`MARKER_OK`] only after sudo authenticated and authorized the command.
pub const WRAPPER: &str = r#"[ -x "$0" ] || { echo SCPTO-SUDO-NOSFTP >&2; exit 127; }; echo SCPTO-SUDO-OK >&2; exec "$0""#;

/// stderr line printed by [`WRAPPER`] right before it executes sftp-server.
pub const MARKER_OK: &str = "SCPTO-SUDO-OK";

/// stderr line printed by [`WRAPPER`] when the sftp-server path is not
/// executable.
pub const MARKER_NOSFTP: &str = "SCPTO-SUDO-NOSFTP";

/// Fixed sftp-server locations probed after the `sshd_config` path, in
/// order.
pub const SFTP_SERVER_CANDIDATES: &[&str] = &[
    "/usr/lib/openssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/sftp-server",
];

/// How long a start waits for the marker or a known failure.
pub const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Maximum stderr bytes accepted before the marker.
pub const PRE_MARKER_LIMIT: usize = 64 * 1024;

/// stderr lines kept for classification after the marker.
pub const STDERR_TAIL_LINES: usize = 64;

/// Maximum length in bytes of a sudo password typed into the masked input.
/// The input buffer is allocated with this capacity up front.
pub const PASSWORD_MAX_LEN: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_fits_in_single_quotes_and_prints_both_markers() {
        // The wrapper is passed to the remote shell inside single quotes.
        assert!(!WRAPPER.contains('\''));
        assert!(WRAPPER.contains(&format!("echo {MARKER_OK} >&2")));
        assert!(WRAPPER.contains(&format!("echo {MARKER_NOSFTP} >&2")));
    }

    #[test]
    fn candidates_are_absolute() {
        assert!(SFTP_SERVER_CANDIDATES.iter().all(|p| p.starts_with('/')));
    }
}
