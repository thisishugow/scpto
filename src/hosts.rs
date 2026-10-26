//! SSH host discovery.
//!
//! Collects host aliases from the user's ssh configuration
//! (`SshClient::config_file` when set, else `~/.ssh/config`): every word of
//! a `Host` line (keyword matched case-insensitively and separated from its
//! arguments by whitespace or `=`, as OpenSSH allows; words split on
//! whitespace, up to a word starting with `#`) that contains none of `*`,
//! `?`, `!`. `Include` is followed per plan 7.6: each argument is
//! `~`-expanded (`~` and `~/...` only; a `~user` argument is reported as a
//! warning), relative paths are taken against `~/.ssh/` (also with
//! `SshClient::config_file`, as OpenSSH does), a glob in the last component
//! is expanded by listing the directory and filtering with
//! `pattern::Pattern`, matches are read in lexical byte order (as OpenSSH
//! does), nesting is limited to depth 16, cycles are detected, and `Include`
//! inside a `Match` block is followed as well. Like glob(3), a glob does not
//! match names starting with `.` unless the glob itself starts with `.`, and
//! directories among the matches are skipped. Like OpenSSH, an included path
//! that does not exist and a glob without matches are skipped silently.
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
//! /// Runs `<program> [-F <file>] -G -T -- <alias>` (stdin null,
//! /// `process_group(0)`, `kill_on_drop`) and parses `hostname` and `user`.
//! /// The `--` keeps an alias starting with `-` from being read as an
//! /// option. A spawn failure or non-zero exit is `Error::CommandFailed`
//! /// with program `ssh -G`.
//! pub async fn resolve(client: &SshClient, alias: &str) -> Result<ResolvedHost, Error>;
//!
//! /// Pure: parses `ssh -G` output.
//! pub fn parse_resolved(output: &str) -> ResolvedHost;
//! ```

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::connections::SshClient;
use crate::error::{ConfigError, Error, ExitStatus};
use crate::pattern::Pattern;

/// OpenSSH's `READCONF_MAX_DEPTH`: the top-level file is depth 0 and files
/// nested deeper than this are refused.
const MAX_INCLUDE_DEPTH: usize = 16;

/// Program name used in `Error::CommandFailed` for `ssh -G` failures.
const RESOLVE_PROGRAM: &str = "ssh -G";

/// How many trailing stderr lines of a failed `ssh -G` are kept.
const STDERR_TAIL_LINES: usize = 5;

/// Characters that make the last component of an `Include` path a glob.
const GLOB_CHARS: [char; 4] = ['*', '?', '[', '\\'];

/// The result of host discovery, passed to the `App` constructor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostList {
    /// Concrete host aliases, sorted and deduplicated.
    pub aliases: Vec<String>,
    /// Files that could not be read and `Include` problems (depth limit,
    /// cycles, unsupported `~user` paths), as `ConfigError::Read` naming the
    /// file or `Include` argument concerned. The app shows them as a status
    /// message; discovery continues past them.
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

/// What [`parse_config`] finds in one configuration text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedConfig {
    /// Concrete aliases of `Host` lines, in file order (not deduplicated).
    pub aliases: Vec<String>,
    /// `Include` arguments as written, in file order.
    pub includes: Vec<String>,
}

/// Reads the configuration and every included file (blocking file I/O;
/// called once at startup, before the TUI starts). A missing top-level file
/// yields an empty list without warnings.
pub fn discover(client: &SshClient, home: &Path) -> HostList {
    let top = client
        .config_file
        .clone()
        .unwrap_or_else(|| home.join(".ssh").join("config"));
    let mut discovery = Discovery {
        home,
        aliases: Vec::new(),
        warnings: Vec::new(),
        ancestors: Vec::new(),
    };
    discovery.visit(&top, 0);

    let mut aliases = discovery.aliases;
    aliases.sort_unstable();
    aliases.dedup();
    // A file included from several places reports its problem once.
    let mut warnings = Vec::new();
    for warning in discovery.warnings {
        if !warnings.contains(&warning) {
            warnings.push(warning);
        }
    }
    HostList { aliases, warnings }
}

