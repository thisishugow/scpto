//! Command-line parsing.
//!
//! This build accepts `--help` / `-h` and `--version` / `-V`; any other
//! argument (or none) is a usage error reported on stderr with exit code
//! [`USAGE_EXIT_CODE`]. Endpoint arguments (`scpto [host[:path]]`,
//! `--left <endpoint>`, `--right <endpoint>`) are added as new
//! [`Invocation`] variants.

use std::ffi::OsString;

/// Exit code for invalid command lines.
pub const USAGE_EXIT_CODE: u8 = 2;

/// Usage text printed by `--help` and after usage errors.
pub const USAGE: &str = "\
Usage: scpto [--help | --version]

Dual-pane terminal file transfer over SFTP.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit

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
}

/// A command line that could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UsageError {
    /// No arguments were given.
    #[error("missing option")]
    Missing,
    /// An argument was not recognized.
    #[error("unrecognized argument '{0}'")]
    Unrecognized(String),
    /// More than one argument was given.
    #[error("unexpected extra argument '{0}'")]
    Extra(String),
}

/// The `--version` output line.
pub fn version_line() -> String {
    format!("scpto {}", env!("CARGO_PKG_VERSION"))
}

/// Parses the arguments after the program name.
pub fn parse<I>(args: I) -> Result<Invocation, UsageError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let first = args.next().ok_or(UsageError::Missing)?;
    let invocation = match first.to_str() {
        Some("-h" | "--help") => Invocation::Help,
        Some("-V" | "--version") => Invocation::Version,
        _ => {
            return Err(UsageError::Unrecognized(
                first.to_string_lossy().into_owned(),
            ))
        }
    };
    match args.next() {
        Some(extra) => Err(UsageError::Extra(extra.to_string_lossy().into_owned())),
        None => Ok(invocation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Invocation, UsageError> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn recognizes_help_and_version() {
        assert_eq!(parse_strs(&["--help"]), Ok(Invocation::Help));
        assert_eq!(parse_strs(&["-h"]), Ok(Invocation::Help));
        assert_eq!(parse_strs(&["--version"]), Ok(Invocation::Version));
        assert_eq!(parse_strs(&["-V"]), Ok(Invocation::Version));
    }

    #[test]
    fn rejects_everything_else() {
        assert_eq!(parse_strs(&[]), Err(UsageError::Missing));
        assert_eq!(
            parse_strs(&["--bogus"]),
            Err(UsageError::Unrecognized("--bogus".into()))
        );
        assert_eq!(
            parse_strs(&["--help", "x"]),
            Err(UsageError::Extra("x".into()))
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
