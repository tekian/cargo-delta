//! Cargo package dependency and input graph.

use crate::cargo::{self, CargoDependency, CargoMetadata};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

pub type PackageId = String;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Packages {
    packages: HashMap<PackageId, Package>,
    #[serde(default)]
    resolution_complete: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    pub workspace_dependencies: Vec<PackageId>,
    pub external_dependencies: Vec<ExternalPackageId>,
    pub dependency_declarations: Vec<DependencyDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExternalPackageId {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DependencyDeclaration {
    pub name: String,
    pub rename: Option<String>,
    pub requirement: String,
    pub source: Option<String>,
    pub path: Option<PathBuf>,
    pub kind: Option<String>,
    pub target: Option<String>,
    pub optional: bool,
    pub default_features: bool,
    pub features: Vec<String>,
    pub registry: Option<String>,
}

pub fn package_id(name: &str, version: &str) -> PackageId {
    format!("{name}@{version}")
}

pub fn package_name(package: &PackageId) -> &str {
    package.rsplit_once('@').map_or(package, |(name, _)| name)
}

pub fn parse(metadata: &CargoMetadata) -> Packages {
    let workspace_packages = cargo::get_workspace_packages(metadata);
    let workspace_ids: HashMap<&str, PackageId> = workspace_packages
        .iter()
        .map(|package| (package.id.as_str(), package_id(&package.name, &package.version)))
        .collect();
    let workspace_names: HashMap<&str, PackageId> = workspace_packages
        .iter()
        .map(|package| (package.name.as_str(), package_id(&package.name, &package.version)))
        .collect();
    let packages_by_id: HashMap<&str, &cargo::CargoPackage> =
        metadata.packages.iter().map(|package| (package.id.as_str(), package)).collect();
    let nodes: HashMap<&str, &cargo::CargoResolveNode> = metadata
        .resolve
        .as_ref()
        .map(|resolve| resolve.nodes.iter().map(|node| (node.id.as_str(), node)).collect())
        .unwrap_or_default();

    let mut packages = HashMap::new();
    for package in workspace_packages {
        let mut workspace_dependencies = BTreeSet::new();
        let mut external_dependencies = BTreeSet::new();
        if let Some(node) = nodes.get(package.id.as_str()) {
            let mut pending = node.deps.iter().map(|dependency| dependency.pkg.as_str()).collect::<Vec<_>>();
            let mut visited = HashSet::new();
            while let Some(dependency) = pending.pop() {
                if !visited.insert(dependency) {
                    continue;
                }
                if let Some(workspace_id) = workspace_ids.get(dependency) {
                    let _ = workspace_dependencies.insert(workspace_id.clone());
                    continue;
                }
                if let Some(external) = packages_by_id.get(dependency) {
                    let _ = external_dependencies.insert(ExternalPackageId {
                        name: external.name.clone(),
                        version: external.version.clone(),
                        source: external.source.clone(),
                    });
                }
                if let Some(node) = nodes.get(dependency) {
                    pending.extend(node.deps.iter().map(|dependency| dependency.pkg.as_str()));
                }
            }
        } else {
            for dependency in &package.dependencies {
                if dependency.source.is_none()
                    && let Some(workspace_id) = workspace_names.get(dependency.name.as_str())
                {
                    let _ = workspace_dependencies.insert(workspace_id.clone());
                }
            }
        }
        let mut dependency_declarations = package
            .dependencies
            .iter()
            .map(|dependency| dependency_declaration(dependency, &metadata.workspace_root))
            .collect::<Vec<_>>();
        dependency_declarations.sort();
        let id = package_id(&package.name, &package.version);
        let _ = packages.insert(
            id,
            Package {
                workspace_dependencies: workspace_dependencies.into_iter().collect(),
                external_dependencies: external_dependencies.into_iter().collect(),
                dependency_declarations,
            },
        );
    }
    Packages {
        packages,
        resolution_complete: metadata.resolve.is_some(),
    }
}

impl Packages {
    pub fn get_dependencies(&self, package: &PackageId) -> Option<&Vec<PackageId>> {
        self.packages.get(package).map(|package| &package.workspace_dependencies)
    }

    pub fn get_dependents(&self, package: &PackageId) -> Option<Vec<PackageId>> {
        if !self.packages.contains_key(package) {
            return None;
        }

        let mut dependents = Vec::new();

        for (name, record) in &self.packages {
            if record.workspace_dependencies.contains(package) {
                dependents.push(name.clone());
            }
        }

        Some(dependents)
    }

    pub fn get_dependencies_transitive(&self, package: &PackageId) -> Option<Vec<PackageId>> {
        if !self.packages.contains_key(package) {
            return None;
        }

        let mut all_dependencies = HashSet::new();
        let mut to_visit = vec![package.clone()];
        let mut visited = HashSet::new();

        while let Some(current_crate) = to_visit.pop() {
            if visited.contains(&current_crate) {
                continue;
            }
            let _ = visited.insert(current_crate.clone());

            if let Some(dependencies) = self.get_dependencies(&current_crate) {
                for dependency in dependencies {
                    if all_dependencies.insert(dependency.clone()) {
                        to_visit.push(dependency.clone());
                    }
                }
            }
        }

        Some(all_dependencies.into_iter().collect())
    }

    pub fn get_dependents_transitive(&self, package: &PackageId) -> Option<Vec<PackageId>> {
        if !self.packages.contains_key(package) {
            return None;
        }

        let mut all_dependents = HashSet::new();
        let mut to_visit = vec![package.clone()];
        let mut visited = HashSet::new();

        while let Some(current_crate) = to_visit.pop() {
            if visited.contains(&current_crate) {
                continue;
            }
            let _ = visited.insert(current_crate.clone());

            if let Some(dependents) = self.get_dependents(&current_crate) {
                for dependent in dependents {
                    if all_dependents.insert(dependent.clone()) {
                        to_visit.push(dependent.clone());
                    }
                }
            }
        }

        Some(all_dependents.into_iter().collect())
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    pub fn get_all_package_ids(&self) -> Vec<PackageId> {
        self.packages.keys().cloned().collect()
    }

    pub fn find_by_name(&self, package: &PackageId) -> Option<PackageId> {
        let name = package_name(package);
        self.packages.keys().find(|candidate| package_name(candidate) == name).cloned()
    }

    pub fn consumers_of_external(&self, dependencies: &HashSet<ExternalPackageId>) -> HashSet<PackageId> {
        self.packages
            .iter()
            .filter(|(_id, package)| {
                package
                    .external_dependencies
                    .iter()
                    .any(|dependency| dependencies.contains(dependency))
            })
            .map(|(id, _package)| id.clone())
            .collect()
    }

    pub fn changed_declarations(&self, baseline: &Self) -> HashSet<PackageId> {
        self.packages
            .iter()
            .filter(|(id, current)| {
                baseline
                    .find_record_by_name(id)
                    .is_none_or(|previous| previous.dependency_declarations != current.dependency_declarations)
            })
            .map(|(id, _package)| id.clone())
            .collect()
    }

    pub const fn resolution_complete(&self) -> bool {
        self.resolution_complete
    }

    pub fn removed_since(&self, current: &Self) -> HashSet<PackageId> {
        self.packages
            .keys()
            .filter(|id| current.find_record_by_name(id).is_none())
            .cloned()
            .collect()
    }

    fn find_record_by_name(&self, package: &PackageId) -> Option<&Package> {
        let name = package_name(package);
        self.packages
            .iter()
            .find(|(candidate, _record)| package_name(candidate) == name)
            .map(|(_id, record)| record)
    }
}

fn dependency_declaration(dependency: &CargoDependency, workspace_root: &std::path::Path) -> DependencyDeclaration {
    let mut features = dependency.features.clone();
    features.sort();
    DependencyDeclaration {
        name: dependency.name.clone(),
        rename: dependency.rename.clone(),
        requirement: dependency.req.clone(),
        source: dependency.source.clone(),
        path: dependency.path.as_ref().map(|path| {
            path.strip_prefix(workspace_root)
                .map_or_else(|_error| path.clone(), std::path::Path::to_path_buf)
        }),
        kind: dependency.kind.clone(),
        target: dependency.target.clone(),
        optional: dependency.optional,
        default_features: dependency.uses_default_features,
        features,
        registry: dependency.registry.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> PackageId {
        package_id(name, "0.1.0")
    }

    fn make_packages(deps: &[(&str, &[&str])]) -> Packages {
        let mut packages = HashMap::new();
        for (name, dep_list) in deps {
            let _ = packages.insert(
                id(name),
                Package {
                    workspace_dependencies: dep_list.iter().map(|dependency| id(dependency)).collect(),
                    ..Package::default()
                },
            );
        }
        Packages {
            packages,
            resolution_complete: false,
        }
    }

    #[test]
    fn get_dependencies_returns_direct_deps() {
        let c = make_packages(&[("app", &["lib-a", "lib-b"]), ("lib-a", &[]), ("lib-b", &[])]);
        let deps = c.get_dependencies(&id("app")).unwrap();
        assert_eq!(deps, &[id("lib-a"), id("lib-b")]);
    }

    #[test]
    fn get_dependencies_returns_none_for_unknown() {
        let c = make_packages(&[("app", &[])]);
        assert!(c.get_dependencies(&id("nonexistent")).is_none());
    }

    #[test]
    fn get_dependents_finds_reverse_deps() {
        let c = make_packages(&[("app", &["lib"]), ("cli", &["lib"]), ("lib", &[])]);
        let mut dependents = c.get_dependents(&id("lib")).unwrap();
        dependents.sort();
        assert_eq!(dependents, vec![id("app"), id("cli")]);
    }

    #[test]
    fn get_dependents_returns_none_for_unknown() {
        let c = make_packages(&[("app", &[])]);
        assert!(c.get_dependents(&id("nonexistent")).is_none());
    }

    #[test]
    fn get_dependents_returns_empty_for_root() {
        let c = make_packages(&[("app", &["lib"]), ("lib", &[])]);
        let dependents = c.get_dependents(&id("app")).unwrap();
        assert!(dependents.is_empty());
    }

    #[test]
    fn get_dependencies_transitive_walks_chain() {
        // app -> lib-a -> lib-b -> lib-c
        let c = make_packages(&[("app", &["lib-a"]), ("lib-a", &["lib-b"]), ("lib-b", &["lib-c"]), ("lib-c", &[])]);
        let mut deps = c.get_dependencies_transitive(&id("app")).unwrap();
        deps.sort();
        assert_eq!(deps, vec![id("lib-a"), id("lib-b"), id("lib-c")]);
    }

    #[test]
    fn get_dependencies_transitive_handles_diamond() {
        // app -> (a, b), a -> c, b -> c
        let c = make_packages(&[("app", &["a", "b"]), ("a", &["c"]), ("b", &["c"]), ("c", &[])]);
        let mut deps = c.get_dependencies_transitive(&id("app")).unwrap();
        deps.sort();
        assert_eq!(deps, vec![id("a"), id("b"), id("c")]);
    }

    #[test]
    fn get_dependencies_transitive_returns_none_for_unknown() {
        let c = make_packages(&[("app", &[])]);
        assert!(c.get_dependencies_transitive(&id("nonexistent")).is_none());
    }

    #[test]
    fn get_dependents_transitive_walks_chain() {
        // a -> b -> c (so dependents of a: b, c)
        let c = make_packages(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        let mut deps = c.get_dependents_transitive(&id("a")).unwrap();
        deps.sort();
        assert_eq!(deps, vec![id("b"), id("c")]);
    }

    #[test]
    fn get_dependents_transitive_returns_none_for_unknown() {
        let c = make_packages(&[("app", &[])]);
        assert!(c.get_dependents_transitive(&id("nonexistent")).is_none());
    }

    #[test]
    fn resolved_external_dependencies_stop_at_workspace_boundaries() {
        let metadata: CargoMetadata = serde_json::from_value(serde_json::json!({
            "packages": [
                {
                    "id": "core",
                    "name": "core",
                    "version": "0.1.0",
                    "source": null,
                    "targets": [],
                    "manifest_path": "core/Cargo.toml",
                    "dependencies": [{"name": "external", "source": "registry+index"}]
                },
                {
                    "id": "app",
                    "name": "app",
                    "version": "0.1.0",
                    "source": null,
                    "targets": [],
                    "manifest_path": "app/Cargo.toml",
                    "dependencies": [{"name": "core", "source": null}]
                },
                {
                    "id": "external",
                    "name": "external",
                    "version": "1.0.0",
                    "source": "registry+index",
                    "targets": [],
                    "manifest_path": "external/Cargo.toml",
                    "dependencies": []
                },
                {
                    "id": "transitive",
                    "name": "transitive",
                    "version": "2.0.0",
                    "source": "registry+index",
                    "targets": [],
                    "manifest_path": "transitive/Cargo.toml",
                    "dependencies": []
                }
            ],
            "workspace_root": ".",
            "target_directory": "target",
            "workspace_members": ["core", "app"],
            "resolve": {
                "nodes": [
                    {"id": "core", "deps": [{"name": "external", "pkg": "external"}]},
                    {"id": "app", "deps": [{"name": "core", "pkg": "core"}]},
                    {"id": "external", "deps": [{"name": "transitive", "pkg": "transitive"}]},
                    {"id": "transitive", "deps": []}
                ]
            }
        }))
        .unwrap();

        let packages = parse(&metadata);
        let core = &packages.packages[&id("core")];
        let app = &packages.packages[&id("app")];

        assert_eq!(app.workspace_dependencies, [id("core")]);
        assert!(app.external_dependencies.is_empty());
        assert_eq!(
            core.external_dependencies,
            [
                ExternalPackageId {
                    name: "external".to_string(),
                    version: "1.0.0".to_string(),
                    source: Some("registry+index".to_string()),
                },
                ExternalPackageId {
                    name: "transitive".to_string(),
                    version: "2.0.0".to_string(),
                    source: Some("registry+index".to_string()),
                }
            ]
        );
    }

    #[test]
    fn len_returns_package_count() {
        let c = make_packages(&[("a", &[]), ("b", &[]), ("c", &[])]);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn get_all_package_ids_returns_all() {
        let c = make_packages(&[("alpha", &[]), ("beta", &[])]);
        let mut names = c.get_all_package_ids();
        names.sort();
        assert_eq!(names, vec![id("alpha"), id("beta")]);
    }
}
