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
//!    to a candidate is accepted. None → `SudoError::SftpServerNotFound`;
//!    any other output (shell startup noise) →
//!    `ConnectError::ProtocolCorrupted`. Both commands go through
//!    `Connections::spawn_remote`, which quotes the candidates; each probe is
//!    bounded by [`START_TIMEOUT`].
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
//! `Connections::ssh_command(endpoint, false)` plus `--`, the host alias, and
//! the remote command. Every word of the remote command is single-quoted by
//! this module's private quoting function, which follows plan 7.3 exactly
//! (`'` → `'\''`, a word containing a newline is rejected). It duplicates the
//! session's quoting helper only because that helper is not part of the
//! frozen contract yet. The wrapper is a fixed string without single quotes;
//! `$0` is the quoted sftp-server path.
//!
//! # Reading stderr
//!
//! A dedicated task reads stderr line by line (stripping `\r\n` / `\n`) for
//! the child's whole life. Before the marker each line is checked in this
//! order:
//!
//! 1. The whole line equals [`MARKER_OK`]: ready. stdin and stdout are
//!    returned in a `RemoteChild` for `Sftp::new` (through
//!    `vfs::sftp::SftpFs`) only now; the task keeps draining into a
//!    `StderrTail` of the last [`STDERR_TAIL_LINES`] lines written after the
//!    marker. The marker must be matched as a whole line because sudo's
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
//! ignored lines that are neither the sudo lecture, nor a line containing a
//! marker (which includes any line echoing the wrapper), nor a line
//! containing the password, plus the exit status. On every failure the child
//! is killed (dropping it sends `SIGKILL` through `kill_on_drop`; tokio reaps
//! it).
//!
//! # Password handling
//!
//! The password arrives as `ui::action::Secret`. A password longer than
//! [`PASSWORD_MAX_LEN`] bytes or containing `\n` or `\r` (sudo ends the
//! password at either, so the rest would reach sftp-server) is rejected
//! before anything is spawned. The line written to stdin is built in a
//! `Zeroizing<Vec<u8>>` with capacity `len + 1`, written with `write_all`
//! plus `flush`, and dropped at once. Nothing logs or formats the password;
//! error texts carry at most its byte count. The masked input box builds its
//! `Zeroizing<String>` with capacity [`PASSWORD_MAX_LEN`] and rejects input
//! beyond it, so the buffer never reallocates.
//!
//! # Public API
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
//! `BgEvent::ConnectFailed` with `Error::Sudo`. The session must pass a
//! password only after stage 1 answered `PasswordRequired`: on a NOPASSWD
//! host stage 2 does not read the line, so it reaches sftp-server, which
//! exits (verified by the `extra_password` integration cases).
//!
//! # Tests
//!
//! Unit tests run sudo's side as a local `sh` script through a test
//! `Connections`. The Docker integration cases (plan 6) run only with
//! `SCPTO_IT=1` against the fixtures of `tests/it/fixtures/` (started with
//! `tests/it/fixtures/up.sh`); `SCPTO_IT_DIR` overrides the directory of the
//! generated `ssh_config` (default `target/it` in the crate root).

use std::collections::VecDeque;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::{timeout, timeout_at, Instant};
use zeroize::Zeroizing;

use crate::connections::{Connections, RemoteChild, StderrTail};
use crate::error::{ConnectError, Error, ExitStatus, SudoError};
use crate::ui::action::Secret;
use crate::vfs::Endpoint;

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

/// The file read for the `Subsystem sftp` line.
const SSHD_CONFIG: &str = "/etc/ssh/sshd_config";

/// Remote probe run as `sh -c PROBE_SCRIPT sh <candidate>...`: prints the
/// first candidate that is a regular executable file, or exits 1.
const PROBE_SCRIPT: &str = r#"for p in "$@"; do if [ -f "$p" ] && [ -x "$p" ]; then printf '%s\n' "$p"; exit 0; fi; done; exit 1"#;

/// Exit status of [`PROBE_SCRIPT`] when no candidate is executable.
const PROBE_NOT_FOUND: i32 = 1;

/// Upper bound for the stdout of a probe command. A larger `sshd_config` is
/// treated as unreadable.
const PROBE_OUTPUT_LIMIT: usize = 256 * 1024;

/// The sudoers pre-check of step 3.
const VALIDATE_WORDS: [&str; 5] = ["env", "LC_ALL=C", "sudo", "-n", "-v"];

/// Longest stderr chunk kept as one tail entry after the marker; longer
/// lines are split.
const TAIL_LINE_LIMIT: usize = 4096;

/// Lines kept from ignored pre-marker stderr for `SudoError::Other`.
const DETAIL_LINES: usize = 4;

/// Longest single line, in characters, quoted in `SudoError::Other`.
const DETAIL_LINE_CHARS: usize = 200;

/// How long to wait for the exit status after stderr closed.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// The sudo lecture (sudo 1.8 and 1.9), compared after trimming. It is
/// ignored and never quoted in an error.
const LECTURE_LINES: [&str; 6] = [
    "We trust you have received the usual lecture from the local System",
    "Administrator. It usually boils down to these three things:",
    "#1) Respect the privacy of others.",
    "#2) Think before you type.",
    "#3) With great power comes great responsibility.",
    "For security reasons, the password you type will not be visible.",
];

/// What one pre-marker stderr line means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineClass {
    /// The whole line is [`MARKER_OK`].
    Ready,
    /// A known failure.
    Failed(SudoError),
    /// Lecture, warnings, anything else.
    Ignored,
}

/// Classifies one pre-marker stderr line, without its line terminator, by
/// the rules of the module documentation. `with_password` is true in stage 2,
/// where `a password is required` means the password was not accepted.
pub fn classify_line(line: &str, with_password: bool) -> LineClass {
    // Whole-line marker checks come first: sudo's "not allowed to execute"
    // message echoes the wrapper, which contains both markers.
    if line == MARKER_OK {
        return LineClass::Ready;
    }
    let failure = if line == MARKER_NOSFTP {
        SudoError::SftpServerNotFound
    } else if line.contains("a password is required") {
        if with_password {
            SudoError::WrongPassword
        } else {
            SudoError::PasswordRequired
        }
    } else if line.starts_with("Sorry, try again.") || line.contains("incorrect password attempt") {
        SudoError::WrongPassword
    } else if line.contains("is not in the sudoers file") || line.contains("may not run sudo on") {
        SudoError::NotInSudoers
    } else if line.contains("must have a tty") {
        SudoError::RequiresTty
    } else if line.contains("is not allowed to execute") {
        SudoError::NotAllowedToExecute
    } else {
        return LineClass::Ignored;
    };
    LineClass::Failed(failure)
}

/// The path of the first `Subsystem sftp <path>` line of an `sshd_config`
/// text, read the way sshd reads it: comment lines are skipped, parsing
/// stops at the first `Match` block (its settings are conditional), and
/// arguments after the path are dropped. `internal-sftp` and relative paths
/// give `None`, because sshd uses only the first definition.
pub fn parse_subsystem_sftp(sshd_config: &str) -> Option<PathBuf> {
    for line in sshd_config.lines() {
        let mut words = line.split_whitespace();
        let Some(keyword) = words.next() else {
            continue;
        };
        if keyword.starts_with('#') {
            continue;
        }
        if keyword.eq_ignore_ascii_case("match") {
            return None;
        }
        if keyword.eq_ignore_ascii_case("subsystem") && words.next() == Some("sftp") {
            return words
                .next()
                .filter(|path| path.starts_with('/'))
                .map(PathBuf::from);
        }
    }
    None
}

/// Finds the sftp-server binary on the host (step 1) through
/// [`Connections::spawn_remote`] on `endpoint` (runs as the login user).
pub async fn locate_sftp_server(
    connections: &dyn Connections,
    endpoint: &Endpoint,
) -> Result<PathBuf, SudoError> {
    let config_argv = vec!["cat".to_owned(), SSHD_CONFIG.to_owned()];
    let config = run_probe(connections, endpoint, &config_argv).await?;
    // An unreadable sshd_config (mode 0600 on RHEL-family hosts) is expected:
    // the fixed candidates still apply.
    let configured = if config.status.success() && !config.truncated {
        parse_subsystem_sftp(&String::from_utf8_lossy(&config.stdout))
    } else {
        None
    };
    let candidates = candidate_list(configured.as_deref());

    let mut probe_argv = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        PROBE_SCRIPT.to_owned(),
        "sh".to_owned(),
    ];
    probe_argv.extend(candidates.iter().cloned());
    let probe = run_probe(connections, endpoint, &probe_argv).await?;
    accept_probe_output(&probe, &candidates)
}

