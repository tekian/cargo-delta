use normpath::PathExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::GitConfig;
use crate::error::{Error, Result};
use crate::host::Host;

#[derive(Debug, Clone)]
pub struct GitDiff {
    pub changed: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct GitComparison {
    pub base: CheckoutState,
    pub current: CheckoutState,
    pub diff: GitDiff,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutState {
    head: String,
    working_tree_sha256: String,
}

impl CheckoutState {
    pub fn clean(head: &str) -> Self {
        Self {
            head: head.to_string(),
            working_tree_sha256: clean_working_tree_digest(),
        }
    }

    pub fn head(&self) -> &str {
        &self.head
    }

    pub fn is_clean(&self) -> bool {
        self.working_tree_sha256 == clean_working_tree_digest()
    }
}

enum GitBranch<'a> {
    Feature(Cow<'a, str>),
    Main(&'static str),
}

impl GitBranch<'_> {
    fn as_str(&self) -> &str {
        match self {
            GitBranch::Feature(b) => b,
            GitBranch::Main(b) => b,
        }
    }
}

pub fn compare(
    host: &mut impl Host,
    workspace_path: &Path,
    config: Option<&GitConfig>,
    base_ref: Option<&str>,
    excluded_paths: &[PathBuf],
) -> Result<GitComparison> {
    let remote_branch = if let Some(base_ref) = base_ref {
        GitBranch::Feature(Cow::Borrowed(base_ref))
    } else if let Some(b) = config.and_then(|d| d.remote_branch.as_deref()) {
        GitBranch::Feature(Cow::Borrowed(b))
    } else {
        let main_branch = best_effort_main_branch(host, workspace_path)?;
        let _ = writeln!(
            host.error(),
            "No remote branch specified, using {main_branch} as base remote branch"
        );
        GitBranch::Main(main_branch)
    };

    let merge_base_output = host
        .run_command("git", &["merge-base", "HEAD", remote_branch.as_str()], Some(workspace_path))
        .map_err(|e| Error::Git(format!("Failed to run git merge-base: {e}")))?;

    if !merge_base_output.status.success() {
        let stderr = String::from_utf8_lossy(&merge_base_output.stderr);
        return Err(Error::Git(format!("git merge-base failed: {stderr}")));
    }

    let merge_base = String::from_utf8(merge_base_output.stdout)
        .map_err(|e| Error::Git(format!("Invalid UTF-8 in git merge-base output: {e}")))?
        .trim()
        .to_string();

    compare_from_commit(host, workspace_path, &merge_base, excluded_paths)
}

pub fn compare_from_commit(
    host: &mut impl Host,
    workspace_path: &Path,
    base_commit: &str,
    excluded_paths: &[PathBuf],
) -> Result<GitComparison> {
    let (current, untracked) = inspect_checkout(host, workspace_path, excluded_paths)?;
    Ok(GitComparison {
        base: CheckoutState::clean(base_commit),
        current,
        diff: working_tree_diff(host, workspace_path, base_commit, excluded_paths, untracked)?,
    })
}

fn working_tree_diff(
    host: &mut impl Host,
    workspace_path: &Path,
    base_commit: &str,
    excluded_paths: &[PathBuf],
    untracked: Vec<PathBuf>,
) -> Result<GitDiff> {
    let diff_output = host
        .run_command(
            "git",
            &["diff", "--name-status", "--no-renames", "-z", base_commit, "--"],
            Some(workspace_path),
        )
        .map_err(|error| Error::Git(format!("Failed to run working-tree git diff: {error}")))?;
    if !diff_output.status.success() {
        return Err(Error::Git(format!(
            "working-tree git diff failed: {}",
            String::from_utf8_lossy(&diff_output.stderr)
        )));
    }

    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    let mut fields = diff_output.stdout.split(|byte| *byte == 0).filter(|field| !field.is_empty());
    while let Some(status) = fields.next() {
        let path = fields
            .next()
            .ok_or_else(|| Error::Git("git diff returned a status without a path".to_string()))?;
        let path = PathBuf::from(
            String::from_utf8(path.to_vec()).map_err(|error| Error::Git(format!("Invalid UTF-8 in git diff path: {error}")))?,
        );
        if is_excluded(workspace_path, &path, excluded_paths) {
            continue;
        }
        if status.first() == Some(&b'D') {
            deleted.push(path);
        } else {
            changed.push(path);
        }
    }

    for path in untracked {
        if !is_excluded(workspace_path, &path, excluded_paths) {
            changed.push(path);
        }
    }
    changed.sort();
    changed.dedup();
    deleted.sort();
    deleted.dedup();
    Ok(GitDiff { changed, deleted })
}

pub fn checkout_state(host: &mut impl Host, git_root: &Path, excluded_paths: &[PathBuf]) -> Result<CheckoutState> {
    inspect_checkout(host, git_root, excluded_paths).map(|(state, _untracked)| state)
}

pub fn file_at(host: &mut impl Host, git_root: &Path, commit: &str, path: &Path) -> Result<Option<Vec<u8>>> {
    let path = path
        .to_str()
        .ok_or_else(|| Error::Git(format!("Git path '{}' is not UTF-8", path.display())))?
        .replace('\\', "/");
    let listing = host
        .run_command("git", &["ls-tree", "--name-only", "-z", commit, "--", &path], Some(git_root))
        .map_err(|error| Error::Git(format!("Failed to inspect baseline path '{path}': {error}")))?;
    if !listing.status.success() {
        return Err(Error::Git(format!(
            "git ls-tree failed for '{path}': {}",
            String::from_utf8_lossy(&listing.stderr)
        )));
    }
    if listing.stdout.is_empty() {
        return Ok(None);
    }
    let object = format!("{commit}:{path}");
    let contents = host
        .run_command("git", &["show", &object], Some(git_root))
        .map_err(|error| Error::Git(format!("Failed to read baseline path '{path}': {error}")))?;
    if !contents.status.success() {
        return Err(Error::Git(format!(
            "git show failed for '{path}': {}",
            String::from_utf8_lossy(&contents.stderr)
        )));
    }
    Ok(Some(contents.stdout))
}

fn inspect_checkout(host: &mut impl Host, git_root: &Path, excluded_paths: &[PathBuf]) -> Result<(CheckoutState, Vec<PathBuf>)> {
    let head = git_stdout(host, git_root, &["rev-parse", "--verify", "HEAD^{commit}"], "resolve HEAD")?;
    let excluded = excluded_pathspecs(git_root, excluded_paths);
    let mut diff_args = vec![
        "diff".to_string(),
        "--binary".to_string(),
        "HEAD".to_string(),
        "--".to_string(),
        ".".to_string(),
    ];
    diff_args.extend(excluded.iter().cloned());
    let diff_args = diff_args.iter().map(String::as_str).collect::<Vec<_>>();
    let diff = host
        .run_command("git", &diff_args, Some(git_root))
        .map_err(|error| Error::Git(format!("Failed to fingerprint tracked changes: {error}")))?;
    if !diff.status.success() {
        return Err(Error::Git(format!(
            "git diff for working-tree fingerprint failed: {}",
            String::from_utf8_lossy(&diff.stderr)
        )));
    }
    let mut untracked_args = vec![
        "ls-files".to_string(),
        "--others".to_string(),
        "--exclude-standard".to_string(),
        "-z".to_string(),
        "--".to_string(),
        ".".to_string(),
    ];
    untracked_args.extend(excluded);
    let untracked_args = untracked_args.iter().map(String::as_str).collect::<Vec<_>>();
    let untracked = host
        .run_command("git", &untracked_args, Some(git_root))
        .map_err(|error| Error::Git(format!("Failed to fingerprint untracked files: {error}")))?;
    if !untracked.status.success() {
        return Err(Error::Git(format!(
            "git ls-files for working-tree fingerprint failed: {}",
            String::from_utf8_lossy(&untracked.stderr)
        )));
    }

    let mut hasher = Sha256::new();
    hasher.update(&diff.stdout);
    hasher.update([0]);
    let mut untracked_paths = Vec::new();
    for path in untracked.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        hasher.update(path);
        hasher.update([0]);
        let path_text =
            String::from_utf8(path.to_vec()).map_err(|error| Error::Git(format!("Invalid UTF-8 in untracked path: {error}")))?;
        untracked_paths.push(PathBuf::from(&path_text));
        let contents = fs::read(git_root.join(path_text))
            .map_err(|error| Error::Git(format!("Failed to read untracked file for fingerprint: {error}")))?;
        hasher.update(u64::try_from(contents.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(contents);
    }
    Ok((
        CheckoutState {
            head,
            working_tree_sha256: format!("{:x}", hasher.finalize()),
        },
        untracked_paths,
    ))
}

fn clean_working_tree_digest() -> String {
    format!("{:x}", Sha256::digest([0]))
}

fn excluded_pathspecs(git_root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let resolved_root = resolve_path(git_root);
    paths
        .iter()
        .filter_map(|path| {
            let absolute = if path.is_absolute() { path.clone() } else { git_root.join(path) };
            resolve_path(&absolute)
                .strip_prefix(&resolved_root)
                .ok()
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(|relative| format!(":(exclude){}", relative.to_string_lossy().replace('\\', "/")))
        })
        .collect()
}

fn is_excluded(git_root: &Path, relative: &Path, excluded_paths: &[PathBuf]) -> bool {
    let candidate = resolve_path(&git_root.join(relative));
    excluded_paths.iter().any(|excluded| {
        let excluded = if excluded.is_absolute() {
            excluded.clone()
        } else {
            git_root.join(excluded)
        };
        let excluded = resolve_path(&excluded);
        candidate == excluded || candidate.starts_with(excluded)
    })
}

fn resolve_path(path: &Path) -> PathBuf {
    let mut unresolved = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(mut resolved) = fs::canonicalize(existing) {
            for component in unresolved.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        let Some(component) = existing.file_name() else {
            return path.to_path_buf();
        };
        unresolved.push(component.to_os_string());
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        existing = parent;
    }
}

fn git_stdout(host: &mut impl Host, git_root: &Path, args: &[&str], operation: &str) -> Result<String> {
    let output = host
        .run_command("git", args, Some(git_root))
        .map_err(|error| Error::Git(format!("Failed to {operation}: {error}")))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git failed to {operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(|error| Error::Git(format!("Invalid UTF-8 while trying to {operation}: {error}")))?
        .trim()
        .to_string())
}

pub fn get_top_level(host: &mut impl Host, working_dir: Option<&Path>) -> Result<PathBuf> {
    let output = host
        .run_command("git", &["rev-parse", "--show-toplevel"], working_dir)
        .map_err(|e| Error::Git(format!("Failed to run git rev-parse --show-toplevel: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!("git rev-parse --show-toplevel failed: {stderr}")));
    }

    let git_root = String::from_utf8(output.stdout)
        .map_err(|e| Error::Git(format!("Invalid UTF-8 in git rev-parse output: {e}")))?
        .trim()
        .to_string();

    let git_root_path = PathBuf::from(git_root);

    let normalized_path = git_root_path
        .normalize()
        .map(normpath::BasePathBuf::into_path_buf)
        .unwrap_or(git_root_path);

    Ok(normalized_path)
}

fn best_effort_main_branch(host: &mut impl Host, workspace_path: &Path) -> Result<&'static str> {
    let candidates = ["origin/master", "origin/main", "origin/trunk"];

    for remote_name in &candidates {
        let branch_name = remote_name.trim_start_matches("origin/");

        let output = host
            .run_command("git", &["ls-remote", "--heads", "origin", branch_name], Some(workspace_path))
            .map_err(|e| Error::Git(format!("Failed to run git ls-remote: {e}")))?;

        // `git ls-remote` always exits with status 0 irrespective of branch existence
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                return Ok(remote_name);
            }
        }
    }

    // If no common main branch is found, default to 'origin/master' (best effort)
    Ok("origin/master")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    #[cfg_attr(miri, ignore)]
    fn get_top_level_returns_path_on_success() {
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("/repo/root\n"))]);

