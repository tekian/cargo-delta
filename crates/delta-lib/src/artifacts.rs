use core::fmt::Write as _;
use core::sync::atomic::{AtomicU64, Ordering};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::cargo::CargoMetadata;
use crate::config::MainConfig;
use crate::crates::Crates;
use crate::error::{Error, Result};
use crate::files::{FileKind, FileNode};
use crate::git::RefContext;
use crate::host::Host;
use crate::{DirtyPolicy, Impact, SNAPSHOT_SCHEMA, WorkspaceTree};

const ARTIFACT_SCHEMA: u32 = 1;
const MANIFEST_FILE: &str = "manifest.json";
const IMPACT_FILE: &str = "impact.json";
const MODIFIED_FILE: &str = "modified.packages";
const AFFECTED_FILE: &str = "affected.packages";
const REQUIRED_FILE: &str = "required.packages";
const BASELINE_SNAPSHOT_FILE: &str = "snapshots/baseline.json";
const CURRENT_SNAPSHOT_FILE: &str = "snapshots/current.json";
const PUBLISH_ORDER: [&str; 6] = [
    BASELINE_SNAPSHOT_FILE,
    CURRENT_SNAPSHOT_FILE,
    IMPACT_FILE,
    MODIFIED_FILE,
    AFFECTED_FILE,
    REQUIRED_FILE,
];

