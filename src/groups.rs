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

use std::fs;
use std::io;
use std::path::Path;

use crate::error::ConfigError;
use crate::pattern::Pattern;
use crate::settings::ConfigPath;

/// Group names that sshto reserves for its own tabs.
const RESERVED_GROUPS: [&str; 2] = ["All", "Other"];

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

/// Parses group settings text.
pub fn parse(path: &Path, text: &str) -> Result<Groups, ConfigError> {
    let mut groups = Groups::default();
    for (index, line) in text.split_terminator('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let error = |message: &str| ConfigError::Parse {
            path: path.to_path_buf(),
            line: index + 1,
            message: message.to_owned(),
        };
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.chars().any(|ch| ch.is_ascii_control()) || line.ends_with('|') {
            return Err(error(
                "Control characters and trailing separators are not allowed",
            ));
        }
        let mut fields = line.splitn(5, '|');
        let kind = fields.next().unwrap_or("");
        let a = fields.next().unwrap_or("");
        let b = fields.next().unwrap_or("");
        let c = fields.next().unwrap_or("");
        let extra = fields.next().unwrap_or("");
        match kind {
            "group" => {
                if a.is_empty()
                    || !b.is_empty()
                    || !c.is_empty()
                    || !extra.is_empty()
                    || RESERVED_GROUPS.contains(&a)
                {
                    return Err(error("Expected group|name (All and Other are reserved)"));
                }
                if groups.names.iter().any(|name| name == a) {
                    return Err(error("Duplicate group"));
                }
                groups.names.push(a.to_owned());
            }
            "match" => {
                if a.is_empty() || c.is_empty() || !extra.is_empty() {
                    return Err(error("Expected match|group|alias, hostname or either|glob"));
                }
                let field = match b {
                    "alias" => MatchField::Alias,
                    "hostname" => MatchField::Hostname,
                    "either" => MatchField::Either,
                    _ => return Err(error("Match field must be alias, hostname or either")),
                };
                let group = groups
                    .names
                    .iter()
                    .position(|name| name == a)
                    .ok_or_else(|| {
                        error("Declare a group before referencing it in a match rule")
                    })?;
                groups.rules.push(MatchRule {
                    group,
                    field,
                    glob: c.to_owned(),
                });
            }
            "exclude" => {
                if a.is_empty() || !b.is_empty() || !c.is_empty() || !extra.is_empty() {
                    return Err(error("Expected exclude|Host alias glob"));
                }
                groups.excludes.push(a.to_owned());
            }
            _ => return Err(error("Unknown directive; expected group, match or exclude")),
        }
    }
    Ok(groups)
}

/// Reads and parses the file.
pub fn load(location: &ConfigPath) -> Result<Groups, ConfigError> {
    // `symlink_metadata` keeps a dangling symlink at the default location an
    // unreadable file instead of a missing one.
    if !location.explicit {
        if let Err(error) = fs::symlink_metadata(&location.path) {
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(Groups::default());
            }
        }
    }
    let text = fs::read_to_string(&location.path).map_err(|error| ConfigError::Read {
        path: location.path.clone(),
        message: error.to_string(),
    })?;
    parse(&location.path, &text)
}

impl Groups {
    /// Whether an `exclude` glob matches the alias; excluded hosts are not
    /// listed at all.
    pub fn is_excluded(&self, alias: &str) -> bool {
        self.excludes.iter().any(|glob| matches(glob, alias))
    }

    /// Index into [`Groups::names`] of the first matching rule, `None` for
    /// "Other". `hostname` is `None` until resolved.
    pub fn classify(&self, alias: &str, hostname: Option<&str>) -> Option<usize> {
        self.rules
            .iter()
            .find(|rule| {
                let alias_matches = || matches(&rule.glob, alias);
                let hostname_matches = || hostname.is_some_and(|value| matches(&rule.glob, value));
                match rule.field {
                    MatchField::Alias => alias_matches(),
                    MatchField::Hostname => hostname_matches(),
                    MatchField::Either => alias_matches() || hostname_matches(),
                }
            })
            .map(|rule| rule.group)
    }
}

