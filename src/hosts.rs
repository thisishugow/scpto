//! SSH host discovery.
//!
//! Parses host aliases from `~/.ssh/config`, following `Include` (filesystem
//! globs filtered with `pattern`, sorted lexically, `~` expansion, relative
//! paths against `~/.ssh/`, depth limit 16, cycle detection, including
//! `Include` inside `Match` blocks), skips wildcard patterns, and classifies
//! hosts into sshto groups. Hostname and user are resolved lazily and
//! concurrently with `ssh -G` so the host picker opens immediately.
//!
//! [`ResolvedHost`] is the payload of `BgEvent::HostResolved`.

/// The result of `ssh -G <alias>` for one alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHost {
    /// The effective `hostname`.
    pub hostname: String,
    /// The effective `user`, when set.
    pub user: Option<String>,
}
