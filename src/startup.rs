//! Process startup: parses the command line and dispatches to the requested
//! mode. Owned by the integration owner (not a frozen contract); the TUI
//! start is wired here in Wave 3.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use crate::cli;

/// Runs scpto with the arguments after the program name and returns the
/// exit code.
pub fn main<I>(args: I) -> anyhow::Result<ExitCode>
where
    I: IntoIterator<Item = OsString>,
{
    match cli::parse(args) {
        Ok(cli::Invocation::Version) => {
            writeln!(std::io::stdout(), "{}", cli::version_line())?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(cli::Invocation::Help) => {
            write!(std::io::stdout(), "{}", cli::USAGE)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            write!(std::io::stderr(), "scpto: {error}\n\n{}", cli::USAGE)?;
            Ok(ExitCode::from(cli::USAGE_EXIT_CODE))
        }
    }
}
