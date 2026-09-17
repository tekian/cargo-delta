#![doc(hidden)]

//! A cargo tool to detect impacted packages from git changes.

use cargo_delta_lib::Host;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write, stderr, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Default host that runs real OS commands.
#[derive(Debug, Clone, Default)]
pub struct RealHost;

impl Host for RealHost {
    fn error(&mut self) -> impl Write {
        stderr()
    }

    fn write_output(&mut self, path: Option<&Path>, contents: &[u8]) -> io::Result<()> {
        let Some(path) = path else {
            return stdout().write_all(contents);
        };
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.current_dir()?.join(path)
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)
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

    #[test]
    fn real_host_writes_output_file() {
        let root = std::env::temp_dir().join(format!("cargo-delta-real-host-output-{}", std::process::id()));
        let path = root.join("nested/output.txt");
        let mut host = RealHost;

        host.write_output(Some(&path), b"output").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "output");
        fs::remove_dir_all(root).unwrap();
    }
}
