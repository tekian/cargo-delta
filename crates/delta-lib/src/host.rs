use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Output;

/// Abstract the host environment to enable testing.
pub trait Host: Send + Sync {
    /// Where to send normal output (e.g., stdout).
    fn output(&mut self) -> impl Write;

    /// Where to send error output (e.g., stderr).
    fn error(&mut self) -> impl Write;

    /// Terminate the process.
    fn exit(&mut self, code: i32);

    /// Return the directory from which the tool was invoked.
    fn current_dir(&self) -> io::Result<PathBuf>;

    /// Run an external command and return its output.
    fn run_command(&mut self, command: &str, args: &[&str], working_dir: Option<&Path>) -> io::Result<Output>;
}
