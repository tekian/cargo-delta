use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cargo::{self, CargoMetadata};
use crate::config::LoadedConfig;
use crate::crates::{self, Packages};
use crate::error::{Error, Result};
use crate::files::{self, FileKind, FileNode};
use crate::git::CheckoutState;
use crate::host::Host;
use crate::output;
use crate::utils;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotKey {
    cargo_delta_version: String,
    workspace: PathBuf,
    config_sha256: String,
    source: CheckoutState,
    workspace_present: bool,
}

impl SnapshotKey {
    fn matches(&self, other: &Self) -> bool {
        self.cargo_delta_version == other.cargo_delta_version
            && self.workspace == other.workspace
            && self.config_sha256 == other.config_sha256
            && self.source == other.source
    }

    #[cfg(test)]
    pub(crate) fn clean_test_key(head: &str) -> Self {
        use sha2::Digest as _;

        Self {
            cargo_delta_version: env!("CARGO_PKG_VERSION").to_string(),
            workspace: PathBuf::new(),
            config_sha256: format!("{:x}", sha2::Sha256::digest(b"<defaults>")),
            source: CheckoutState::clean(head),
            workspace_present: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_key: Option<SnapshotKey>,
    pub(crate) files: FileNode,
    pub(crate) packages: Packages,
}

impl Snapshot {
    pub fn build(host: &mut impl Host, context: &SnapshotContext<'_>, source: &CheckoutState) -> Result<Self> {
        let packages = cargo::get_workspace_packages(context.metadata);
        let mut files = files::build_tree(host, context.metadata, &packages, &context.config.value);
        let packages = crates::parse(context.metadata);
        files.make_relative_paths(context.git_root);
        Ok(Self {
            cache_key: Some(context.key(source, true)?),
            files,
            packages,
        })
    }

    pub fn missing_workspace(context: &SnapshotContext<'_>, source: &CheckoutState) -> Result<Self> {
        let workspace = context.workspace_relative_path()?;
        Ok(Self {
            cache_key: Some(context.key(source, false)?),
            files: FileNode::new(workspace.join("Cargo.toml"), FileKind::Workspace),
            packages: Packages::default(),
        })
    }

    pub fn load(path: &Path, label: &str) -> Result<Self> {
        let snapshot: Self = utils::deser_json(path)?;
        if snapshot.cache_key.is_none() {
            return Err(Error::Other(format!(
                "Supplied {label} snapshot '{}' has no cache key and is not a valid snapshot",
                path.display()
            )));
        }
        Ok(snapshot)
    }

    pub fn clean_head<'a>(&'a self, path: &Path) -> Result<&'a str> {
        self.key().source.is_clean().then(|| self.key().source.head()).ok_or_else(|| {
            Error::Other(format!(
                "Explicit baseline snapshot '{}' contains working-tree changes and cannot define a Git diff base",
                path.display()
            ))
        })
    }

    pub fn warn_if_stale(&self, host: &mut impl Host, path: &Path, expected: &SnapshotKey, label: &str) {
        if !self.key().matches(expected) {
            let _ = writeln!(
                host.error(),
                "Warning: supplied {label} snapshot '{}' is not up to date for the current comparison",
                path.display()
            );
        }
    }

    pub fn matches(&self, expected: &SnapshotKey) -> bool {
        self.cache_key.as_ref().is_some_and(|key| key.matches(expected))
    }

    pub const fn workspace_present(&self) -> bool {
        self.key().workspace_present
    }

    const fn key(&self) -> &SnapshotKey {
        self.cache_key
            .as_ref()
            .expect("Snapshot::load and constructors guarantee every runtime snapshot has a key")
    }
}

#[derive(Clone, Copy)]
pub struct SnapshotContext<'a> {
    pub config: &'a LoadedConfig,
    pub metadata: &'a CargoMetadata,
    pub git_root: &'a Path,
}

