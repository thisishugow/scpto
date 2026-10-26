//! scpto binary entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    match scpto::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("scpto: {error:#}");
            ExitCode::FAILURE
        }
    }
}
