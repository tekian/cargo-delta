use core::sync::atomic::{AtomicU64, Ordering};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cargo;
use crate::error::{Error, Result};
use crate::git::CheckoutState;
use crate::host::Host;
use crate::snapshot::{Snapshot, SnapshotContext};
use crate::utils;

static NEXT_WORKTREE: AtomicU64 = AtomicU64::new(0);

pub struct SnapshotCache<'a, H> {
    host: &'a mut H,
    context: SnapshotContext<'a>,
    cache_dir: PathBuf,
}

impl<'a, H: Host> SnapshotCache<'a, H> {
    pub fn new(host: &'a mut H, context: SnapshotContext<'a>) -> Self {
        let cache_dir = context.cache_dir();
        Self { host, context, cache_dir }
    }

    pub fn current(&mut self, state: &CheckoutState) -> Result<Snapshot> {
        let path = self.cache_dir.join("current.json");
        let key = self.context.key(state, true)?;
        if let Some(snapshot) = read_cache(&path, &key) {
            let _ = writeln!(self.host.error(), "Using cached current snapshot: {}", path.display());
            return Ok(snapshot);
        }
        let metadata = cargo::complete_snapshot_metadata(
            self.host,
            Some(&self.context.metadata.workspace_root),
            self.context.metadata.clone(),
        )
        .map_err(|error| Error::Other(format!("Failed to read resolved current Cargo metadata: {error}")))?;
        let context = SnapshotContext {
            config: self.context.config,
            metadata: &metadata,
            git_root: self.context.git_root,
        };
        let snapshot = Snapshot::build(self.host, &context, state)?;
        write_cache(&path, &snapshot)?;
        Ok(snapshot)
    }

    pub fn baseline(&mut self, state: &CheckoutState) -> Result<Snapshot> {
        let cache_path = self.cache_dir.join("baseline.json");
        let expected_key = self.context.key(state, true)?;
        if let Some(snapshot) = read_cache(&cache_path, &expected_key) {
            let _ = writeln!(self.host.error(), "Using cached baseline snapshot: {}", cache_path.display());
            return Ok(snapshot);
        }

        let workspace = self.context.workspace_relative_path()?;
        let mut worktree = TemporaryWorktree::create(self.host, self.context.git_root, state.head())?;
        let workspace_dir = worktree.path.join(&workspace);
        let snapshot_result = if workspace_dir.join("Cargo.toml").is_file() {
            let metadata = cargo::snapshot_metadata(self.host, Some(&workspace_dir))
                .map_err(|error| Error::Other(format!("Failed to read resolved baseline Cargo metadata: {error}")))?;
            let context = SnapshotContext {
                config: self.context.config,
                metadata: &metadata,
                git_root: &worktree.path,
            };
            Snapshot::build(self.host, &context, state)
        } else {
            Snapshot::missing_workspace(&self.context, state)
        };
        let cleanup_result = worktree.cleanup(self.host);
        let snapshot = match (snapshot_result, cleanup_result) {
            (Ok(snapshot), Ok(())) => snapshot,
            (Err(primary), Ok(())) => return Err(primary),
            (Ok(_), Err(cleanup)) => return Err(cleanup),
            (Err(primary), Err(cleanup)) => {
                return Err(Error::Other(format!(
                    "{primary}; additionally, temporary worktree cleanup failed: {cleanup}"
                )));
            }
        };
        write_cache(&cache_path, &snapshot)?;
        Ok(snapshot)
    }
}

fn read_cache(path: &Path, expected: &crate::snapshot::SnapshotKey) -> Option<Snapshot> {
    let snapshot: Snapshot = utils::deser_json(path).ok()?;
    snapshot.matches(expected).then_some(snapshot)
}

fn write_cache(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Other(format!("Cache path '{}' has no parent", path.display())))?;
    fs::create_dir_all(parent)
        .map_err(|error| Error::Other(format!("Failed to create cache directory '{}': {error}", parent.display())))?;
    let mut bytes = serde_json::to_vec_pretty(snapshot)?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|error| Error::Other(format!("Failed to write snapshot cache '{}': {error}", path.display())))
}

struct TemporaryWorktree {
    git_root: PathBuf,
    path: PathBuf,
    active: bool,
}