/// Runs stage 1, or stage 2 when `password` is given, and returns the
/// running root sftp-server once the marker arrived. Stage 1 answers
/// `Err(SudoError::PasswordRequired)` when a password is needed (after the
/// `sudo -n -v` sudoers check of step 3).
///
/// The returned child's `stderr` tail holds only lines written after the
/// marker. The password is used for this call only and never retained.
pub async fn start(
    connections: &dyn Connections,
    endpoint: &Endpoint,
    sftp_server: &Path,
    password: Option<&Secret>,
) -> Result<RemoteChild, SudoError> {
    start_within(connections, endpoint, sftp_server, password, START_TIMEOUT).await
}

/// [`start`] with an explicit marker timeout (tests use a short one).
async fn start_within(
    connections: &dyn Connections,
    endpoint: &Endpoint,
    sftp_server: &Path,
    password: Option<&Secret>,
    limit: Duration,
) -> Result<RemoteChild, SudoError> {
    let path = sftp_server.to_str().ok_or_else(|| SudoError::Other {
        detail: format!(
            "the sftp-server path {} is not valid UTF-8",
            sftp_server.display()
        ),
    })?;
    match password {
        None => {
            let command = stage_command(SudoStage::NoPassword, path)?;
            match run_stage(connections, endpoint, &command, None, None, limit).await {
                Err(SudoError::PasswordRequired) => {
                    Err(sudoers_check(connections, endpoint, limit).await)
                }
                other => other,
            }
        }
        Some(secret) => {
            let line = password_line(secret)?;
            let command = stage_command(SudoStage::Password, path)?;
            run_stage(
                connections,
                endpoint,
                &command,
                Some(line),
                Some(secret.expose()),
                limit,
            )
            .await
        }
    }
}

/// Which sudo invocation runs the wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SudoStage {
    /// Stage 1: `sudo -n`, never reads stdin.
    NoPassword,
    /// Stage 2: `sudo -k -S -p ''`. `-k` ignores cached credentials, so sudo
    /// reads exactly one line whenever the policy requires authentication.
    Password,
}

/// The remote command of a stage, every word quoted.
fn stage_command(stage: SudoStage, sftp_server: &str) -> Result<String, SudoError> {
    let sudo: &[&str] = match stage {
        SudoStage::NoPassword => &["sudo", "-n"],
        SudoStage::Password => &["sudo", "-k", "-S", "-p", ""],
    };
    let mut words = vec!["env", "LC_ALL=C"];
    words.extend_from_slice(sudo);
    words.extend_from_slice(&["sh", "-c", WRAPPER, sftp_server]);
    join_words(&words)
}

/// Single-quotes one word for the remote POSIX shell exactly as plan 7.3
/// specifies: `'` becomes `'\''`, and a word containing a newline is
/// rejected.
fn quote_word(word: &str) -> Result<String, SudoError> {
    if word.contains('\n') {
        return Err(SudoError::Other {
            detail: format!(
                "{} contains a newline and cannot be used in a remote command",
                word.escape_debug()
            ),
        });
    }
    Ok(format!("'{}'", word.replace('\'', r"'\''")))
}

/// Quotes every word and joins them with single spaces.
fn join_words(words: &[&str]) -> Result<String, SudoError> {
    let quoted = words
        .iter()
        .map(|word| quote_word(word))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(quoted.join(" "))
}

/// The exact bytes written to stage 2's stdin: the password and `\n`, in a
/// zeroized buffer allocated once with capacity `len + 1`.
fn password_line(password: &Secret) -> Result<Zeroizing<Vec<u8>>, SudoError> {
    let text = password.expose();
    if text.len() > PASSWORD_MAX_LEN {
        return Err(SudoError::Other {
            detail: format!(
                "the sudo password is {} bytes long; at most {PASSWORD_MAX_LEN} are supported",
                text.len()
            ),
        });
    }
    if text.contains(['\n', '\r']) {
        return Err(SudoError::Other {
            detail: "the sudo password must not contain a line break".to_owned(),
        });
    }
    let mut line = Zeroizing::new(Vec::with_capacity(text.len() + 1));
    line.extend_from_slice(text.as_bytes());
    line.push(b'\n');
    Ok(line)
}

/// The probe candidates: the configured path first, then the fixed list,
/// without duplicates.
fn candidate_list(configured: Option<&Path>) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::with_capacity(SFTP_SERVER_CANDIDATES.len() + 1);
    if let Some(path) = configured.and_then(Path::to_str) {
        candidates.push(path.to_owned());
    }
    for fixed in SFTP_SERVER_CANDIDATES {
        if !candidates.iter().any(|candidate| candidate == fixed) {
            candidates.push((*fixed).to_owned());
        }
    }
    candidates
}

/// Maps a crate error from the connection layer to a sudo error.
fn sudo_error(error: Error) -> SudoError {
    match error {
        Error::Sudo(error) => error,
        Error::Connect(error) => SudoError::Connect(error),
        other => SudoError::Other {
            detail: other.to_string(),
        },
    }
}

/// How a child process ended, in the crate's terms.
fn exit_status(status: std::process::ExitStatus) -> ExitStatus {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitStatus::Code(code),
        (None, Some(signal)) => ExitStatus::Signal(signal),
        (None, None) => ExitStatus::NotStarted,
    }
}

/// A snapshot of a stderr tail, oldest line first.
fn tail_lines(tail: &StderrTail) -> Vec<String> {
    tail.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

/// Appends one line to a bounded stderr tail.
fn push_tail(tail: &StderrTail, line: String) {
    let mut lines = tail.lock().unwrap_or_else(PoisonError::into_inner);
    if lines.len() == STDERR_TAIL_LINES {
        lines.pop_front();
    }
    lines.push_back(line);
}

/// Result of one probe command.
#[derive(Debug)]
struct ProbeOutput {
    status: std::process::ExitStatus,
    /// stdout, at most [`PROBE_OUTPUT_LIMIT`] bytes.
    stdout: Vec<u8>,
    /// stdout was longer than [`PROBE_OUTPUT_LIMIT`].
    truncated: bool,
    /// The child's stderr tail when it ended.
    stderr: Vec<String>,
}

/// Runs one probe command through `spawn_remote` and collects its stdout,
/// bounded by [`START_TIMEOUT`].
async fn run_probe(
    connections: &dyn Connections,
    endpoint: &Endpoint,
    argv: &[String],
) -> Result<ProbeOutput, SudoError> {
    let run = async {
        let RemoteChild {
            mut child,
            stdin,
            stdout,
            stderr,
        } = connections
            .spawn_remote(endpoint, argv)
            .await
            .map_err(sudo_error)?;
        // The probes read no input.
        drop(stdin);
        let mut output = Vec::new();
        let limit = PROBE_OUTPUT_LIMIT as u64 + 1;
        stdout
            .take(limit)
            .read_to_end(&mut output)
            .await
            .map_err(|error| SudoError::Other {
                detail: format!("cannot read the sftp-server probe output: {error}"),
            })?;
        let truncated = output.len() > PROBE_OUTPUT_LIMIT;
        output.truncate(PROBE_OUTPUT_LIMIT);
        let status = child.wait().await.map_err(|error| SudoError::Other {
            detail: format!("cannot wait for the sftp-server probe: {error}"),
        })?;
        Ok::<ProbeOutput, SudoError>(ProbeOutput {
            status,
            stdout: output,
            truncated,
            stderr: tail_lines(&stderr),
        })
    };
    timeout(START_TIMEOUT, run)
        .await
        .map_err(|_elapsed| SudoError::Other {
            detail: format!(
                "the sftp-server probe did not finish within {} s",
                START_TIMEOUT.as_secs()
            ),
        })?
}

/// Interprets the output of [`PROBE_SCRIPT`]. The remote output is
/// untrusted: only a line exactly equal to a candidate is accepted.
fn accept_probe_output(probe: &ProbeOutput, candidates: &[String]) -> Result<PathBuf, SudoError> {
    let noise = || {
        SudoError::Connect(ConnectError::ProtocolCorrupted {
            detail: "unexpected output from the sftp-server probe".to_owned(),
        })
    };
    match probe.status.code() {
        Some(0) => {
            let line = probe.stdout.strip_suffix(b"\n").ok_or_else(noise)?;
            candidates
                .iter()
                .find(|candidate| candidate.as_bytes() == line)
                .map(PathBuf::from)
                .ok_or_else(noise)
        }
        Some(PROBE_NOT_FOUND) if probe.stdout.is_empty() => Err(SudoError::SftpServerNotFound),
        Some(PROBE_NOT_FOUND) => Err(noise()),
        _ => {
            let stderr = probe
                .stderr
                .iter()
                .rev()
                .take(DETAIL_LINES)
                .rev()
                .map(|line| line.chars().take(DETAIL_LINE_CHARS).collect::<String>())
                .collect::<Vec<_>>()
                .join("; ");
            let status = exit_status(probe.status);
            Err(SudoError::Other {
                detail: if stderr.is_empty() {
                    format!("the sftp-server probe failed ({status})")
                } else {
                    format!("the sftp-server probe failed ({status}): {stderr}")
                },
            })
        }
    }
}

/// One stderr chunk forwarded by the drain task before the marker.
#[derive(Debug)]
struct StderrLine {
    /// The text without its `\r\n` / `\n` terminator.
    text: String,
    /// Bytes read from the pipe for this chunk, terminator included.
    raw_len: usize,
    /// The chunk is a complete line: it starts at a line start and ends at a
    /// newline or at the end of the stream.
    whole: bool,
}

/// A spawned sudo child before its stdin and stdout are handed off.
/// Dropping it kills the process (`kill_on_drop`); tokio reaps it.
struct SudoChild {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    /// Pre-marker stderr chunks from the drain task.
    lines: mpsc::Receiver<io::Result<StderrLine>>,
    /// Post-marker stderr lines.
    tail: StderrTail,
}

/// How reading pre-marker stderr ended.
#[derive(Debug)]
enum Driven {
    /// The marker arrived.
    Ready,
    /// A known failure, the timeout, or the output limit.
    Failed(SudoError),
    /// stderr closed first; the kept ignored lines for the error detail.
    Closed(Vec<String>),
}

/// Spawns `remote_command` on `endpoint` through the session's mux argv,
/// with all three stdio piped, in its own process group, killed on drop,
/// and starts the stderr drain task.
async fn spawn_child(
    connections: &dyn Connections,
    endpoint: &Endpoint,
    remote_command: &str,
) -> Result<SudoChild, SudoError> {
    let host = match endpoint {
        Endpoint::Remote { host, .. } => host,
        Endpoint::Local => {
            return Err(SudoError::Other {
                detail: "sudo mode needs a remote host".to_owned(),
            })
        }
    };
    let argv = connections
        .ssh_command(endpoint, false)
        .await
        .map_err(sudo_error)?;
    let (program, args) = argv.split_first().ok_or_else(|| SudoError::Other {
        detail: "the session returned an empty ssh command".to_owned(),
    })?;
    let mut child = Command::new(program)
        .args(args)
        .arg("--")
        .arg(host)
        .arg(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            SudoError::Connect(ConnectError::SshUnavailable {
                message: error.to_string(),
            })
        })?;
    let missing = |stream: &str| SudoError::Other {
        detail: format!("the ssh child has no {stream} pipe"),
    };
    let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;
    let (lines, tail) = spawn_drain(stderr);
    Ok(SudoChild {
        child,
        stdin,
        stdout,
        lines,
        tail,
    })
}

