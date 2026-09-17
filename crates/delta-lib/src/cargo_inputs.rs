use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::crates::{ExternalPackageId, PackageId};
use crate::error::{Error, Result};
use crate::git::{CheckoutState, GitDiff};
use crate::host::Host;
use crate::snapshot::Snapshot;

#[derive(Default)]
pub struct CargoInputChanges {
    pub modified: HashSet<PackageId>,
    pub affected: HashSet<PackageId>,
    pub(crate) scoped_paths: HashSet<PathBuf>,
    pub(crate) global_paths: BTreeSet<PathBuf>,
}

impl CargoInputChanges {
    pub fn is_scoped(&self, path: &Path) -> bool {
        self.scoped_paths.contains(path)
    }

    pub const fn global_paths(&self) -> &BTreeSet<PathBuf> {
        &self.global_paths
    }
}

pub fn classify(
    host: &mut impl Host,
    git_root: &Path,
    base: &CheckoutState,
    diff: &GitDiff,
    baseline: &Snapshot,
    current: &Snapshot,
) -> Result<CargoInputChanges> {
    let mut changes = CargoInputChanges::default();
    let workspace = current.workspace();
    let lock_path = workspace.join("Cargo.lock");
    if contains(diff, &lock_path) && baseline.packages.resolution_complete() && current.packages.resolution_complete() {
        let baseline_lock = crate::git::file_at(host, git_root, base.head(), &lock_path)?;
        let current_lock = read_optional(&git_root.join(&lock_path))?;
        let (removed_or_changed, added_or_changed) = changed_lock_packages(baseline_lock.as_deref(), current_lock.as_deref())?;
        changes
            .modified
            .extend(baseline.packages.consumers_of_external(&removed_or_changed));
        changes.modified.extend(current.packages.consumers_of_external(&added_or_changed));
        let _ = changes.scoped_paths.insert(lock_path);
    } else if contains(diff, &lock_path) {
        let _ = changes.global_paths.insert(lock_path);
    }

    let manifest_path = workspace.join("Cargo.toml");
    if contains(diff, &manifest_path) {
        let baseline_manifest = crate::git::file_at(host, git_root, base.head(), &manifest_path)?;
        let current_manifest = read_optional(&git_root.join(&manifest_path))?;
        if workspace_global_manifest(baseline_manifest.as_deref())? == workspace_global_manifest(current_manifest.as_deref())? {
            changes.modified.extend(current.packages.changed_declarations(&baseline.packages));
            for removed in baseline.packages.removed_since(&current.packages) {
                if let Some(dependents) = baseline.packages.get_dependents_transitive(&removed) {
                    changes.affected.extend(
                        dependents
                            .into_iter()
                            .filter_map(|dependent| current.packages.find_by_name(&dependent)),
                    );
                }
            }
            let _ = changes.scoped_paths.insert(manifest_path);
        } else {
            let _ = changes.global_paths.insert(manifest_path);
        }
    }
    Ok(changes)
}

