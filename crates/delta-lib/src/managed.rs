use core::sync::atomic::{AtomicU64, Ordering};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cargo::CargoMetadata;
use crate::config::MainConfig;
use crate::error::{Error, Result};
use crate::files::{FileKind, FileNode};
use crate::git::GitComparison;
use crate::host::Host;
use crate::{WorkspaceTree, build_snapshot};

const CACHE_VERSION: u32 = 1;
static NEXT_WORKTREE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCacheKey {
    cache_version: u32,
    cargo_delta_version: String,
    workspace: PathBuf,
    config_sha256: String,
    source: SnapshotSource,
    workspace_present: bool,
}

impl SnapshotCacheKey {
    fn matches_identity(&self, other: &Self) -> bool {
        self.cache_version == other.cache_version
            && self.cargo_delta_version == other.cargo_delta_version
            && self.workspace == other.workspace
            && self.config_sha256 == other.config_sha256
            && self.source == other.source
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotSource {
    head: String,
    working_tree_sha256: String,
}

pub struct ManagedImpact {
    pub baseline: WorkspaceTree,
    pub current: WorkspaceTree,
    pub diff: crate::git::GitDiff,
    pub widen: bool,
}

pub struct ResolveContext<'a> {
    pub config_path: Option<&'a PathBuf>,
    pub comparison: GitComparison,
    pub git_root: &'a Path,
    pub current_metadata: &'a CargoMetadata,
    pub baseline_path: Option<&'a Path>,
    pub current_path: Option<&'a Path>,
    pub excluded_paths: &'a [PathBuf],
}

pub fn current_cache_key(
    host: &mut impl Host,
    metadata: &CargoMetadata,
    git_root: &Path,
    config_path: Option<&PathBuf>,
    workspace_present: bool,
    excluded_paths: &[PathBuf],
) -> Result<SnapshotCacheKey> {
    Ok(SnapshotCacheKey {
        cache_version: CACHE_VERSION,
        cargo_delta_version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: workspace_relative_path(metadata, git_root)?,
        config_sha256: config_digest(config_path)?,
        source: SnapshotSource {
            head: crate::git::head_commit(host, git_root)?,
            working_tree_sha256: crate::git::working_tree_digest(host, git_root, excluded_paths)?,
        },
        workspace_present,
    })
}

pub fn baseline_cache_key(
    metadata: &CargoMetadata,
    git_root: &Path,
    config_path: Option<&PathBuf>,
    merge_base: &str,
    workspace_present: bool,
) -> Result<SnapshotCacheKey> {
    Ok(SnapshotCacheKey {
        cache_version: CACHE_VERSION,
        cargo_delta_version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: workspace_relative_path(metadata, git_root)?,
        config_sha256: config_digest(config_path)?,
        source: SnapshotSource {
            head: merge_base.to_string(),
            working_tree_sha256: format!("{:x}", Sha256::digest([0])),
        },
        workspace_present,
    })
}

pub fn resolve(host: &mut impl Host, config: &MainConfig, context: ResolveContext<'_>) -> Result<ManagedImpact> {
    let ResolveContext {
        config_path,
        comparison,
        git_root,
        current_metadata,
        baseline_path,
        current_path,
        excluded_paths,
    } = context;
    let cache_dir = current_metadata.target_directory.join("cargo-delta");
    let mut key_exclusions = excluded_paths.to_vec();
    key_exclusions.push(cache_dir.clone());
    let current_key = current_cache_key(host, current_metadata, git_root, config_path, true, &key_exclusions)?;
    let current = match current_path {
        Some(path) => load_explicit(host, path, &current_key, "current")?,
        None => cached_or_create_current(host, config, current_metadata, git_root, &cache_dir, current_key)?,
    };

    let baseline_key = baseline_cache_key(current_metadata, git_root, config_path, &comparison.merge_base, true)?;
    let baseline = match baseline_path {
        Some(path) => load_explicit(host, path, &baseline_key, "baseline")?,
        None => cached_or_create_baseline(
            host,
            config,
            current_metadata,
            git_root,
            &cache_dir,
            &comparison.merge_base,
            baseline_key,
        )?,
    };
    let widen = baseline.cache_key.as_ref().is_some_and(|key| !key.workspace_present);
    Ok(ManagedImpact {
        baseline,
        current,
        diff: comparison.diff,
        widen,
    })
}

fn cached_or_create_current(
    host: &mut impl Host,
    config: &MainConfig,
    metadata: &CargoMetadata,
    git_root: &Path,
    cache_dir: &Path,
    key: SnapshotCacheKey,
) -> Result<WorkspaceTree> {
    let path = cache_dir.join("current.json");
    if let Some(snapshot) = read_cache(&path, &key) {
        let _ = writeln!(host.error(), "Using cached current snapshot: {}", path.display());
        return Ok(snapshot);
    }
    let snapshot = build_snapshot(host, config, metadata, git_root, Some(key));
    write_cache(&path, &snapshot)?;
    Ok(snapshot)
}

fn cached_or_create_baseline(
    host: &mut impl Host,
    config: &MainConfig,
    current_metadata: &CargoMetadata,
    git_root: &Path,
    cache_dir: &Path,
    merge_base: &str,
    expected_key: SnapshotCacheKey,
) -> Result<WorkspaceTree> {
    let cache_path = cache_dir.join("baseline.json");
    if let Some(snapshot) = read_cache(&cache_path, &expected_key) {
        let _ = writeln!(host.error(), "Using cached baseline snapshot: {}", cache_path.display());
        return Ok(snapshot);
    }

    let workspace = workspace_relative_path(current_metadata, git_root)?;
    let mut worktree = TemporaryWorktree::create(host, git_root, merge_base)?;
    let workspace_dir = worktree.path.join(&workspace);
    let snapshot_result = if workspace_dir.join("Cargo.toml").is_file() {
        let metadata = crate::cargo::metadata(host, Some(&workspace_dir))
            .map_err(|error| Error::Other(format!("Failed to read baseline Cargo metadata: {error}")))?;
        Ok(build_snapshot(host, config, &metadata, &worktree.path, Some(expected_key)))
    } else {
        let mut key = expected_key;
        key.workspace_present = false;
        Ok(WorkspaceTree {
            cache_key: Some(key),
            files: FileNode::new(workspace.join("Cargo.toml"), FileKind::Workspace),
            packages: crate::crates::Packages::default(),
        })
    };
    let cleanup_result = worktree.cleanup(host);
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

fn load_explicit(host: &mut impl Host, path: &Path, expected: &SnapshotCacheKey, label: &str) -> Result<WorkspaceTree> {
    let snapshot: WorkspaceTree = crate::utils::deser_json(path)?;
    warn_if_stale(host, path, &snapshot, expected, label);
    Ok(snapshot)
}

pub fn warn_if_stale(host: &mut impl Host, path: &Path, snapshot: &WorkspaceTree, expected: &SnapshotCacheKey, label: &str) {
    match snapshot.cache_key.as_ref() {
        Some(actual) if !actual.matches_identity(expected) => {
            let _ = writeln!(
                host.error(),
                "Warning: supplied {label} snapshot '{}' is not up to date for the current comparison",
                path.display()
            );
        }
        None => {
            let _ = writeln!(
                host.error(),
                "Warning: supplied {label} snapshot '{}' has no cache key; freshness cannot be verified",
                path.display()
            );
        }
        Some(_) => {}
    }
}

fn read_cache(path: &Path, expected: &SnapshotCacheKey) -> Option<WorkspaceTree> {
    let snapshot: WorkspaceTree = crate::utils::deser_json(path).ok()?;
    snapshot
        .cache_key
        .as_ref()
        .is_some_and(|actual| actual.matches_identity(expected))
        .then_some(snapshot)
}

fn write_cache(path: &Path, snapshot: &WorkspaceTree) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Other(format!("Cache path '{}' has no parent", path.display())))?;
    fs::create_dir_all(parent)
        .map_err(|error| Error::Other(format!("Failed to create cache directory '{}': {error}", parent.display())))?;
    let mut bytes = serde_json::to_vec_pretty(snapshot)?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|error| Error::Other(format!("Failed to write snapshot cache '{}': {error}", path.display())))
}