/// Starts the task that reads `stderr` for the child's whole life (a full
/// stderr pipe would stall sftp-server). Chunks up to and including the
/// marker line are forwarded to the start flow; every later line goes only
/// into the returned bounded tail.
fn spawn_drain(stderr: ChildStderr) -> (mpsc::Receiver<io::Result<StderrLine>>, StderrTail) {
    let (sender, receiver) = mpsc::channel(STDERR_TAIL_LINES);
    let tail: StderrTail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
    let task_tail = Arc::clone(&tail);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut forward = Some(sender);
        let mut at_line_start = true;
        loop {
            // A pre-marker chunk may exceed the pre-marker limit by one byte,
            // which is what lets the start flow detect the overflow.
            let limit = if forward.is_some() {
                PRE_MARKER_LIMIT + 1
            } else {
                TAIL_LINE_LIMIT
            };
            let mut buf = Vec::new();
            let read = (&mut reader)
                .take(limit as u64)
                .read_until(b'\n', &mut buf)
                .await;
            let raw_len = match read {
                Ok(0) => break,
                Ok(raw_len) => raw_len,
                Err(error) => {
                    match forward.take() {
                        Some(sender) => {
                            // When the start flow already ended, nobody waits
                            // for this error and the failed send is expected.
                            let _unheard = sender.send(Err(error)).await;
                        }
                        None => push_tail(&task_tail, format!("<cannot read stderr: {error}>")),
                    }
                    break;
                }
            };
            let ends_line = buf.last() == Some(&b'\n');
            // A chunk shorter than the limit without a newline ends the stream.
            let whole = at_line_start && (ends_line || raw_len < limit);
            at_line_start = ends_line;
            let text = String::from_utf8_lossy(&buf)
                .trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_owned();
            match forward.take() {
                Some(sender) => {
                    let marker = whole && text == MARKER_OK;
                    let line = StderrLine {
                        text,
                        raw_len,
                        whole,
                    };
                    // A failed send means the start flow ended (and killed
                    // the child); keep draining into the tail until EOF.
                    if sender.send(Ok(line)).await.is_ok() && !marker {
                        forward = Some(sender);
                    }
                }
                None => push_tail(&task_tail, text),
            }
        }
    });
    (receiver, tail)
}

/// Writes the password line once and flushes it.
async fn write_password(stdin: &mut ChildStdin, line: &[u8]) -> io::Result<()> {
    stdin.write_all(line).await?;
    stdin.flush().await
}

/// Reads pre-marker stderr until the marker, a known failure, the output
/// limit, the deadline, or the end of the stream.
async fn drive(
    child: &mut SudoChild,
    with_password: bool,
    secret: Option<&str>,
    deadline: Instant,
) -> Driven {
    let mut seen = 0usize;
    let mut kept: VecDeque<String> = VecDeque::with_capacity(DETAIL_LINES);
    loop {
        let line = match timeout_at(deadline, child.lines.recv()).await {
            Err(_elapsed) => return Driven::Failed(SudoError::Timeout),
            Ok(None) => return Driven::Closed(kept.into()),
            Ok(Some(Err(error))) => {
                return Driven::Failed(SudoError::Other {
                    detail: format!("cannot read sudo's output: {error}"),
                })
            }
            Ok(Some(Ok(line))) => line,
        };
        if line.whole {
            match classify_line(&line.text, with_password) {
                LineClass::Ready => return Driven::Ready,
                LineClass::Failed(error) => return Driven::Failed(error),
                LineClass::Ignored => {}
            }
        }
        seen += line.raw_len;
        if seen > PRE_MARKER_LIMIT {
            return Driven::Failed(SudoError::UnexpectedOutput);
        }
        if quotable(&line.text, secret) {
            if kept.len() == DETAIL_LINES {
                kept.pop_front();
            }
            kept.push_back(line.text.chars().take(DETAIL_LINE_CHARS).collect());
        }
    }
}

/// Whether an ignored pre-marker line may be quoted in an error: not blank,
/// not the sudo lecture, not echoing the wrapper or a marker, and not
/// containing the password.
fn quotable(line: &str, secret: Option<&str>) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && !LECTURE_LINES.contains(&trimmed)
        && !line.contains(MARKER_OK)
        && !line.contains(MARKER_NOSFTP)
        && !secret.is_some_and(|secret| !secret.is_empty() && line.contains(secret))
}

/// The error for a child whose stderr closed before the marker.
async fn closed_error(mut child: SudoChild, lines: Vec<String>) -> SudoError {
    let status = match timeout(EXIT_WAIT, child.child.wait()).await {
        Ok(Ok(status)) => exit_status(status).to_string(),
        Ok(Err(error)) => format!("exit status unknown: {error}"),
        Err(_elapsed) => "still running".to_owned(),
    };
    // Dropping `child` kills a process that is still running.
    drop(child);
    SudoError::Other {
        detail: if lines.is_empty() {
            format!("the remote command ended before sftp-server started ({status})")
        } else {
            format!("{} ({status})", lines.join("; "))
        },
    }
}

/// Spawns one stage, writes `password` (if any) exactly once, and reads
/// stderr until the outcome. On the marker the child is returned for the
/// SFTP handshake; on every failure it is killed.
async fn run_stage(
    connections: &dyn Connections,
    endpoint: &Endpoint,
    remote_command: &str,
    password: Option<Zeroizing<Vec<u8>>>,
    secret: Option<&str>,
    limit: Duration,
) -> Result<RemoteChild, SudoError> {
    let mut child = spawn_child(connections, endpoint, remote_command).await?;
    let deadline = Instant::now() + limit;
    if let Some(line) = password {
        let written = write_password(&mut child.stdin, &line).await;
        let len = line.len();
        drop(line);
        if let Err(error) = written {
            drop(child);
            return Err(SudoError::Other {
                detail: format!("cannot send the password ({len} bytes) to sudo: {error}"),
            });
        }
    }
    match drive(&mut child, secret.is_some(), secret, deadline).await {
        Driven::Ready => {
            let SudoChild {
                child,
                stdin,
                stdout,
                lines,
                tail,
            } = child;
            // The drain task stopped forwarding at the marker.
            drop(lines);
            Ok(RemoteChild {
                child,
                stdin,
                stdout,
                stderr: tail,
            })
        }
        Driven::Failed(error) => {
            drop(child);
            Err(error)
        }
        Driven::Closed(lines) => Err(closed_error(child, lines).await),
    }
}

