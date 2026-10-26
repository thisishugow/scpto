//! sshto group settings: `$SSHTO_CONFIG`, else
//! `$XDG_CONFIG_HOME/sshto/config`, else `~/.config/sshto/config` (resolved
//! with `settings::ConfigPath::resolve`), in sshto's line format, used to
//! group hosts in the picker:
//!
//! ```text
//! group|<name>
//! match|<group>|alias|<glob>
//! match|<group>|hostname|<glob>
//! match|<group>|either|<glob>
//! exclude|<alias glob>
//! ```
//!
//! Parsing follows sshto exactly: `#` lines and empty lines are skipped; a
//! trailing `\r` is dropped; control characters and a trailing `|` are
//! rejected; `All` and `Other` are reserved group names; duplicate groups
//! are rejected; a `match` must name a group declared above it. The first
//! bad line fails with `ConfigError::Parse`. A missing default file means no
//! groups; a missing explicit file is `ConfigError::Read`.
//!
//! Matching lowercases the alias or hostname and matches it with
//! `pattern::Pattern`. Rules are tried in file order and the first match
//! wins; hosts no rule matches belong to the implicit "Other" group.
//! Hostname rules can only be evaluated once `ssh -G` has resolved the alias
//! (lazily, `BgEvent::HostResolved`); until then only alias rules apply, so a
//! host may move to another group when its hostname arrives.
//!
//! # Public API (Wave 2, with `pattern`)
//!
//! ```text
//! /// Parses group settings text.
//! pub fn parse(path: &Path, text: &str) -> Result<Groups, ConfigError>;
//!
//! /// Reads and parses the file.
//! pub fn load(location: &ConfigPath) -> Result<Groups, ConfigError>;
//!
//! impl Groups {
//!     /// Whether an `exclude` glob matches the alias; excluded hosts are
//!     /// not listed at all.
//!     pub fn is_excluded(&self, alias: &str) -> bool;
//!     /// Index into `Groups::names` of the first matching rule, `None` for
//!     /// "Other". `hostname` is `None` until resolved.
//!     pub fn classify(&self, alias: &str, hostname: Option<&str>) -> Option<usize>;
//! }
//! ```

/// Which host attribute a `match` rule tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchField {
    /// The ssh alias.
    Alias,
    /// The resolved `hostname` from `ssh -G`.
    Hostname,
    /// Either of the two.
    Either,
}

/// One `match|<group>|<field>|<glob>` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRule {
    /// Index into [`Groups::names`].
    pub group: usize,
    /// The attribute tested.
    pub field: MatchField,
    /// The glob, as written (compiled with `pattern::Pattern` when matching).
    pub glob: String,
}

/// Parsed sshto group settings. `Default` is "no groups": every host is in
/// "Other".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Groups {
    /// Group names in declaration order (never `All` or `Other`).
    pub names: Vec<String>,
    /// `match` rules in file order.
    pub rules: Vec<MatchRule>,
    /// `exclude` alias globs in file order.
    pub excludes: Vec<String>,
}
