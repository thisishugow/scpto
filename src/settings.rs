//! scpto settings: types, defaults, and directive/key names.
//!
//! The file lives at `$SCPTO_CONFIG`, else `$XDG_CONFIG_HOME/scpto/config`,
//! else `~/.config/scpto/config` (non-absolute XDG values are ignored). It
//! uses sshto's line-based pipe-delimited format; `#` starts a comment and
//! unknown directives are reported with their line number:
//!
//! ```text
//! set|concurrency|3
//! set|tar-threshold|200
//! set|show-hidden|false
//! set|mtime-tolerance|2
//! vpn|nadi-*|ssh -t ts-dgx-dev ovpn
//! ```
//!
//! Settings are data: a `vpn` command runs only when the user presses "Run"
//! in the VPN dialog, via `/bin/sh -c`.
//!
//! [`ConfigPath`] resolves both this file and sshto's group file (`groups`)
//! with the same rules.
//!
//! # Public API (Wave 2 implements `parse` and `load`)
//!
//! ```text
//! /// Parses settings text. Lines are split on `\n` (a trailing `\r` is
//! /// dropped); empty lines and lines starting with `#` are skipped.
//! /// `set|<key>|<value>` must name a key of `ALL_KEYS` with a valid value:
//! /// `concurrency`, `tar-threshold`: integer >= 1; `show-hidden`: `true` or
//! /// `false`; `mtime-tolerance`: integer >= 0; `reconnect-base-ms`,
//! /// `reconnect-cap-ms`: integer >= 1 with cap >= base after the whole file;
//! /// `keepalive-interval`: integer >= 1 (seconds). A later `set` of the same
//! /// key wins. `vpn|<glob>|<command>` splits at the first two `|` only, so
//! /// the command may contain `|`; glob and command must be non-empty. The
//! /// first bad line fails with `ConfigError::Parse` (one-based line).
//! pub fn parse(path: &Path, text: &str) -> Result<Settings, ConfigError>;
//!
//! /// Reads and parses the file. A missing file at a default location gives
//! /// `Settings::default()`; a missing explicit file (or a dangling symlink,
//! /// or any read error) is `ConfigError::Read`.
//! pub fn load(location: &ConfigPath) -> Result<Settings, ConfigError>;
//!
//! impl Settings {
//!     /// Wave 3 (vpn): the first rule whose glob (`pattern`, matched against
//!     /// the lowercased value like sshto) matches the alias or, when known,
//!     /// the resolved hostname.
//!     pub fn vpn_rule(&self, alias: &str, hostname: Option<&str>) -> Option<&VpnRule>;
//! }
//! ```

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variable naming an explicit settings file.
pub const CONFIG_ENV: &str = "SCPTO_CONFIG";

/// Directive that sets a key: `set|<key>|<value>`.
pub const DIRECTIVE_SET: &str = "set";

/// Directive that adds a VPN rule: `vpn|<glob>|<command>`.
pub const DIRECTIVE_VPN: &str = "vpn";

/// Key: number of items transferred at the same time (positive integer).
pub const KEY_CONCURRENCY: &str = "concurrency";

/// Key: minimum file count of a job that uses tar mode (positive integer).
pub const KEY_TAR_THRESHOLD: &str = "tar-threshold";

/// Key: whether panes show dotfiles initially (`true` / `false`).
pub const KEY_SHOW_HIDDEN: &str = "show-hidden";

/// Key: whole seconds of mtime difference treated as equal (non-negative
/// integer).
pub const KEY_MTIME_TOLERANCE: &str = "mtime-tolerance";

/// Key: first reconnect delay in milliseconds. Test-only; not documented in
/// the README.
pub const KEY_RECONNECT_BASE_MS: &str = "reconnect-base-ms";

/// Key: maximum reconnect delay in milliseconds. Test-only; not documented
/// in the README.
pub const KEY_RECONNECT_CAP_MS: &str = "reconnect-cap-ms";

/// Key: `ServerAliveInterval` of each master connection in seconds.
/// Test-only; not documented in the README.
pub const KEY_KEEPALIVE_INTERVAL: &str = "keepalive-interval";

/// Every key accepted by the `set` directive.
pub const ALL_KEYS: &[&str] = &[
    KEY_CONCURRENCY,
    KEY_TAR_THRESHOLD,
    KEY_SHOW_HIDDEN,
    KEY_MTIME_TOLERANCE,
    KEY_RECONNECT_BASE_MS,
    KEY_RECONNECT_CAP_MS,
    KEY_KEEPALIVE_INTERVAL,
];

/// Where a configuration file is read from, and whether the user named it
/// explicitly (an explicit file must exist; a default one may be missing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPath {
    /// The file.
    pub path: PathBuf,
    /// Whether it came from the explicit environment variable.
    pub explicit: bool,
}

