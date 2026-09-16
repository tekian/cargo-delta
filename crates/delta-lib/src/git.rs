use normpath::PathExt;
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
    pub merge_base: String,
    pub diff: GitDiff,
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
    include_worktree: bool,
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

    if include_worktree {
        return Ok(GitComparison {
            diff: working_tree_diff(host, workspace_path, &merge_base, excluded_paths)?,
            merge_base,
        });
    }

    let diff_arg = format!("{merge_base}..HEAD");
    let diff_output = host
        .run_command("git", &["diff", "--name-only", &diff_arg], Some(workspace_path))
        .map_err(|e| Error::Git(format!("Failed to run git diff: {e}")))?;

    if !diff_output.status.success() {
        let stderr = String::from_utf8_lossy(&diff_output.stderr);
        return Err(Error::Git(format!("git diff failed: {stderr}")));
    }

    let diff_output_str =
        String::from_utf8(diff_output.stdout).map_err(|e| Error::Git(format!("Invalid UTF-8 in git diff output: {e}")))?;

    let all_file_paths: Vec<PathBuf> = diff_output_str
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let path = workspace_path.join(line.trim());
            path.normalize().map_or_else(|_| path.clone(), normpath::BasePathBuf::into_path_buf)
        })
        .collect();

    let changed: Vec<PathBuf> = all_file_paths
        .iter()
        .filter(|path| path.exists())
        .filter_map(|path| path.strip_prefix(workspace_path).ok().map(Path::to_path_buf))
        .collect();

    let deleted: Vec<PathBuf> = all_file_paths
        .iter()
        .filter(|path| !path.exists())
        .filter_map(|path| path.strip_prefix(workspace_path).ok().map(Path::to_path_buf))
        .collect();

    Ok(GitComparison {
        merge_base,
        diff: GitDiff { changed, deleted },
    })
}

fn working_tree_diff(host: &mut impl Host, workspace_path: &Path, merge_base: &str, excluded_paths: &[PathBuf]) -> Result<GitDiff> {
    let diff_output = host
        .run_command(
            "git",
            &["diff", "--name-status", "--no-renames", "-z", merge_base, "--"],
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

    let untracked = host
        .run_command(
            "git",
            &["ls-files", "--others", "--exclude-standard", "-z", "--"],
            Some(workspace_path),
        )
        .map_err(|error| Error::Git(format!("Failed to list untracked files: {error}")))?;
    if !untracked.status.success() {
        return Err(Error::Git(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&untracked.stderr)
        )));
    }
    for path in untracked.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        let path = PathBuf::from(
            String::from_utf8(path.to_vec()).map_err(|error| Error::Git(format!("Invalid UTF-8 in untracked path: {error}")))?,
        );
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

pub fn head_commit(host: &mut impl Host, git_root: &Path) -> Result<String> {
    git_stdout(host, git_root, &["rev-parse", "--verify", "HEAD^{commit}"], "resolve HEAD")
}

pub fn working_tree_digest(host: &mut impl Host, git_root: &Path, excluded_paths: &[PathBuf]) -> Result<String> {
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
    for path in untracked.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        hasher.update(path);
        hasher.update([0]);
        let path_text =
            String::from_utf8(path.to_vec()).map_err(|error| Error::Git(format!("Invalid UTF-8 in untracked path: {error}")))?;
        let contents = fs::read(git_root.join(path_text))
            .map_err(|error| Error::Git(format!("Failed to read untracked file for fingerprint: {error}")))?;
        hasher.update(u64::try_from(contents.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(contents);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn excluded_pathspecs(git_root: &Path, paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| {
            let absolute = if path.is_absolute() { path.clone() } else { git_root.join(path) };
            absolute
                .strip_prefix(git_root)
                .ok()
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(|relative| format!(":(exclude){}", relative.to_string_lossy().replace('\\', "/")))
        })
        .collect()
}

fn is_excluded(git_root: &Path, relative: &Path, excluded_paths: &[PathBuf]) -> bool {
    let candidate = git_root.join(relative);
    excluded_paths.iter().any(|excluded| {
        let excluded = if excluded.is_absolute() {
            excluded.clone()
        } else {
            git_root.join(excluded)
        };
        candidate == excluded || candidate.starts_with(excluded)
    })
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
            Ok(success_output("abc123\n")),     // merge-base
            Ok(success_output("src/lib.rs\n")), // diff
        ]);

        let result = compare(&mut host, &tmp, Some(&git_config), None, false, &[]).unwrap().diff;

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
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("abc123\n")), Ok(success_output(""))]);

        let result = compare(&mut host, &tmp, None, Some("origin/explicit"), false, &[]).unwrap().diff;

        assert!(result.changed.is_empty());
        assert!(!host.stderr_str().contains("No remote branch specified"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn working_tree_comparison_includes_tracked_deleted_and_untracked_paths() {
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("merge-base\n")),
            Ok(success_output("M\0src/lib.rs\0D\0src/old.rs\0")),
            Ok(success_output("src/new.rs\0")),
        ]);

        let comparison = compare(&mut host, Path::new("/repo"), None, Some("origin/main"), true, &[]).unwrap();

        assert_eq!(comparison.merge_base, "merge-base");
        assert_eq!(comparison.diff.changed, [PathBuf::from("src/lib.rs"), PathBuf::from("src/new.rs")]);
        assert_eq!(comparison.diff.deleted, [PathBuf::from("src/old.rs")]);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn working_tree_comparison_excludes_requested_subtrees() {
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("merge-base\n")),
            Ok(success_output("M\0generated/cache.json\0M\0src/lib.rs\0")),
            Ok(success_output("generated/new.json\0src/new.rs\0")),
        ]);

        let comparison = compare(
            &mut host,
            Path::new("/repo"),
            None,
            Some("origin/main"),
            true,
            &[PathBuf::from("/repo/generated")],
        )
        .unwrap();

        assert_eq!(comparison.diff.changed, [PathBuf::from("src/lib.rs"), PathBuf::from("src/new.rs")]);
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

        let result = compare(&mut host, &tmp, Some(&git_config), None, false, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("merge-base"));

        let _ = fs::remove_dir_all(&tmp);
    }
}