fn config_digest(config_path: Option<&PathBuf>) -> Result<String> {
    let contents = match config_path {
        Some(path) => fs::read(path).map_err(|error| Error::Other(format!("Failed to read config for cache key: {error}")))?,
        None => b"<defaults>".to_vec(),
    };
    Ok(format!("{:x}", Sha256::digest(contents)))
}

fn workspace_relative_path(metadata: &CargoMetadata, git_root: &Path) -> Result<PathBuf> {
    let workspace_root = fs::canonicalize(&metadata.workspace_root).map_err(|error| {
        Error::Other(format!(
            "Failed to resolve Cargo workspace '{}': {error}",
            metadata.workspace_root.display()
        ))
    })?;
    let git_root = fs::canonicalize(git_root)
        .map_err(|error| Error::Other(format!("Failed to resolve Git root '{}': {error}", git_root.display())))?;
    workspace_root.strip_prefix(&git_root).map(Path::to_path_buf).map_err(|_error| {
        Error::Other(format!(
            "Cargo workspace '{}' is outside Git root '{}'",
            metadata.workspace_root.display(),
            git_root.display()
        ))
    })
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
    }

    impl Host for ProcessHost {
        fn output(&mut self) -> impl Write {
            &mut self.stdout
        }

        fn error(&mut self) -> impl Write {
            &mut self.stderr
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

    fn repository(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cargo-delta-managed-{name}-{}", std::process::id()));
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
            "[package]\nname = \"managed-lib\"\nversion = \"1.2.3\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("lib/src/lib.rs"), format!("pub fn value() -> u32 {{ {value} }}\n")).unwrap();
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
    fn workspace_path_uses_resolved_filesystem_identity() {
        let root = std::env::temp_dir().join(format!("cargo-delta-resolved-workspace-{}", std::process::id()));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let metadata = CargoMetadata {
            packages: Vec::new(),
            workspace_root: nested.join(".."),
            target_directory: root.join("target"),
        };

        assert_eq!(workspace_relative_path(&metadata, &root).unwrap(), PathBuf::new());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn managed_impact_reuses_baseline_and_current_until_worktree_content_changes() {
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
        let output = PathBuf::from("impact.packages");
        let output_file = workspace.join(&output);

        let first = invoke(&workspace, &output);
        assert_eq!(first.exit_code, None, "{}", first.stderr());
        assert_eq!(fs::read_to_string(&output_file).unwrap(), "managed-lib@1.2.3\n");
        assert_eq!(first.worktree_adds(), 1);
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

        let second = invoke(&workspace, &output);
        assert_eq!(second.exit_code, None, "{}", second.stderr());
        assert!(second.stderr().contains("Using cached baseline snapshot"));
        assert!(second.stderr().contains("Using cached current snapshot"));
        assert!(!second.stderr().contains("impact.packages"));
        assert_eq!(second.worktree_adds(), 0);

        write_workspace(&workspace, 3);
        let stale = invoke_with_current(&workspace, &output, Some(&stale_current));
        assert_eq!(stale.exit_code, None, "{}", stale.stderr());
        assert!(stale.stderr().contains("supplied current snapshot"));
        assert!(stale.stderr().contains("not up to date"));

        let third = invoke(&workspace, &output);
        assert_eq!(third.exit_code, None, "{}", third.stderr());
        assert!(third.stderr().contains("Using cached baseline snapshot"));
        assert!(!third.stderr().contains("Using cached current snapshot"));
        assert_eq!(third.worktree_adds(), 0);

        fs::write(workspace.join("untracked.txt"), "one").unwrap();
        let untracked = invoke(&workspace, &output);
        assert_eq!(untracked.exit_code, None, "{}", untracked.stderr());
        assert!(!untracked.stderr().contains("Using cached current snapshot"));

        fs::write(workspace.join("untracked.txt"), "two").unwrap();
        let changed_untracked = invoke(&workspace, &output);
        assert_eq!(changed_untracked.exit_code, None, "{}", changed_untracked.stderr());
        assert!(!changed_untracked.stderr().contains("Using cached current snapshot"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn managed_impact_with_no_changes_writes_empty_output() {
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
        assert_eq!(fs::read_to_string(output).unwrap(), "managed-lib@1.2.3\n");
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
        fs::write(&config, "[git]\nremote_branch = \"baseline\"\n").unwrap();
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
    fn explicit_snapshot_freshness_is_checked_when_only_one_has_a_cache_key() {
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
        let mut baseline_json: serde_json::Value = serde_json::from_slice(&fs::read(&baseline).unwrap()).unwrap();
        baseline_json["cache_key"]["workspace"] = "stale".into();
        fs::write(&baseline, serde_json::to_vec_pretty(&baseline_json).unwrap()).unwrap();
        let mut current_json: serde_json::Value = serde_json::from_slice(&fs::read(&current).unwrap()).unwrap();
        current_json["cache_key"] = serde_json::Value::Null;
        fs::write(&current, serde_json::to_vec_pretty(&current_json).unwrap()).unwrap();

        let config = root.join("delta.toml");
        fs::write(&config, "[git]\nremote_branch = \"baseline\"\n").unwrap();
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
                "--current".to_string(),
                current.display().to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, None, "{}", host.stderr());
        assert!(host.stderr().contains("supplied baseline snapshot"));
        assert!(host.stderr().contains("not up to date"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn snapshots_generated_explicitly_are_reused_as_managed_cache_entries() {
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
    fn redirected_snapshot_is_reused_from_managed_current_cache_path() {
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
