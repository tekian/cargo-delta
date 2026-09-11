use crate::error::{Error, Result};
use crate::host::Host;
use normpath::PathExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoMetadata {
    pub packages: Vec<CargoCrate>,
    pub workspace_members: Vec<String>,
    pub workspace_root: PathBuf,
    pub target_directory: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoCrate {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub targets: Vec<CargoTarget>,
    pub manifest_path: PathBuf,
    pub dependencies: Vec<CargoDependency>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoTarget {
    pub name: String,
    pub kind: Vec<String>,
    pub src_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoDependency {
    pub name: String,
    pub source: Option<String>,
    pub path: Option<PathBuf>,
}

/// Get Cargo metadata from the requested working directory.
pub fn metadata(host: &mut impl Host, working_dir: Option<&std::path::Path>) -> Result<CargoMetadata> {
    let output = host.run_command("cargo", &["metadata", "--format-version", "1", "--no-deps"], working_dir)?;

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

pub fn get_workspace_crates(metadata: &CargoMetadata) -> Vec<&CargoCrate> {
    let members: HashSet<&str> = metadata.workspace_members.iter().map(String::as_str).collect();
    metadata.packages.iter().filter(|pkg| members.contains(pkg.id.as_str())).collect()
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
                "id": "path+file:///repo/my-crate#0.1.0",
                "name": "my-crate",
                "version": "0.1.0",
                "source": null,
                "targets": [{"name": "my-crate", "kind": ["lib"], "src_path": "src/lib.rs"}],
                "manifest_path": "Cargo.toml",
                "dependencies": []
            }],
            "workspace_members": ["path+file:///repo/my-crate#0.1.0"],
            "workspace_root": ".",
            "target_directory": "target"
        });

        let mut host = TestHost::new().with_commands(vec![Ok(success_output(&json.to_string()))]);

        let result = metadata(&mut host, None).unwrap();
        assert_eq!(result.packages.len(), 1);
        assert_eq!(result.packages[0].name, "my-crate");
        assert_eq!(result.packages[0].version, "0.1.0");
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
    fn get_workspace_crates_filters_external_packages() {
        let meta = CargoMetadata {
            packages: vec![
                CargoCrate {
                    id: "path+file:///repo/local#0.1.0".to_string(),
                    name: "local".to_string(),
                    version: "0.1.0".to_string(),
                    source: None,
                    targets: vec![],
                    manifest_path: PathBuf::from("Cargo.toml"),
                    dependencies: vec![],
                },
                CargoCrate {
                    id: "registry+https://github.com/rust-lang/crates.io-index#external@1.0.0".to_string(),
                    name: "external".to_string(),
                    version: "1.0.0".to_string(),
                    source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
                    targets: vec![],
                    manifest_path: PathBuf::from("Cargo.toml"),
                    dependencies: vec![],
                },
            ],
            workspace_members: vec!["path+file:///repo/local#0.1.0".to_string()],
            workspace_root: PathBuf::from("."),
            target_directory: PathBuf::from("target"),
        };

        let result = get_workspace_crates(&meta);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "local");
    }
}
