use normpath::PathExt;
use std::borrow::Cow;
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

pub fn diff(host: &mut impl Host, workspace_path: &Path, config: Option<&GitConfig>, base_ref: Option<&str>) -> Result<GitDiff> {
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

    Ok(GitDiff { changed, deleted })
}

pub fn get_top_level(host: &mut impl Host) -> Result<PathBuf> {
    let output = host
        .run_command("git", &["rev-parse", "--show-toplevel"], None)
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

        let result = get_top_level(&mut host);
        let _ = result.unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn get_top_level_returns_error_on_nonzero_exit() {
        let mut host = TestHost::new().with_commands(vec![Ok(failure_output("fatal: not a git repository"))]);

        let result = get_top_level(&mut host);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not a git repository"));
    }

    #[test]
    fn get_top_level_returns_error_on_io_failure() {
        let mut host = TestHost::new().with_commands(vec![Err(std::io::Error::new(std::io::ErrorKind::NotFound, "git not found"))]);

        let result = get_top_level(&mut host);
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
        let _ = std::fs::create_dir_all(&tmp);

        // Create a file so it shows as "changed" (exists on disk)
        let src_dir = tmp.join("src");
        let _ = std::fs::create_dir_all(&src_dir);
        std::fs::write(src_dir.join("lib.rs"), "fn main() {}").unwrap();

        let git_config = GitConfig {
            remote_branch: Some("origin/feature".to_string()),
        };

        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("abc123\n")),     // merge-base
            Ok(success_output("src/lib.rs\n")), // diff
        ]);

        let result = diff(&mut host, &tmp, Some(&git_config), None).unwrap();

        assert_eq!(result.changed.len(), 1);
        assert!(result.deleted.is_empty());
        // No "No remote branch" message since branch was configured
        assert!(!host.stderr_str().contains("No remote branch"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn diff_with_explicit_base_ref_skips_discovery() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_diff_base_ref");
        let _ = std::fs::create_dir_all(&tmp);
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("abc123\n")), Ok(success_output(""))]);

        let result = diff(&mut host, &tmp, None, Some("origin/explicit")).unwrap();

        assert!(result.changed.is_empty());
        assert!(!host.stderr_str().contains("No remote branch specified"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn diff_merge_base_failure() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_diff_fail");
        let _ = std::fs::create_dir_all(&tmp);

        let git_config = GitConfig {
            remote_branch: Some("origin/feature".to_string()),
        };

        let mut host = TestHost::new().with_commands(vec![Ok(failure_output("fatal: not a valid commit"))]);

        let result = diff(&mut host, &tmp, Some(&git_config), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("merge-base"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
