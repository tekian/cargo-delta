#![doc(hidden)]

//! A cargo tool to detect impacted packages from git changes.

use cargo_delta_lib::Host;
use std::io::{self, Write, stderr, stdout};
use std::path::Path;
use std::process::{Command, Output};

/// Default host that runs real OS commands.
#[derive(Debug, Clone, Default)]
pub struct RealHost;

impl Host for RealHost {
    fn output(&mut self) -> impl Write {
        stdout()
    }

    fn error(&mut self) -> impl Write {
        stderr()
    }

    fn exit(&mut self, code: i32) {
        std::process::exit(code);
    }

    fn run_command(&mut self, command: &str, args: &[&str], working_dir: Option<&Path>) -> io::Result<Output> {
        let mut cmd = Command::new(command);
        let _ = cmd.args(args);
        if let Some(dir) = working_dir {
            let _ = cmd.current_dir(dir);
        }
        cmd.output()
    }
}

fn main() {
    cargo_delta_lib::run(&mut RealHost, std::env::args());
}