impl SnapshotContext<'_> {
    pub fn key(&self, source: &CheckoutState, workspace_present: bool) -> Result<SnapshotKey> {
        Ok(SnapshotKey {
            cargo_delta_version: env!("CARGO_PKG_VERSION").to_string(),
            workspace: self.workspace_relative_path()?,
            config_sha256: self.config.digest().to_string(),
            source: source.clone(),
            workspace_present,
        })
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.metadata.target_directory.join("cargo-delta")
    }

    pub(crate) fn workspace_relative_path(&self) -> Result<PathBuf> {
        let workspace_root = std::fs::canonicalize(&self.metadata.workspace_root).map_err(|error| {
            Error::Other(format!(
                "Failed to resolve Cargo workspace '{}': {error}",
                self.metadata.workspace_root.display()
            ))
        })?;
        let git_root = std::fs::canonicalize(self.git_root)
            .map_err(|error| Error::Other(format!("Failed to resolve Git root '{}': {error}", self.git_root.display())))?;
        workspace_root.strip_prefix(&git_root).map(Path::to_path_buf).map_err(|_error| {
            Error::Other(format!(
                "Cargo workspace '{}' is outside Git root '{}'",
                self.metadata.workspace_root.display(),
                git_root.display()
            ))
        })
    }
}

pub fn run(host: &mut impl Host, config: &LoadedConfig, config_path: Option<&PathBuf>, output_path: Option<&Path>) {
    let start = Instant::now();
    let _ = writeln!(host.error(), "Snapshotting workspace..");
    if let Some(path) = config_path {
        let _ = writeln!(host.error(), "\nUsing config file  : {}", path.display());
    }

    let caller_dir = match host.current_dir() {
        Ok(path) => path,
        Err(error) => {
            let _ = writeln!(host.error(), "Error getting current directory: {error}");
            host.exit(1);
            return;
        }
    };
    let metadata = match cargo::metadata(host, Some(&caller_dir)) {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = writeln!(host.error(), "Error getting cargo metadata: {error}");
            host.exit(1);
            return;
        }
    };
    let git_root = match crate::git::get_top_level(host, Some(&caller_dir)) {
        Ok(root) => root,
        Err(error) => {
            let _ = writeln!(host.error(), "Error getting git root: {error}");
            host.exit(1);
            return;
        }
    };
    let _ = writeln!(
        host.error(),
        "\nDetected Git root        : {}\nDetected Cargo workspace : {}\n",
        git_root.display(),
        metadata.workspace_root.display()
    );

    let mut excluded_paths = vec![metadata.target_directory.join("cargo-delta")];
    if let Some(path) = output_path {
        excluded_paths.push(if path.is_absolute() {
            path.to_path_buf()
        } else {
            caller_dir.join(path)
        });
    }
    let context = SnapshotContext {
        config,
        metadata: &metadata,
        git_root: &git_root,
    };
    let snapshot =
        match crate::git::checkout_state(host, &git_root, &excluded_paths).and_then(|state| Snapshot::build(host, &context, &state)) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = writeln!(host.error(), "Error creating snapshot: {error}");
                host.exit(1);
                return;
            }
        };
    let _ = writeln!(host.error(), "Found {} package(s) in the workspace.", snapshot.packages.len());
    let _ = writeln!(host.error(), "Found {} file(s) in the workspace.\n", snapshot.files.len());

    let json = match serde_json::to_string_pretty(&snapshot) {
        Ok(json) => format!("{json}\n"),
        Err(error) => {
            let _ = writeln!(host.error(), "Error serializing workspace tree to JSON: {error}");
            host.exit(1);
            return;
        }
    };
    if !output::write(host, output_path, &json) {
        return;
    }

    report_unrelated(host, &config.value, &git_root, &snapshot);
    let duration = start.elapsed();
    let _ = writeln!(host.error(), "\nSnapshot finished in {duration:.2?}");
}

