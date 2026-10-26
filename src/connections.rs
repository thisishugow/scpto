//! Access to connected endpoints for the transfer engine and the effect
//! executors.
//!
//! The engine and every executor take an `Arc<dyn Connections>` instead of
//! talking to sessions directly, so tests can substitute a mock. The session
//! layer implements the trait; it owns reconnecting, and callers only see
//! handles that are ready to use. `TransferCommand::EndpointReconnected`
//! stays a plain notification: after it, callers fetch fresh handles from
//! [`Connections::fs`].
//!
//! Methods return [`BoxFuture`] for the same dyn-compatibility reason as
//! [`Fs`].
//!
//! # Local endpoints
//!
//! [`Connections::fs`] serves `Endpoint::Local` with the local file system
//! for either channel. [`Connections::spawn_remote`],
//! [`Connections::ssh_command`], and [`Connections::tools`] are for remote
//! endpoints only: local tar runs in process (the `tar` crate) and local
//! rsync is run and probed by the sync executor itself. Implementations
//! reject `Endpoint::Local` there with an error, never a panic.
//!
//! # sudo endpoints
//!
//! There is one master connection per host alias; `sudo` is a property of
//! the sftp-server child only. For `Endpoint::Remote { sudo: true, .. }`,
//! [`Connections::spawn_remote`], [`Connections::ssh_command`], and
//! [`Connections::tools`] behave exactly as for the same host without sudo:
//! commands run as the login user over the same master.
//!
//! Whether a sudo endpoint's [`FsChannel::Browse`] and
//! [`FsChannel::Transfer`] share one root sftp-server or each run their own
//! is decided by the session. The sudo password is never retained after a
//! start: when a second sudo channel needs a password, the session asks
//! again through `BgEvent::SudoPasswordRequired`.
//!
//! # Quoting (plan 7.3)
//!
//! [`quote`] and [`join_command`] are the crate's only POSIX shell quoting
//! helper, used for every remote command string: the session's
//! [`Connections::spawn_remote`] and probes, sudo's stage commands, and the
//! `t` shell. Each word is wrapped in single quotes with every `'` written
//! as `'\''`; a word containing `\n` fails with `Error::NewlineInPath`.
//! rsync's `-e` string is not a shell command and is encoded by `sync`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::error::Error;
use crate::vfs::{Endpoint, Fs};

/// Quotes one word for a remote POSIX shell: wrapped in single quotes, with
/// every `'` written as `'\''`. A word containing a newline is rejected with
/// [`Error::NewlineInPath`] (plan 7.3).
pub fn quote(word: &str) -> Result<String, Error> {
    if word.contains('\n') {
        return Err(Error::NewlineInPath {
            path: PathBuf::from(word),
        });
    }
    Ok(format!("'{}'", word.replace('\'', r"'\''")))
}

/// Quotes every word with [`quote`] and joins them with single spaces.
pub fn join_command(words: &[impl AsRef<str>]) -> Result<String, Error> {
    let quoted = words
        .iter()
        .map(|word| quote(word.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(quoted.join(" "))
}

/// Which of a remote endpoint's two SFTP channels to use (plan 7.3 S17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsChannel {
    /// Directory listings, file operations, previews, and edits.
    Browse,
    /// Transfer engine jobs and engine-based syncs.
    Transfer,
}

/// The most recent stderr lines of a background child, filled by the
/// session's drain task and kept bounded by it (plan 7.3 S10). Lines are
/// stored without their `\r\n` / `\n` terminator, oldest first. Read it to
/// classify or report a failure after the child ended.
pub type StderrTail = Arc<Mutex<VecDeque<String>>>;

/// A non-interactive command running on a remote endpoint over the master
/// connection.
///
/// Dropping it kills the process (`kill_on_drop`). Callers stream through
/// `stdin` and `stdout` (shutting `stdin` down to signal end of input), then
/// await `child` and consult `stderr` when the exit status is not success.
#[derive(Debug)]
pub struct RemoteChild {
    /// The local `ssh` process carrying the remote command.
    pub child: Child,
    /// The remote command's standard input.
    pub stdin: ChildStdin,
    /// The remote command's standard output.
    pub stdout: ChildStdout,
    /// The tail of the remote command's (and ssh's) standard error.
    pub stderr: StderrTail,
}

/// Remote tools found by probing a remote endpoint once per connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteTools {
    /// `tar` is available (tar acceleration, plan 7.5 S8).
    pub tar: bool,
    /// `rsync --version` reports a real rsync 3.1 or newer (not openrsync),
    /// so it may serve as an rsync sync peer (plan 7.5 S14). Decided by
    /// [`rsync_version_ok`].
    pub rsync_ok: bool,
}

