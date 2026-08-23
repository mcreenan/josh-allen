#![forbid(unsafe_code)]

use std::process::ExitCode;

mod app;
mod package;

fn main() -> ExitCode {
    app::main()
}
