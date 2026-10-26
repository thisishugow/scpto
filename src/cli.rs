//! Command-line parsing.
//!
//! Accepts `--help` / `-h`, `--version` / `-V` and the endpoint arguments
//! below; anything else is a usage error reported on stderr with exit code
//! [`USAGE_EXIT_CODE`].
//!
//! # Command line
//!
//! ```text
//! scpto                          left: local cwd, right: host picker
//! scpto <host>[:<path>]          right pane on that host
//! scpto [--left <ep>] [--right <ep>]
//! ```
//!
//! - The positional argument is always remote: `host` alone (no colon)
//!   means the host's start directory. It conflicts with `--right`.
//! - `<ep>` follows scp rules (plan 7.6): `host:path` and `host:` are
//!   remote; a value without `:`, or with a `/` before the first `:` (as in
//!   `./a:b`), is a local path; `[v6addr]:path` is remote with the brackets
//!   removed from the host. An empty host (`:path`) or an empty value is a
//!   usage error.
//! - `--left`/`--right` may each appear once, as `--left <ep>` or
//!   `--left=<ep>`. `--help` and `--version` must be the only argument.
//! - Paths are kept as given (relative local paths, `~`); startup makes
//!   local paths absolute against the process cwd. An empty remote path is
//!   `None`.
//! - `USAGE` documents all of this and keeps mentioning `SCPTO_CONFIG` and
//!   `SSHTO_CONFIG`.
//!
//! # Public API
//!
//! ```text
//! pub enum Invocation {
//!     Help,
//!     Version,
//!     /// Start the TUI.
//!     Launch(Launch),
//! }
//!
//! pub struct Launch {
//!     /// `None`: the local cwd.
//!     pub left: Option<EndpointArg>,
//!     /// `None`: open the host picker on the right pane.
//!     pub right: Option<EndpointArg>,
//! }
//!
//! pub enum EndpointArg {
//!     Local(PathBuf),
//!     Remote { host: String, path: Option<PathBuf> },
//! }
//!
//! /// Pure: one `<ep>` value by scp rules.
//! pub fn parse_endpoint(value: &OsStr) -> Result<EndpointArg, UsageError>;
//!
//! /// No arguments yields `Launch` with both sides `None`.
//! pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Invocation, UsageError>;
//!
//! pub enum UsageError {
//!     Unrecognized(String),
//!     Extra(String),
//!     /// `--left`/`--right` without a value.
//!     MissingValue(&'static str),
//!     /// `--left`/`--right` given twice, or a positional host with `--right`.
//!     Duplicate(&'static str),
//!     /// An endpoint value that cannot be parsed.
//!     BadEndpoint(String),
//! }
//! ```

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Exit code for invalid command lines.
pub const USAGE_EXIT_CODE: u8 = 2;

/// Usage text printed by `--help` and after usage errors.
pub const USAGE: &str = "\
Usage: scpto [--left <ep>] [--right <ep>]
       scpto <host>[:<path>] [--left <ep>]
       scpto --help | --version

Dual-pane terminal file transfer over SFTP.

With no arguments the left pane shows the local working directory and the
right pane opens the host picker.

Arguments:
  <host>[:<path>]  Open the right pane on that host (at <path> if given);
                   conflicts with --right

Options:
      --left <ep>   Endpoint for the left pane
      --right <ep>  Endpoint for the right pane
  -h, --help        Print this help and exit
  -V, --version     Print the version and exit

Endpoints (<ep>) follow scp rules:
  host:path        Remote path on host
  host:            Remote start directory on host
  [v6addr]:path    Remote path on an IPv6 host
  path             Local path (also when a / comes before the first colon,
                   as in ./a:b)

Environment:
  SCPTO_CONFIG     scpto settings file
                   (default: $XDG_CONFIG_HOME/scpto/config, else ~/.config/scpto/config)
  SSHTO_CONFIG     sshto group settings used to group hosts
                   (default: $XDG_CONFIG_HOME/sshto/config, else ~/.config/sshto/config)
";

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Print usage and exit.
    Help,
    /// Print the version and exit.
    Version,
    /// Start the TUI.
    Launch(Launch),
}

/// The pane endpoints requested for the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// `None`: the local cwd.
    pub left: Option<EndpointArg>,
    /// `None`: open the host picker on the right pane.
    pub right: Option<EndpointArg>,
}