impl ConfigPath {
    /// Resolves `<explicit>` (the value of [`CONFIG_ENV`] or `SSHTO_CONFIG`;
    /// empty counts as unset), else `$XDG_CONFIG_HOME/<app>/config` when that
    /// value is an absolute path (the XDG spec ignores relative values), else
    /// `<home>/.config/<app>/config`.
    pub fn resolve(
        explicit: Option<&OsStr>,
        xdg_config_home: Option<&OsStr>,
        home: &Path,
        app: &str,
    ) -> ConfigPath {
        if let Some(path) = explicit.filter(|value| !value.is_empty()) {
            return ConfigPath {
                path: PathBuf::from(path),
                explicit: true,
            };
        }
        let base = xdg_config_home
            .map(Path::new)
            .filter(|value| value.is_absolute())
            .map_or_else(|| home.join(".config"), Path::to_path_buf);
        ConfigPath {
            path: base.join(app).join("config"),
            explicit: false,
        }
    }
}

/// A `vpn` rule: when a connection to a host matching `pattern` is
/// unreachable, offer to run `command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpnRule {
    /// Shell glob matched against the host alias or its resolved hostname.
    pub pattern: String,
    /// Command run via `/bin/sh -c` in the foreground.
    pub command: String,
    /// One-based line number in the settings file, for messages.
    pub line: usize,
}

/// Effective settings. `Default` gives the documented defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Items transferred at the same time.
    pub concurrency: usize,
    /// Minimum file count of a job that uses tar mode.
    pub tar_threshold: usize,
    /// Whether panes show dotfiles initially.
    pub show_hidden: bool,
    /// Whole seconds of mtime difference treated as equal.
    pub mtime_tolerance: u64,
    /// VPN rules in file order; the first match wins.
    pub vpn_rules: Vec<VpnRule>,
    /// First reconnect delay; doubled after each failed attempt.
    pub reconnect_base: Duration,
    /// Maximum reconnect delay.
    pub reconnect_cap: Duration,
    /// `ServerAliveInterval` of master connections.
    pub keepalive_interval: Duration,
}

/// Default of [`Settings::concurrency`].
pub const DEFAULT_CONCURRENCY: usize = 3;
/// Default of [`Settings::tar_threshold`].
pub const DEFAULT_TAR_THRESHOLD: usize = 200;
/// Default of [`Settings::show_hidden`].
pub const DEFAULT_SHOW_HIDDEN: bool = false;
/// Default of [`Settings::mtime_tolerance`].
pub const DEFAULT_MTIME_TOLERANCE: u64 = 0;
/// Default of [`Settings::reconnect_base`].
pub const DEFAULT_RECONNECT_BASE: Duration = Duration::from_secs(1);
/// Default of [`Settings::reconnect_cap`].
pub const DEFAULT_RECONNECT_CAP: Duration = Duration::from_secs(30);
/// Default of [`Settings::keepalive_interval`].
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// `ServerAliveCountMax` of master connections (not configurable).
pub const KEEPALIVE_COUNT_MAX: u32 = 3;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            concurrency: DEFAULT_CONCURRENCY,
            tar_threshold: DEFAULT_TAR_THRESHOLD,
            show_hidden: DEFAULT_SHOW_HIDDEN,
            mtime_tolerance: DEFAULT_MTIME_TOLERANCE,
            vpn_rules: Vec::new(),
            reconnect_base: DEFAULT_RECONNECT_BASE,
            reconnect_cap: DEFAULT_RECONNECT_CAP,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_plan() {
        let settings = Settings::default();
        assert_eq!(settings.concurrency, 3);
        assert_eq!(settings.tar_threshold, 200);
        assert!(!settings.show_hidden);
        assert_eq!(settings.mtime_tolerance, 0);
        assert!(settings.vpn_rules.is_empty());
        assert_eq!(settings.reconnect_base, Duration::from_secs(1));
        assert_eq!(settings.reconnect_cap, Duration::from_secs(30));
        assert_eq!(settings.keepalive_interval, Duration::from_secs(15));
    }

    #[test]
    fn config_path_prefers_explicit_then_absolute_xdg_then_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            ConfigPath::resolve(Some(OsStr::new("rel/cfg")), None, home, "scpto"),
            ConfigPath {
                path: "rel/cfg".into(),
                explicit: true
            }
        );
        let xdg = Some(OsStr::new("/x"));
        assert_eq!(
            ConfigPath::resolve(Some(OsStr::new("")), xdg, home, "scpto"),
            ConfigPath {
                path: "/x/scpto/config".into(),
                explicit: false
            }
        );
        for ignored in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
            assert_eq!(
                ConfigPath::resolve(None, ignored, home, "sshto").path,
                PathBuf::from("/home/u/.config/sshto/config")
            );
        }
    }

    #[test]
    fn key_names_are_unique() {
        let mut keys = ALL_KEYS.to_vec();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), ALL_KEYS.len());
    }
}