/// sshto's case-insensitive match: glob and value are both lowercased.
fn matches(glob: &str, value: &str) -> bool {
    Pattern::new(&glob.to_ascii_lowercase()).matches(&value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse_ok(text: &str) -> Groups {
        parse(Path::new("rules"), text).unwrap()
    }

    #[test]
    fn invalid_config_messages_and_line_numbers() {
        for (text, line, message) in [
            (
                "group|All\n",
                1,
                "Expected group|name (All and Other are reserved)",
            ),
            (
                "group|Other\n",
                1,
                "Expected group|name (All and Other are reserved)",
            ),
            ("group|X\ngroup|X\n", 2, "Duplicate group"),
            (
                "match|Missing|alias|*\n",
                1,
                "Declare a group before referencing it in a match rule",
            ),
            (
                "group|X\nmatch|X|unknown|*\n",
                2,
                "Match field must be alias, hostname or either",
            ),
            (
                "group|X|bad\n",
                1,
                "Expected group|name (All and Other are reserved)",
            ),
            (
                "group|X\x1b\n",
                1,
                "Control characters and trailing separators are not allowed",
            ),
            (
                "group|X|\n",
                1,
                "Control characters and trailing separators are not allowed",
            ),
            (
                "group|\tX\n",
                1,
                "Control characters and trailing separators are not allowed",
            ),
            (
                "group",
                1,
                "Expected group|name (All and Other are reserved)",
            ),
            (
                "match|X|alias",
                1,
                "Expected match|group|alias, hostname or either|glob",
            ),
            (
                "match||alias|*",
                1,
                "Expected match|group|alias, hostname or either|glob",
            ),
            (
                "group|X\nmatch|X|alias|*|extra",
                2,
                "Expected match|group|alias, hostname or either|glob",
            ),
            ("exclude", 1, "Expected exclude|Host alias glob"),
            ("exclude|x|extra", 1, "Expected exclude|Host alias glob"),
            (
                " group|X",
                1,
                "Unknown directive; expected group, match or exclude",
            ),
            (
                "# comment\n\nunknown",
                3,
                "Unknown directive; expected group, match or exclude",
            ),
        ] {
            let error = parse(Path::new("rules"), text).unwrap_err();
            assert_eq!(
                error,
                ConfigError::Parse {
                    path: PathBuf::from("rules"),
                    line,
                    message: message.to_owned(),
                },
                "{text:?}"
            );
            assert_eq!(error.to_string(), format!("rules:{line}: {message}"));
        }
    }

    #[test]
    fn rules_keep_file_order_and_globs_as_written() {
        let groups = parse_ok(
            "group|Work\ngroup|Home\nmatch|Home|either|HOME-*\nmatch|Work|hostname|10.*\nexclude|GitHub.*",
        );
        assert_eq!(groups.names, ["Work", "Home"]);
        assert_eq!(
            groups.rules,
            [
                MatchRule {
                    group: 1,
                    field: MatchField::Either,
                    glob: "HOME-*".into(),
                },
                MatchRule {
                    group: 0,
                    field: MatchField::Hostname,
                    glob: "10.*".into(),
                },
            ]
        );
        assert_eq!(groups.excludes, ["GitHub.*"]);
    }

    #[test]
    fn posix_classes_exclude_and_match_after_lowercasing() {
        let groups = parse_ok(
            "group|Upper\ngroup|Lower\nmatch|Upper|alias|[[:upper:]]\nmatch|Lower|alias|[[:lower:]]\nexclude|work-[[:alpha:]]",
        );
        assert!(groups.is_excluded("work-A"));
        assert!(!groups.is_excluded("work-1"));
        assert_eq!(groups.classify("A", None), Some(1));
    }

    #[test]
    fn names_are_data_and_whitespace_is_preserved() {
        let name = "$(touch /nonexistent/executed)";
        let groups = parse_ok(&format!(
            "# ignored\x1b\r\n\r\ngroup|{name}\r\nmatch|{name}|alias|*\r\ngroup| spaced "
        ));
        assert_eq!(groups.names, [name, " spaced "]);
        assert_eq!(groups.rules[0].group, 0);
    }

    #[test]
    fn negated_brackets_and_unclosed_brackets_are_patterns() {
        for pattern in ["[^A]*", "[!A]*"] {
            let groups = parse_ok(&format!("exclude|{pattern}"));
            assert!(groups.is_excluded("b"));
            assert!(!groups.is_excluded("A"));
        }
        let groups = parse_ok("group|X\nmatch|X|alias|[\nexclude|unclosed[");
        assert_eq!(groups.classify("[", None), Some(0));
        assert_eq!(groups.classify("x", None), None);
        assert!(groups.is_excluded("UNCLOSED["));
        assert!(!groups.is_excluded("unclosed"));
    }

    #[test]
    fn glob_fields_are_case_insensitive_and_match_slashes_and_leading_dots() {
        let groups = parse_ok(
            "group|A\ngroup|B\ngroup|C\n\
             match|A|alias|[^X]OST-?\nmatch|B|hostname|*.EXAMPLE\nmatch|C|either|foo*\nexclude|GITHUB.*",
        );
        assert_eq!(groups.classify("HOST-a", Some("server.example")), Some(0));
        assert_eq!(groups.classify("xost-a", Some(".EXAMPLE")), Some(1));
        assert_eq!(groups.classify("unmatched", Some("foo/bar")), Some(2));
        assert_eq!(groups.classify("FOObar", Some("unmatched")), Some(2));
        assert_eq!(groups.classify("unmatched", Some("unmatched")), None);
        assert!(groups.is_excluded("GitHub.Example"));
        assert!(!groups.is_excluded("example.com"));
    }

    #[test]
    fn hostname_rules_wait_for_resolution_and_first_match_wins() {
        let groups = parse_ok(
            "group|Lab\ngroup|Personal\n\
             match|Lab|hostname|192.0.2.*\nmatch|Personal|either|home-*\nmatch|Lab|alias|home-lab",
        );
        // Before `ssh -G` answers only alias globs apply.
        assert_eq!(groups.classify("box", None), None);
        assert_eq!(groups.classify("home-lab", None), Some(1));
        // Once the hostname is known, the earlier hostname rule wins.
        assert_eq!(groups.classify("box", Some("192.0.2.7")), Some(0));
        assert_eq!(groups.classify("home-lab", Some("192.0.2.7")), Some(0));
        // `either` tests the hostname too.
        assert_eq!(groups.classify("box", Some("home-nas")), Some(1));
        assert_eq!(Groups::default().classify("any", Some("any")), None);
        assert!(!Groups::default().is_excluded("any"));
    }

    #[test]
    fn missing_default_is_empty_but_explicit_or_dangling_file_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut location = ConfigPath {
            path: directory.path().join("config"),
            explicit: false,
        };
        assert_eq!(load(&location).unwrap(), Groups::default());

        location.explicit = true;
        assert!(matches!(
            load(&location).unwrap_err(),
            ConfigError::Read { path, .. } if path == location.path
        ));

        fs::write(&location.path, "group|X\nmatch|X|alias|x-*").unwrap();
        assert_eq!(load(&location).unwrap().names, ["X"]);
        fs::write(&location.path, "group|X\nbogus").unwrap();
        assert!(matches!(
            load(&location).unwrap_err(),
            ConfigError::Parse { line: 2, .. }
        ));
        fs::remove_file(&location.path).unwrap();

        std::os::unix::fs::symlink(directory.path().join("missing"), &location.path).unwrap();
        location.explicit = false;
        assert!(matches!(
            load(&location).unwrap_err(),
            ConfigError::Read { path, .. } if path == location.path
        ));
    }
}