        let result = get_top_level(&mut host, None);
        let _ = result.unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn get_top_level_returns_error_on_nonzero_exit() {
        let mut host = TestHost::new().with_commands(vec![Ok(failure_output("fatal: not a git repository"))]);

        let result = get_top_level(&mut host, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not a git repository"));
    }

    #[test]
    fn get_top_level_returns_error_on_io_failure() {
        let mut host = TestHost::new().with_commands(vec![Err(std::io::Error::new(std::io::ErrorKind::NotFound, "git not found"))]);

        let result = get_top_level(&mut host, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("git not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn file_at_reads_existing_path() {
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("Cargo.toml\0")), Ok(success_output("manifest"))]);

        let contents = file_at(&mut host, Path::new("/repo"), "base", Path::new("Cargo.toml")).unwrap();

        assert_eq!(contents, Some(b"manifest".to_vec()));
        assert_eq!(
            host.command_calls[0].1,
            ["ls-tree", "--name-only", "-z", "base", "--", "Cargo.toml"]
        );
        assert_eq!(host.command_calls[1].1, ["show", "base:Cargo.toml"]);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn file_at_returns_none_for_missing_path() {
        let mut host = TestHost::new().with_commands(vec![Ok(success_output(""))]);

        let contents = file_at(&mut host, Path::new("/repo"), "base", Path::new("Cargo.toml")).unwrap();

        assert!(contents.is_none());
        assert_eq!(host.command_calls.len(), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn best_effort_finds_master() {
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("abc123\trefs/heads/master\n"))]);

        let result = best_effort_main_branch(&mut host, Path::new("/fake")).unwrap();
        assert_eq!(result, "origin/master");
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn best_effort_finds_main_when_no_master() {
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("")),                          // master not found
            Ok(success_output("abc123\trefs/heads/main\n")), // main found
        ]);

        let result = best_effort_main_branch(&mut host, Path::new("/fake")).unwrap();
        assert_eq!(result, "origin/main");
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn best_effort_defaults_when_none_found() {
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("")), // master not found
            Ok(success_output("")), // main not found
            Ok(success_output("")), // trunk not found
        ]);

        let result = best_effort_main_branch(&mut host, Path::new("/fake")).unwrap();
        assert_eq!(result, "origin/master");
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn diff_with_configured_branch() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_diff_configured");
        let _ = fs::create_dir_all(&tmp);

        // Create a file so it shows as "changed" (exists on disk)
        let src_dir = tmp.join("src");
        let _ = fs::create_dir_all(&src_dir);
        fs::write(src_dir.join("lib.rs"), "fn main() {}").unwrap();

        let git_config = GitConfig {
            remote_branch: Some("origin/feature".to_string()),
        };

        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("abc123\n")),        // merge-base
            Ok(success_output("head\n")),          // HEAD
            Ok(success_output("")),                // working-tree digest
            Ok(success_output("")),                // untracked files
            Ok(success_output("M\0src/lib.rs\0")), // changed files
        ]);

        let result = compare(&mut host, &tmp, Some(&git_config), None, &[]).unwrap().diff;

        assert_eq!(result.changed.len(), 1);
        assert!(result.deleted.is_empty());
        // No "No remote branch" message since branch was configured
        assert!(!host.stderr_str().contains("No remote branch"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn diff_with_explicit_base_ref_skips_discovery() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_diff_base_ref");
        let _ = fs::create_dir_all(&tmp);
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("abc123\n")),
            Ok(success_output("head\n")),
            Ok(success_output("")),
            Ok(success_output("")),
            Ok(success_output("")),
        ]);

        let result = compare(&mut host, &tmp, None, Some("origin/explicit"), &[]).unwrap().diff;

        assert!(result.changed.is_empty());
        assert!(!host.stderr_str().contains("No remote branch specified"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn working_tree_comparison_includes_tracked_deleted_and_untracked_paths() {
        let root = std::env::temp_dir().join(format!("cargo-delta-git-comparison-{}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/new.rs"), "new").unwrap();
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("merge-base\n")),
            Ok(success_output("head\n")),
            Ok(success_output("tracked diff")),
            Ok(success_output("src/new.rs\0")),
            Ok(success_output("M\0src/lib.rs\0D\0src/old.rs\0")),
        ]);

        let comparison = compare(&mut host, &root, None, Some("origin/main"), &[]).unwrap();

        assert_eq!(comparison.base.head(), "merge-base");
        assert_eq!(comparison.current.head(), "head");
        assert_eq!(comparison.diff.changed, [PathBuf::from("src/lib.rs"), PathBuf::from("src/new.rs")]);
        assert_eq!(comparison.diff.deleted, [PathBuf::from("src/old.rs")]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn working_tree_comparison_excludes_requested_subtrees() {
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("merge-base\n")),
            Ok(success_output("head\n")),
            Ok(success_output("")),
            Ok(success_output("")),
            Ok(success_output("M\0generated/cache.json\0M\0src/lib.rs\0M\0src/new.rs\0")),
        ]);

        let comparison = compare(
            &mut host,
            Path::new("/repo"),
            None,
            Some("origin/main"),
            &[PathBuf::from("/repo/generated")],
        )
        .unwrap();

        assert_eq!(comparison.diff.changed, [PathBuf::from("src/lib.rs"), PathBuf::from("src/new.rs")]);
    }

    #[test]
    fn exclusions_resolve_equivalent_paths_with_missing_leaf() {
        let root = std::env::temp_dir().join(format!("cargo-delta-exclusion-path-{}", std::process::id()));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let spelled_root = nested.join("..");
        let output = root.join("output.txt");

        assert_eq!(
            excluded_pathspecs(&spelled_root, core::slice::from_ref(&output)),
            [":(exclude)output.txt"]
        );
        assert!(is_excluded(&spelled_root, Path::new("output.txt"), &[output]));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn diff_merge_base_failure() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_diff_fail");
        let _ = fs::create_dir_all(&tmp);

        let git_config = GitConfig {
            remote_branch: Some("origin/feature".to_string()),
        };

        let mut host = TestHost::new().with_commands(vec![Ok(failure_output("fatal: not a valid commit"))]);

        let result = compare(&mut host, &tmp, Some(&git_config), None, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("merge-base"));

        let _ = fs::remove_dir_all(&tmp);
    }
}
