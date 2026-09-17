use crate::host::Host;
use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Output;

pub struct TestHost {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub command_calls: Vec<(OsString, Vec<String>, Option<PathBuf>)>,
    command_responses: VecDeque<io::Result<Output>>,
    current_dir: PathBuf,
    env_vars: HashMap<String, OsString>,
    output_error: Option<String>,
}

impl TestHost {
    pub fn new() -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: None,
            command_calls: Vec::new(),
            command_responses: VecDeque::new(),
            current_dir: std::env::current_dir().expect("tests require a current directory"),
            env_vars: HashMap::new(),
            output_error: None,
        }
    }

    pub fn with_commands(mut self, responses: Vec<io::Result<Output>>) -> Self {
        self.command_responses = VecDeque::from(responses);
        self
    }

    pub fn with_env_var(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        let _ = self.env_vars.insert(key.into(), value.into());
        self
    }

    pub fn with_output_error(mut self, message: impl Into<String>) -> Self {
        self.output_error = Some(message.into());
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
    fn error(&mut self) -> impl Write {
        &mut self.stderr
    }

    fn write_output(&mut self, path: Option<&Path>, contents: &[u8]) -> io::Result<()> {
        if let Some(message) = &self.output_error {
            return Err(io::Error::other(message.clone()));
        }
        let Some(path) = path else {
            return self.stdout.write_all(contents);
        };
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.current_dir.join(path)
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)
    }

    fn exit(&mut self, code: i32) {
        self.exit_code = Some(code);
    }

    fn current_dir(&self) -> io::Result<PathBuf> {
        Ok(self.current_dir.clone())
    }

    fn env_var_os(&self, key: &str) -> Option<OsString> {
        self.env_vars.get(key).cloned()
    }

    fn run_command(&mut self, command: impl AsRef<OsStr>, args: &[&str], working_dir: Option<&Path>) -> io::Result<Output> {
        self.command_calls.push((
            command.as_ref().to_os_string(),
            args.iter().map(|arg| (*arg).to_string()).collect(),
            working_dir.map(Path::to_path_buf),
        ));
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