fn report_unrelated(host: &mut impl Host, config: &crate::config::MainConfig, git_root: &Path, snapshot: &Snapshot) {
    let excludes: Vec<PathBuf> = snapshot.files.distinct().into_iter().collect();
    let unrelated = utils::find_unrelated(git_root, &excludes, &config.file_exclude_patterns, &config.trip_wire_patterns);
    if !config.file_exclude_patterns.is_empty() {
        let _ = writeln!(
            host.error(),
            "\nExcluded patterns       : {}",
            config.file_exclude_patterns.join(", ")
        );
    }
    if !config.trip_wire_patterns.is_empty() {
        let _ = writeln!(host.error(), "Trip wire patterns      : {}", config.trip_wire_patterns.join(", "));
    }
    for (heading, files) in [
        ("Excluded file(s): (filtered out by exclude patterns)", &unrelated.filtered),
        ("Trip wire file(s): (changes to these trigger a full rebuild)", &unrelated.trip_wire),
        ("Needs triage: (unknown impact, not matched by any rule)", &unrelated.unaccounted),
    ] {
        if !files.is_empty() {
            let _ = writeln!(host.error(), "\n{heading}");
            for file in files {
                let _ = writeln!(host.error(), "  {}", file.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MainConfig, load_config};
    use crate::test_helpers::TestHost;

    #[test]
    fn workspace_path_uses_resolved_filesystem_identity() {
        let root = std::env::temp_dir().join(format!("cargo-delta-resolved-workspace-{}", std::process::id()));
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let metadata = CargoMetadata {
            packages: Vec::new(),
            workspace_root: nested.join(".."),
            target_directory: root.join("target"),
        };
        let config = load_config(None).unwrap();
        let context = SnapshotContext {
            config: &config,
            metadata: &metadata,
            git_root: &root,
        };

        assert_eq!(context.workspace_relative_path().unwrap(), PathBuf::new());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_presence_distinguishes_regular_and_missing_snapshots() {
        let root = std::env::temp_dir().join(format!("cargo-delta-workspace-presence-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let metadata = CargoMetadata {
            packages: Vec::new(),
            workspace_root: root.clone(),
            target_directory: root.join("target"),
        };
        let config = load_config(None).unwrap();
        let context = SnapshotContext {
            config: &config,
            metadata: &metadata,
            git_root: &root,
        };
        let state = CheckoutState::clean("head");
        let present = Snapshot {
            cache_key: Some(context.key(&state, true).unwrap()),
            files: FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace),
            packages: Packages::default(),
        };
        let missing = Snapshot::missing_workspace(&context, &state).unwrap();

        assert!(present.workspace_present());
        assert!(!missing.workspace_present());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unrelated_report_omits_empty_configuration_and_file_sections() {
        let root = std::env::temp_dir().join(format!("cargo-delta-empty-report-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let snapshot = Snapshot {
            cache_key: Some(SnapshotKey::clean_test_key("head")),
            files: FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace),
            packages: Packages::default(),
        };
        let config = MainConfig {
            file_exclude_patterns: Vec::new(),
            ..MainConfig::default()
        };
        let mut host = TestHost::new();

        report_unrelated(&mut host, &config, &root, &snapshot);

        assert!(host.stderr.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unrelated_report_emits_configured_and_populated_sections() {
        let root = std::env::temp_dir().join(format!("cargo-delta-populated-report-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::write(root.join("Cargo.lock"), "").unwrap();
        std::fs::write(root.join("README.md"), "").unwrap();
        let snapshot = Snapshot {
            cache_key: Some(SnapshotKey::clean_test_key("head")),
            files: FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace),
            packages: Packages::default(),
        };
        let config = MainConfig {
            file_exclude_patterns: Vec::new(),
            trip_wire_patterns: vec!["Cargo.lock".to_string()],
            ..MainConfig::default()
        };
        let mut host = TestHost::new();

        report_unrelated(&mut host, &config, &root, &snapshot);

        let report = host.stderr_str();
        assert!(report.contains("Trip wire patterns      : Cargo.lock"));
        assert!(report.contains("Trip wire file(s)"));
        assert!(report.contains("Cargo.lock"));
        assert!(report.contains("Needs triage"));
        assert!(report.contains("README.md"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