/// One endpoint argument, as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointArg {
    /// A local path, kept as given.
    Local(PathBuf),
    /// A remote host with an optional path (`None`: the start directory).
    Remote {
        /// Host name or address, without IPv6 brackets.
        host: String,
        /// Remote path, kept as given.
        path: Option<PathBuf>,
    },
}

/// A command line that could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UsageError {
    /// An argument was not recognized.
    #[error("unrecognized argument '{0}'")]
    Unrecognized(String),
    /// An argument was not expected here.
    #[error("unexpected extra argument '{0}'")]
    Extra(String),
    /// `--left`/`--right` without a value.
    #[error("option '{0}' requires a value")]
    MissingValue(&'static str),
    /// `--left`/`--right` given twice, or a positional host with `--right`.
    #[error("option '{0}' given more than once or in conflict with the host argument")]
    Duplicate(&'static str),
    /// An endpoint value that cannot be parsed.
    #[error("invalid endpoint '{0}'")]
    BadEndpoint(String),
}

/// The `--version` output line.
pub fn version_line() -> String {
    format!("scpto {}", env!("CARGO_PKG_VERSION"))
}

/// Parses one `<ep>` value by scp rules.
pub fn parse_endpoint(value: &OsStr) -> Result<EndpointArg, UsageError> {
    let bad = || UsageError::BadEndpoint(value.to_string_lossy().into_owned());
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(bad());
    }
    let (host, rest) = if bytes[0] == b'[' {
        let close = bytes.windows(2).position(|w| w == b"]:").ok_or_else(bad)?;
        (&bytes[1..close], &bytes[close + 2..])
    } else {
        let Some(colon) = bytes.iter().position(|&b| b == b':') else {
            return Ok(EndpointArg::Local(PathBuf::from(value)));
        };
        if bytes[..colon].contains(&b'/') {
            return Ok(EndpointArg::Local(PathBuf::from(value)));
        }
        (&bytes[..colon], &bytes[colon + 1..])
    };
    if host.is_empty() {
        return Err(bad());
    }
    let host = std::str::from_utf8(host).map_err(|_| bad())?.to_owned();
    let path = (!rest.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(rest)));
    Ok(EndpointArg::Remote { host, path })
}

/// Parses the positional `<host>[:<path>]` argument, which is always remote.
fn parse_positional(value: &OsStr) -> Result<EndpointArg, UsageError> {
    let bad = || UsageError::BadEndpoint(value.to_string_lossy().into_owned());
    let bytes = value.as_bytes();
    if bytes.contains(&b':') {
        return match parse_endpoint(value)? {
            remote @ EndpointArg::Remote { .. } => Ok(remote),
            EndpointArg::Local(_) => Err(bad()),
        };
    }
    if bytes.is_empty() || bytes.contains(&b'/') {
        return Err(bad());
    }
    let host = value.to_str().ok_or_else(bad)?;
    Ok(EndpointArg::Remote {
        host: host.to_owned(),
        path: None,
    })
}

/// Which pane option a value belongs to.
#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn flag(self) -> &'static str {
        match self {
            Side::Left => "--left",
            Side::Right => "--right",
        }
    }
}