static NEXT_WORKTREE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotCacheIdentity {
    commit: String,
    config_sha256: String,
    snapshot_schema: u32,
    cargo_delta_version: String,
    workspace_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactFile {
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactManifest {
    schema: u32,
    generation: String,
    base_ref: String,
    base_commit: String,
    merge_base: String,
    current_commit: String,
    dirty_policy: String,
    widened: bool,
    baseline_workspace: bool,
    baseline_cache: SnapshotCacheIdentity,
    current_cache: SnapshotCacheIdentity,
    files: BTreeMap<String, ArtifactFile>,
}

struct SnapshotArtifact {
    tree: WorkspaceTree,
    bytes: Vec<u8>,
    workspace_present: bool,
    cache_hit: bool,
}

struct ResolvedOutputDirectory {
    absolute: PathBuf,
    relative_to_git_root: Option<PathBuf>,
}

pub fn impact_from_ref(
    host: &mut impl Host,
    config: &MainConfig,
    config_path: Option<&PathBuf>,
    base_ref: &str,
    output_dir: &Path,
    dirty_policy: DirtyPolicy,
) {
    let _ = writeln!(host.error(), "Generating impact artifacts..");
    crate::print_common_props(host, config_path);

    if let Err(error) = generate(host, config, base_ref, output_dir, dirty_policy) {
        let _ = writeln!(host.error(), "Error generating impact artifacts: {error}");
        host.exit(1);
    }
}

fn generate(host: &mut impl Host, config: &MainConfig, base_ref: &str, output_dir: &Path, dirty_policy: DirtyPolicy) -> Result<()> {
    let caller_dir = host
        .current_dir()
        .map_err(|error| Error::Other(format!("Failed to determine current directory: {error}")))?;
    let git_root = crate::git::get_top_level(host, Some(&caller_dir))?;
    let current_metadata = crate::cargo::metadata(host, Some(&caller_dir))
        .map_err(|error| Error::Other(format!("Failed to read current Cargo workspace metadata: {error}")))?;
    let workspace_relative = workspace_relative_path(&current_metadata, &git_root)?;
    let workspace_path = portable_path(&workspace_relative);
    let output_directory = resolve_output_directory(host, &caller_dir, &git_root, output_dir)?;

    let ref_context = crate::git::resolve_ref_context(host, &git_root, base_ref)?;
    let dirty_paths = crate::git::dirty_paths(
        host,
        &git_root,
        &current_metadata.target_directory,
        output_directory.relative_to_git_root.as_deref(),
    )?;
    if dirty_policy == DirtyPolicy::Error && !dirty_paths.is_empty() {
        let paths = dirty_paths.iter().map(|path| portable_path(path)).collect::<Vec<_>>().join(", ");
        return Err(Error::Other(format!(
            "Workspace has tracked or non-ignored untracked changes: {paths}. \
             Commit or remove them, or use '--dirty workspace' to widen every tier"
        )));
    }

    let output_dir = output_directory.absolute;
    fs::create_dir_all(output_dir.join("snapshots"))
        .map_err(|error| Error::Other(format!("Failed to create artifact directory '{}': {error}", output_dir.display())))?;

    let previous_manifest = load_previous_manifest(host, &output_dir)?;
    let config_sha256 = effective_config_hash(config)?;
    let baseline_cache = snapshot_cache_identity(&ref_context.merge_base, &config_sha256, &workspace_path);
    let current_cache = snapshot_cache_identity(&ref_context.head_commit, &config_sha256, &workspace_path);

    let mut baseline = cached_snapshot(
        previous_manifest.as_ref(),
        &output_dir,
        BASELINE_SNAPSHOT_FILE,
        &baseline_cache,
        CacheSide::Baseline,
    )?;
    if baseline.is_none() {
        baseline = Some(snapshot_commit(
            host,
            config,
            &git_root,
            &workspace_relative,
            &ref_context.merge_base,
            true,
        )?);
    }
    let baseline = baseline.unwrap_or_else(|| unreachable!("cache miss is filled above"));

    let mut current = cached_snapshot(
        previous_manifest.as_ref(),
        &output_dir,
        CURRENT_SNAPSHOT_FILE,
        &current_cache,
        CacheSide::Current,
    )?;
    if current.is_none() {
        current = Some(snapshot_commit(
            host,
            config,
            &git_root,
            &workspace_relative,
            &ref_context.head_commit,
            false,
        )?);
    }
    let current = current.unwrap_or_else(|| unreachable!("cache miss is filled above"));

    let widened = !baseline.workspace_present || (!dirty_paths.is_empty() && dirty_policy == DirtyPolicy::Workspace);
    let impact = if widened {
        all_packages_impact(&current.tree)
    } else {
        crate::get_impacted_crates(host, &baseline.tree, &current.tree, &ref_context.diff, config)?
    };
    let dirty_policy_name = match dirty_policy {
        DirtyPolicy::Error => "error",
        DirtyPolicy::Workspace => "workspace",
    };
    publish_artifacts(
        &output_dir,
        base_ref,
        &ref_context,
        dirty_policy_name,
        widened,
        &baseline_cache,
        &current_cache,
        &baseline,
        &current,
        &impact,
    )?;

    let baseline_status = if baseline.cache_hit { "hit" } else { "miss" };
    let current_status = if current.cache_hit { "hit" } else { "miss" };
    let _ = writeln!(
        host.error(),
        "Wrote impact artifacts to {} (baseline cache {baseline_status}, current cache {current_status}, widened={widened})",
        output_dir.display()
    );
    Ok(())
}

#[expect(clippy::too_many_arguments, reason = "publication combines every generation identity input")]
fn publish_artifacts(
    output_dir: &Path,
    base_ref: &str,
    ref_context: &RefContext,
    dirty_policy: &str,
    widened: bool,
    baseline_cache: &SnapshotCacheIdentity,
    current_cache: &SnapshotCacheIdentity,
    baseline: &SnapshotArtifact,
    current: &SnapshotArtifact,
    impact: &Impact,
) -> Result<()> {
    let artifacts = render_artifacts(impact, baseline, current)?;
    let file_metadata = artifacts
        .iter()
        .map(|(path, bytes)| {
            (
                (*path).to_string(),
                ArtifactFile {
                    sha256: sha256(bytes),
                    bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let manifest = ArtifactManifest {
        schema: ARTIFACT_SCHEMA,
        generation: generation_identity(
            base_ref,
            ref_context,
            dirty_policy,
            widened,
            baseline.workspace_present,
            baseline_cache,
            current_cache,
            &file_metadata,
        )?,
        base_ref: base_ref.to_string(),
        base_commit: ref_context.base_commit.clone(),
        merge_base: ref_context.merge_base.clone(),
        current_commit: ref_context.head_commit.clone(),
        dirty_policy: dirty_policy.to_string(),
        widened,
        baseline_workspace: baseline.workspace_present,
        baseline_cache: baseline_cache.clone(),
        current_cache: current_cache.clone(),
        files: file_metadata,
    };

    for relative_path in PUBLISH_ORDER {
        let bytes = artifacts
            .get(relative_path)
            .unwrap_or_else(|| unreachable!("every published artifact is rendered above"));
        crate::output::atomic_write(&output_dir.join(relative_path), bytes).map_err(|error| {
            Error::Other(format!(
                "Failed to atomically write artifact '{}': {error}",
                output_dir.join(relative_path).display()
            ))
        })?;
    }

    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    manifest_bytes.push(b'\n');
    crate::output::atomic_write(&output_dir.join(MANIFEST_FILE), &manifest_bytes).map_err(|error| {
        Error::Other(format!(
            "Failed to atomically publish manifest '{}': {error}",
            output_dir.join(MANIFEST_FILE).display()
        ))
    })
}

fn workspace_relative_path(metadata: &CargoMetadata, git_root: &Path) -> Result<PathBuf> {
    metadata
        .workspace_root
        .strip_prefix(git_root)
        .map(Path::to_path_buf)
        .map_err(|_error| {
            Error::Other(format!(
                "Current Cargo workspace '{}' is outside Git root '{}'",
                metadata.workspace_root.display(),
                git_root.display()
            ))
        })
}

fn resolve_output_directory(host: &mut impl Host, caller_dir: &Path, git_root: &Path, requested: &Path) -> Result<ResolvedOutputDirectory> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        caller_dir.join(requested)
    };
    let absolute = lexical_normalize(&absolute)?;

    if git_root.starts_with(&absolute) {
        return Err(Error::Other(format!(
            "Output directory '{}' must not be the Git root or one of its ancestors",
            absolute.display()
        )));
    }

    let relative_to_git_root = absolute.strip_prefix(git_root).ok().map(Path::to_path_buf);
    if let Some(relative) = relative_to_git_root.as_deref() {
        let first_component = relative.components().next().and_then(|component| match component {
            std::path::Component::Normal(component) => component.to_str(),
            _ => None,
        });
        if first_component.is_some_and(|component| component.eq_ignore_ascii_case(".git")) {
            return Err(Error::Other(format!(
                "Output directory '{}' must not be inside Git metadata",
                absolute.display()
            )));
        }
        reject_symlinked_output_path(git_root, relative)?;
        if let Some(tracked_path) = crate::git::tracked_path_overlap(host, git_root, relative)? {
            return Err(Error::Other(format!(
                "Output directory '{}' overlaps tracked path '{}'; choose a dedicated directory with no tracked content",
                absolute.display(),
                portable_path(&tracked_path)
            )));
        }
    }

    Ok(ResolvedOutputDirectory {
        absolute,
        relative_to_git_root,
    })
}

fn lexical_normalize(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir | std::path::Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(Error::Other(format!(
                        "Output directory '{}' traverses above the filesystem root",
                        path.display()
                    )));
                }
            }
        }
    }
    if !normalized.is_absolute() {
        return Err(Error::Other(format!(
            "Output directory '{}' could not be resolved to an absolute path",
            path.display()
        )));
    }
    Ok(normalized)
}

