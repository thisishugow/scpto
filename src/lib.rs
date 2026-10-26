//! scpto: a dual-pane terminal file transfer tool over SFTP.
//!
//! The binary in `main.rs` only calls [`run`], which never changes; startup
//! wiring lives in [`startup`]. Everything else lives in this library so
//! integration tests can use the same API.

#![warn(missing_docs)]

#[cfg(not(unix))]
compile_error!("scpto supports macOS and Linux only");

pub mod cli;
pub mod connections;
pub mod edit;
pub mod error;
pub mod groups;
pub mod hosts;
pub mod pattern;
pub mod preview;
pub mod session;
pub mod settings;
pub mod startup;
pub mod state;
pub mod sync;
pub mod term;
pub mod transfer;
pub mod ui;
pub mod vfs;

use std::process::ExitCode;

/// Runs scpto with the process arguments and returns the exit code.
pub fn run() -> anyhow::Result<ExitCode> {
    startup::main(std::env::args_os().skip(1))
}
