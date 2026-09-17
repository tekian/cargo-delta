use crate::error::{Error, Result};
use crate::host::Host;
use normpath::PathExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CargoMetadata {
    pub packages: Vec<CargoPackage>,
    pub workspace_root: PathBuf,
    pub target_directory: PathBuf,
    #[serde(default)]
    pub workspace_members: Vec<String>,
    #[serde(default)]
    pub resolve: Option<CargoResolve>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CargoPackage {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub targets: Vec<CargoTarget>,
    pub manifest_path: PathBuf,
    pub dependencies: Vec<CargoDependency>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CargoTarget {
    pub name: String,
    pub kind: Vec<String>,
    pub src_path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CargoDependency {
    pub name: String,
    pub source: Option<String>,
    #[serde(default)]
    pub req: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub rename: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub uses_default_features: bool,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub registry: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoResolve {
    pub nodes: Vec<CargoResolveNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoResolveNode {
    pub id: String,
    #[serde(default)]
    pub deps: Vec<CargoResolveDependency>,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoResolveDependency {
    pub name: String,
    pub pkg: String,
    #[serde(default)]
    pub dep_kinds: Vec<CargoDependencyKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CargoDependencyKind {
    pub kind: Option<String>,
    pub target: Option<String>,
}

/// Get cargo metadata from current working directory
pub fn metadata(host: &mut impl Host, working_dir: Option<&Path>) -> Result<CargoMetadata> {
    metadata_with_args(host, working_dir, &["metadata", "--format-version", "1", "--no-deps"])
}

pub fn snapshot_metadata(host: &mut impl Host, working_dir: Option<&Path>) -> Result<CargoMetadata> {
    let metadata = metadata(host, working_dir)?;
    if !metadata.workspace_root.join("Cargo.lock").is_file() {
        return Ok(metadata);
    }
    metadata_with_args(
        host,
        working_dir,
        &["metadata", "--format-version", "1", "--all-features", "--locked"],
    )
}

fn metadata_with_args(host: &mut impl Host, working_dir: Option<&Path>, args: &[&str]) -> Result<CargoMetadata> {
    let cargo = host
        .env_var_os("CARGO")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsStr::new("cargo").to_os_string());
    let output = host.run_command(&cargo, args, working_dir)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::CargoCommand(stderr.to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut metadata: CargoMetadata = serde_json::from_str(&stdout)?;

    // Normalize the workspace root path
    metadata.workspace_root = metadata
        .workspace_root
        .normalize()
        .map(normpath::BasePathBuf::into_path_buf)
        .unwrap_or(metadata.workspace_root);

    Ok(metadata)
}

pub fn get_workspace_packages(metadata: &CargoMetadata) -> Vec<&CargoPackage> {
    if metadata.workspace_members.is_empty() {
        return metadata.packages.iter().filter(|package| package.source.is_none()).collect();
    }
    let members: HashSet<&str> = metadata.workspace_members.iter().map(String::as_str).collect();
    metadata
        .packages
        .iter()
        .filter(|package| members.contains(package.id.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    #[cfg_attr(miri, ignore)]
    fn metadata_parses_valid_output() {
        let json = serde_json::json!({
            "packages": [{
                "id": "path+file:///workspace#my-crate@0.1.0",
                "name": "my-crate",
                "version": "0.1.0",
                "source": null,
                "targets": [{"name": "my-crate", "kind": ["lib"], "src_path": "src/lib.rs"}],
                "manifest_path": "Cargo.toml",
                "dependencies": []
            }],
            "workspace_root": ".",
            "target_directory": "target",
            "workspace_members": ["path+file:///workspace#my-crate@0.1.0"],
            "resolve": null
        });

        let mut host = TestHost::new().with_commands(vec![Ok(success_output(&json.to_string()))]);

        let result = metadata(&mut host, None).unwrap();
        assert_eq!(result.packages.len(), 1);
        assert_eq!(result.packages[0].name, "my-crate");
        assert_eq!(result.packages[0].version, "0.1.0");
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn metadata_uses_the_invoking_cargo_executable() {
        let json = serde_json::json!({
            "packages": [],
            "workspace_root": ".",
            "target_directory": "target"
        });
        let cargo = PathBuf::from("toolchain with spaces").join("cargo");
        let mut host = TestHost::new()
            .with_env_var("CARGO", cargo.clone())
            .with_commands(vec![Ok(success_output(&json.to_string()))]);

        let _metadata = metadata(&mut host, Some(Path::new("workspace"))).unwrap();

        assert_eq!(host.command_calls[0].0, cargo);
        assert_eq!(host.command_calls[0].2.as_deref(), Some(Path::new("workspace")));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn snapshot_metadata_requests_all_features_for_locked_workspace() {
        let root = std::env::temp_dir().join(format!("cargo-delta-resolved-metadata-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.lock"), "version = 4\n").unwrap();
        let json = serde_json::json!({
            "packages": [],
            "workspace_root": root,
            "target_directory": root.join("target"),
            "workspace_members": [],
            "resolve": {"nodes": []}
        });
        let mut host = TestHost::new().with_commands(vec![Ok(success_output(&json.to_string())), Ok(success_output(&json.to_string()))]);

        let _metadata = snapshot_metadata(&mut host, Some(&root)).unwrap();

        assert_eq!(
            host.command_calls[1].1,
            ["metadata", "--format-version", "1", "--all-features", "--locked"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn metadata_returns_error_on_command_failure() {
        let mut host = TestHost::new().with_commands(vec![Ok(failure_output("cargo not found"))]);

        let result = metadata(&mut host, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cargo not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn metadata_returns_error_on_invalid_json() {
        let mut host = TestHost::new().with_commands(vec![Ok(success_output("not valid json"))]);

        let result = metadata(&mut host, None);
        let _ = result.unwrap_err();
    }

    #[test]
    fn metadata_returns_error_on_io_failure() {
        let mut host = TestHost::new().with_commands(vec![Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cargo not installed"))]);

        let result = metadata(&mut host, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cargo not installed"));
    }

    #[test]
    fn get_workspace_packages_filters_external_packages() {
        let meta = CargoMetadata {
            packages: vec![
                CargoPackage {
                    id: "local".to_string(),
                    name: "local".to_string(),
                    version: "0.1.0".to_string(),
                    source: None,
                    targets: vec![],
                    manifest_path: PathBuf::from("Cargo.toml"),
                    dependencies: vec![],
                },
                CargoPackage {
                    id: "external".to_string(),
                    name: "external".to_string(),
                    version: "1.0.0".to_string(),
                    source: None,
                    targets: vec![],
                    manifest_path: PathBuf::from("Cargo.toml"),
                    dependencies: vec![],
                },
            ],
            workspace_root: PathBuf::from("."),
            target_directory: PathBuf::from("target"),
            workspace_members: vec!["local".to_string()],
            resolve: None,
        };

        let result = get_workspace_packages(&meta);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "local");
    }
}