fn reject_symlinked_output_path(git_root: &Path, relative: &Path) -> Result<()> {
    let mut path = git_root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::Other(format!(
                    "Output directory path '{}' traverses symlink '{}'",
                    git_root.join(relative).display(),
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(Error::Other(format!(
                    "Failed to inspect output directory component '{}': {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn effective_config_hash(config: &MainConfig) -> Result<String> {
    let mut value = serde_json::to_value(config)?;
    canonicalize_json(&mut value);
    Ok(sha256(&serde_json::to_vec(&value)?))
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values.iter_mut() {
                canonicalize_json(value);
            }
            values.sort_by_cached_key(serde_json::Value::to_string);
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                canonicalize_json(value);
            }
        }
        _ => {}
    }
}

fn snapshot_cache_identity(commit: &str, config_sha256: &str, workspace_path: &str) -> SnapshotCacheIdentity {
    SnapshotCacheIdentity {
        commit: commit.to_string(),
        config_sha256: config_sha256.to_string(),
        snapshot_schema: SNAPSHOT_SCHEMA,
        cargo_delta_version: env!("CARGO_PKG_VERSION").to_string(),
        workspace_path: workspace_path.to_string(),
    }
}

fn load_previous_manifest(host: &mut impl Host, output_dir: &Path) -> Result<Option<ArtifactManifest>> {
    let path = output_dir.join(MANIFEST_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Other(format!(
                "Failed to read existing artifact manifest '{}': {error}",
                path.display()
            )));
        }
    };

    match serde_json::from_slice::<ArtifactManifest>(&bytes) {
        Ok(manifest) if manifest.schema == ARTIFACT_SCHEMA => Ok(Some(manifest)),
        Ok(manifest) => {
            let _ = writeln!(
                host.error(),
                "Ignoring artifact manifest with unsupported schema {}",
                manifest.schema
            );
            Ok(None)
        }
        Err(error) => {
            let _ = writeln!(host.error(), "Ignoring invalid artifact manifest '{}': {error}", path.display());
            Ok(None)
        }
    }
}

#[derive(Clone, Copy)]
enum CacheSide {
    Baseline,
    Current,
}

fn cached_snapshot(
    manifest: Option<&ArtifactManifest>,
    output_dir: &Path,
    relative_path: &str,
    identity: &SnapshotCacheIdentity,
    side: CacheSide,
) -> Result<Option<SnapshotArtifact>> {
    let Some(manifest) = manifest else {
        return Ok(None);
    };
    let cached_identity = match side {
        CacheSide::Baseline => &manifest.baseline_cache,
        CacheSide::Current => &manifest.current_cache,
    };
    if cached_identity != identity {
        return Ok(None);
    }
    let Some(expected_file) = manifest.files.get(relative_path) else {
        return Ok(None);
    };
    let path = output_dir.join(relative_path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Other(format!(
                "Failed to read cached snapshot '{}': {error}",
                path.display()
            )));
        }
    };
    if sha256(&bytes) != expected_file.sha256 || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_file.bytes {
        return Ok(None);
    }

    let tree = match serde_json::from_slice::<WorkspaceTree>(&bytes) {
        Ok(tree) if tree.validate().is_ok() => tree,
        Ok(_) | Err(_) => return Ok(None),
    };
    let workspace_present = match side {
        CacheSide::Baseline => manifest.baseline_workspace,
        CacheSide::Current => true,
    };
    Ok(Some(SnapshotArtifact {
        tree,
        bytes,
        workspace_present,
        cache_hit: true,
    }))
}

