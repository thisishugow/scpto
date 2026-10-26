//! Error classification shared by every module.
//!
//! Errors are classified rather than stringly typed because the class decides
//! what the UI does next: an unreachable host may show a VPN hint and retry
//! with backoff, an authentication failure hands the terminal to OpenSSH, a
//! lost connection parks transfer items as `WaitingForReconnect`, and a failed
//! trash operation offers a permanent delete. All `Display` texts are written
//! for end users and are shown verbatim in the TUI.
//!
//! Every type here is `Clone` so it can be stored on queue items and sent in
//! events. Messages captured from external sources (stderr, OS errors) are
//! kept as strings. No error text may ever contain a sudo password.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Crate-wide error: one of the classified module errors, or a failure of an
/// external helper command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Establishing or keeping an SSH connection failed.
    #[error(transparent)]
    Connect(#[from] ConnectError),
    /// Starting a sudo sftp-server failed.
    #[error(transparent)]
    Sudo(#[from] SudoError),
    /// A file system operation failed.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// A configuration file could not be read or parsed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A path cannot be passed safely to a remote shell command.
    #[error("The path {} contains a newline and cannot be used in a remote command", path.display())]
    NewlineInPath {
        /// The rejected path.
        path: PathBuf,
    },
    /// An external program (tar, rsync, editor, shell, VPN command) failed.
    #[error("{program} failed ({status}){}", stderr_suffix(stderr))]
    CommandFailed {
        /// Program name as shown to the user, for example `rsync`.
        program: String,
        /// How the program ended.
        status: ExitStatus,
        /// The last lines of its stderr, possibly empty.
        stderr: String,
    },
    /// A tar archive was malformed or contained an unsafe entry.
    #[error("Archive error: {message}")]
    Archive {
        /// What was wrong with the archive.
        message: String,
    },
    /// A preview could not be produced: the image could not be decoded or is
    /// larger than `preview::IMAGE_FETCH_LIMIT`.
    #[error("Cannot preview this file: {message}")]
    Preview {
        /// What prevented the preview.
        message: String,
    },
    /// The state file (`state.json`, `partials.json`) could not be written.
    #[error("Cannot save {}: {message}", path.display())]
    StateWrite {
        /// The state file path.
        path: PathBuf,
        /// The underlying reason.
        message: String,
    },
}

/// How an external program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// The program could not be started at all.
    NotStarted,
    /// The program exited with this status code.
    Code(i32),
    /// The program was terminated by this signal number.
    Signal(i32),
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitStatus::NotStarted => f.write_str("could not be started"),
            ExitStatus::Code(code) => write!(f, "exit status {code}"),
            ExitStatus::Signal(signal) => write!(f, "killed by signal {signal}"),
        }
    }
}

fn stderr_suffix(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

/// Why a host could not be reached at the network level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreachableReason {
    /// The connection attempt timed out.
    Timeout,
    /// No route to the host or the network is unreachable.
    NoRoute,
    /// The host actively refused the connection.
    Refused,
    /// The host name could not be resolved.
    NameResolution,
}

impl fmt::Display for UnreachableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnreachableReason::Timeout => "the connection timed out",
            UnreachableReason::NoRoute => "there is no route to the host",
            UnreachableReason::Refused => "the connection was refused",
            UnreachableReason::NameResolution => "the host name could not be resolved",
        })
    }
}

