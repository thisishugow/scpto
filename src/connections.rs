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
//! for either channel. [`Connections::spawn_remote`] and
//! [`Connections::tools`] are for remote endpoints only: local tar runs in
//! process (the `tar` crate) and local rsync is run and probed by the sync
//! executor itself. Implementations reject `Endpoint::Local` there with an
//! error, never a panic.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::error::Error;
use crate::vfs::{Endpoint, Fs};

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
    /// so it may serve as an rsync sync peer (plan 7.5 S14).
    pub rsync_ok: bool,
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
    /// the implementation quotes each with the session's single quoting
    /// helper, so an argument containing a newline fails with
    /// `Error::NewlineInPath`.
    fn spawn_remote<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        argv: &'a [String],
    ) -> BoxFuture<'a, Result<RemoteChild, Error>>;

    /// The tools available on a remote endpoint, probed once per connection
    /// and cached.
    fn tools<'a>(&'a self, endpoint: &'a Endpoint) -> BoxFuture<'a, Result<RemoteTools, Error>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_handle_is_dyn_compatible_and_shareable() {
        // Compiles only while `Connections` stays dyn-compatible and
        // `Arc<dyn Connections>` can cross threads.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn Connections>>();
        assert_send_sync::<RemoteChild>();
    }
}
