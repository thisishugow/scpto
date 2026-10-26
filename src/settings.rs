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
//! # Public API
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
//!     /// The first VPN rule whose glob (`pattern`, matched against
//!     /// the lowercased value like sshto) matches the alias or, when known,
//!     /// the resolved hostname.
//!     pub fn vpn_rule(&self, alias: &str, hostname: Option<&str>) -> Option<&VpnRule>;
//! }
//! ```

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::ConfigError;
use crate::pattern::Pattern;

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

impl Settings {
    /// The first `vpn` rule, in file order, whose glob matches the alias or,
    /// when known, the resolved hostname; both are compared lowercased like
    /// sshto.
    pub fn vpn_rule(&self, alias: &str, hostname: Option<&str>) -> Option<&VpnRule> {
        let alias = alias.to_ascii_lowercase();
        let hostname = hostname.map(str::to_ascii_lowercase);
        self.vpn_rules.iter().find(|rule| {
            let pattern = Pattern::new(&rule.pattern.to_ascii_lowercase());
            pattern.matches(&alias)
                || hostname
                    .as_deref()
                    .is_some_and(|name| pattern.matches(name))
        })
    }
}

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

/// Parses settings text. Lines are split on `\n` (a trailing `\r` is
/// dropped); empty lines and lines starting with `#` are skipped. The first
/// bad line fails with [`ConfigError::Parse`] carrying its one-based number.
pub fn parse(path: &Path, text: &str) -> Result<Settings, ConfigError> {
    let mut settings = Settings::default();
    // Lines of the reconnect keys, for the cross-key check after the file.
    let mut base_line = None;
    let mut cap_line = None;
    for (index, raw) in text.split('\n').enumerate() {
        let line = index + 1;
        let content = raw.strip_suffix('\r').unwrap_or(raw);
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let fail = |message: String| ConfigError::Parse {
            path: path.to_path_buf(),
            line,
            message,
        };
        let mut fields = content.splitn(3, '|');
        let directive = fields.next().unwrap_or_default();
        match (directive, fields.next(), fields.next()) {
            (DIRECTIVE_SET, Some(key), Some(value)) => match key {
                KEY_CONCURRENCY => settings.concurrency = count(key, value, 1).map_err(fail)?,
                KEY_TAR_THRESHOLD => settings.tar_threshold = count(key, value, 1).map_err(fail)?,
                KEY_SHOW_HIDDEN => {
                    settings.show_hidden = value.parse().map_err(|_| {
                        fail(format!("{key} must be true or false, got \"{value}\""))
                    })?;
                }
                KEY_MTIME_TOLERANCE => {
                    settings.mtime_tolerance = number(key, value, 0).map_err(fail)?;
                }
                KEY_RECONNECT_BASE_MS => {
                    settings.reconnect_base =
                        Duration::from_millis(number(key, value, 1).map_err(fail)?);
                    base_line = Some(line);
                }
                KEY_RECONNECT_CAP_MS => {
                    settings.reconnect_cap =
                        Duration::from_millis(number(key, value, 1).map_err(fail)?);
                    cap_line = Some(line);
                }
                KEY_KEEPALIVE_INTERVAL => {
                    settings.keepalive_interval =
                        Duration::from_secs(number(key, value, 1).map_err(fail)?);
                }
                _ => {
                    return Err(fail(format!(
                        "unknown key \"{key}\" (expected one of: {})",
                        ALL_KEYS.join(", ")
                    )))
                }
            },
            (DIRECTIVE_VPN, Some(pattern), Some(command)) => {
                if pattern.is_empty() || command.is_empty() {
                    return Err(fail("vpn needs a non-empty glob and command".to_owned()));
                }
                settings.vpn_rules.push(VpnRule {
                    pattern: pattern.to_owned(),
                    command: command.to_owned(),
                    line,
                });
            }
            (DIRECTIVE_SET, _, _) => return Err(fail("expected set|<key>|<value>".to_owned())),
            (DIRECTIVE_VPN, _, _) => return Err(fail("expected vpn|<glob>|<command>".to_owned())),
            _ => return Err(fail(format!("unknown directive \"{directive}\""))),
        }
    }
    if settings.reconnect_cap < settings.reconnect_base {
        return Err(ConfigError::Parse {
            path: path.to_path_buf(),
            line: base_line.max(cap_line).unwrap_or(1),
            message: format!("{KEY_RECONNECT_CAP_MS} must be at least {KEY_RECONNECT_BASE_MS}"),
        });
    }
    Ok(settings)
}

/// Parses an integer value of `key` that must be at least `min`.
fn number(key: &str, value: &str, min: u64) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(parsed) if parsed >= min => Ok(parsed),
        _ => Err(format!(
            "{key} must be an integer >= {min}, got \"{value}\""
        )),
    }
}

/// Like [`number`], for values stored as `usize`.
fn count(key: &str, value: &str, min: u64) -> Result<usize, String> {
    usize::try_from(number(key, value, min)?).map_err(|_| format!("{key} is too large: {value}"))
}