/// Classified SSH connection failure (plan 2.5 and 7.3 S1, S10).
///
/// `detail` fields carry the relevant stderr excerpt from `ssh`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    /// Network-level failure: timeout, no route, refused, or DNS.
    #[error("Cannot reach the host: {reason}")]
    Unreachable {
        /// Which network failure occurred.
        reason: UnreachableReason,
        /// The stderr excerpt from `ssh`.
        detail: String,
    },
    /// Authentication failed (key, agent, or password rejected).
    #[error("Authentication failed: {detail}")]
    AuthFailed {
        /// The stderr excerpt from `ssh`.
        detail: String,
    },
    /// The host key could not be verified against `known_hosts`.
    #[error("Host key verification failed: {detail}")]
    HostKeyRejected {
        /// The stderr excerpt from `ssh`.
        detail: String,
    },
    /// The SFTP stream was corrupted, typically by a remote shell startup
    /// file printing to stdout.
    #[error(
        "The SFTP session was corrupted ({detail}). Remote shell startup files \
         (.bashrc, .profile) must not print anything for non-interactive sessions"
    )]
    ProtocolCorrupted {
        /// The stderr or protocol error excerpt.
        detail: String,
    },
    /// The server refused to open another channel, for example because of
    /// its `MaxSessions` limit.
    #[error(
        "The server refused to open another session channel ({detail}). \
         Its MaxSessions limit may be too low"
    )]
    ChannelOpenFailed {
        /// The stderr excerpt from `ssh`.
        detail: String,
    },
    /// The server has no SFTP subsystem.
    #[error("The server does not provide SFTP: {detail}")]
    SftpServerMissing {
        /// The stderr excerpt from `ssh`.
        detail: String,
    },
    /// The server's SFTP implementation is not supported by scpto.
    #[error("The server's SFTP implementation is not supported: {detail}")]
    UnsupportedServer {
        /// What the client reported as incompatible.
        detail: String,
    },
    /// The local `ssh` program could not be started.
    #[error("Cannot run ssh: {message}")]
    SshUnavailable {
        /// The operating system error.
        message: String,
    },
    /// An established connection dropped.
    #[error("The connection was lost")]
    ConnectionLost,
    /// Any failure that does not match a known class.
    #[error("Connection failed: {detail}")]
    Other {
        /// The stderr excerpt from `ssh`.
        detail: String,
    },
}

impl ConnectError {
    /// Whether this failure, when it happened under `BatchMode=yes`, must be
    /// retried as a foreground `ssh` so OpenSSH can prompt or explain (7.3 S1).
    pub fn needs_foreground_auth(&self) -> bool {
        matches!(
            self,
            ConnectError::AuthFailed { .. } | ConnectError::HostKeyRejected { .. }
        )
    }

    /// Whether the session should retry automatically with backoff.
    /// Authentication and host key failures are never retried (2.5).
    pub fn should_auto_reconnect(&self) -> bool {
        matches!(
            self,
            ConnectError::Unreachable { .. } | ConnectError::ConnectionLost
        )
    }

    /// Whether a matching `vpn` rule should be offered to the user (2.5).
    pub fn vpn_hint_eligible(&self) -> bool {
        matches!(self, ConnectError::Unreachable { .. })
    }
}

/// Classified failure of the two-stage sudo sftp-server start (plan 7.4).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SudoError {
    /// `sudo -n -v` reported that a password is required. This is a signal
    /// to ask the user, not a final failure.
    #[error("sudo requires a password")]
    PasswordRequired,
    /// sudo rejected the password (`Sorry, try again`).
    #[error("Incorrect sudo password")]
    WrongPassword,
    /// The user is not in the sudoers file.
    #[error("Your account is not allowed to use sudo on this host")]
    NotInSudoers,
    /// sudo is configured with `requiretty`, which an SFTP channel cannot
    /// satisfy.
    #[error(
        "sudo on this host requires a terminal (requiretty), which an SFTP \
         session cannot provide"
    )]
    RequiresTty,
    /// sudo policy forbids running the sftp-server binary.
    #[error("sudo does not allow running the sftp-server program on this host")]
    NotAllowedToExecute,
    /// No usable sftp-server binary was found on the host.
    #[error("sftp-server not found on the host")]
    SftpServerNotFound,
    /// The connection underneath failed before sudo could answer.
    #[error(transparent)]
    Connect(#[from] ConnectError),
    /// Any other sudo failure.
    #[error("sudo failed: {detail}")]
    Other {
        /// The stderr excerpt from sudo.
        detail: String,
    },
}

