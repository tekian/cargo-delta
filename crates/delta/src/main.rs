#![doc(hidden)]

//! A cargo tool to detect impacted packages from git changes.

use cargo_delta_lib::Host;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write, stderr, stdout};
use std::path::{Path, PathBuf};
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

    fn current_dir(&self) -> io::Result<PathBuf> {
        std::env::current_dir()
    }

    fn env_var_os(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }

    fn run_command(&mut self, command: impl AsRef<OsStr>, args: &[&str], working_dir: Option<&Path>) -> io::Result<Output> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_host_reports_process_context() {
        let host = RealHost;
        assert_eq!(host.current_dir().unwrap(), std::env::current_dir().unwrap());
        assert_eq!(host.env_var_os("PATH"), std::env::var_os("PATH"));
    }
}