/// Pure: the aliases of one configuration text, in file order, together with
/// its `Include` arguments; used by [`discover`] and its tests.
pub fn parse_config(text: &str) -> ParsedConfig {
    let mut parsed = ParsedConfig::default();
    for line in text.lines() {
        let (keyword, arguments) = split_keyword(line);
        let words = arguments
            .split_ascii_whitespace()
            .take_while(|word| !word.starts_with('#'));
        if keyword.eq_ignore_ascii_case("host") {
            parsed.aliases.extend(
                words
                    .filter(|word| !word.contains(['*', '?', '!']))
                    .map(str::to_owned),
            );
        } else if keyword.eq_ignore_ascii_case("include") {
            parsed.includes.extend(words.map(str::to_owned));
        }
    }
    parsed
}

/// Runs `<program> [-F <file>] -G -T -- <alias>` (stdin null,
/// `process_group(0)`, `kill_on_drop`) and parses `hostname` and `user`. A
/// spawn failure or non-zero exit is `Error::CommandFailed` with program
/// `ssh -G`.
pub async fn resolve(client: &SshClient, alias: &str) -> Result<ResolvedHost, Error> {
    let mut command = Command::new(&client.program);
    if let Some(file) = &client.config_file {
        command.arg("-F").arg(file);
    }
    command
        .args(["-G", "-T", "--", alias])
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let output = command
        .output()
        .await
        .map_err(|error| Error::CommandFailed {
            program: RESOLVE_PROGRAM.to_owned(),
            status: ExitStatus::NotStarted,
            stderr: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: RESOLVE_PROGRAM.to_owned(),
            status: output.status.into(),
            stderr: stderr_tail(&output.stderr),
        });
    }
    Ok(parse_resolved(&String::from_utf8_lossy(&output.stdout)))
}

/// Pure: parses `ssh -G` output. Only lines starting with exactly
/// `hostname ` or `user ` count, the last one wins, and the value is its
/// second word. A missing or empty `user` is `None`.
pub fn parse_resolved(output: &str) -> ResolvedHost {
    let mut hostname = String::new();
    let mut user = None;
    for line in output.lines() {
        let value = line.split_ascii_whitespace().nth(1).unwrap_or("");
        if line.starts_with("hostname ") {
            hostname = value.to_owned();
        } else if line.starts_with("user ") {
            user = Some(value.to_owned()).filter(|value| !value.is_empty());
        }
    }
    ResolvedHost { hostname, user }
}

/// State of one discovery walk over the configuration tree.
struct Discovery<'a> {
    home: &'a Path,
    aliases: Vec<String>,
    warnings: Vec<ConfigError>,
    /// Canonical paths of the files currently being read, outermost first;
    /// a file that appears here again closes an `Include` cycle.
    ancestors: Vec<PathBuf>,
}

impl Discovery<'_> {
    fn visit(&mut self, path: &Path, depth: usize) {
        let canonical = match fs::canonicalize(path) {
            Ok(canonical) => canonical,
            // OpenSSH skips missing files without a message.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => return self.warn(path, error.to_string()),
        };
        if self.ancestors.contains(&canonical) {
            return self.warn(path, "it includes itself (Include cycle)".to_owned());
        }
        if depth > MAX_INCLUDE_DEPTH {
            return self.warn(
                path,
                format!("Include is nested deeper than {MAX_INCLUDE_DEPTH} levels"),
            );
        }
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => return self.warn(path, error.to_string()),
        };
        let parsed = parse_config(&text);
        self.aliases.extend(parsed.aliases);
        self.ancestors.push(canonical);
        for argument in &parsed.includes {
            match include_paths(argument, self.home) {
                Ok(paths) => {
                    for included in paths {
                        self.visit(&included, depth + 1);
                    }
                }
                Err(warning) => self.warnings.push(warning),
            }
        }
        self.ancestors.pop();
    }

    fn warn(&mut self, path: &Path, message: String) {
        self.warnings.push(ConfigError::Read {
            path: path.to_path_buf(),
            message,
        });
    }
}