/// Parses the arguments after the program name.
pub fn parse<I>(args: I) -> Result<Invocation, UsageError>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    let is_info = |a: &OsString| matches!(a.to_str(), Some("-h" | "--help" | "-V" | "--version"));
    if let Some(index) = args.iter().position(is_info) {
        if args.len() > 1 {
            let other = &args[if index == 0 { 1 } else { 0 }];
            return Err(UsageError::Extra(other.to_string_lossy().into_owned()));
        }
        return Ok(match args[0].to_str() {
            Some("-h" | "--help") => Invocation::Help,
            _ => Invocation::Version,
        });
    }

    let mut left: Option<EndpointArg> = None;
    let mut right: Option<EndpointArg> = None;
    let mut positional: Option<EndpointArg> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_str();
        let (side, inline) = match text {
            Some("--left") => (Side::Left, None),
            Some("--right") => (Side::Right, None),
            Some(t) if t.starts_with("--left=") => (Side::Left, Some(&t["--left=".len()..])),
            Some(t) if t.starts_with("--right=") => (Side::Right, Some(&t["--right=".len()..])),
            _ => {
                if text.is_some_and(|t| t.starts_with('-') && t != "-") {
                    return Err(UsageError::Unrecognized(arg.to_string_lossy().into_owned()));
                }
                if positional.is_some() {
                    return Err(UsageError::Extra(arg.to_string_lossy().into_owned()));
                }
                if right.is_some() {
                    return Err(UsageError::Duplicate(Side::Right.flag()));
                }
                positional = Some(parse_positional(&arg)?);
                continue;
            }
        };
        let value = match inline {
            Some(v) => OsString::from(v),
            None => iter.next().ok_or(UsageError::MissingValue(side.flag()))?,
        };
        let slot = match side {
            Side::Left => &mut left,
            Side::Right => &mut right,
        };
        if slot.is_some() || (matches!(side, Side::Right) && positional.is_some()) {
            return Err(UsageError::Duplicate(side.flag()));
        }
        *slot = Some(parse_endpoint(&value)?);
    }
    Ok(Invocation::Launch(Launch {
        left,
        right: right.or(positional),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Invocation, UsageError> {
        parse(args.iter().map(OsString::from))
    }

    fn ep(value: &str) -> Result<EndpointArg, UsageError> {
        parse_endpoint(OsStr::new(value))
    }

    fn local(path: &str) -> EndpointArg {
        EndpointArg::Local(PathBuf::from(path))
    }

    fn remote(host: &str, path: Option<&str>) -> EndpointArg {
        EndpointArg::Remote {
            host: host.into(),
            path: path.map(PathBuf::from),
        }
    }

    fn launch(
        left: Option<EndpointArg>,
        right: Option<EndpointArg>,
    ) -> Result<Invocation, UsageError> {
        Ok(Invocation::Launch(Launch { left, right }))
    }

    #[test]
    fn recognizes_help_and_version() {
        assert_eq!(parse_strs(&["--help"]), Ok(Invocation::Help));
        assert_eq!(parse_strs(&["-h"]), Ok(Invocation::Help));
        assert_eq!(parse_strs(&["--version"]), Ok(Invocation::Version));
        assert_eq!(parse_strs(&["-V"]), Ok(Invocation::Version));
    }

    #[test]
    fn help_and_version_must_be_alone() {
        assert_eq!(
            parse_strs(&["--help", "x"]),
            Err(UsageError::Extra("x".into()))
        );
        assert_eq!(
            parse_strs(&["host", "-V"]),
            Err(UsageError::Extra("host".into()))
        );
        assert_eq!(
            parse_strs(&["-h", "-V"]),
            Err(UsageError::Extra("-V".into()))
        );
    }

    #[test]
    fn no_arguments_launches_defaults() {
        assert_eq!(parse_strs(&[]), launch(None, None));
    }

    #[test]
    fn rejects_unknown_options() {
        assert_eq!(
            parse_strs(&["--bogus"]),
            Err(UsageError::Unrecognized("--bogus".into()))
        );
        assert_eq!(
            parse_strs(&["-x"]),
            Err(UsageError::Unrecognized("-x".into()))
        );
        assert_eq!(
            parse_strs(&["--leftx", "a"]),
            Err(UsageError::Unrecognized("--leftx".into()))
        );
    }

    #[test]
    fn endpoint_without_colon_is_local() {
        assert_eq!(ep("docs"), Ok(local("docs")));
        assert_eq!(ep("~/x"), Ok(local("~/x")));
        assert_eq!(ep("/abs/path"), Ok(local("/abs/path")));
    }

    #[test]
    fn endpoint_with_slash_before_colon_is_local() {
        assert_eq!(ep("./a:b"), Ok(local("./a:b")));
        assert_eq!(ep("/tmp/a:b"), Ok(local("/tmp/a:b")));
    }

    #[test]
    fn endpoint_host_path_and_host_colon_are_remote() {
        assert_eq!(ep("web:/var/log"), Ok(remote("web", Some("/var/log"))));
        assert_eq!(ep("web:rel/dir"), Ok(remote("web", Some("rel/dir"))));
        assert_eq!(ep("web:~"), Ok(remote("web", Some("~"))));
        assert_eq!(ep("web:"), Ok(remote("web", None)));
        assert_eq!(ep("web:a:b"), Ok(remote("web", Some("a:b"))));
    }

    #[test]
    fn endpoint_bracketed_ipv6_drops_brackets() {
        assert_eq!(ep("[::1]:/srv"), Ok(remote("::1", Some("/srv"))));
        assert_eq!(ep("[fe80::1]:"), Ok(remote("fe80::1", None)));
    }

    #[test]
    fn endpoint_rejects_malformed_values() {
        assert_eq!(ep(""), Err(UsageError::BadEndpoint(String::new())));
        assert_eq!(ep(":path"), Err(UsageError::BadEndpoint(":path".into())));
        assert_eq!(ep("[::1]"), Err(UsageError::BadEndpoint("[::1]".into())));
        assert_eq!(
            ep("[::1]x:y"),
            Err(UsageError::BadEndpoint("[::1]x:y".into()))
        );
        assert_eq!(ep("[]:p"), Err(UsageError::BadEndpoint("[]:p".into())));
    }

    #[test]
    fn positional_is_always_remote() {
        assert_eq!(
            parse_strs(&["web"]),
            launch(None, Some(remote("web", None)))
        );
        assert_eq!(
            parse_strs(&["web:"]),
            launch(None, Some(remote("web", None)))
        );
        assert_eq!(
            parse_strs(&["web:/etc"]),
            launch(None, Some(remote("web", Some("/etc"))))
        );
        assert_eq!(
            parse_strs(&["[::1]:/etc"]),
            launch(None, Some(remote("::1", Some("/etc"))))
        );
    }

    #[test]
    fn positional_rejects_local_forms() {
        assert_eq!(
            parse_strs(&["/tmp"]),
            Err(UsageError::BadEndpoint("/tmp".into()))
        );
        assert_eq!(
            parse_strs(&["./a:b"]),
            Err(UsageError::BadEndpoint("./a:b".into()))
        );
        assert_eq!(
            parse_strs(&[":x"]),
            Err(UsageError::BadEndpoint(":x".into()))
        );
        assert_eq!(
            parse_strs(&[""]),
            Err(UsageError::BadEndpoint(String::new()))
        );
    }

    #[test]
    fn second_positional_is_extra() {
        assert_eq!(parse_strs(&["a", "b"]), Err(UsageError::Extra("b".into())));
    }

    #[test]
    fn left_and_right_accept_separate_and_inline_values() {
        assert_eq!(
            parse_strs(&["--left", "docs", "--right", "web:/srv"]),
            launch(Some(local("docs")), Some(remote("web", Some("/srv"))))
        );
        assert_eq!(
            parse_strs(&["--left=web:", "--right=./x"]),
            launch(Some(remote("web", None)), Some(local("./x")))
        );
    }

    #[test]
    fn either_pane_can_be_remote_or_local() {
        assert_eq!(
            parse_strs(&["--left", "a:/x", "--right", "b:/y"]),
            launch(Some(remote("a", Some("/x"))), Some(remote("b", Some("/y"))))
        );
    }

    #[test]
    fn positional_combines_with_left() {
        assert_eq!(
            parse_strs(&["--left", "docs", "web"]),
            launch(Some(local("docs")), Some(remote("web", None)))
        );
        assert_eq!(
            parse_strs(&["web", "--left", "docs"]),
            launch(Some(local("docs")), Some(remote("web", None)))
        );
    }

    #[test]
    fn missing_values_are_reported() {
        assert_eq!(
            parse_strs(&["--left"]),
            Err(UsageError::MissingValue("--left"))
        );
        assert_eq!(
            parse_strs(&["web", "--right"]),
            Err(UsageError::MissingValue("--right"))
        );
        assert_eq!(
            parse_strs(&["--right"]),
            Err(UsageError::MissingValue("--right"))
        );
        assert_eq!(
            parse_strs(&["--right="]),
            Err(UsageError::BadEndpoint(String::new()))
        );
    }

    #[test]
    fn duplicates_and_conflicts_are_reported() {
        assert_eq!(
            parse_strs(&["--left", "a", "--left=b"]),
            Err(UsageError::Duplicate("--left"))
        );
        assert_eq!(
            parse_strs(&["--right", "a:", "--right", "b:"]),
            Err(UsageError::Duplicate("--right"))
        );
        assert_eq!(
            parse_strs(&["web", "--right", "b:"]),
            Err(UsageError::Duplicate("--right"))
        );
        assert_eq!(
            parse_strs(&["--right", "b:", "web"]),
            Err(UsageError::Duplicate("--right"))
        );
    }

    #[test]
    fn non_utf8_local_path_is_preserved() {
        let value = OsStr::from_bytes(b"d\xffir");
        assert_eq!(
            parse_endpoint(value),
            Ok(EndpointArg::Local(PathBuf::from(value)))
        );
    }

    #[test]
    fn version_line_matches_package() {
        assert_eq!(version_line(), "scpto 0.1.0");
    }

    #[test]
    fn usage_names_both_config_variables() {
        assert!(USAGE.contains("SCPTO_CONFIG"));
        assert!(USAGE.contains("SSHTO_CONFIG"));
    }
}