/// Whether `rsync --version` output comes from rsync 3.1 or newer. Its
/// first line must read `rsync version <major>.<minor>...` (an optional `v`
/// before the number is accepted), which excludes openrsync (`openrsync:
/// protocol version 29`) and rsync 2.x. Shared by the session's remote
/// probe and the sync executor's local probe.
pub fn rsync_version_ok(version_output: &str) -> bool {
    let Some(first_line) = version_output.lines().next() else {
        return false;
    };
    let mut words = first_line.split_whitespace();
    if words.next() != Some("rsync") || words.next() != Some("version") {
        return false;
    }
    let Some(version) = words.next() else {
        return false;
    };
    let version = version.strip_prefix('v').unwrap_or(version);
    let mut parts = version.split('.');
    let number = |part: Option<&str>| -> Option<u32> {
        let digits: String = part?.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    match (number(parts.next()), number(parts.next())) {
        (Some(major), Some(minor)) => (major, minor) >= (3, 1),
        _ => false,
    }
}

/// How scpto invokes the local OpenSSH client. Shared by the session (master
/// and children), host discovery, and `ssh -G` resolution so that all of
/// them see the same configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshClient {
    /// The `ssh` program, looked up in `PATH` when it has no slash.
    pub program: PathBuf,
    /// An explicit configuration file. When set, every `ssh` invocation gets
    /// `-F <file>` and host discovery reads this file instead of
    /// `~/.ssh/config`. The product leaves it unset; integration tests point
    /// it at a generated configuration.
    pub config_file: Option<PathBuf>,
}

impl Default for SshClient {
    fn default() -> Self {
        SshClient {
            program: PathBuf::from("ssh"),
            config_file: None,
        }
    }
}