fn contains(diff: &GitDiff, path: &Path) -> bool {
    diff.changed.iter().chain(&diff.deleted).any(|candidate| candidate == path)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Other(format!("Failed to read Cargo input '{}': {error}", path.display()))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Lockfile {
    #[serde(default, rename = "package")]
    packages: Vec<LockedPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LockedValue {
    checksum: Option<String>,
    dependencies: Vec<String>,
}

fn changed_lock_packages(
    baseline: Option<&[u8]>,
    current: Option<&[u8]>,
) -> Result<(HashSet<ExternalPackageId>, HashSet<ExternalPackageId>)> {
    let baseline = parse_lockfile(baseline)?;
    let current = parse_lockfile(current)?;
    let identities: BTreeSet<_> = baseline.keys().chain(current.keys()).cloned().collect();
    let mut removed_or_changed = HashSet::new();
    let mut added_or_changed = HashSet::new();
    for identity in identities {
        if baseline.get(&identity) == current.get(&identity) {
            continue;
        }
        if baseline.contains_key(&identity) {
            let _ = removed_or_changed.insert(identity.clone());
        }
        if current.contains_key(&identity) {
            let _ = added_or_changed.insert(identity);
        }
    }
    Ok((removed_or_changed, added_or_changed))
}

fn parse_lockfile(contents: Option<&[u8]>) -> Result<BTreeMap<ExternalPackageId, LockedValue>> {
    let Some(contents) = contents else {
        return Ok(BTreeMap::new());
    };
    let text = core::str::from_utf8(contents).map_err(|error| Error::Other(format!("Cargo.lock is not UTF-8: {error}")))?;
    let lockfile: Lockfile = toml::from_str(text).map_err(|error| Error::Other(format!("Failed to parse Cargo.lock: {error}")))?;
    Ok(lockfile
        .packages
        .into_iter()
        .map(|package| {
            let identity = ExternalPackageId {
                name: package.name,
                version: package.version,
                source: package.source,
            };
            let mut dependencies = package.dependencies;
            dependencies.sort();
            (
                identity,
                LockedValue {
                    checksum: package.checksum,
                    dependencies,
                },
            )
        })
        .collect())
}

fn workspace_global_manifest(contents: Option<&[u8]>) -> Result<Option<toml::Value>> {
    let Some(contents) = contents else {
        return Ok(None);
    };
    let text = core::str::from_utf8(contents).map_err(|error| Error::Other(format!("Cargo.toml is not UTF-8: {error}")))?;
    let mut manifest: toml::Value = toml::from_str(text).map_err(|error| Error::Other(format!("Failed to parse Cargo.toml: {error}")))?;
    if let Some(workspace) = manifest.get_mut("workspace").and_then(toml::Value::as_table_mut) {
        let _ = workspace.remove("dependencies");
        let _ = workspace.remove("members");
        let _ = workspace.remove("exclude");
    }
    Ok(Some(manifest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::CargoMetadata;
    use crate::crates;
    use crate::files::{FileKind, FileNode};
    use crate::snapshot::SnapshotKey;
    use crate::test_helpers::{TestHost, success_output};

    const SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

    fn metadata(root: &Path, external_version: &str, features: &[&str]) -> CargoMetadata {
        serde_json::from_value(serde_json::json!({
            "packages": [
                {
                    "id": "core",
                    "name": "core",
                    "version": "0.1.0",
                    "source": null,
                    "targets": [],
                    "manifest_path": root.join("core/Cargo.toml"),
                    "dependencies": [{
                        "name": "external",
                        "source": SOURCE,
                        "req": "^1",
                        "kind": null,
                        "rename": null,
                        "optional": false,
                        "uses_default_features": false,
                        "features": features,
                        "target": null,
                        "registry": null,
                        "path": null
                    }]
                },
                {
                    "id": "app",
                    "name": "app",
                    "version": "0.1.0",
                    "source": null,
                    "targets": [],
                    "manifest_path": root.join("app/Cargo.toml"),
                    "dependencies": [{"name": "core", "source": null}]
                },
                {
                    "id": "unrelated",
                    "name": "unrelated",
                    "version": "0.1.0",
                    "source": null,
                    "targets": [],
                    "manifest_path": root.join("unrelated/Cargo.toml"),
                    "dependencies": []
                },
                {
                    "id": format!("{SOURCE}#external@{external_version}"),
                    "name": "external",
                    "version": external_version,
                    "source": SOURCE,
                    "targets": [],
                    "manifest_path": "external/Cargo.toml",
                    "dependencies": []
                }
            ],
            "workspace_root": root,
            "target_directory": root.join("target"),
            "workspace_members": ["core", "app", "unrelated"],
            "resolve": {
                "nodes": [
                    {"id": "core", "deps": [{"name": "external", "pkg": format!("{SOURCE}#external@{external_version}")}]},
                    {"id": "app", "deps": [{"name": "core", "pkg": "core"}]},
                    {"id": "unrelated", "deps": []},
                    {"id": format!("{SOURCE}#external@{external_version}"), "deps": []}
                ]
            }
        }))
        .unwrap()
    }

    fn snapshot(root: &Path, external_version: &str, features: &[&str]) -> Snapshot {
        Snapshot {
            cache_key: Some(SnapshotKey::clean_test_key("base")),
            files: FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace),
            packages: crates::parse(&metadata(root, external_version, features)),
        }
    }

    fn membership_snapshot(root: &Path, members: &[&str]) -> Snapshot {
        let metadata = CargoMetadata {
            packages: vec![
                crate::cargo::CargoPackage {
                    id: "core".to_string(),
                    name: "core".to_string(),
                    version: "0.1.0".to_string(),
                    manifest_path: root.join("core/Cargo.toml"),
                    ..crate::cargo::CargoPackage::default()
                },
                crate::cargo::CargoPackage {
                    id: "app".to_string(),
                    name: "app".to_string(),
                    version: "0.1.0".to_string(),
                    manifest_path: root.join("app/Cargo.toml"),
                    dependencies: vec![crate::cargo::CargoDependency {
                        name: "core".to_string(),
                        ..crate::cargo::CargoDependency::default()
                    }],
                    ..crate::cargo::CargoPackage::default()
                },
            ],
            workspace_root: root.to_path_buf(),
            target_directory: root.join("target"),
            workspace_members: members.iter().map(|member| (*member).to_string()).collect(),
            resolve: None,
        };
        Snapshot {
            cache_key: Some(SnapshotKey::clean_test_key("base")),
            files: FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace),
            packages: crates::parse(&metadata),
        }
    }

    fn lock(version: &str) -> String {
        lock_with_checksum(version, version)
    }

    fn lock_with_checksum(version: &str, checksum: &str) -> String {
        format!(
            "version = 4\n\n[[package]]\nname = \"external\"\nversion = \"{version}\"\nsource = \"{SOURCE}\"\nchecksum = \"{checksum}\"\n"
        )
    }

    #[test]
    fn lock_change_selects_only_external_consumers() {
        let root = std::env::temp_dir().join(format!("cargo-delta-lock-consumers-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.lock"), lock("2.0.0")).unwrap();
        let baseline = snapshot(&root, "1.0.0", &[]);
        let current = snapshot(&root, "2.0.0", &[]);
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("Cargo.lock\0")), Ok(success_output(&lock("1.0.0")))]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.lock")],
            deleted: Vec::new(),
        };

        let changes = classify(&mut host, &root, &CheckoutState::clean("base"), &diff, &baseline, &current).unwrap();

        assert_eq!(changes.modified, HashSet::from(["core@0.1.0".to_string()]));
        assert!(changes.is_scoped(Path::new("Cargo.lock")));
        assert_eq!(
            current.packages.get_dependents_transitive(&"core@0.1.0".to_string()).unwrap(),
            ["app@0.1.0".to_string()]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unused_lock_change_selects_no_package() {
        let baseline = lock("1.0.0") + "\n[[package]]\nname = \"unused\"\nversion = \"1.0.0\"\n";
        let current = lock("1.0.0") + "\n[[package]]\nname = \"unused\"\nversion = \"2.0.0\"\n";
        let root = std::env::temp_dir().join(format!("cargo-delta-unused-lock-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.lock"), &current).unwrap();
        let tree = snapshot(&root, "1.0.0", &[]);
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("Cargo.lock\0")), Ok(success_output(&baseline))]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.lock")],
            deleted: Vec::new(),
        };

        let changes = classify(&mut host, &root, &CheckoutState::clean("base"), &diff, &tree, &tree).unwrap();

        assert!(changes.modified.is_empty());
        assert!(changes.is_scoped(Path::new("Cargo.lock")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checksum_only_lock_change_selects_consumer() {
        let root = std::env::temp_dir().join(format!("cargo-delta-lock-checksum-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.lock"), lock_with_checksum("1.0.0", "new")).unwrap();
        let tree = snapshot(&root, "1.0.0", &[]);
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("Cargo.lock\0")),
            Ok(success_output(&lock_with_checksum("1.0.0", "old"))),
        ]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.lock")],
            deleted: Vec::new(),
        };

        let changes = classify(&mut host, &root, &CheckoutState::clean("base"), &diff, &tree, &tree).unwrap();

        assert_eq!(changes.modified, HashSet::from(["core@0.1.0".to_string()]));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_change_without_resolved_snapshots_remains_unscoped() {
        let root = std::env::temp_dir().join(format!("cargo-delta-lock-incomplete-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.lock"), lock("1.0.0")).unwrap();
        let mut tree = snapshot(&root, "1.0.0", &[]);
        tree.packages = crates::parse(&CargoMetadata {
            packages: metadata(&root, "1.0.0", &[]).packages,
            workspace_root: root.clone(),
            target_directory: root.join("target"),
            workspace_members: vec!["core".to_string(), "app".to_string(), "unrelated".to_string()],
            resolve: None,
        });
        let mut host = TestHost::new();
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.lock")],
            deleted: Vec::new(),
        };

        let changes = classify(&mut host, &root, &CheckoutState::clean("base"), &diff, &tree, &tree).unwrap();

        assert!(!changes.is_scoped(Path::new("Cargo.lock")));
        assert!(changes.global_paths().contains(Path::new("Cargo.lock")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_dependency_feature_change_selects_inheriting_package() {
        let root = std::env::temp_dir().join(format!("cargo-delta-workspace-dependency-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let baseline_manifest = "[workspace]\nmembers = [\"core\", \"app\", \"unrelated\"]\n[workspace.dependencies]\nexternal = \"1\"\n";
        let current_manifest = "[workspace]\nmembers = [\"core\", \"app\", \"unrelated\"]\n[workspace.dependencies]\nexternal = { version = \"1\", features = [\"extra\"] }\n";
        fs::write(root.join("Cargo.toml"), current_manifest).unwrap();
        let baseline = snapshot(&root, "1.0.0", &[]);
        let current = snapshot(&root, "1.0.0", &["extra"]);
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("Cargo.toml\0")), Ok(success_output(baseline_manifest))]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.toml")],
            deleted: Vec::new(),
        };

        let changes = classify(&mut host, &root, &CheckoutState::clean("base"), &diff, &baseline, &current).unwrap();

        assert_eq!(changes.modified, HashSet::from(["core@0.1.0".to_string()]));
        assert!(changes.is_scoped(Path::new("Cargo.toml")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_change_remains_global() {
        let root = std::env::temp_dir().join(format!("cargo-delta-workspace-profile-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let baseline_manifest = "[workspace]\nmembers = [\"core\"]\n";
        let current_manifest = "[workspace]\nmembers = [\"core\"]\n[profile.release]\nlto = true\n";
        fs::write(root.join("Cargo.toml"), current_manifest).unwrap();
        let tree = snapshot(&root, "1.0.0", &[]);
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("Cargo.toml\0")), Ok(success_output(baseline_manifest))]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.toml")],
            deleted: Vec::new(),
        };

        let changes = classify(&mut host, &root, &CheckoutState::clean("base"), &diff, &tree, &tree).unwrap();

        assert!(!changes.is_scoped(Path::new("Cargo.toml")));
        assert!(changes.global_paths().contains(Path::new("Cargo.toml")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removed_workspace_member_selects_surviving_dependents() {
        let root = std::env::temp_dir().join(format!("cargo-delta-removed-member-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let baseline_manifest = "[workspace]\nmembers = [\"core\", \"app\"]\n";
        let current_manifest = "[workspace]\nmembers = [\"app\"]\n";
        fs::write(root.join("Cargo.toml"), current_manifest).unwrap();
        let baseline = membership_snapshot(&root, &["core", "app"]);
        let current = membership_snapshot(&root, &["app"]);
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("Cargo.toml\0")), Ok(success_output(baseline_manifest))]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.toml")],
            deleted: Vec::new(),
        };

        let changes = classify(&mut host, &root, &CheckoutState::clean("base"), &diff, &baseline, &current).unwrap();

        assert!(changes.modified.is_empty());
        assert_eq!(changes.affected, HashSet::from(["app@0.1.0".to_string()]));
        assert!(changes.is_scoped(Path::new("Cargo.toml")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn optional_file_read_distinguishes_missing_path_from_other_errors() {
        let root = std::env::temp_dir().join(format!("cargo-delta-optional-file-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();

        assert!(read_optional(&root.join("missing")).unwrap().is_none());
        let _error = read_optional(&root).unwrap_err();
        fs::remove_dir_all(root).unwrap();
    }
}