impl TemporaryWorktree {
    fn create(host: &mut impl Host, git_root: &Path, commit: &str) -> Result<Self> {
        let parent = git_root
            .parent()
            .ok_or_else(|| Error::Other(format!("Git root '{}' has no parent", git_root.display())))?;
        let sequence = NEXT_WORKTREE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".cargo-delta-worktree-{}-{sequence}", std::process::id()));
        let path_arg = path
            .to_str()
            .ok_or_else(|| Error::Other(format!("Temporary worktree path '{}' is not UTF-8", path.display())))?;
        let output = host
            .run_command("git", &["worktree", "add", "--detach", "--force", path_arg, commit], Some(git_root))
            .map_err(|error| Error::Git(format!("Failed to create temporary worktree: {error}")))?;
        if !output.status.success() {
            return Err(Error::Git(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(Self {
            git_root: git_root.to_path_buf(),
            path,
            active: true,
        })
    }

    fn cleanup(&mut self, host: &mut impl Host) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let path_arg = self
            .path
            .to_str()
            .ok_or_else(|| Error::Other(format!("Temporary worktree path '{}' is not UTF-8", self.path.display())))?;
        let output = host
            .run_command("git", &["worktree", "remove", "--force", path_arg], Some(&self.git_root))
            .map_err(|error| Error::Git(format!("Failed to remove temporary worktree: {error}")))?;
        if !output.status.success() {
            return Err(Error::Git(format!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        self.active = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::process::{Command, Output};

    struct ProcessHost {
        current_dir: PathBuf,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_code: Option<i32>,
        command_calls: Vec<(String, Vec<String>)>,
    }

    impl ProcessHost {
        fn new(current_dir: PathBuf) -> Self {
            Self {
                current_dir,
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: None,
                command_calls: Vec::new(),
            }
        }

        fn stderr(&self) -> String {
            String::from_utf8_lossy(&self.stderr).into_owned()
        }

        fn worktree_adds(&self) -> usize {
            self.command_calls
                .iter()
                .filter(|(command, args)| command == "git" && args.starts_with(&["worktree".to_string(), "add".to_string()]))
                .count()
        }

        fn worktree_removes(&self) -> usize {
            self.command_calls
                .iter()
                .filter(|(command, args)| command == "git" && args.starts_with(&["worktree".to_string(), "remove".to_string()]))
                .count()
        }

        fn metadata_calls(&self) -> usize {
            self.command_calls
                .iter()
                .filter(|(_command, args)| args.first().is_some_and(|argument| argument == "metadata"))
                .count()
        }
    }

    impl Host for ProcessHost {
        fn error(&mut self) -> impl Write {
            &mut self.stderr
        }

        fn write_output(&mut self, path: Option<&Path>, contents: &[u8]) -> io::Result<()> {
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
            std::env::var_os(key)
        }

        fn run_command(&mut self, command: impl AsRef<OsStr>, args: &[&str], working_dir: Option<&Path>) -> io::Result<Output> {
            let command = command.as_ref();
            self.command_calls.push((
                command.to_string_lossy().into_owned(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
            ));
            let mut process = Command::new(command);
            let _ = process.args(args);
            if let Some(working_dir) = working_dir {
                let _ = process.current_dir(working_dir);
            }
            process.output()
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("test Git command should start");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("test Git command should start");
        assert!(output.status.success(), "git {} failed", args.join(" "));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn run_cargo(root: &Path, args: &[&str]) {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let output = Command::new(cargo)
            .args(args)
            .current_dir(root)
            .output()
            .expect("test Cargo command should start");
        assert!(
            output.status.success(),
            "cargo {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cargo-delta-impact-snapshots-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--initial-branch=main"]);
        git(&root, &["config", "user.name", "cargo-delta"]);
        git(&root, &["config", "user.email", "cargo-delta@example.com"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        git(&root, &["config", "core.safecrlf", "false"]);
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        root
    }

    fn write_workspace(root: &Path, value: u32) {
        fs::create_dir_all(root.join("lib/src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"lib\"]\nresolver = \"2\"\n").unwrap();
        fs::write(
            root.join("lib/Cargo.toml"),
            "[package]\nname = \"workspace-lib\"\nversion = \"1.2.3\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("lib/src/lib.rs"), format!("pub fn value() -> u32 {{ {value} }}\n")).unwrap();
    }

    fn write_dependency_workspace(root: &Path, external_version: &str, enable_extra: bool) {
        fs::create_dir_all(root.join("core/src")).unwrap();
        fs::create_dir_all(root.join("app/src")).unwrap();
        fs::create_dir_all(root.join("unrelated/src")).unwrap();
        fs::create_dir_all(root.join("vendor/external/src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = [\"core\", \"app\", \"unrelated\"]\nexclude = [\"vendor/external\"]\nresolver = \"2\"\n\
                 [workspace.dependencies]\nexternal = {{ path = \"vendor/external\", version = \"{external_version}\"{} }}\n",
                if enable_extra { ", features = [\"extra\"]" } else { "" }
            ),
        )
        .unwrap();
        fs::write(
            root.join("core/Cargo.toml"),
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nexternal.workspace = true\n",
        )
        .unwrap();
        fs::write(root.join("core/src/lib.rs"), "pub fn core() { external::external(); }\n").unwrap();
        fs::write(
            root.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\ncore = { path = \"../core\" }\n",
        )
        .unwrap();
        fs::write(root.join("app/src/lib.rs"), "pub fn app() { core::core(); }\n").unwrap();
        fs::write(
            root.join("unrelated/Cargo.toml"),
            "[package]\nname = \"unrelated\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("unrelated/src/lib.rs"), "pub fn unrelated() {}\n").unwrap();
        fs::write(
            root.join("vendor/external/Cargo.toml"),
            format!(
                "[package]\nname = \"external\"\nversion = \"{external_version}\"\nedition = \"2024\"\n\
                 [features]\nextra = []\n"
            ),
        )
        .unwrap();
        fs::write(root.join("vendor/external/src/lib.rs"), "pub fn external() {}\n").unwrap();
    }

    fn commit(root: &Path, message: &str) {
        git(root, &["add", "-A"]);
        git(root, &["commit", "-m", message]);
    }

    fn invoke(root: &Path, output: &Path) -> ProcessHost {
        invoke_with_current(root, output, None)
    }

    fn invoke_with_current(root: &Path, output: &Path, current: Option<&Path>) -> ProcessHost {
        let mut host = ProcessHost::new(root.to_path_buf());
        let mut args = vec![
            "cargo".to_string(),
            "delta".to_string(),
            "impact".to_string(),
            "--base-ref".to_string(),
            "baseline".to_string(),
            "--modified".to_string(),
            "--format".to_string(),
            "packages".to_string(),
            "--output".to_string(),
            output.display().to_string(),
        ];
        if let Some(current) = current {
            args.extend(["--current".to_string(), current.display().to_string()]);
        }
        crate::run(&mut host, args);
        host
    }

    fn snapshot(root: &Path, output: Option<&Path>) -> ProcessHost {
        let mut host = ProcessHost::new(root.to_path_buf());
        let mut args = vec!["cargo".to_string(), "delta".to_string(), "snapshot".to_string()];
        if let Some(output) = output {
            args.extend(["--output".to_string(), output.display().to_string()]);
        }
        crate::run(&mut host, args);
        host
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn cached_impact_reuses_baseline_and_current_until_worktree_content_changes() {
        let root = repository("cache");
        let workspace = root.join("rust");
        write_workspace(&workspace, 1);
        fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"unavailable-baseline-toolchain\"\n",
        )
        .unwrap();
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        fs::remove_file(root.join("rust-toolchain.toml")).unwrap();
        commit(&root, "remove baseline toolchain");
        write_workspace(&workspace, 2);
        let caller_workspace = workspace.join("path-alias").join("..");
        fs::create_dir_all(workspace.join("path-alias")).unwrap();
        let output = PathBuf::from("impact.packages");
        let output_file = workspace.join(&output);

        let first = invoke(&caller_workspace, &output);
        assert_eq!(first.exit_code, None, "{}", first.stderr());
        assert_eq!(fs::read_to_string(&output_file).unwrap(), "workspace-lib@1.2.3\n");
        assert_eq!(first.worktree_adds(), 1);
        assert_eq!(first.worktree_removes(), 1);
        assert_eq!(first.metadata_calls(), 2);
        let cached_current: serde_json::Value =
            serde_json::from_slice(&fs::read(workspace.join("target/cargo-delta/current.json")).unwrap()).unwrap();
        assert_eq!(
            cached_current["cache_key"]["source"]["head"],
            git_output(&root, &["rev-parse", "HEAD"])
        );
        assert_eq!(cached_current["cache_key"]["workspace"], "rust");
        let cached_baseline: serde_json::Value =
            serde_json::from_slice(&fs::read(workspace.join("target/cargo-delta/baseline.json")).unwrap()).unwrap();
        assert_eq!(
            cached_baseline["cache_key"]["source"]["head"],
            git_output(&root, &["rev-parse", "baseline^{commit}"])
        );
        assert_eq!(
            cached_baseline["cache_key"]["source"]["working_tree_sha256"],
            format!("{:x}", Sha256::digest([0]))
        );
        let stale_current = workspace.join("target/stale-current.json");
        let _copied = fs::copy(workspace.join("target/cargo-delta/current.json"), &stale_current).unwrap();

        let second = invoke(&caller_workspace, &output);
        assert_eq!(second.exit_code, None, "{}", second.stderr());
        assert!(second.stderr().contains("Using cached baseline snapshot"));
        assert!(second.stderr().contains("Using cached current snapshot"));
        assert!(!second.stderr().contains("impact.packages"));
        assert_eq!(second.worktree_adds(), 0);
        assert_eq!(second.metadata_calls(), 1);

        write_workspace(&workspace, 3);
        let stale = invoke_with_current(&caller_workspace, &output, Some(&stale_current));
        assert_eq!(stale.exit_code, None, "{}", stale.stderr());
        assert!(stale.stderr().contains("supplied current snapshot"));
        assert!(stale.stderr().contains("not up to date"));

        let third = invoke(&caller_workspace, &output);
        assert_eq!(third.exit_code, None, "{}", third.stderr());
        assert!(third.stderr().contains("Using cached baseline snapshot"));
        assert!(!third.stderr().contains("Using cached current snapshot"));
        assert_eq!(third.worktree_adds(), 0);

        fs::write(workspace.join("untracked.txt"), "one").unwrap();
        let untracked = invoke(&caller_workspace, &output);
        assert_eq!(untracked.exit_code, None, "{}", untracked.stderr());
        assert!(!untracked.stderr().contains("Using cached current snapshot"));

        fs::write(workspace.join("untracked.txt"), "two").unwrap();
        let changed_untracked = invoke(&caller_workspace, &output);
        assert_eq!(changed_untracked.exit_code, None, "{}", changed_untracked.stderr());
        assert!(!changed_untracked.stderr().contains("Using cached current snapshot"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn cached_impact_with_no_changes_writes_empty_output() {
        let root = repository("no-changes");
        write_workspace(&root, 1);
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        let output = root.join("impact.packages");

        let host = invoke(&root, &output);

        assert_eq!(host.exit_code, Some(0), "{}", host.stderr());
        assert_eq!(fs::metadata(output).unwrap().len(), 0);
        assert!(host.stderr().contains("No file has been changed or deleted"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn lock_and_workspace_dependency_changes_are_scoped_to_consumers() {
        let root = repository("dependency-inputs");
        fs::write(root.join(".delta.toml"), "trip_wire_patterns = [\"Cargo.lock\", \"Cargo.toml\"]\n").unwrap();
        write_dependency_workspace(&root, "1.0.0", false);
        run_cargo(&root, &["generate-lockfile"]);
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        write_dependency_workspace(&root, "2.0.0", false);
        run_cargo(&root, &["generate-lockfile"]);
        commit(&root, "update external dependency");

        let modified_output = root.join("target/modified.packages");
        let affected_output = root.join("target/affected.packages");
        let mut modified = ProcessHost::new(root.clone());
        crate::run(
            &mut modified,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "-c".to_string(),
                root.join(".delta.toml").display().to_string(),
                "impact".to_string(),
                "--base-ref".to_string(),
                "baseline".to_string(),
                "--modified".to_string(),
                "--format".to_string(),
                "packages".to_string(),
                "--output".to_string(),
                modified_output.display().to_string(),
            ],
        );
        let mut affected = ProcessHost::new(root.clone());
        crate::run(
            &mut affected,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "-c".to_string(),
                root.join(".delta.toml").display().to_string(),
                "impact".to_string(),
                "--base-ref".to_string(),
                "baseline".to_string(),
                "--affected".to_string(),
                "--format".to_string(),
                "packages".to_string(),
                "--output".to_string(),
                affected_output.display().to_string(),
            ],
        );

        assert_eq!(modified.exit_code, None, "{}", modified.stderr());
        assert_eq!(affected.exit_code, None, "{}", affected.stderr());
        assert_eq!(fs::read_to_string(modified_output).unwrap(), "core@0.1.0\n");
        assert_eq!(fs::read_to_string(affected_output).unwrap(), "app@0.1.0\ncore@0.1.0\n");
        assert!(!modified.stderr().contains("Trip wire activated"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn workspace_dependency_feature_change_is_scoped_with_unchanged_lockfile() {
        let root = repository("dependency-features");
        fs::write(root.join(".delta.toml"), "trip_wire_patterns = [\"Cargo.toml\"]\n").unwrap();
        write_dependency_workspace(&root, "1.0.0", false);
        run_cargo(&root, &["generate-lockfile"]);
        let baseline_lock = fs::read(root.join("Cargo.lock")).unwrap();
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        write_dependency_workspace(&root, "1.0.0", true);
        run_cargo(&root, &["generate-lockfile"]);
        assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), baseline_lock);
        commit(&root, "enable external feature");
        let output = root.join("target/modified.packages");
        let mut host = ProcessHost::new(root.clone());

        crate::run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "-c".to_string(),
                root.join(".delta.toml").display().to_string(),
                "impact".to_string(),
                "--base-ref".to_string(),
                "baseline".to_string(),
                "--modified".to_string(),
                "--format".to_string(),
                "packages".to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, None, "{}", host.stderr());
        assert_eq!(fs::read_to_string(output).unwrap(), "core@0.1.0\n");
        assert!(!host.stderr().contains("Trip wire activated"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn missing_baseline_workspace_widens_to_every_current_package() {
        let root = repository("missing-workspace");
        fs::write(root.join("README.md"), "before workspace\n").unwrap();
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        let workspace = root.join("rust");
        write_workspace(&workspace, 1);
        commit(&root, "add workspace");
        let output = workspace.join("target/impact.packages");

        let host = invoke(&workspace, &output);

        assert_eq!(host.exit_code, None, "{}", host.stderr());
        assert_eq!(fs::read_to_string(output).unwrap(), "workspace-lib@1.2.3\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn explicit_baseline_allows_current_snapshot_generation() {
        let root = repository("explicit-baseline");
        write_workspace(&root, 1);
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        write_workspace(&root, 2);
        commit(&root, "current");
        let output = root.join("target/impact.packages");
        let initial = invoke(&root, &output);
        assert_eq!(initial.exit_code, None, "{}", initial.stderr());

        let baseline = root.join("target/cargo-delta/baseline.json");
        fs::remove_file(root.join("target/cargo-delta/current.json")).unwrap();
        let config = root.join("delta.toml");
        fs::write(&config, "[git]\nremote_branch = \"HEAD\"\n").unwrap();
        let mut host = ProcessHost::new(root.clone());
        crate::run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "-c".to_string(),
                config.display().to_string(),
                "impact".to_string(),
                "--baseline".to_string(),
                baseline.display().to_string(),
                "--modified".to_string(),
                "--format".to_string(),
                "packages".to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, None, "{}", host.stderr());
        assert_eq!(host.worktree_adds(), 0);
        assert!(root.join("target/cargo-delta/current.json").is_file());
        assert!(host.stderr().contains("supplied baseline snapshot"));
        assert!(host.stderr().contains("not up to date"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn explicit_snapshots_without_cache_keys_are_rejected() {
        let root = repository("mixed-explicit-cache-keys");
        write_workspace(&root, 1);
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        write_workspace(&root, 2);
        commit(&root, "current");
        let output = root.join("target/impact.packages");
        let initial = invoke(&root, &output);
        assert_eq!(initial.exit_code, None, "{}", initial.stderr());

        let baseline = root.join("target/cargo-delta/baseline.json");
        let current = root.join("target/cargo-delta/current.json");
        let original_baseline = fs::read(&baseline).unwrap();
        let original_current = fs::read(&current).unwrap();
        let mut current_json: serde_json::Value = serde_json::from_slice(&fs::read(&current).unwrap()).unwrap();
        current_json["cache_key"] = serde_json::Value::Null;
        fs::write(&current, serde_json::to_vec_pretty(&current_json).unwrap()).unwrap();

        let mut host = ProcessHost::new(root.clone());
        crate::run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "impact".to_string(),
                "--baseline".to_string(),
                baseline.display().to_string(),
                "--current".to_string(),
                current.display().to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr().contains("current snapshot"));
        assert!(host.stderr().contains("has no cache key"));

        fs::write(&current, original_current).unwrap();
        let mut baseline_json: serde_json::Value = serde_json::from_slice(&original_baseline).unwrap();
        baseline_json["cache_key"] = serde_json::Value::Null;
        fs::write(&baseline, serde_json::to_vec_pretty(&baseline_json).unwrap()).unwrap();
        let mut host = ProcessHost::new(root.clone());
        crate::run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "impact".to_string(),
                "--baseline".to_string(),
                baseline.display().to_string(),
                "--current".to_string(),
                current.display().to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr().contains("baseline snapshot"));
        assert!(host.stderr().contains("has no cache key"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn explicitly_generated_snapshots_are_reused_as_cache_entries() {
        let root = repository("explicit-cache-generation");
        write_workspace(&root, 1);
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        let cache_dir = root.join("target/cargo-delta");
        let baseline = cache_dir.join("baseline.json");

        let baseline_host = snapshot(&root, Some(&baseline));
        assert_eq!(baseline_host.exit_code, None, "{}", baseline_host.stderr());

        write_workspace(&root, 2);
        commit(&root, "current");
        let current = cache_dir.join("current.json");
        let current_host = snapshot(&root, Some(&current));
        assert_eq!(current_host.exit_code, None, "{}", current_host.stderr());

        let output = root.join("target/impact.packages");
        let impact_host = invoke(&root, &output);
        assert_eq!(impact_host.exit_code, None, "{}", impact_host.stderr());
        assert!(impact_host.stderr().contains("Using cached baseline snapshot"));
        assert!(impact_host.stderr().contains("Using cached current snapshot"));
        assert_eq!(impact_host.worktree_adds(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn explicit_baseline_with_working_tree_changes_is_rejected() {
        let root = repository("dirty-explicit-baseline");
        write_workspace(&root, 1);
        commit(&root, "baseline");
        write_workspace(&root, 2);
        let baseline = root.join("target/dirty-baseline.json");
        let snapshot_host = snapshot(&root, Some(&baseline));
        assert_eq!(snapshot_host.exit_code, None, "{}", snapshot_host.stderr());

        let output = root.join("target/impact.packages");
        let mut impact_host = ProcessHost::new(root.clone());
        crate::run(
            &mut impact_host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "impact".to_string(),
                "--baseline".to_string(),
                baseline.display().to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert_eq!(impact_host.exit_code, Some(1));
        assert!(impact_host.stderr().contains("contains working-tree changes"));
        assert!(!output.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn redirected_snapshot_is_reused_from_current_cache_path() {
        let root = repository("redirected-current-cache");
        fs::write(root.join(".gitignore"), "").unwrap();
        write_workspace(&root, 1);
        commit(&root, "baseline");
        git(&root, &["tag", "baseline"]);
        write_workspace(&root, 2);
        commit(&root, "current");

        let current = root.join("target/cargo-delta/current.json");
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, "").unwrap();
        let snapshot_host = snapshot(&root, None);
        assert_eq!(snapshot_host.exit_code, None, "{}", snapshot_host.stderr());
        fs::write(&current, snapshot_host.stdout).unwrap();

        let output = root.join("target/impact.packages");
        let impact_host = invoke(&root, &output);
        assert_eq!(impact_host.exit_code, None, "{}", impact_host.stderr());
        assert!(impact_host.stderr().contains("Using cached current snapshot"));
        fs::remove_dir_all(root).unwrap();
    }
}