/// Reads and parses the file. A missing file at a default location gives
/// `Settings::default()`; a missing explicit file, a dangling symlink, or any
/// other read error is [`ConfigError::Read`].
pub fn load(location: &ConfigPath) -> Result<Settings, ConfigError> {
    let read_error = |error: std::io::Error| ConfigError::Read {
        path: location.path.clone(),
        message: error.to_string(),
    };
    match std::fs::read_to_string(&location.path) {
        Ok(text) => parse(&location.path, &text),
        // A dangling symlink also reports NotFound, but the link itself
        // exists, so the user meant to have a file there.
        Err(error)
            if error.kind() == ErrorKind::NotFound
                && !location.explicit
                && location.path.symlink_metadata().is_err() =>
        {
            Ok(Settings::default())
        }
        Err(error) => Err(read_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpn_rule_matches_alias_or_hostname_case_insensitively_first_wins() {
        let settings = Settings {
            vpn_rules: vec![
                VpnRule {
                    pattern: "nadi-*".into(),
                    command: "first".into(),
                    line: 1,
                },
                VpnRule {
                    pattern: "*.LAB".into(),
                    command: "second".into(),
                    line: 2,
                },
            ],
            ..Settings::default()
        };
        let command = |alias: &str, hostname: Option<&str>| {
            settings
                .vpn_rule(alias, hostname)
                .map(|rule| rule.command.as_str())
        };
        assert_eq!(command("NADI-gpu", Some("x.lab")), Some("first"));
        assert_eq!(command("box", Some("x.Lab")), Some("second"));
        assert_eq!(command("box", None), None);
        assert_eq!(command("box", Some("x.example")), None);
    }

    #[test]
    fn defaults_set_transfer_display_and_connection_options() {
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

    fn parse_ok(text: &str) -> Settings {
        parse(Path::new("cfg"), text).expect("parse")
    }

    fn parse_line(text: &str) -> usize {
        match parse(Path::new("cfg"), text) {
            Err(ConfigError::Parse { line, .. }) => line,
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn parses_all_keys_comments_crlf_and_last_set_wins() {
        let settings = parse_ok(
            "# c\r\n\r\nset|concurrency|5\r\nset|concurrency|6\nset|tar-threshold|10\n\
             set|show-hidden|true\nset|mtime-tolerance|2\nset|reconnect-base-ms|50\n\
             set|reconnect-cap-ms|400\nset|keepalive-interval|7\n",
        );
        assert_eq!(settings.concurrency, 6);
        assert_eq!(settings.tar_threshold, 10);
        assert!(settings.show_hidden);
        assert_eq!(settings.mtime_tolerance, 2);
        assert_eq!(settings.reconnect_base, Duration::from_millis(50));
        assert_eq!(settings.reconnect_cap, Duration::from_millis(400));
        assert_eq!(settings.keepalive_interval, Duration::from_secs(7));
    }

    #[test]
    fn vpn_command_keeps_pipes_and_records_the_line() {
        let settings = parse_ok("\nvpn|nadi-*|ssh -t jump 'a|b'\n");
        assert_eq!(
            settings.vpn_rules,
            vec![VpnRule {
                pattern: "nadi-*".into(),
                command: "ssh -t jump 'a|b'".into(),
                line: 2
            }]
        );
    }

    #[test]
    fn bad_lines_report_their_one_based_number() {
        assert_eq!(parse_line("set|concurrency|0"), 1);
        assert_eq!(parse_line("# x\nset|concurrency|abc"), 2);
        assert_eq!(parse_line("\n\nset|show-hidden|yes"), 3);
        assert_eq!(parse_line("set|mtime-tolerance|-1"), 1);
        assert_eq!(parse_line("set|nope|1"), 1);
        assert_eq!(parse_line("set|concurrency"), 1);
        assert_eq!(parse_line("vpn||cmd"), 1);
        assert_eq!(parse_line("ok\n"), 1);
        assert_eq!(parse_line("vpn|glob"), 1);
        assert_eq!(parse_line("vpn|glob|"), 1);
    }

    #[test]
    fn mtime_tolerance_zero_is_valid_but_other_minimums_are_one() {
        assert_eq!(parse_ok("set|mtime-tolerance|0").mtime_tolerance, 0);
        assert_eq!(parse_line("set|keepalive-interval|0"), 1);
        assert_eq!(parse_line("set|reconnect-base-ms|0"), 1);
    }

    #[test]
    fn reconnect_cap_below_base_fails_after_the_whole_file() {
        assert_eq!(
            parse_line("set|reconnect-cap-ms|10\nset|reconnect-base-ms|20"),
            2
        );
        assert_eq!(parse_line("set|reconnect-base-ms|60000"), 1);
        let fixed = parse_ok("set|reconnect-base-ms|60000\nset|reconnect-cap-ms|90000");
        assert_eq!(fixed.reconnect_cap, Duration::from_millis(90000));
    }

    #[test]
    fn load_handles_missing_default_missing_explicit_and_dangling_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("config");
        let default = ConfigPath {
            path: file.clone(),
            explicit: false,
        };
        assert_eq!(load(&default).expect("default"), Settings::default());
        let explicit = ConfigPath {
            path: file.clone(),
            explicit: true,
        };
        assert!(matches!(load(&explicit), Err(ConfigError::Read { .. })));

        std::os::unix::fs::symlink(dir.path().join("gone"), &file).expect("symlink");
        assert!(matches!(load(&default), Err(ConfigError::Read { .. })));
        std::fs::remove_file(&file).expect("unlink");

        std::fs::write(&file, "set|concurrency|9\n").expect("write");
        assert_eq!(load(&default).expect("load").concurrency, 9);
        std::fs::write(&file, "bogus\n").expect("write");
        assert!(matches!(
            load(&default),
            Err(ConfigError::Parse { line: 1, .. })
        ));
    }
}