/// Ready-to-use handles to endpoints. Dyn-compatible: held as
/// `Arc<dyn Connections>`.
pub trait Connections: Send + Sync {
    /// The file system of `endpoint` on `channel`. While the endpoint is
    /// reconnecting this waits until it is back; it fails when the endpoint
    /// is not connected and will not reconnect on its own (for example after
    /// an authentication failure, or once no pane uses it any more).
    fn fs<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        channel: FsChannel,
    ) -> BoxFuture<'a, Result<Arc<dyn Fs>, Error>>;

    /// Starts `argv` on a remote endpoint as a multiplexed, non-interactive
    /// child (plan 7.2, 7.3, 8.5): the common non-interactive options plus
    /// `BatchMode=yes` and `ControlMaster=no`, after `ssh -O check`, spawned
    /// with `process_group(0)`, piped stdin and stdout, `kill_on_drop`, and
    /// stderr drained into [`RemoteChild::stderr`]. Callers pass raw words;
    /// the implementation quotes each with [`join_command`], so an argument
    /// containing a newline fails with `Error::NewlineInPath`.
    fn spawn_remote<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        argv: &'a [String],
    ) -> BoxFuture<'a, Result<RemoteChild, Error>>;

    /// The complete local `ssh` argv that reaches `endpoint`'s host through
    /// its master connection, without the destination host and without a
    /// remote command. The caller appends the host alias (and, for the shell,
    /// the remote command quoted with [`join_command`]).
    ///
    /// Shape, in this order: [`SshClient::program`]; `-F <file>` when
    /// [`SshClient::config_file`] is set; `-o ControlPath=<path>`,
    /// `-o ControlMaster=no`; then
    ///
    /// - `interactive == false` (rsync `-e`): the plan 7.3 common options
    ///   `-o RemoteCommand=none -o RequestTTY=no -o ClearAllForwardings=yes
    ///   -o PermitLocalCommand=no -o ForwardX11=no -o ForwardAgent=no`, then
    ///   `-o BatchMode=yes`. The sync runner encodes the argv for rsync's
    ///   own `-e` parser (not a shell); rsync appends the host and its
    ///   server command itself.
    /// - `interactive == true` (the `t` shell): only `-o RemoteCommand=none`
    ///   and `-t`, so the remote shell gets a terminal and OpenSSH may prompt.
    ///
    /// Like [`Connections::spawn_remote`], it runs `ssh -O check` first and
    /// rebuilds the master when it is gone, so the argv is usable right away.
    /// `Endpoint::Local` fails with an error.
    fn ssh_command<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        interactive: bool,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>>;

    /// The tools available on a remote endpoint, probed once per connection
    /// and cached.
    fn tools<'a>(&'a self, endpoint: &'a Endpoint) -> BoxFuture<'a, Result<RemoteTools, Error>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_string()).collect()
    }

    #[test]
    fn quote_wraps_words_in_single_quotes() {
        assert_eq!(quote("plain").unwrap(), "'plain'");
        assert_eq!(quote("").unwrap(), "''");
        assert_eq!(quote("a b").unwrap(), "'a b'");
        assert_eq!(quote("it's").unwrap(), r"'it'\''s'");
        assert_eq!(quote("$HOME `x` \"y\"").unwrap(), "'$HOME `x` \"y\"'");
        assert_eq!(quote("\u{7a7a}\u{767d}").unwrap(), "'\u{7a7a}\u{767d}'");
    }

    #[test]
    fn quote_rejects_newlines() {
        assert_eq!(
            quote("a\nb"),
            Err(Error::NewlineInPath {
                path: PathBuf::from("a\nb")
            })
        );
        // A carriage return alone is an ordinary byte for the shell.
        assert_eq!(quote("a\rb").unwrap(), "'a\rb'");
    }

    #[test]
    fn join_command_quotes_every_word() {
        let words = lines(&["tar", "-C", "/srv/my dir", "-xf", "-"]);
        assert_eq!(
            join_command(&words).unwrap(),
            "'tar' '-C' '/srv/my dir' '-xf' '-'"
        );
        assert_eq!(join_command(&[] as &[String]).unwrap(), "");
        assert!(matches!(
            join_command(&lines(&["ok", "bad\nname"])),
            Err(Error::NewlineInPath { .. })
        ));
    }

    #[test]
    fn quoted_words_survive_a_real_shell() {
        let words = lines(&["printf", "%s|", "a b", "it's", "$HOME", "\\", "*"]);
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(join_command(&words).unwrap())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "a b|it's|$HOME|\\|*|"
        );
    }

    #[test]
    fn connections_handle_is_dyn_compatible_and_shareable() {
        // Compiles only while `Connections` stays dyn-compatible and
        // `Arc<dyn Connections>` can cross threads.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn Connections>>();
        assert_send_sync::<RemoteChild>();
    }

    #[test]
    fn rsync_version_requires_real_rsync_3_1() {
        assert!(rsync_version_ok(
            "rsync  version 3.2.7  protocol version 31\nCopyright"
        ));
        assert!(rsync_version_ok(
            "rsync  version v3.4.1  protocol version 32"
        ));
        assert!(rsync_version_ok(
            "rsync  version 3.1.0  protocol version 31"
        ));
        assert!(!rsync_version_ok(
            "rsync  version 3.0.9  protocol version 30"
        ));
        assert!(!rsync_version_ok(
            "rsync  version 2.6.9  protocol version 29"
        ));
        assert!(!rsync_version_ok(
            "openrsync: protocol version 29\nrsync version 2.6.9 compatible"
        ));
        assert!(!rsync_version_ok(""));
        assert!(!rsync_version_ok("rsync  version"));
    }

    #[test]
    fn ssh_client_defaults_to_path_lookup_without_config_file() {
        let client = SshClient::default();
        assert_eq!(client.program, PathBuf::from("ssh"));
        assert_eq!(client.config_file, None);
    }
}