/// Step 3: after stage 1 answered `a password is required`, `sudo -n -v`
/// tells a non-sudoer apart before the user is asked for a password.
async fn sudoers_check(
    connections: &dyn Connections,
    endpoint: &Endpoint,
    limit: Duration,
) -> SudoError {
    let command = match join_words(&VALIDATE_WORDS) {
        Ok(command) => command,
        Err(error) => return error,
    };
    let mut child = match spawn_child(connections, endpoint, &command).await {
        Ok(child) => child,
        Err(error) => return error,
    };
    let deadline = Instant::now() + limit;
    let verdict = match drive(&mut child, false, None, deadline).await {
        Driven::Failed(SudoError::PasswordRequired) | Driven::Ready | Driven::Closed(_) => {
            SudoError::PasswordRequired
        }
        Driven::Failed(error) => error,
    };
    drop(child);
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::os::unix::fs::PermissionsExt;

    use futures_util::future::BoxFuture;

    use crate::connections::{FsChannel, RemoteTools};
    use crate::error::FsError;
    use crate::vfs::Fs;

    const UNIT_PASSWORD: &str = "unit-secret-pw-7f3a";

    fn secret(text: &str) -> Secret {
        Secret::new(Zeroizing::new(text.to_owned()))
    }

    fn remote(host: &str) -> Endpoint {
        Endpoint::Remote {
            host: host.to_owned(),
            sudo: true,
        }
    }

    fn unsupported(operation: &str) -> Error {
        Error::Fs(FsError::Unsupported {
            operation: operation.to_owned(),
            reason: "not provided by the sudo test harness".to_owned(),
        })
    }

    /// Drains a test child's stderr into a bounded tail.
    fn drain_to_tail(stderr: ChildStderr) -> StderrTail {
        let tail: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
        let task_tail = Arc::clone(&tail);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => push_tail(&task_tail, line),
                    Ok(None) => break,
                    Err(error) => {
                        push_tail(&task_tail, format!("<cannot read stderr: {error}>"));
                        break;
                    }
                }
            }
        });
        tail
    }

    /// Spawns a local test child the way `Connections::spawn_remote` does.
    fn spawn_local(program: &str, args: &[String]) -> Result<RemoteChild, Error> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                Error::Connect(ConnectError::SshUnavailable {
                    message: error.to_string(),
                })
            })?;
        let missing = || unsupported("piped stdio");
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        let stderr = drain_to_tail(child.stderr.take().ok_or_else(missing)?);
        Ok(RemoteChild {
            child,
            stdin,
            stdout,
            stderr,
        })
    }

    /// Asserts that no captured text contains any of the secrets.
    fn assert_no_secret(texts: &[String], secrets: &[&str]) {
        for text in texts {
            for secret in secrets {
                assert!(
                    !text.contains(secret),
                    "a secret leaked into captured text ({} bytes)",
                    text.len()
                );
            }
        }
    }

    /// Every text a caller could log or show for a start result.
    fn captured(result: &Result<RemoteChild, SudoError>) -> Vec<String> {
        match result {
            Ok(child) => tail_lines(&child.stderr),
            Err(error) => vec![error.to_string(), format!("{error:?}")],
        }
    }

    // ---- pure parsers and builders ----

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

    #[test]
    fn subsystem_line_is_parsed_like_sshd() {
        let parse = parse_subsystem_sftp;
        assert_eq!(
            parse("Port 22\nSubsystem sftp /usr/lib/ssh/sftp-server\n"),
            Some(PathBuf::from("/usr/lib/ssh/sftp-server"))
        );
        assert_eq!(
            parse("# comment\n  subsystem\tsftp\t/usr/lib/openssh/sftp-server -l INFO\n"),
            Some(PathBuf::from("/usr/lib/openssh/sftp-server"))
        );
        assert_eq!(parse("Subsystem sftp internal-sftp\n"), None);
        assert_eq!(parse("Subsystem sftp sftp-server\n"), None);
        assert_eq!(parse("#Subsystem sftp /commented\n"), None);
        assert_eq!(parse("Match User x\nSubsystem sftp /in/match\n"), None);
        assert_eq!(parse("Subsystem other /x\n"), None);
        assert_eq!(parse("Subsystem sftp\n"), None);
        assert_eq!(parse(""), None);
        assert_eq!(
            parse("Subsystem sftp /first\nSubsystem sftp /second\n"),
            Some(PathBuf::from("/first"))
        );
    }

    #[test]
    fn classification_follows_rule_order() {
        let failed = LineClass::Failed;
        assert_eq!(classify_line("SCPTO-SUDO-OK", false), LineClass::Ready);
        assert_eq!(classify_line("SCPTO-SUDO-OK", true), LineClass::Ready);
        assert_eq!(classify_line(" SCPTO-SUDO-OK", false), LineClass::Ignored);
        assert_eq!(classify_line("SCPTO-SUDO-OK ", false), LineClass::Ignored);
        assert_eq!(
            classify_line("SCPTO-SUDO-NOSFTP", false),
            failed(SudoError::SftpServerNotFound)
        );
        assert_eq!(
            classify_line("sudo: a password is required", false),
            failed(SudoError::PasswordRequired)
        );
        assert_eq!(
            classify_line("sudo: a password is required", true),
            failed(SudoError::WrongPassword)
        );
        assert_eq!(
            classify_line("Sorry, try again.", true),
            failed(SudoError::WrongPassword)
        );
        assert_eq!(
            classify_line("sudo: 1 incorrect password attempt", true),
            failed(SudoError::WrongPassword)
        );
        assert_eq!(
            classify_line("plain is not in the sudoers file.", true),
            failed(SudoError::NotInSudoers)
        );
        assert_eq!(
            classify_line(
                "plain is not in the sudoers file.  This incident will be reported.",
                true
            ),
            failed(SudoError::NotInSudoers)
        );
        assert_eq!(
            classify_line("Sorry, user plain may not run sudo on 531c6f91a35f.", false),
            failed(SudoError::NotInSudoers)
        );
        assert_eq!(
            classify_line("sudo: sorry, you must have a tty to run sudo", false),
            failed(SudoError::RequiresTty)
        );
        // sudo's real "not allowed" line echoes the wrapper, markers included.
        let echoed = format!(
            "Sorry, user limited is not allowed to execute '/bin/sh -c {WRAPPER} \
             /usr/lib/ssh/sftp-server' as root on 37fae30256d2."
        );
        assert_eq!(
            classify_line(&echoed, true),
            failed(SudoError::NotAllowedToExecute)
        );
        for lecture in LECTURE_LINES {
            assert_eq!(classify_line(lecture, true), LineClass::Ignored);
            assert_eq!(
                classify_line(&format!("    {lecture}"), true),
                LineClass::Ignored
            );
        }
        assert_eq!(
            classify_line(
                "** WARNING: connection is not using a post-quantum key exchange algorithm.",
                false
            ),
            LineClass::Ignored
        );
        assert_eq!(classify_line("", false), LineClass::Ignored);
    }

    #[test]
    fn candidates_put_the_configured_path_first_without_duplicates() {
        assert_eq!(candidate_list(None), SFTP_SERVER_CANDIDATES.to_vec());
        let configured = candidate_list(Some(Path::new("/opt/ssh/sftp-server")));
        assert_eq!(configured[0], "/opt/ssh/sftp-server");
        assert_eq!(configured[1..], *SFTP_SERVER_CANDIDATES);
        let duplicate = candidate_list(Some(Path::new("/usr/lib/ssh/sftp-server")));
        assert_eq!(duplicate[0], "/usr/lib/ssh/sftp-server");
        assert_eq!(duplicate.len(), SFTP_SERVER_CANDIDATES.len());
        assert_eq!(
            duplicate
                .iter()
                .filter(|path| *path == "/usr/lib/ssh/sftp-server")
                .count(),
            1
        );
    }

    #[test]
    fn quoting_follows_plan_7_3() {
        assert_eq!(quote_word("plain").unwrap(), "'plain'");
        assert_eq!(quote_word("").unwrap(), "''");
        assert_eq!(quote_word("it's").unwrap(), r"'it'\''s'");
        assert!(matches!(quote_word("a\nb"), Err(SudoError::Other { .. })));
    }

    #[test]
    fn stage_commands_match_plan_8_5() {
        let path = "/usr/lib/ssh/sftp-server";
        assert_eq!(
            stage_command(SudoStage::NoPassword, path).unwrap(),
            format!("'env' 'LC_ALL=C' 'sudo' '-n' 'sh' '-c' '{WRAPPER}' '{path}'")
        );
        assert_eq!(
            stage_command(SudoStage::Password, path).unwrap(),
            format!("'env' 'LC_ALL=C' 'sudo' '-k' '-S' '-p' '' 'sh' '-c' '{WRAPPER}' '{path}'")
        );
        assert_eq!(
            join_words(&VALIDATE_WORDS).unwrap(),
            "'env' 'LC_ALL=C' 'sudo' '-n' '-v'"
        );
        // A quote in the path cannot break out of its word.
        assert!(stage_command(SudoStage::NoPassword, "/x'; rm -rf /")
            .unwrap()
            .ends_with(r"'/x'\''; rm -rf /'"));
    }

    #[test]
    fn password_line_is_exact_and_allocated_once() {
        let line = password_line(&secret(UNIT_PASSWORD)).unwrap();
        assert_eq!(&line[..], format!("{UNIT_PASSWORD}\n").as_bytes());
        assert_eq!(line.capacity(), UNIT_PASSWORD.len() + 1);

        let longest = "x".repeat(PASSWORD_MAX_LEN);
        assert_eq!(
            password_line(&secret(&longest)).unwrap().len(),
            PASSWORD_MAX_LEN + 1
        );
        let too_long = "x".repeat(PASSWORD_MAX_LEN + 1);
        let error = password_line(&secret(&too_long)).unwrap_err();
        assert!(!error.to_string().contains(&too_long));

        for broken in ["pw\nrest", "pw\rrest"] {
            let error = password_line(&secret(broken)).unwrap_err();
            assert_no_secret(
                &[error.to_string(), format!("{error:?}")],
                &[broken, "rest"],
            );
        }
    }

    #[test]
    fn probe_output_accepts_only_an_exact_candidate() {
        use std::process::ExitStatus as StdStatus;
        let candidates = candidate_list(None);
        let probe = |code: i32, stdout: &[u8]| ProbeOutput {
            status: StdStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            truncated: false,
            stderr: vec!["ssh: connect to host h port 22: Connection refused".to_owned()],
        };
        assert_eq!(
            accept_probe_output(&probe(0, b"/usr/lib/ssh/sftp-server\n"), &candidates).unwrap(),
            PathBuf::from("/usr/lib/ssh/sftp-server")
        );
        for noisy in [
            &b"/usr/lib/ssh/sftp-server"[..],
            b"/usr/lib/ssh/sftp-server\n\n",
            b"NOISY\n/usr/lib/ssh/sftp-server\n",
            b"/tmp/evil\n",
            b"",
        ] {
            assert!(matches!(
                accept_probe_output(&probe(0, noisy), &candidates),
                Err(SudoError::Connect(ConnectError::ProtocolCorrupted { .. }))
            ));
        }
        assert_eq!(
            accept_probe_output(&probe(1, b""), &candidates),
            Err(SudoError::SftpServerNotFound)
        );
        assert!(matches!(
            accept_probe_output(&probe(1, b"NOISY\n"), &candidates),
            Err(SudoError::Connect(ConnectError::ProtocolCorrupted { .. }))
        ));
        let Err(SudoError::Other { detail }) = accept_probe_output(&probe(255, b""), &candidates)
        else {
            panic!("ssh failure must be reported as Other");
        };
        assert!(detail.contains("exit status 255"));
        assert!(detail.contains("Connection refused"));
    }

    // ---- start flow against a local stand-in for ssh and sudo ----

    type RemoteMap = Box<dyn Fn(&[String]) -> Vec<String> + Send + Sync>;

    /// A `Connections` whose `ssh_command` is a local `sh` script playing
    /// ssh plus sudo (`$1` is `--`, `$2` the host, `$3` the remote command),
    /// and whose `spawn_remote` runs a local argv chosen by `remote`.
    struct Fake {
        script: String,
        remote: RemoteMap,
    }

    impl Fake {
        /// Dispatches on the remote command: the `sudo -n -v` pre-check,
        /// stage 2 (`-k`), or stage 1.
        fn sudo(stage1: &str, precheck: &str, stage2: &str) -> Fake {
            Fake {
                script: format!(
                    "case \"$3\" in\n*\"'-v'\"*) {precheck} ;;\n*\"'-k'\"*) {stage2} ;;\n*) {stage1} ;;\nesac"
                ),
                remote: Box::new(|argv| argv.to_vec()),
            }
        }

        fn probe(remote: RemoteMap) -> Fake {
            Fake {
                script: "exit 99".to_owned(),
                remote,
            }
        }
    }

    impl Connections for Fake {
        fn fs<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _channel: FsChannel,
        ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
            Box::pin(async { Err(unsupported("fs")) })
        }

        fn spawn_remote<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            argv: &'a [String],
        ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
            Box::pin(async move {
                let local = (self.remote)(argv);
                let (program, args) = local.split_first().ok_or_else(|| unsupported("argv"))?;
                spawn_local(program, args)
            })
        }

        fn ssh_command<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
            _interactive: bool,
        ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
            Box::pin(async move {
                Ok(vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    self.script.clone(),
                    "fake-ssh".to_owned(),
                ])
            })
        }

        fn tools<'a>(
            &'a self,
            _endpoint: &'a Endpoint,
        ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
            Box::pin(async { Err(unsupported("tools")) })
        }
    }

    const PATH: &str = "/usr/lib/ssh/sftp-server";
    const NEED_PW: &str = "echo 'sudo: a password is required' >&2; exit 1";

    async fn start_fake(fake: &Fake, password: Option<&str>) -> Result<RemoteChild, SudoError> {
        let secret = password.map(secret);
        start_within(
            fake,
            &remote("box"),
            Path::new(PATH),
            secret.as_ref(),
            Duration::from_secs(5),
        )
        .await
    }

    /// Polls `tail` until a line satisfies `want` (the drain runs in its own
    /// task).
    async fn wait_for_tail(tail: &StderrTail, want: impl Fn(&str) -> bool) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let lines = tail_lines(tail);
            if lines.iter().any(|line| want(line)) || Instant::now() >= deadline {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Writes bytes into the handed-off stdin and reads them back from
    /// stdout (the fake sftp-server is `cat`).
    async fn assert_clean_stream(child: &mut RemoteChild) {
        child.stdin.write_all(b"SFTP-INIT\n").await.unwrap();
        child.stdin.flush().await.unwrap();
        let mut echoed = [0u8; 10];
        child.stdout.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"SFTP-INIT\n");
    }

    async fn with_timeout<F: Future>(future: F) -> F::Output {
        timeout(Duration::from_secs(15), future)
            .await
            .expect("test took too long")
    }

    #[tokio::test]
    async fn stage1_passes_the_exact_command_and_hands_off_after_the_marker() {
        let expected = quote_word(&stage_command(SudoStage::NoPassword, PATH).unwrap()).unwrap();
        let stage1 = format!(
            "[ \"$1\" = -- ] && [ \"$2\" = box ] && [ \"$3\" = {expected} ] || exit 9; \
             echo 'sudo: some warning' >&2; echo SCPTO-SUDO-OK >&2; echo post-marker-note >&2; exec cat"
        );
        let fake = Fake::sudo(&stage1, "exit 98", "exit 97");
        let mut child = with_timeout(start_fake(&fake, None)).await.unwrap();
        assert_clean_stream(&mut child).await;
        let tail = wait_for_tail(&child.stderr, |line| line == "post-marker-note").await;
        assert_eq!(tail, vec!["post-marker-note".to_owned()]);
    }

    #[tokio::test]
    async fn stage2_reads_exactly_one_line_and_ignores_the_lecture() {
        let stage2 = format!(
            "IFS= read -r pw; [ \"$pw\" = {} ] || {{ echo 'Sorry, try again.' >&2; exec sleep 30; }}; \
             printf '\\n%s\\n%s\\n\\n    %s\\n    %s\\n    %s\\n\\n%s\\n' \
             '{}' '{}' '{}' '{}' '{}' '{}' >&2; \
             printf 'SCPTO-SUDO-OK\\r\\n' >&2; exec cat",
            quote_word(UNIT_PASSWORD).unwrap(),
            LECTURE_LINES[0],
            LECTURE_LINES[1],
            LECTURE_LINES[2],
            LECTURE_LINES[3],
            LECTURE_LINES[4],
            LECTURE_LINES[5],
        );
        let fake = Fake::sudo("exit 98", "exit 97", &stage2);
        let mut child = with_timeout(start_fake(&fake, Some(UNIT_PASSWORD)))
            .await
            .unwrap();
        // Only bytes written after the handoff come back: sudo consumed the
        // password line and nothing else.
        assert_clean_stream(&mut child).await;
        assert_no_secret(&tail_lines(&child.stderr), &[UNIT_PASSWORD]);
    }

    #[tokio::test]
    async fn wrong_password_kills_the_child_at_the_first_failure_line() {
        let stage2 = "IFS= read -r pw; echo 'Sorry, try again.' >&2; exec sleep 30";
        let fake = Fake::sudo("exit 98", "exit 97", stage2);
        let started = Instant::now();
        let result = with_timeout(start_fake(&fake, Some("not-the-password"))).await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(matches!(result, Err(SudoError::WrongPassword)));
        assert_no_secret(&captured(&result), &["not-the-password"]);
    }

    #[tokio::test]
    async fn password_required_runs_the_sudoers_check() {
        let flags = tempfile::tempdir().unwrap();
        let flag = flags.path().join("precheck");
        let precheck = format!(
            "touch {}; echo 'sudo: a password is required' >&2; exit 1",
            quote_word(flag.to_str().unwrap()).unwrap()
        );
        let fake = Fake::sudo(NEED_PW, &precheck, "exit 97");
        let result = with_timeout(start_fake(&fake, None)).await;
        assert!(matches!(result, Err(SudoError::PasswordRequired)));
        assert!(flag.exists());
    }

    #[tokio::test]
    async fn non_sudoer_is_reported_before_asking_for_a_password() {
        let precheck = "echo 'Sorry, user plain may not run sudo on box.' >&2; exit 1";
        let fake = Fake::sudo(NEED_PW, precheck, "exit 97");
        let result = with_timeout(start_fake(&fake, None)).await;
        assert!(matches!(result, Err(SudoError::NotInSudoers)));
    }

    #[tokio::test]
    async fn requiretty_fails_in_stage1_without_the_sudoers_check() {
        let flags = tempfile::tempdir().unwrap();
        let flag = flags.path().join("precheck");
        let precheck = format!(
            "touch {}; exit 1",
            quote_word(flag.to_str().unwrap()).unwrap()
        );
        let stage1 = "echo 'sudo: sorry, you must have a tty to run sudo' >&2; exit 1";
        let fake = Fake::sudo(stage1, &precheck, "exit 97");
        let result = with_timeout(start_fake(&fake, None)).await;
        assert!(matches!(result, Err(SudoError::RequiresTty)));
        assert!(!flag.exists());
    }

    #[tokio::test]
    async fn missing_sftp_server_marker_is_classified() {
        let fake = Fake::sudo("echo SCPTO-SUDO-NOSFTP >&2; exit 127", "exit 98", "exit 97");
        let result = with_timeout(start_fake(&fake, None)).await;
        assert!(matches!(result, Err(SudoError::SftpServerNotFound)));
    }

    #[tokio::test]
    async fn not_allowed_line_is_classified_and_never_shown() {
        let echoed = format!(
            "Sorry, user limited is not allowed to execute '/bin/sh -c {WRAPPER} {PATH}' as root on box."
        );
        let stage2 = format!(
            "IFS= read -r pw; printf '%s\\n' {} >&2; exec sleep 30",
            quote_word(&echoed).unwrap()
        );
        let fake = Fake::sudo("exit 98", "exit 97", &stage2);
        let result = with_timeout(start_fake(&fake, Some(UNIT_PASSWORD))).await;
        assert!(matches!(result, Err(SudoError::NotAllowedToExecute)));
        for text in captured(&result) {
            assert!(!text.contains("SCPTO-SUDO"));
        }
    }

    #[tokio::test]
    async fn silence_times_out() {
        let fake = Fake::sudo("exec sleep 30", "exit 98", "exit 97");
        let result = with_timeout(start_within(
            &fake,
            &remote("box"),
            Path::new(PATH),
            None,
            Duration::from_millis(300),
        ))
        .await;
        assert!(matches!(result, Err(SudoError::Timeout)));
    }

    #[tokio::test]
    async fn too_many_lines_before_the_marker_are_rejected() {
        let stage1 =
            "i=0; while [ $i -lt 3000 ]; do echo 'noise noise noise noise noise noise' >&2; \
                      i=$((i+1)); done; echo SCPTO-SUDO-OK >&2; exec sleep 30";
        let fake = Fake::sudo(stage1, "exit 98", "exit 97");
        let result = with_timeout(start_fake(&fake, None)).await;
        assert!(matches!(result, Err(SudoError::UnexpectedOutput)));
    }

    #[tokio::test]
    async fn one_huge_line_before_the_marker_is_rejected() {
        let stage1 = "head -c 70000 /dev/zero | tr '\\000' a >&2; echo >&2; \
                      echo SCPTO-SUDO-OK >&2; exec sleep 30";
        let fake = Fake::sudo(stage1, "exit 98", "exit 97");
        let result = with_timeout(start_fake(&fake, None)).await;
        assert!(matches!(result, Err(SudoError::UnexpectedOutput)));
    }

    #[tokio::test]
    async fn early_exit_reports_filtered_lines_and_status() {
        let stage1 = format!(
            "printf '%s\\n' '{}' 'sudo: unable to resolve host box' 'echo of SCPTO-SUDO-OK inside' '' >&2; exit 3",
            LECTURE_LINES[0]
        );
        let fake = Fake::sudo(&stage1, "exit 98", "exit 97");
        let result = with_timeout(start_fake(&fake, None)).await;
        let Err(SudoError::Other { detail }) = result else {
            panic!("expected Other, got {result:?}");
        };
        assert_eq!(detail, "sudo: unable to resolve host box (exit status 3)");
    }

    #[tokio::test]
    async fn silent_exit_reports_the_status() {
        let fake = Fake::sudo("exit 0", "exit 98", "exit 97");
        let result = with_timeout(start_fake(&fake, None)).await;
        let Err(SudoError::Other { detail }) = result else {
            panic!("expected Other, got {result:?}");
        };
        assert_eq!(
            detail,
            "the remote command ended before sftp-server started (exit status 0)"
        );
    }

    #[tokio::test]
    async fn password_never_appears_in_errors_or_tails() {
        // A misbehaving remote echoes the password before the marker; the
        // error of every stage-2 failure path is captured. (After the marker
        // the password is gone: the tail belongs to sftp-server.)
        let echo_then_exit = "IFS= read -r pw; echo \"sudo: got $pw\" >&2; exit 1";
        let echo_then_sorry =
            "IFS= read -r pw; echo \"$pw\" >&2; echo 'Sorry, try again.' >&2; exec sleep 30";
        let echo_then_flood = "IFS= read -r pw; i=0; while [ $i -lt 3000 ]; do \
                               echo \"$pw $pw $pw\" >&2; i=$((i+1)); done; exec sleep 30";
        let mut texts = Vec::new();
        for stage2 in [echo_then_exit, echo_then_sorry, echo_then_flood, "exit 1"] {
            let fake = Fake::sudo("exit 98", "exit 97", stage2);
            let result = with_timeout(start_fake(&fake, Some(UNIT_PASSWORD))).await;
            assert!(result.is_err());
            texts.extend(captured(&result));
        }
        let rejected = start_fake(
            &Fake::sudo("exit 98", "exit 97", "exit 97"),
            Some("pw\nrest"),
        )
        .await;
        texts.extend(captured(&rejected));
        assert_no_secret(&texts, &[UNIT_PASSWORD, "pw\nrest"]);
    }

    #[tokio::test]
    async fn local_endpoint_is_rejected() {
        let fake = Fake::sudo("exit 98", "exit 98", "exit 98");
        let result = start(&fake, &Endpoint::Local, Path::new(PATH), None).await;
        assert!(matches!(result, Err(SudoError::Other { .. })));
    }

    #[tokio::test]
    async fn locate_prefers_the_configured_executable_path() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("sftp-server");
        std::fs::write(&server, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = format!("Port 22\nSubsystem sftp {} -l INFO\n", server.display());
        let fake = Fake::probe(Box::new(move |argv| {
            if argv[0] == "cat" {
                vec!["printf".to_owned(), "%s".to_owned(), config.clone()]
            } else {
                argv.to_vec()
            }
        }));
        let found = with_timeout(locate_sftp_server(&fake, &remote("box")))
            .await
            .unwrap();
        assert_eq!(found, server);
    }

    #[tokio::test]
    async fn locate_reports_missing_server_and_noise() {
        let none = Fake::probe(Box::new(|_argv| {
            vec!["sh".to_owned(), "-c".to_owned(), "exit 1".to_owned()]
        }));
        assert_eq!(
            with_timeout(locate_sftp_server(&none, &remote("box"))).await,
            Err(SudoError::SftpServerNotFound)
        );
        let noisy = Fake::probe(Box::new(|argv| {
            if argv[0] == "cat" {
                vec!["sh".to_owned(), "-c".to_owned(), "exit 1".to_owned()]
            } else {
                vec![
                    "printf".to_owned(),
                    "NOISY-STARTUP-OUTPUT\n/usr/lib/ssh/sftp-server\n".to_owned(),
                ]
            }
        }));
        assert!(matches!(
            with_timeout(locate_sftp_server(&noisy, &remote("box"))).await,
            Err(SudoError::Connect(ConnectError::ProtocolCorrupted { .. }))
        ));
    }

    /// Docker integration cases (plan 6), run only with `SCPTO_IT=1`: the 13
    /// cases of the Wave 1 sudo spike on both fixture images.
    mod it {
        use super::*;

        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use openssh_sftp_client::{Sftp, SftpOptions};

        /// The fixture sudo password (tests/it/fixtures/README.md).
        const PASSWORD: &str = "scpto-test-pw";
        const WRONG_PASSWORD: &str = "scpto-wrong-pw-2c91";
        const SFTP_SERVER: &str = "/usr/lib/ssh/sftp-server";
        const COMMON: [&str; 12] = [
            "-o",
            "RemoteCommand=none",
            "-o",
            "RequestTTY=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "ForwardX11=no",
            "-o",
            "ForwardAgent=no",
        ];
        /// SSH_FXP_INIT version 3: length 5, type 1, version 3.
        const RAW_INIT: [u8; 9] = [0, 0, 0, 5, 1, 0, 0, 0, 3];
        const SSH_FXP_VERSION: u8 = 2;

        /// Serializes master logins: sshd drops concurrent unauthenticated
        /// connections beyond `MaxStartups`.
        static MASTER_LOGIN: Mutex<()> = Mutex::new(());
        static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

        fn enabled() -> bool {
            std::env::var_os("SCPTO_IT").is_some_and(|value| value == "1")
        }

        fn ssh_config() -> PathBuf {
            std::env::var_os("SCPTO_IT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/it"))
                .join("ssh_config")
        }

        /// One ControlMaster to a fixture alias plus the `Connections` the
        /// sudo module needs. Dropping it closes the master.
        struct Harness {
            config: PathBuf,
            dir: PathBuf,
            control_path: String,
            host: String,
            endpoint: Endpoint,
        }

        impl Harness {
            fn start(image: &str, user: &str) -> Harness {
                let host = format!("it-{image}-{user}");
                // SAFETY: getuid has no preconditions and cannot fail.
                let uid = unsafe { libc::getuid() };
                let dir = PathBuf::from(format!(
                    "/tmp/scpto-it-{uid}/{}-{}",
                    std::process::id(),
                    NEXT_DIR.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&dir)
                    .expect("create the ControlPath directory");
                let harness = Harness {
                    config: ssh_config(),
                    control_path: format!("ControlPath={}/%C", dir.display()),
                    dir,
                    host: host.clone(),
                    endpoint: Endpoint::Remote { host, sudo: true },
                };
                let _login = MASTER_LOGIN.lock().unwrap_or_else(PoisonError::into_inner);
                let status = std::process::Command::new("ssh")
                    .arg("-F")
                    .arg(&harness.config)
                    .args(COMMON)
                    .args(["-o", "BatchMode=yes", "-o", "ControlMaster=auto"])
                    .args(["-o", "ControlPersist=10m", "-o", "ServerAliveInterval=15"])
                    .args(["-o", &harness.control_path, "--", &harness.host, "true"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .status()
                    .expect("run ssh");
                assert!(
                    status.success(),
                    "master to {} failed: {status}",
                    harness.host
                );
                harness
            }

            fn mux_argv(&self) -> Vec<String> {
                let mut argv = vec![
                    "ssh".to_owned(),
                    "-F".to_owned(),
                    self.config.display().to_string(),
                    "-o".to_owned(),
                    self.control_path.clone(),
                    "-o".to_owned(),
                    "ControlMaster=no".to_owned(),
                ];
                argv.extend(COMMON.iter().map(|word| (*word).to_owned()));
                argv.extend(["-o".to_owned(), "BatchMode=yes".to_owned()]);
                argv
            }

            async fn check(&self) -> Result<(), Error> {
                let output = Command::new("ssh")
                    .arg("-F")
                    .arg(&self.config)
                    .args(["-o", &self.control_path, "-O", "check", "--", &self.host])
                    .stdin(Stdio::null())
                    .output()
                    .await
                    .map_err(|error| {
                        Error::Connect(ConnectError::SshUnavailable {
                            message: error.to_string(),
                        })
                    })?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(Error::Connect(ConnectError::Other {
                        detail: String::from_utf8_lossy(&output.stderr).into_owned(),
                    }))
                }
            }

            /// Starts `cmd` as a raw remote command string through the sudo
            /// module's own stage runner.
            async fn stage(
                &self,
                cmd: &str,
                input: Option<Zeroizing<Vec<u8>>>,
            ) -> Result<RemoteChild, SudoError> {
                run_stage(
                    self,
                    &self.endpoint,
                    cmd,
                    input,
                    Some(PASSWORD),
                    START_TIMEOUT,
                )
                .await
            }
        }

        impl Drop for Harness {
            fn drop(&mut self) {
                let exit = std::process::Command::new("ssh")
                    .arg("-F")
                    .arg(&self.config)
                    .args(["-o", &self.control_path, "-O", "exit", "--", &self.host])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if !exit.as_ref().is_ok_and(std::process::ExitStatus::success) {
                    eprintln!("teardown: ssh -O exit {} failed: {exit:?}", self.host);
                }
                // `-O exit` returns before the master unlinks its socket.
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while std::fs::read_dir(&self.dir).is_ok_and(|mut entries| entries.next().is_some())
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if let Err(error) = std::fs::remove_dir(&self.dir) {
                    eprintln!("teardown: cannot remove {}: {error}", self.dir.display());
                }
            }
        }

        impl Connections for Harness {
            fn fs<'a>(
                &'a self,
                _endpoint: &'a Endpoint,
                _channel: FsChannel,
            ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>> {
                Box::pin(async { Err(unsupported("fs")) })
            }

            fn spawn_remote<'a>(
                &'a self,
                _endpoint: &'a Endpoint,
                argv: &'a [String],
            ) -> BoxFuture<'a, Result<RemoteChild, Error>> {
                Box::pin(async move {
                    self.check().await?;
                    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
                    let command = join_words(&words).map_err(Error::Sudo)?;
                    let mut args = self.mux_argv();
                    let program = args.remove(0);
                    args.extend(["--".to_owned(), self.host.clone(), command]);
                    spawn_local(&program, &args)
                })
            }

            fn ssh_command<'a>(
                &'a self,
                _endpoint: &'a Endpoint,
                interactive: bool,
            ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
                Box::pin(async move {
                    if interactive {
                        return Err(unsupported("interactive ssh"));
                    }
                    self.check().await?;
                    Ok(self.mux_argv())
                })
            }

            fn tools<'a>(
                &'a self,
                _endpoint: &'a Endpoint,
            ) -> BoxFuture<'a, Result<RemoteTools, Error>> {
                Box::pin(async { Err(unsupported("tools")) })
            }
        }

        fn pw() -> Secret {
            secret(PASSWORD)
        }

        fn two_password_lines() -> Zeroizing<Vec<u8>> {
            let mut lines = Zeroizing::new(Vec::with_capacity(2 * (PASSWORD.len() + 1)));
            for _ in 0..2 {
                lines.extend_from_slice(PASSWORD.as_bytes());
                lines.push(b'\n');
            }
            lines
        }

        async fn located(h: &Harness) -> PathBuf {
            let path = locate_sftp_server(h, &h.endpoint).await.unwrap();
            assert_eq!(path, Path::new(SFTP_SERVER));
            path
        }

        fn assert_clean(result: &Result<RemoteChild, SudoError>) {
            assert_no_secret(&captured(result), &[PASSWORD, WRONG_PASSWORD]);
        }

        /// Writes a file into root-owned `/srv/rootonly` over the sudo
        /// channel, checks that root owns it, and removes it.
        async fn assert_root_write(started: Result<RemoteChild, SudoError>, h: &Harness) {
            assert_clean(&started);
            let RemoteChild {
                mut child,
                stdin,
                stdout,
                stderr,
            } = started.unwrap();
            let sftp = Sftp::new(stdin, stdout, SftpOptions::default())
                .await
                .expect("sftp handshake");
            let path = format!(
                "/srv/rootonly/scpto-it-{}-{}.txt",
                h.host,
                std::process::id()
            );
            let mut file = sftp.create(&path).await.unwrap();
            file.write_all(b"written by scpto sudo test\n")
                .await
                .unwrap();
            file.close().await.unwrap();
            let mut fs = sftp.fs();
            let meta = fs.metadata(&path).await.unwrap();
            fs.remove_file(&path).await.unwrap();
            drop(fs);
            sftp.close().await.unwrap();
            assert_eq!(meta.uid(), Some(0));
            assert!(child.wait().await.unwrap().success());
            assert_no_secret(&tail_lines(&stderr), &[PASSWORD]);
        }

        /// The marker arrived, but the SFTP handshake breaks: input meant for
        /// sudo reached sftp-server. Returns the still running child and its
        /// tail.
        async fn assert_broken_stream(
            started: Result<RemoteChild, SudoError>,
        ) -> (Child, StderrTail) {
            assert_clean(&started);
            let RemoteChild {
                child,
                stdin,
                stdout,
                stderr,
            } = started.expect("the marker must arrive");
            let handshake = timeout(
                Duration::from_secs(10),
                Sftp::new(stdin, stdout, SftpOptions::default()),
            )
            .await;
            assert!(
                !matches!(handshake, Ok(Ok(_))),
                "the SFTP stream must be corrupted"
            );
            (child, stderr)
        }

        async fn nopw_ok(h: &Harness) {
            let path = located(h).await;
            assert_root_write(start(h, &h.endpoint, &path, None).await, h).await;
        }

        async fn nopw_missing_sftp_server(h: &Harness) {
            let missing = Path::new("/usr/lib/ssh/scpto-missing-sftp-server");
            let result = start(h, &h.endpoint, missing, None).await;
            assert_eq!(result.err(), Some(SudoError::SftpServerNotFound));
        }

        async fn nopw_extra_password(h: &Harness) {
            // NOPASSWD: stage 2 does not read the line, so it reaches
            // sftp-server. This is why only PasswordRequired leads to stage 2.
            let path = located(h).await;
            assert_broken_stream(start(h, &h.endpoint, &path, Some(&pw())).await).await;
        }

        async fn pw_ok(h: &Harness) {
            let path = located(h).await;
            let stage1 = start(h, &h.endpoint, &path, None).await;
            assert_eq!(stage1.err(), Some(SudoError::PasswordRequired));
            assert_root_write(start(h, &h.endpoint, &path, Some(&pw())).await, h).await;
        }

        async fn pw_wrong_password(h: &Harness) {
            let path = located(h).await;
            let started = Instant::now();
            let result = start(h, &h.endpoint, &path, Some(&secret(WRONG_PASSWORD))).await;
            assert!(started.elapsed() < Duration::from_secs(5));
            assert_clean(&result);
            assert_eq!(result.err(), Some(SudoError::WrongPassword));
        }

        async fn pw_cached(h: &Harness) {
            // Credentials cached by another session do not help: without a
            // tty the timestamp is keyed by the parent pid.
            let path = located(h).await;
            let prime = "env LC_ALL=C sudo -S -p '' -v && echo SCPTO-SUDO-OK >&2";
            let primed = h.stage(prime, Some(password_line(&pw()).unwrap())).await;
            assert!(primed.is_ok(), "priming sudo -v failed");
            drop(primed);
            let stage1 = start(h, &h.endpoint, &path, None).await;
            assert_eq!(stage1.err(), Some(SudoError::PasswordRequired));
            assert_root_write(start(h, &h.endpoint, &path, Some(&pw())).await, h).await;
        }

        async fn pw_session_cache_hazard(h: &Harness) {
            // Without -k a cached timestamp in the same shell makes sudo skip
            // stdin, so the second password line reaches sftp-server.
            let path = located(h).await;
            let cmd = format!(
                "env LC_ALL=C sudo -S -p '' -v && env LC_ALL=C sudo -S -p '' sh -c {} {}; exit $?",
                quote_word(WRAPPER).unwrap(),
                quote_word(path.to_str().unwrap()).unwrap()
            );
            assert_broken_stream(h.stage(&cmd, Some(two_password_lines())).await).await;
        }

        async fn pw_session_cache_k(h: &Harness) {
            // The production stage-2 command (-k) reads its line even then.
            let path = located(h).await;
            let stage2 = stage_command(SudoStage::Password, path.to_str().unwrap()).unwrap();
            let cmd = format!("env LC_ALL=C sudo -S -p '' -v && {stage2}; exit $?");
            assert_root_write(h.stage(&cmd, Some(two_password_lines())).await, h).await;
        }

        async fn pw_overread(h: &Harness) {
            // sudo does not read past the password line: the INIT sent in the
            // same write reaches sftp-server intact.
            let path = located(h).await;
            let mut input = Zeroizing::new(Vec::with_capacity(PASSWORD.len() + 1 + RAW_INIT.len()));
            input.extend_from_slice(PASSWORD.as_bytes());
            input.push(b'\n');
            input.extend_from_slice(&RAW_INIT);
            let stage2 = stage_command(SudoStage::Password, path.to_str().unwrap()).unwrap();
            let started = h.stage(&stage2, Some(input)).await;
            assert_clean(&started);
            let mut child = started.unwrap();
            let mut len = [0u8; 4];
            child.stdout.read_exact(&mut len).await.unwrap();
            let len = u32::from_be_bytes(len) as usize;
            assert!((5..=64 * 1024).contains(&len), "bad packet length {len}");
            let mut body = vec![0u8; len];
            child.stdout.read_exact(&mut body).await.unwrap();
            assert_eq!(body[0], SSH_FXP_VERSION);
            assert_eq!(u32::from_be_bytes([body[1], body[2], body[3], body[4]]), 3);
        }

        async fn pw_plan_command(h: &Harness) {
            // The original plan 7.4 command prints the marker before the
            // sudo that runs sftp-server fails: a false positive.
            let path = located(h).await;
            let cmd = format!(
                "sudo -S -p '' -v && printf 'SCPTO-SUDO-OK\\n' >&2 && exec sudo -n {}",
                quote_word(path.to_str().unwrap()).unwrap()
            );
            let started = h.stage(&cmd, Some(password_line(&pw()).unwrap())).await;
            let (child, tail) = assert_broken_stream(started).await;
            let lines = wait_for_tail(&tail, |line| line.contains("a password is required")).await;
            drop(child);
            assert!(lines
                .iter()
                .any(|line| line.contains("a password is required")));
        }

        async fn tty_requiretty(h: &Harness) {
            let path = located(h).await;
            let result = start(h, &h.endpoint, &path, None).await;
            assert_eq!(result.err(), Some(SudoError::RequiresTty));
        }

        async fn limited_not_allowed(h: &Harness) {
            let path = located(h).await;
            let stage1 = start(h, &h.endpoint, &path, None).await;
            assert_eq!(stage1.err(), Some(SudoError::PasswordRequired));
            let result = start(h, &h.endpoint, &path, Some(&pw())).await;
            assert_clean(&result);
            for text in captured(&result) {
                assert!(!text.contains("SCPTO-SUDO"));
            }
            assert_eq!(result.err(), Some(SudoError::NotAllowedToExecute));
        }

        async fn plain_not_in_sudoers(h: &Harness) {
            let path = located(h).await;
            let result = start(h, &h.endpoint, &path, None).await;
            assert_eq!(result.err(), Some(SudoError::NotInSudoers));
        }

        macro_rules! it_cases {
            ($($name:ident: $image:literal, $user:literal, $case:ident;)*) => {
                $(
                    #[tokio::test]
                    async fn $name() {
                        if !enabled() {
                            return;
                        }
                        let harness = Harness::start($image, $user);
                        with_timeout($case(&harness)).await;
                    }
                )*
            };
        }

        it_cases! {
            modern_nopw_ok: "modern", "sudonopw", nopw_ok;
            modern_nopw_missing_sftp_server: "modern", "sudonopw", nopw_missing_sftp_server;
            modern_nopw_extra_password: "modern", "sudonopw", nopw_extra_password;
            modern_pw_ok: "modern", "sudopw", pw_ok;
            modern_pw_wrong_password: "modern", "sudopw", pw_wrong_password;
            modern_pw_cached: "modern", "sudopw", pw_cached;
            modern_pw_session_cache_hazard: "modern", "sudopw", pw_session_cache_hazard;
            modern_pw_session_cache_k: "modern", "sudopw", pw_session_cache_k;
            modern_pw_overread: "modern", "sudopw", pw_overread;
            modern_pw_plan_command: "modern", "sudopw", pw_plan_command;
            modern_tty_requiretty: "modern", "sudotty", tty_requiretty;
            modern_limited_not_allowed: "modern", "limited", limited_not_allowed;
            modern_plain_not_in_sudoers: "modern", "plain", plain_not_in_sudoers;
            legacy_nopw_ok: "legacy", "sudonopw", nopw_ok;
            legacy_nopw_missing_sftp_server: "legacy", "sudonopw", nopw_missing_sftp_server;
            legacy_nopw_extra_password: "legacy", "sudonopw", nopw_extra_password;
            legacy_pw_ok: "legacy", "sudopw", pw_ok;
            legacy_pw_wrong_password: "legacy", "sudopw", pw_wrong_password;
            legacy_pw_cached: "legacy", "sudopw", pw_cached;
            legacy_pw_session_cache_hazard: "legacy", "sudopw", pw_session_cache_hazard;
            legacy_pw_session_cache_k: "legacy", "sudopw", pw_session_cache_k;
            legacy_pw_overread: "legacy", "sudopw", pw_overread;
            legacy_pw_plan_command: "legacy", "sudopw", pw_plan_command;
            legacy_tty_requiretty: "legacy", "sudotty", tty_requiretty;
            legacy_limited_not_allowed: "legacy", "limited", limited_not_allowed;
            legacy_plain_not_in_sudoers: "legacy", "plain", plain_not_in_sudoers;
        }
    }
}