fn snapshot_commit(
    host: &mut impl Host,
    config: &MainConfig,
    git_root: &Path,
    workspace_relative: &Path,
    commit: &str,
    allow_missing_workspace: bool,
) -> Result<SnapshotArtifact> {
    let mut worktree = TemporaryWorktree::create(host, git_root, commit)?;
    let workspace_dir = worktree.path.join(workspace_relative);
    let manifest_path = workspace_dir.join("Cargo.toml");
    let snapshot_result = (|| -> Result<SnapshotArtifact> {
        match fs::metadata(&manifest_path) {
            Ok(metadata) if metadata.is_file() => {
                let metadata = crate::cargo::metadata(host, Some(&workspace_dir))?;
                let tree = crate::build_workspace_tree(host, config, &metadata, &worktree.path)?;
                let bytes = crate::snapshot_bytes(&tree)?;
                Ok(SnapshotArtifact {
                    tree,
                    bytes,
                    workspace_present: true,
                    cache_hit: false,
                })
            }
            Ok(_) => Err(Error::Other(format!(
                "Expected Cargo workspace manifest '{}' to be a file",
                manifest_path.display()
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing_workspace => {
                let tree = empty_snapshot(workspace_relative)?;
                let bytes = crate::snapshot_bytes(&tree)?;
                Ok(SnapshotArtifact {
                    tree,
                    bytes,
                    workspace_present: false,
                    cache_hit: false,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(Error::Other(format!(
                "HEAD does not contain the current Cargo workspace manifest '{}'",
                portable_path(&workspace_relative.join("Cargo.toml"))
            ))),
            Err(error) => Err(Error::Other(format!(
                "Failed to inspect workspace manifest '{}': {error}",
                manifest_path.display()
            ))),
        }
    })();
    let cleanup_result = worktree.cleanup(host);

    finish_snapshot(snapshot_result, cleanup_result)
}

fn finish_snapshot(snapshot_result: Result<SnapshotArtifact>, cleanup_result: Result<()>) -> Result<SnapshotArtifact> {
    match (snapshot_result, cleanup_result) {
        (Ok(snapshot), Ok(())) => Ok(snapshot),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => Err(Error::Other(format!(
            "{primary}; additionally, temporary worktree cleanup failed: {cleanup}"
        ))),
    }
}

fn empty_snapshot(workspace_relative: &Path) -> Result<WorkspaceTree> {
    let tree = WorkspaceTree {
        schema: SNAPSHOT_SCHEMA,
        packages: Vec::new(),
        files: FileNode::new(workspace_relative.join("Cargo.toml"), FileKind::Workspace),
        crates: Crates::empty(),
    };
    tree.validate()?;
    Ok(tree)
}

struct TemporaryWorktree {
    git_root: PathBuf,
    path: PathBuf,
    active: bool,
}

impl TemporaryWorktree {
    fn create(host: &mut impl Host, git_root: &Path, commit: &str) -> Result<Self> {
        let parent = git_root.parent().ok_or_else(|| {
            Error::Other(format!(
                "Cannot create a temporary worktree beside Git root '{}'",
                git_root.display()
            ))
        })?;
        let path = unique_worktree_path(parent)?;
        let path_argument = path.to_str().ok_or_else(|| {
            Error::Other(format!(
                "Temporary worktree path '{}' cannot be passed to Git because it is not UTF-8",
                path.display()
            ))
        })?;
        let output = host
            .run_command(
                "git",
                &["worktree", "add", "--detach", "--force", path_argument, commit],
                Some(git_root),
            )
            .map_err(|error| Error::Git(format!("Failed to run git worktree add: {error}")))?;
        if !output.status.success() {
            return Err(Error::Git(format!(
                "git worktree add failed for commit {commit}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
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
        let path_argument = self.path.to_str().ok_or_else(|| {
            Error::Other(format!(
                "Temporary worktree path '{}' cannot be passed to Git because it is not UTF-8",
                self.path.display()
            ))
        })?;
        let output = host
            .run_command("git", &["worktree", "remove", "--force", path_argument], Some(&self.git_root))
            .map_err(|error| Error::Git(format!("Failed to run git worktree remove: {error}")))?;
        if !output.status.success() {
            return Err(Error::Git(format!(
                "git worktree remove failed for '{}': {}",
                self.path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        self.active = false;
        Ok(())
    }
}

fn unique_worktree_path(parent: &Path) -> Result<PathBuf> {
    for _ in 0..100 {
        let sequence = NEXT_WORKTREE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".cargo-delta-worktree-{}-{sequence}", std::process::id()));
        match path.try_exists() {
            Ok(false) => return Ok(path),
            Ok(true) => {}
            Err(error) => {
                return Err(Error::Other(format!(
                    "Failed to inspect temporary worktree path '{}': {error}",
                    path.display()
                )));
            }
        }
    }
    Err(Error::Other(
        "Unable to choose a unique temporary Git worktree path after 100 attempts".to_string(),
    ))
}

fn all_packages_impact(current: &WorkspaceTree) -> Impact {
    let packages: HashSet<String> = current.crates.get_all_package_ids().into_iter().collect();
    Impact {
        modified: packages.clone(),
        affected: packages.clone(),
        required: packages,
    }
}

fn render_artifacts(impact: &Impact, baseline: &SnapshotArtifact, current: &SnapshotArtifact) -> Result<BTreeMap<&'static str, Vec<u8>>> {
    let mut artifacts = BTreeMap::new();
    let _ = artifacts.insert(
        IMPACT_FILE,
        crate::emit_result(
            impact,
            &current.tree,
            crate::OutputFormat::Json,
            crate::TierMask::resolve(false, false, false),
        )?,
    );
    let _ = artifacts.insert(MODIFIED_FILE, crate::lines(&current.tree.package_specs(&impact.modified)?));
    let _ = artifacts.insert(AFFECTED_FILE, crate::lines(&current.tree.package_specs(&impact.affected)?));
    let _ = artifacts.insert(REQUIRED_FILE, crate::lines(&current.tree.package_specs(&impact.required)?));
    let _ = artifacts.insert(BASELINE_SNAPSHOT_FILE, baseline.bytes.clone());
    let _ = artifacts.insert(CURRENT_SNAPSHOT_FILE, current.bytes.clone());
    Ok(artifacts)
}

#[expect(clippy::too_many_arguments, reason = "generation identity covers every published contract field")]
fn generation_identity(
    base_ref: &str,
    ref_context: &RefContext,
    dirty_policy: &str,
    widened: bool,
    baseline_workspace: bool,
    baseline_cache: &SnapshotCacheIdentity,
    current_cache: &SnapshotCacheIdentity,
    files: &BTreeMap<String, ArtifactFile>,
) -> Result<String> {
    let seed = serde_json::json!({
        "schema": ARTIFACT_SCHEMA,
        "base_ref": base_ref,
        "base_commit": ref_context.base_commit,
        "merge_base": ref_context.merge_base,
        "current_commit": ref_context.head_commit,
        "dirty_policy": dirty_policy,
        "widened": widened,
        "baseline_workspace": baseline_workspace,
        "baseline_cache": baseline_cache,
        "current_cache": current_cache,
        "files": files,
    });
    Ok(sha256(&serde_json::to_vec(&seed)?))
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut rendered = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(rendered, "{byte:02x}");
    }
    rendered
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::test_directory;
    use std::process::{Command, Output};

    #[derive(Debug)]
    struct ProcessHost {
        current_dir: PathBuf,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_code: Option<i32>,
        command_calls: Vec<(String, Vec<String>, Option<PathBuf>)>,
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
            String::from_utf8_lossy(&self.stderr).to_string()
        }

        fn worktree_adds(&self) -> usize {
            self.command_calls
                .iter()
                .filter(|(command, args, _)| {
                    command == "git" && matches!(args.as_slice(), [worktree, add, ..] if worktree == "worktree" && add == "add")
                })
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

        fn run_command(&mut self, command: &str, args: &[&str], working_dir: Option<&Path>) -> io::Result<Output> {
            self.command_calls.push((
                command.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
                working_dir.map(Path::to_path_buf),
            ));
            let mut process = Command::new(command);
            let _ = process.args(args);
            if let Some(working_dir) = working_dir {
                let _ = process.current_dir(working_dir);
            }
            process.output()
        }
    }

    fn git(root: &Path, args: &[&str]) -> String {
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
        String::from_utf8(output.stdout)
            .expect("test Git output should be UTF-8")
            .trim()
            .to_string()
    }

    fn initialize_repository(name: &str) -> PathBuf {
        let root = test_directory(name);
        let _ = git(&root, &["init", "--quiet"]);
        let _ = git(&root, &["config", "user.email", "cargo-delta@example.invalid"]);
        let _ = git(&root, &["config", "user.name", "cargo-delta tests"]);
        let _ = git(&root, &["config", "core.autocrlf", "false"]);
        let _ = git(&root, &["config", "core.safecrlf", "false"]);
        root
    }

    fn write_workspace(root: &Path) {
        fs::create_dir_all(root.join(".cargo")).expect("fixture Cargo config directory should be created");
        fs::create_dir_all(root.join("core").join("src")).expect("fixture core crate directory should be created");
        fs::create_dir_all(root.join("app").join("src")).expect("fixture app crate directory should be created");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"core\", \"app\"]\nresolver = \"2\"\n",
        )
        .expect("fixture workspace manifest should be written");
        fs::write(root.join(".cargo").join("config.toml"), "[build]\ntarget-dir = \"build-output\"\n")
            .expect("fixture Cargo config should be written");
        fs::write(root.join(".gitignore"), "ignored.txt\n").expect("fixture ignore file should be written");
        fs::write(
            root.join("core").join("Cargo.toml"),
            "[package]\nname = \"core-lib\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
        )
        .expect("fixture core manifest should be written");
        fs::write(root.join("core").join("src").join("lib.rs"), "pub fn value() -> u32 { 1 }\n")
            .expect("fixture core source should be written");
        fs::write(
            root.join("app").join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\nedition = \"2024\"\n\n[dependencies]\ncore-lib = { path = \"../core\" }\n",
        )
        .expect("fixture app manifest should be written");
        fs::write(
            root.join("app").join("src").join("lib.rs"),
            "pub fn app() -> u32 { core_lib::value() }\n",
        )
        .expect("fixture app source should be written");
    }

    fn commit_all(root: &Path, message: &str) -> String {
        let _ = git(root, &["add", "--all"]);
        let _ = git(root, &["commit", "--quiet", "-m", message]);
        git(root, &["rev-parse", "HEAD"])
    }

    fn commit_paths(root: &Path, message: &str, paths: &[&str]) -> String {
        let mut add_arguments = vec!["add", "--"];
        add_arguments.extend_from_slice(paths);
        let _ = git(root, &add_arguments);
        let _ = git(root, &["commit", "--quiet", "-m", message]);
        git(root, &["rev-parse", "HEAD"])
    }

    fn invoke(root: &Path, base_ref: &str, output_dir: &Path, dirty: Option<&str>, config: Option<&Path>) -> ProcessHost {
        let mut arguments = vec![
            "cargo".to_string(),
            "delta".to_string(),
            "impact".to_string(),
            "--base-ref".to_string(),
            base_ref.to_string(),
            "--output-dir".to_string(),
            output_dir.display().to_string(),
        ];
        if let Some(dirty) = dirty {
            arguments.push("--dirty".to_string());
            arguments.push(dirty.to_string());
        }
        if let Some(config) = config {
            arguments.push("-c".to_string());
            arguments.push(config.display().to_string());
        }

        let mut host = ProcessHost::new(root.to_path_buf());
        crate::run(&mut host, arguments);
        host
    }

    fn read_manifest(output_dir: &Path) -> ArtifactManifest {
        serde_json::from_slice(&fs::read(output_dir.join(MANIFEST_FILE)).expect("manifest should exist"))
            .expect("manifest should be valid JSON")
    }

    fn assert_worktrees_clean(root: &Path) {
        let worktrees = git(root, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees.lines().filter(|line| line.starts_with("worktree ")).count(),
            1,
            "temporary worktrees were not cleaned:\n{worktrees}"
        );
    }

    #[test]
    fn cleanup_failure_is_reported_without_hiding_snapshot_failure() {
        let result = finish_snapshot(
            Err(Error::Other("primary snapshot failure".to_string())),
            Err(Error::Other("cleanup failure".to_string())),
        );
        let error = result.err().expect("combined operation should fail");

        assert!(error.to_string().contains("primary snapshot failure"));
        assert!(error.to_string().contains("cleanup failure"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn clean_flow_is_portable_hash_verified_and_reuses_then_invalidates_cache() {
        let root = initialize_repository("artifact flow with spaces");
        write_workspace(&root);
        let _ = commit_all(&root, "baseline");
        let _ = git(&root, &["tag", "baseline"]);
        fs::write(root.join("core").join("src").join("lib.rs"), "pub fn value() -> u32 { 2 }\n").expect("fixture source should be changed");
        let _ = commit_all(&root, "change core");
        let output_dir = root.join("artifacts");
        let requested_output_dir = Path::new("unused").join("..").join("artifacts");

        let first = invoke(&root, "baseline", &requested_output_dir, None, None);

        assert_eq!(first.exit_code, None, "{}", first.stderr());
        assert_eq!(first.worktree_adds(), 2);
        assert_eq!(fs::read(output_dir.join(MODIFIED_FILE)).unwrap(), b"core-lib@1.0.0\n");
        assert_eq!(fs::read(output_dir.join(AFFECTED_FILE)).unwrap(), b"app@1.0.0\ncore-lib@1.0.0\n");
        assert_eq!(fs::read(output_dir.join(REQUIRED_FILE)).unwrap(), b"app@1.0.0\ncore-lib@1.0.0\n");
        let first_manifest = read_manifest(&output_dir);
        assert!(!first_manifest.widened);
        for (relative_path, expected) in &first_manifest.files {
            assert!(relative_path.contains('/') || !relative_path.contains('\\'));
            let bytes = fs::read(output_dir.join(relative_path)).unwrap();
            assert_eq!(sha256(&bytes), expected.sha256);
            assert_eq!(u64::try_from(bytes.len()).unwrap(), expected.bytes);
        }
        let current: serde_json::Value = serde_json::from_slice(&fs::read(output_dir.join(CURRENT_SNAPSHOT_FILE)).unwrap()).unwrap();
        for package in current["packages"].as_array().unwrap() {
            let manifest_path = package["manifest_path"].as_str().unwrap();
            assert!(!manifest_path.contains('\\'));
            assert!(!manifest_path.starts_with('/'));
        }
        assert_worktrees_clean(&root);
        assert!(git(&root, &["status", "--porcelain"]).contains("artifacts"));

        let second = invoke(&root, "baseline", &requested_output_dir, None, None);

        assert_eq!(second.exit_code, None, "{}", second.stderr());
        assert_eq!(second.worktree_adds(), 0);
        assert!(
            second
                .command_calls
                .iter()
                .any(|(command, args, _)| command == "git" && args.first().is_some_and(|arg| arg == "status"))
        );
        assert!(second.command_calls.iter().any(|(command, args, _)| command == "git"
            && matches!(args.as_slice(), [diff, option, ..] if diff == "diff" && option == "--name-status")));
        let second_manifest = read_manifest(&output_dir);
        assert!(!second_manifest.widened);
        assert_eq!(second_manifest.generation, first_manifest.generation);

        fs::write(
            root.join("app").join("src").join("lib.rs"),
            "pub fn app() -> u32 { core_lib::value() + 1 }\n",
        )
        .unwrap();
        let _ = commit_paths(&root, "change app", &["app/src/lib.rs"]);
        let new_head = invoke(&root, "baseline", &output_dir, None, None);
        assert_eq!(new_head.exit_code, None, "{}", new_head.stderr());
        assert_eq!(new_head.worktree_adds(), 1);

        let config = output_dir.join("delta-config.toml");
        fs::write(&config, "[parser]\nmods = false\n").unwrap();
        let changed_config = invoke(&root, "baseline", &output_dir, None, Some(&config));
        assert_eq!(changed_config.exit_code, None, "{}", changed_config.stderr());
        assert_eq!(changed_config.worktree_adds(), 2);
        assert_worktrees_clean(&root);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn output_directory_rejects_repository_root_and_tracked_subtrees() {
        let root = initialize_repository("unsafe-output-placement");
        write_workspace(&root);
        let _ = commit_all(&root, "workspace");

        let repository_root = invoke(&root, "HEAD", Path::new("."), None, None);
        assert_eq!(repository_root.exit_code, Some(1));
        assert!(repository_root.stderr().contains("must not be the Git root"));
        assert!(!root.join(MANIFEST_FILE).exists());

        let tracked_subtree = invoke(&root, "HEAD", Path::new("core"), None, None);
        assert_eq!(tracked_subtree.exit_code, Some(1));
        assert!(tracked_subtree.stderr().contains("overlaps tracked path"));
        assert!(!root.join("core").join(MANIFEST_FILE).exists());

        assert_worktrees_clean(&root);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn dirty_policy_ignores_ignored_and_target_files_then_errors_or_widens() {
        let root = initialize_repository("dirty-policy");
        write_workspace(&root);
        let _ = commit_all(&root, "baseline");
        let _ = git(&root, &["tag", "baseline"]);
        fs::write(root.join("core").join("src").join("lib.rs"), "pub fn value() -> u32 { 2 }\n").unwrap();
        let _ = commit_all(&root, "head");
        fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
        fs::create_dir_all(root.join("build-output")).unwrap();
        fs::write(root.join("build-output").join("generated.txt"), "target\n").unwrap();
        let output_dir = root.join("artifacts");

        let ignored_only = invoke(&root, "baseline", &output_dir, None, None);
        assert_eq!(ignored_only.exit_code, None, "{}", ignored_only.stderr());
        let old_manifest = fs::read(output_dir.join(MANIFEST_FILE)).unwrap();

        fs::write(root.join("visible.txt"), "untracked\n").unwrap();
        let rejected = invoke(&root, "baseline", &output_dir, None, None);

        assert_eq!(rejected.exit_code, Some(1));
        assert!(rejected.stderr().contains("visible.txt"));
        assert!(!rejected.stderr().contains("ignored.txt"));
        assert!(!rejected.stderr().contains("build-output"));
        assert_eq!(fs::read(output_dir.join(MANIFEST_FILE)).unwrap(), old_manifest);

        let widened = invoke(&root, "baseline", &output_dir, Some("workspace"), None);

        assert_eq!(widened.exit_code, None, "{}", widened.stderr());
        assert!(read_manifest(&output_dir).widened);
        for tier in [MODIFIED_FILE, AFFECTED_FILE, REQUIRED_FILE] {
            assert_eq!(fs::read(output_dir.join(tier)).unwrap(), b"app@1.0.0\ncore-lib@1.0.0\n");
        }
        assert_worktrees_clean(&root);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn baseline_without_workspace_emits_empty_snapshot_and_widens_current_workspace() {
        let root = initialize_repository("baseline-without-workspace");
        fs::write(root.join("README.md"), "before Cargo\n").unwrap();
        let _ = commit_all(&root, "before workspace");
        let _ = git(&root, &["tag", "before-workspace"]);
        write_workspace(&root);
        let _ = commit_all(&root, "add workspace");
        let output_dir = root.join("artifacts");

        let host = invoke(&root, "before-workspace", &output_dir, None, None);

        assert_eq!(host.exit_code, None, "{}", host.stderr());
        let manifest = read_manifest(&output_dir);
        assert!(manifest.widened);
        assert!(!manifest.baseline_workspace);
        let baseline: WorkspaceTree = serde_json::from_slice(&fs::read(output_dir.join(BASELINE_SNAPSHOT_FILE)).unwrap()).unwrap();
        assert!(baseline.packages.is_empty());
        assert_eq!(fs::read(output_dir.join(MODIFIED_FILE)).unwrap(), b"app@1.0.0\ncore-lib@1.0.0\n");
        assert_worktrees_clean(&root);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn snapshot_failure_cleans_temporary_worktree() {
        let root = initialize_repository("failure-cleanup");
        fs::write(root.join("Cargo.toml"), "[workspace\n").unwrap();
        let _ = commit_all(&root, "invalid workspace");
        let _ = git(&root, &["tag", "invalid-workspace"]);
        write_workspace(&root);
        let _ = commit_all(&root, "valid workspace");
        let output_dir = root.join("artifacts");

        let host = invoke(&root, "invalid-workspace", &output_dir, None, None);

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr().contains("Cargo command failed"));
        assert!(!output_dir.join(MANIFEST_FILE).exists());
        assert_worktrees_clean(&root);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn partial_artifact_failure_does_not_publish_a_new_manifest() {
        let root = initialize_repository("atomic-output");
        write_workspace(&root);
        let _ = commit_all(&root, "baseline");
        let _ = git(&root, &["tag", "baseline"]);
        fs::write(root.join("core").join("src").join("lib.rs"), "pub fn value() -> u32 { 2 }\n").unwrap();
        let _ = commit_all(&root, "head");
        let output_dir = root.join("artifacts");
        fs::create_dir_all(output_dir.join(AFFECTED_FILE)).unwrap();
        fs::write(output_dir.join(MANIFEST_FILE), "previous manifest\n").unwrap();

        let host = invoke(&root, "baseline", &output_dir, None, None);

        assert_eq!(host.exit_code, Some(1));
        assert_eq!(fs::read(output_dir.join(MANIFEST_FILE)).unwrap(), b"previous manifest\n");
        assert!(output_dir.join(BASELINE_SNAPSHOT_FILE).is_file());
        assert!(output_dir.join(CURRENT_SNAPSHOT_FILE).is_file());
        assert!(output_dir.join(IMPACT_FILE).is_file());
        assert!(output_dir.join(MODIFIED_FILE).is_file());
        assert!(!output_dir.join(REQUIRED_FILE).exists());
        assert_worktrees_clean(&root);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn empty_impact_emits_zero_byte_tiers_and_newline_terminated_json() {
        let root = initialize_repository("empty-impact");
        write_workspace(&root);
        let _ = commit_all(&root, "workspace");
        let output_dir = root.join("artifacts");

        let host = invoke(&root, "HEAD", &output_dir, None, None);

        assert_eq!(host.exit_code, None, "{}", host.stderr());
        for tier in [MODIFIED_FILE, AFFECTED_FILE, REQUIRED_FILE] {
            assert_eq!(fs::metadata(output_dir.join(tier)).unwrap().len(), 0);
        }
        for json_file in [MANIFEST_FILE, IMPACT_FILE, BASELINE_SNAPSHOT_FILE, CURRENT_SNAPSHOT_FILE] {
            assert_eq!(fs::read(output_dir.join(json_file)).unwrap().last(), Some(&b'\n'));
        }
        assert_worktrees_clean(&root);
        fs::remove_dir_all(root).unwrap();
    }
}
