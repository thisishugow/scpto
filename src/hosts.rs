//! SSH host discovery.
//!
//! Collects host aliases from the user's ssh configuration
//! (`SshClient::config_file` when set, else `~/.ssh/config`): every word of
//! a `Host` line (keyword matched case-insensitively, words up to a `#`)
//! that contains none of `*`, `?`, `!`. `Include` is followed per plan 7.6:
//! each argument is `~`-expanded, relative paths are taken against
//! `~/.ssh/`, a glob in the last component is expanded by listing the
//! directory and filtering with `pattern::Pattern`, matches are read in
//! lexical order (as OpenSSH does), nesting is limited to depth 16, cycles
//! are detected, and `Include` inside a `Match` block is followed as well.
//! Problems with included files never abort discovery; they are collected in
//! [`HostList::warnings`]. Group classification lives in `groups`.
//!
//! Hostname and user are resolved lazily and concurrently: the app requests
//! `Effect::ResolveHost(alias)` for each alias it shows, the session effect
//! executor runs one `resolve` per alias in its own
//! task, and each answer arrives as `BgEvent::HostResolved`, so the host
//! picker opens immediately.
//!
//! # Public API (Wave 2)
//!
//! ```text
//! /// Reads the configuration and every included file (blocking file I/O;
//! /// called once at startup, before the TUI starts). A missing top-level
//! /// file yields an empty list without warnings.
//! pub fn discover(client: &SshClient, home: &Path) -> HostList;
//!
//! /// Pure: the aliases of one configuration text, in file order, together
//! /// with its `Include` arguments; used by `discover` and its tests.
//! pub fn parse_config(text: &str) -> ParsedConfig;
//!
//! pub struct ParsedConfig {
//!     pub aliases: Vec<String>,
//!     pub includes: Vec<String>,
//! }
//!
//! /// Runs `<program> [-F <file>] -G -T <alias>` (stdin null,
//! /// `process_group(0)`, `kill_on_drop`) and parses `hostname` and `user`.
//! /// A spawn failure or non-zero exit is `Error::CommandFailed` with
//! /// program `ssh -G`.
//! pub async fn resolve(client: &SshClient, alias: &str) -> Result<ResolvedHost, Error>;
//!
//! /// Pure: parses `ssh -G` output.
//! pub fn parse_resolved(output: &str) -> ResolvedHost;
//! ```

use crate::error::ConfigError;

/// The result of host discovery, passed to the `App` constructor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostList {
    /// Concrete host aliases, sorted and deduplicated.
    pub aliases: Vec<String>,
    /// Files that could not be read and `Include` problems (depth limit,
    /// cycles), as `ConfigError::Read` or `ConfigError::Parse`. The app
    /// shows them as a status message; discovery continues past them.
    pub warnings: Vec<ConfigError>,
}

/// The result of `ssh -G <alias>` for one alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHost {
    /// The effective `hostname`.
    pub hostname: String,
    /// The effective `user`, when set.
    pub user: Option<String>,
}
