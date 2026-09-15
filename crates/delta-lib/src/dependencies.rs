use crate::cargo::CargoPackage;
use normpath::PathExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageDependencies {
    dependencies: BTreeMap<String, Vec<String>>,
}

pub fn parse(packages: &[&CargoPackage], git_root: &Path) -> Result<PackageDependencies> {
    let workspace_packages = packages
        .iter()
        .map(|package| Ok((*package, crate::package_manifest_path(package, git_root)?)))
        .collect::<Result<Vec<_>>>()?;
    let mut dependencies = BTreeMap::new();

    for (_, manifest_path) in &workspace_packages {
        let _ = dependencies.insert(manifest_path.clone(), Vec::new());
    }

    for (package, manifest_path) in &workspace_packages {
        let package_deps = dependencies
            .get_mut(manifest_path)
            .ok_or_else(|| Error::Other(format!("Package manifest '{manifest_path}' is not a workspace member")))?;
        for dependency in &package.dependencies {
            if dependency.source.is_some() {
                continue;
            }

            let matches: Vec<_> = workspace_packages
                .iter()
                .filter(|(candidate, _)| dependency_matches(dependency.path.as_deref(), &dependency.name, candidate))
                .collect();
            match matches.as_slice() {
                [] => {}
                [(_, dependency_manifest)] => package_deps.push(dependency_manifest.clone()),
                _ => {
                    return Err(Error::Other(format!(
                        "Workspace dependency '{}' of package manifest '{}' does not map to exactly one workspace package",
                        dependency.name, manifest_path
                    )));
                }
            }
        }
        package_deps.sort();
        package_deps.dedup();
    }

    Ok(PackageDependencies { dependencies })
}

fn dependency_matches(dependency_path: Option<&Path>, dependency_name: &str, package: &CargoPackage) -> bool {
    if let Some(dependency_path) = dependency_path {
        let Some(package_directory) = package.manifest_path.parent() else {
            return false;
        };
        return normalized_or_original(dependency_path) == normalized_or_original(package_directory);
    }
    package.name == dependency_name
}

fn normalized_or_original(path: &Path) -> PathBuf {
    path.normalize()
        .map_or_else(|_| path.to_path_buf(), normpath::BasePathBuf::into_path_buf)
}

impl PackageDependencies {
    pub fn get_dependencies(&self, package_manifest: &str) -> Option<&Vec<String>> {
        self.dependencies.get(package_manifest)
    }

    pub fn get_dependents(&self, package_manifest: &str) -> Option<Vec<String>> {
        if !self.dependencies.contains_key(package_manifest) {
            return None;
        }

        let mut dependents = Vec::new();

        for (name, deps) in &self.dependencies {
            if deps.iter().any(|dependency| dependency == package_manifest) {
                dependents.push(name.clone());
            }
        }

        Some(dependents)
    }

    pub fn get_dependencies_transitive(&self, package_manifest: &str) -> Option<Vec<String>> {
        if !self.dependencies.contains_key(package_manifest) {
            return None;
        }

        let mut all_dependencies = HashSet::new();
        let mut to_visit = vec![package_manifest.to_string()];
        let mut visited = HashSet::new();

        while let Some(current_package) = to_visit.pop() {
            if visited.contains(&current_package) {
                continue;
            }
            let _ = visited.insert(current_package.clone());

            if let Some(dependencies) = self.get_dependencies(&current_package) {
                for dependency in dependencies {
                    if all_dependencies.insert(dependency.clone()) {
                        to_visit.push(dependency.clone());
                    }
                }
            }
        }

        Some(all_dependencies.into_iter().collect())
    }

