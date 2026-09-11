use crate::host::Host;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::Path;
use std::process::Output;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCall {
    pub command: String,
    pub args: Vec<String>,
    pub working_dir: Option<std::path::PathBuf>,
}

pub struct TestHost {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub command_calls: Vec<CommandCall>,
    current_dir: std::path::PathBuf,
    command_responses: VecDeque<io::Result<Output>>,
}

impl TestHost {
    pub fn new() -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: None,
            command_calls: Vec::new(),
            current_dir: std::env::current_dir().expect("tests require a current working directory"),
            command_responses: VecDeque::new(),
        }
    }

    pub fn with_commands(mut self, responses: Vec<io::Result<Output>>) -> Self {
        self.command_responses = VecDeque::from(responses);
        self
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).to_string()
    }
}

impl Host for TestHost {
    fn output(&mut self) -> impl Write {
        &mut self.stdout
    }

    fn error(&mut self) -> impl Write {
        &mut self.stderr
    }

    fn exit(&mut self, code: i32) {
        self.exit_code = Some(code);
    }

    fn current_dir(&self) -> io::Result<std::path::PathBuf> {
        Ok(self.current_dir.clone())
    }

    fn run_command(&mut self, command: &str, args: &[&str], working_dir: Option<&Path>) -> io::Result<Output> {
        self.command_calls.push(CommandCall {
            command: command.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            working_dir: working_dir.map(Path::to_path_buf),
        });
        self.command_responses
            .pop_front()
            .unwrap_or_else(|| Err(io::Error::other("no more mock command responses")))
    }
}

pub fn make_output(code: i32, stdout: &str, stderr: &str) -> Output {
    let exit_arg = format!("exit {code}");
    let status = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", &exit_arg]).status().unwrap()
    } else {
        std::process::Command::new("sh").args(["-c", &exit_arg]).status().unwrap()
    };
    Output {
        status,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

pub fn success_output(stdout: &str) -> Output {
    make_output(0, stdout, "")
}

pub fn failure_output(stderr: &str) -> Output {
    make_output(1, "", stderr)
}

pub fn test_directory(name: &str) -> std::path::PathBuf {
    use core::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::current_dir()
        .expect("tests require a current working directory")
        .join("target")
        .join("cargo-delta-tests")
        .join(format!("{name}-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&path).expect("test directory should be creatable beneath target");
    path
}