/// The files one `Include` argument names, in the order they are read.
fn include_paths(argument: &str, home: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let expanded = if argument == "~" {
        home.to_path_buf()
    } else if let Some(rest) = argument.strip_prefix("~/") {
        home.join(rest)
    } else if argument.starts_with('~') {
        return Err(ConfigError::Read {
            path: PathBuf::from(argument),
            message: "Include paths starting with ~user are not supported".to_owned(),
        });
    } else {
        PathBuf::from(argument)
    };
    let path = if expanded.is_absolute() {
        expanded
    } else {
        home.join(".ssh").join(expanded)
    };
    expand_glob(&path)
}

/// Expands a glob in the last component of `path` (glob(3) rules for leading
/// dots, directories skipped), sorted by bytes. A path without a glob is
/// returned as is.
fn expand_glob(path: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let (Some(directory), Some(glob)) = (path.parent(), path.file_name().and_then(OsStr::to_str))
    else {
        return Ok(vec![path.to_path_buf()]);
    };
    if !glob.contains(GLOB_CHARS) {
        return Ok(vec![path.to_path_buf()]);
    }
    let read_error = |error: io::Error| ConfigError::Read {
        path: directory.to_path_buf(),
        message: error.to_string(),
    };
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        // A glob in a directory that does not exist simply has no matches.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(read_error(error)),
    };
    let pattern = Pattern::new(glob);
    let dot_allowed = glob.starts_with('.');
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(read_error)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with('.') && !dot_allowed) || !pattern.matches(&name) {
            continue;
        }
        let candidate = entry.path();
        // `metadata` follows symlinks. When it fails, the entry is kept so
        // that `visit` reports the problem (or skips a dangling link).
        if fs::metadata(&candidate).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        matches.push(candidate);
    }
    matches.sort_unstable_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    Ok(matches)
}