    pub fn get_dependents_transitive(&self, package_manifest: &str) -> Option<Vec<String>> {
        if !self.dependencies.contains_key(package_manifest) {
            return None;
        }

        let mut all_dependents = HashSet::new();
        let mut to_visit = vec![package_manifest.to_string()];
        let mut visited = HashSet::new();

        while let Some(current_package) = to_visit.pop() {
            if visited.contains(&current_package) {
                continue;
            }
            let _ = visited.insert(current_package.clone());

            if let Some(dependents) = self.get_dependents(&current_package) {
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
        self.dependencies.len()
    }

    pub fn get_all_package_manifests(&self) -> Vec<String> {
        self.dependencies.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dependencies(deps: &[(&str, &[&str])]) -> PackageDependencies {
        let mut dependencies = BTreeMap::new();
        for (name, dep_list) in deps {
            let _ = dependencies.insert((*name).to_string(), dep_list.iter().map(|d| (*d).to_string()).collect());
        }
        PackageDependencies { dependencies }
    }

    #[test]
    fn get_dependencies_returns_direct_deps() {
        let c = make_dependencies(&[("app", &["lib-a", "lib-b"]), ("lib-a", &[]), ("lib-b", &[])]);
        let deps = c.get_dependencies("app").unwrap();
        assert_eq!(deps, &["lib-a".to_string(), "lib-b".to_string()]);
    }

    #[test]
    fn get_dependencies_returns_none_for_unknown() {
        let c = make_dependencies(&[("app", &[])]);
        assert!(c.get_dependencies("nonexistent").is_none());
    }

    #[test]
    fn get_dependents_finds_reverse_deps() {
        let c = make_dependencies(&[("app", &["lib"]), ("cli", &["lib"]), ("lib", &[])]);
        let mut dependents = c.get_dependents("lib").unwrap();
        dependents.sort();
        assert_eq!(dependents, vec!["app", "cli"]);
    }

    #[test]
    fn get_dependents_returns_none_for_unknown() {
        let c = make_dependencies(&[("app", &[])]);
        assert!(c.get_dependents("nonexistent").is_none());
    }

    #[test]
    fn get_dependents_returns_empty_for_root() {
        let c = make_dependencies(&[("app", &["lib"]), ("lib", &[])]);
        let dependents = c.get_dependents("app").unwrap();
        assert!(dependents.is_empty());
    }

    #[test]
    fn get_dependencies_transitive_walks_chain() {
        // app -> lib-a -> lib-b -> lib-c
        let c = make_dependencies(&[("app", &["lib-a"]), ("lib-a", &["lib-b"]), ("lib-b", &["lib-c"]), ("lib-c", &[])]);
        let mut deps = c.get_dependencies_transitive("app").unwrap();
        deps.sort();
        assert_eq!(deps, vec!["lib-a", "lib-b", "lib-c"]);
    }

    #[test]
    fn get_dependencies_transitive_handles_diamond() {
        // app -> (a, b), a -> c, b -> c
        let c = make_dependencies(&[("app", &["a", "b"]), ("a", &["c"]), ("b", &["c"]), ("c", &[])]);
        let mut deps = c.get_dependencies_transitive("app").unwrap();
        deps.sort();
        assert_eq!(deps, vec!["a", "b", "c"]);
    }

    #[test]
    fn get_dependencies_transitive_returns_none_for_unknown() {
        let c = make_dependencies(&[("app", &[])]);
        assert!(c.get_dependencies_transitive("nonexistent").is_none());
    }

    #[test]
    fn get_dependents_transitive_walks_chain() {
        // a -> b -> c (so dependents of a: b, c)
        let c = make_dependencies(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        let mut deps = c.get_dependents_transitive("a").unwrap();
        deps.sort();
        assert_eq!(deps, vec!["b", "c"]);
    }

    #[test]
    fn get_dependents_transitive_returns_none_for_unknown() {
        let c = make_dependencies(&[("app", &[])]);
        assert!(c.get_dependents_transitive("nonexistent").is_none());
    }

    #[test]
    fn len_returns_package_count() {
        let c = make_dependencies(&[("a", &[]), ("b", &[]), ("c", &[])]);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn get_all_package_manifests_returns_all() {
        let c = make_dependencies(&[("alpha", &[]), ("beta", &[])]);
        let mut names = c.get_all_package_manifests();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }
}