/// Classified file system failure, for local and remote endpoints alike.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    /// The path does not exist.
    #[error("{} does not exist", path.display())]
    NotFound {
        /// The path concerned.
        path: PathBuf,
    },
    /// The operation is not permitted on the path.
    #[error("Permission denied: {}", path.display())]
    PermissionDenied {
        /// The path concerned.
        path: PathBuf,
    },
    /// The target path already exists.
    #[error("{} already exists", path.display())]
    AlreadyExists {
        /// The path concerned.
        path: PathBuf,
    },
    /// A directory was expected.
    #[error("{} is not a directory", path.display())]
    NotADirectory {
        /// The path concerned.
        path: PathBuf,
    },
    /// A non-directory was expected.
    #[error("{} is a directory", path.display())]
    IsADirectory {
        /// The path concerned.
        path: PathBuf,
    },
    /// A directory could not be removed because it is not empty.
    #[error("{} is not empty", path.display())]
    DirectoryNotEmpty {
        /// The path concerned.
        path: PathBuf,
    },
    /// The endpoint does not support the operation (for example symlinks or
    /// trash on a remote endpoint).
    #[error("{operation} is not supported here: {reason}")]
    Unsupported {
        /// The operation, for example `symlink`.
        operation: String,
        /// Why it is unsupported.
        reason: String,
    },
    /// Moving the path to the system trash failed; a permanent delete may be
    /// offered instead (plan 7.6 S9).
    #[error("Cannot move {} to the trash: {message}", path.display())]
    TrashFailed {
        /// The path concerned.
        path: PathBuf,
        /// The reason reported by the trash backend.
        message: String,
    },
    /// The connection to the endpoint was lost during the operation. Items
    /// failing with this error wait for reconnect instead of failing (7.3 S10).
    #[error("The connection was lost")]
    Disconnected,
    /// Any other I/O failure.
    #[error("{}: {message}", path.display())]
    Io {
        /// The path concerned.
        path: PathBuf,
        /// The underlying error text.
        message: String,
    },
}

impl FsError {
    /// Classifies a local `std::io::Error` that occurred on `path`.
    pub fn from_io(error: &io::Error, path: impl Into<PathBuf>) -> FsError {
        let path = path.into();
        match error.kind() {
            io::ErrorKind::NotFound => FsError::NotFound { path },
            io::ErrorKind::PermissionDenied => FsError::PermissionDenied { path },
            io::ErrorKind::AlreadyExists => FsError::AlreadyExists { path },
            io::ErrorKind::NotADirectory => FsError::NotADirectory { path },
            io::ErrorKind::IsADirectory => FsError::IsADirectory { path },
            io::ErrorKind::DirectoryNotEmpty => FsError::DirectoryNotEmpty { path },
            _ => FsError::Io {
                path,
                message: error.to_string(),
            },
        }
    }
}

/// Failure reading a configuration file (scpto settings or sshto groups).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("Cannot read {}: {message}", path.display())]
    Read {
        /// The configuration file.
        path: PathBuf,
        /// The underlying error text.
        message: String,
    },
    /// A line could not be parsed (unknown directive, bad value).
    #[error("{}:{line}: {message}", path.display())]
    Parse {
        /// The configuration file.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// What is wrong with the line.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_are_classified_by_kind() {
        let error = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(
            FsError::from_io(&error, "/a"),
            FsError::NotFound { path: "/a".into() }
        );
        let error = io::Error::other("disk on fire");
        assert_eq!(
            FsError::from_io(&error, "/b"),
            FsError::Io {
                path: "/b".into(),
                message: "disk on fire".into()
            }
        );
    }

    #[test]
    fn connect_error_policies_follow_the_plan() {
        let unreachable = ConnectError::Unreachable {
            reason: UnreachableReason::Timeout,
            detail: String::new(),
        };
        assert!(unreachable.vpn_hint_eligible());
        assert!(unreachable.should_auto_reconnect());
        assert!(!unreachable.needs_foreground_auth());

        let auth = ConnectError::AuthFailed {
            detail: String::new(),
        };
        assert!(auth.needs_foreground_auth());
        assert!(!auth.should_auto_reconnect());
        assert!(!auth.vpn_hint_eligible());

        assert!(ConnectError::ConnectionLost.should_auto_reconnect());
        assert!(!ConnectError::ConnectionLost.vpn_hint_eligible());
    }

    #[test]
    fn command_failure_message_includes_trimmed_stderr() {
        let error = Error::CommandFailed {
            program: "rsync".into(),
            status: ExitStatus::Code(23),
            stderr: "  some files vanished\n".into(),
        };
        assert_eq!(
            error.to_string(),
            "rsync failed (exit status 23): some files vanished"
        );
        let error = Error::CommandFailed {
            program: "tar".into(),
            status: ExitStatus::Signal(9),
            stderr: String::new(),
        };
        assert_eq!(error.to_string(), "tar failed (killed by signal 9)");
    }
}