/// Splits a configuration line into its keyword and the rest, like
/// OpenSSH's `strdelim`: the keyword ends at whitespace or `=`, and one `=`
/// surrounded by optional whitespace may separate it from the arguments.
fn split_keyword(line: &str) -> (&str, &str) {
    let line = line.trim_start_matches(|ch: char| ch.is_ascii_whitespace());
    let end = line
        .find(|ch: char| ch.is_ascii_whitespace() || ch == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start_matches(|ch: char| ch.is_ascii_whitespace());
    (keyword, rest.strip_prefix('=').unwrap_or(rest))
}

/// The last [`STDERR_TAIL_LINES`] non-empty lines of `stderr`.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A fake home directory with a `.ssh` directory.
    fn home() -> TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".ssh")).unwrap();
        home
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn discover_home(home: &Path) -> HostList {
        discover(&SshClient::default(), home)
    }

    #[test]
    fn aliases_are_concrete_words_of_host_lines() {
        let parsed = parse_config(
            "Include other\nHost z A a [literal] a? b!c * x#y #stop ignored\n\
             hOsT z a \u{e9}\nHostName not-an-alias\n  Host=eq\nHost = spaced=eq tail\n\
             \tHost\ttabbed\r\nHosts nope\n# Host commented\nMatch host m*\n",
        );
        assert_eq!(
            parsed.aliases,
            [
                "z",
                "A",
                "a",
                "[literal]",
                "x#y",
                "z",
                "a",
                "\u{e9}",
                "eq",
                "spaced=eq",
                "tail",
                "tabbed",
            ]
        );
        assert_eq!(parsed.includes, ["other"]);
    }

    #[test]
    fn include_lines_keep_every_argument_in_order() {
        let parsed = parse_config(
            "include a b # c\nINCLUDE=~/x\n  Include   /abs/*.conf\nIncluded no\nInclude\n",
        );
        assert_eq!(parsed.includes, ["a", "b", "~/x", "/abs/*.conf"]);
        assert!(parsed.aliases.is_empty());
        assert_eq!(parse_config(""), ParsedConfig::default());
    }

    #[test]
    fn missing_top_level_file_gives_an_empty_list() {
        let home = home();
        assert_eq!(discover_home(home.path()), HostList::default());
        let client = SshClient {
            config_file: Some(home.path().join("absent")),
            ..SshClient::default()
        };
        assert_eq!(discover(&client, home.path()), HostList::default());
    }

    #[test]
    fn aliases_are_sorted_and_deduplicated_across_files() {
        let home = home();
        let ssh = home.path().join(".ssh");
        write(
            &ssh.join("config"),
            "Host web db *.internal\nInclude extra\nHost web\n",
        );
        write(&ssh.join("extra"), "Host cache db\n");
        assert_eq!(
            discover_home(home.path()),
            HostList {
                aliases: vec!["cache".into(), "db".into(), "web".into()],
                warnings: Vec::new(),
            }
        );
    }

    #[test]
    fn includes_expand_tilde_and_resolve_relative_paths_against_dot_ssh() {
        let home = home();
        let ssh = home.path().join(".ssh");
        let elsewhere = tempfile::tempdir().unwrap();
        let absolute = elsewhere.path().join("abs.conf");
        write(
            &ssh.join("config"),
            &format!(
                "Include conf.d/relative ~/tilde.conf {}\n",
                absolute.display()
            ),
        );
        write(&ssh.join("conf.d/relative"), "Host relative\n");
        write(&home.path().join("tilde.conf"), "Host tilde\n");
        write(&absolute, "Host absolute\n");
        assert_eq!(
            discover_home(home.path()).aliases,
            ["absolute", "relative", "tilde"]
        );
    }

    #[test]
    fn explicit_config_file_still_resolves_relative_includes_against_dot_ssh() {
        let home = home();
        let elsewhere = tempfile::tempdir().unwrap();
        let config = elsewhere.path().join("generated");
        write(&config, "Host top\nInclude part\n");
        write(&home.path().join(".ssh/part"), "Host part\n");
        write(&elsewhere.path().join("part"), "Host wrong\n");
        // The default file is not read when an explicit one is set.
        write(&home.path().join(".ssh/config"), "Host default\n");
        let client = SshClient {
            config_file: Some(config),
            ..SshClient::default()
        };
        assert_eq!(discover(&client, home.path()).aliases, ["part", "top"]);
    }

    #[test]
    fn globs_skip_dotfiles_and_directories_and_sort_by_bytes() {
        let home = home();
        let directory = home.path().join(".ssh/config.d");
        for name in ["b.conf", "a.conf", "Z.conf", ".hidden.conf", "notes.txt"] {
            write(&directory.join(name), "");
        }
        fs::create_dir(directory.join("sub.conf")).unwrap();
        assert_eq!(
            expand_glob(&directory.join("*.conf")).unwrap(),
            [
                directory.join("Z.conf"),
                directory.join("a.conf"),
                directory.join("b.conf"),
            ]
        );
        assert_eq!(
            expand_glob(&directory.join(".*")).unwrap(),
            [directory.join(".hidden.conf")]
        );
        assert_eq!(
            expand_glob(&directory.join("[ab].conf")).unwrap(),
            [directory.join("a.conf"), directory.join("b.conf")]
        );
        assert!(expand_glob(&directory.join("*.none")).unwrap().is_empty());
        assert!(expand_glob(&home.path().join("missing/*"))
            .unwrap()
            .is_empty());
        let literal = directory.join("not-there");
        assert_eq!(expand_glob(&literal).unwrap(), [literal]);
    }

    #[test]
    fn glob_includes_are_read_and_missing_files_are_skipped_silently() {
        let home = home();
        let ssh = home.path().join(".ssh");
        write(
            &ssh.join("config"),
            "Include config.d/* missing.conf nodir/*\n",
        );
        write(&ssh.join("config.d/10-work"), "Host work\n");
        write(&ssh.join("config.d/20-home"), "Host home\n");
        write(&ssh.join("config.d/.swp"), "Host swap\n");
        assert_eq!(
            discover_home(home.path()),
            HostList {
                aliases: vec!["home".into(), "work".into()],
                warnings: Vec::new(),
            }
        );
    }

    #[test]
    fn include_inside_match_block_is_followed() {
        let home = home();
        let ssh = home.path().join(".ssh");
        write(
            &ssh.join("config"),
            "Match host *.corp exec \"true\"\n    Include corp.conf\nHost plain\n",
        );
        write(&ssh.join("corp.conf"), "Host corp-1 corp-2\n");
        assert_eq!(
            discover_home(home.path()).aliases,
            ["corp-1", "corp-2", "plain"]
        );
    }

    #[test]
    fn nesting_is_limited_to_sixteen_levels() {
        let home = home();
        let ssh = home.path().join(".ssh");
        write(&ssh.join("config"), "Host level0\nInclude level1\n");
        for level in 1..=17 {
            write(
                &ssh.join(format!("level{level}")),
                &format!("Host level{level}\nInclude level{}\n", level + 1),
            );
        }
        let list = discover_home(home.path());
        let mut expected: Vec<String> = (0..=16).map(|level| format!("level{level}")).collect();
        expected.sort_unstable();
        assert_eq!(list.aliases, expected);
        assert_eq!(
            list.warnings,
            [ConfigError::Read {
                path: ssh.join("level17"),
                message: "Include is nested deeper than 16 levels".into(),
            }]
        );
    }

    #[test]
    fn cycles_are_reported_and_discovery_continues() {
        let home = home();
        let ssh = home.path().join(".ssh");
        // `config*` matches the top-level file itself.
        write(&ssh.join("config"), "Host top\nInclude config* a\n");
        write(&ssh.join("config-extra"), "Host extra\n");
        write(&ssh.join("a"), "Host a\nInclude b\n");
        write(&ssh.join("b"), "Host b\nInclude a\n");
        let list = discover_home(home.path());
        assert_eq!(list.aliases, ["a", "b", "extra", "top"]);
        assert_eq!(
            list.warnings,
            [
                ConfigError::Read {
                    path: ssh.join("config"),
                    message: "it includes itself (Include cycle)".into(),
                },
                ConfigError::Read {
                    path: ssh.join("a"),
                    message: "it includes itself (Include cycle)".into(),
                },
            ]
        );
    }

    #[test]
    fn a_file_included_twice_without_a_cycle_is_read_twice() {
        let home = home();
        let ssh = home.path().join(".ssh");
        write(&ssh.join("config"), "Include left right\n");
        write(&ssh.join("left"), "Host left\nInclude shared\n");
        write(&ssh.join("right"), "Host right\nInclude shared\n");
        write(&ssh.join("shared"), "Host shared\n");
        let list = discover_home(home.path());
        assert_eq!(list.aliases, ["left", "right", "shared"]);
        assert!(list.warnings.is_empty());
    }

    #[test]
    fn unreadable_includes_become_warnings() {
        let home = home();
        let ssh = home.path().join(".ssh");
        write(
            &ssh.join("config"),
            "Host ok\nInclude dir ~other/conf binary\n",
        );
        fs::create_dir(ssh.join("dir")).unwrap();
        fs::write(ssh.join("binary"), b"Host bad\xff\n").unwrap();
        let list = discover_home(home.path());
        assert_eq!(list.aliases, ["ok"]);
        let paths: Vec<PathBuf> = list
            .warnings
            .iter()
            .map(|warning| match warning {
                ConfigError::Read { path, .. } => path.clone(),
                ConfigError::Parse { .. } => panic!("unexpected {warning:?}"),
            })
            .collect();
        assert_eq!(
            paths,
            [
                ssh.join("dir"),
                PathBuf::from("~other/conf"),
                ssh.join("binary")
            ]
        );
    }

    #[test]
    fn ssh_output_uses_last_exact_lowercase_key_with_space() {
        assert_eq!(
            parse_resolved(
                "hostname first\nuser first\nHostName ignored\n hostname ignored\n\
                 hostname\tignored\nuser\tignored\nhostname final extra\nuser tester extra\n"
            ),
            ResolvedHost {
                hostname: "final".into(),
                user: Some("tester".into()),
            }
        );
        assert_eq!(
            parse_resolved("hostname 192.0.2.10\r\nuser tester\r\n"),
            ResolvedHost {
                hostname: "192.0.2.10".into(),
                user: Some("tester".into()),
            }
        );
        assert_eq!(
            parse_resolved("hostname example.com\nuser \n"),
            ResolvedHost {
                hostname: "example.com".into(),
                user: None,
            }
        );
        assert_eq!(
            parse_resolved(""),
            ResolvedHost {
                hostname: String::new(),
                user: None,
            }
        );
    }

    #[test]
    fn stderr_tail_keeps_the_last_non_empty_lines() {
        assert_eq!(stderr_tail(b"a\r\n\r\nb\nc\nd\ne\nf\n\n"), "b\nc\nd\ne\nf");
        assert_eq!(stderr_tail(b"  \n"), "");
    }

    #[tokio::test]
    async fn resolve_runs_ssh_g_with_the_configured_file() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config");
        write(
            &config,
            "Host box\n  HostName box.example.invalid\n  User tester\n",
        );
        let client = SshClient {
            config_file: Some(config),
            ..SshClient::default()
        };
        assert_eq!(
            resolve(&client, "box").await.unwrap(),
            ResolvedHost {
                hostname: "box.example.invalid".into(),
                user: Some("tester".into()),
            }
        );
    }

    #[tokio::test]
    async fn resolve_passes_the_alias_after_double_dash() {
        use std::os::unix::fs::PermissionsExt;

        // A program that reports its argv as the hostname shows exactly what
        // `resolve` runs; `--` keeps an alias such as `-oHostName=x` from
        // being read as an option.
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("print-argv");
        write(
            &program,
            "#!/bin/sh\nIFS=,\nprintf 'hostname %s\\nuser me\\n' \"$*\"\n",
        );
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let mut client = SshClient {
            program,
            config_file: None,
        };
        assert_eq!(
            resolve(&client, "-oHostName=x").await.unwrap().hostname,
            "-G,-T,--,-oHostName=x"
        );
        client.config_file = Some(PathBuf::from("/cfg"));
        assert_eq!(
            resolve(&client, "box").await.unwrap(),
            ResolvedHost {
                hostname: "-F,/cfg,-G,-T,--,box".into(),
                user: Some("me".into()),
            }
        );
    }

    #[tokio::test]
    async fn resolve_reports_ssh_failures() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config");
        write(&config, "NoSuchKeyword yes\n");
        let client = SshClient {
            config_file: Some(config),
            ..SshClient::default()
        };
        match resolve(&client, "box").await.unwrap_err() {
            Error::CommandFailed {
                program,
                status,
                stderr,
            } => {
                assert_eq!(program, "ssh -G");
                assert_eq!(status, ExitStatus::Code(255));
                // OpenSSH lowercases the keyword in its message.
                assert!(
                    stderr.to_ascii_lowercase().contains("nosuchkeyword"),
                    "{stderr}"
                );
            }
            other => panic!("unexpected {other:?}"),
        }

        let client = SshClient {
            program: directory.path().join("no-such-ssh"),
            config_file: None,
        };
        assert!(matches!(
            resolve(&client, "box").await.unwrap_err(),
            Error::CommandFailed {
                status: ExitStatus::NotStarted,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn resolve_reports_exit_codes_and_signal_deaths() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("exit-status");
        for (script, expected) in [
            ("#!/bin/sh\nkill -9 $$\n", ExitStatus::Signal(9)),
            ("#!/bin/sh\nexit 3\n", ExitStatus::Code(3)),
        ] {
            write(&program, script);
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
            let client = SshClient {
                program: program.clone(),
                config_file: None,
            };
            assert!(matches!(
                resolve(&client, "box").await.unwrap_err(),
                Error::CommandFailed { status, .. } if status == expected
            ));
        }
    }
}
