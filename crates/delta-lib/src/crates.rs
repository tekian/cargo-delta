use crate::cargo::CargoMetadata;
use normpath::PathExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crates {
    crates: BTreeMap<String, Vec<String>>,
}

pub fn parse(metadata: &CargoMetadata) -> Result<Crates> {
    let workspace: HashSet<&str> = metadata.workspace_members.iter().map(String::as_str).collect();
    let workspace_packages: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| workspace.contains(package.id.as_str()))
        .collect();
    let mut dependencies = BTreeMap::new();

    for package_id in &metadata.workspace_members {
        let _ = dependencies.insert(package_id.clone(), Vec::new());
    }

    for package in &workspace_packages {
        let package_deps = dependencies
            .get_mut(&package.id)
            .ok_or_else(|| Error::Other(format!("Cargo package '{}' is not a workspace member", package.id)))?;
        for dependency in &package.dependencies {
            if dependency.source.is_some() {
                continue;
            }

            let matches: Vec<_> = workspace_packages
                .iter()
                .filter(|candidate| dependency_matches(dependency.path.as_deref(), &dependency.name, candidate))
                .collect();
            match matches.as_slice() {
                [] => {}
                [dependency_package] => package_deps.push(dependency_package.id.clone()),
                _ => {
                    return Err(Error::Other(format!(
                        "Workspace dependency '{}' of package '{}' does not map to exactly one Cargo package ID",
                        dependency.name, package.id
                    )));
                }
            }
        }
        package_deps.sort();
        package_deps.dedup();
    }

    Ok(Crates { crates: dependencies })
}

fn dependency_matches(dependency_path: Option<&Path>, dependency_name: &str, package: &crate::cargo::CargoCrate) -> bool {
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

impl Crates {
    pub fn get_dependencies(&self, package_id: &str) -> Option<&Vec<String>> {
        self.crates.get(package_id)
    }

    pub fn get_dependents(&self, package_id: &str) -> Option<Vec<String>> {
        if !self.crates.contains_key(package_id) {
            return None;
        }

        let mut dependents = Vec::new();

        for (name, deps) in &self.crates {
            if deps.iter().any(|dependency| dependency == package_id) {
                dependents.push(name.clone());
            }
        }

        Some(dependents)
    }

    pub fn get_dependencies_transitive(&self, package_id: &str) -> Option<Vec<String>> {
        if !self.crates.contains_key(package_id) {
            return None;
        }

        let mut all_dependencies = HashSet::new();
        let mut to_visit = vec![package_id.to_string()];
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

    pub fn get_dependents_transitive(&self, package_id: &str) -> Option<Vec<String>> {
        if !self.crates.contains_key(package_id) {
            return None;
        }

        let mut all_dependents = HashSet::new();
        let mut to_visit = vec![package_id.to_string()];
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
        self.crates.len()
    }

    pub fn get_all_package_ids(&self) -> Vec<String> {
        self.crates.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_crates(deps: &[(&str, &[&str])]) -> Crates {
        let mut crates = BTreeMap::new();
        for (name, dep_list) in deps {
            let _ = crates.insert((*name).to_string(), dep_list.iter().map(|d| (*d).to_string()).collect());
        }
        Crates { crates }
    }

    #[test]
    fn get_dependencies_returns_direct_deps() {
        let c = make_crates(&[("app", &["lib-a", "lib-b"]), ("lib-a", &[]), ("lib-b", &[])]);
        let deps = c.get_dependencies("app").unwrap();
        assert_eq!(deps, &["lib-a".to_string(), "lib-b".to_string()]);
    }

    #[test]
    fn get_dependencies_returns_none_for_unknown() {
        let c = make_crates(&[("app", &[])]);
        assert!(c.get_dependencies("nonexistent").is_none());
    }

    #[test]
    fn get_dependents_finds_reverse_deps() {
        let c = make_crates(&[("app", &["lib"]), ("cli", &["lib"]), ("lib", &[])]);
        let mut dependents = c.get_dependents("lib").unwrap();
        dependents.sort();
        assert_eq!(dependents, vec!["app", "cli"]);
    }

    #[test]
    fn get_dependents_returns_none_for_unknown() {
        let c = make_crates(&[("app", &[])]);
        assert!(c.get_dependents("nonexistent").is_none());
    }

    #[test]
    fn get_dependents_returns_empty_for_root() {
        let c = make_crates(&[("app", &["lib"]), ("lib", &[])]);
        let dependents = c.get_dependents("app").unwrap();
        assert!(dependents.is_empty());
    }

    #[test]
    fn get_dependencies_transitive_walks_chain() {
        // app -> lib-a -> lib-b -> lib-c
        let c = make_crates(&[("app", &["lib-a"]), ("lib-a", &["lib-b"]), ("lib-b", &["lib-c"]), ("lib-c", &[])]);
        let mut deps = c.get_dependencies_transitive("app").unwrap();
        deps.sort();
        assert_eq!(deps, vec!["lib-a", "lib-b", "lib-c"]);
    }

    #[test]
    fn get_dependencies_transitive_handles_diamond() {
        // app -> (a, b), a -> c, b -> c
        let c = make_crates(&[("app", &["a", "b"]), ("a", &["c"]), ("b", &["c"]), ("c", &[])]);
        let mut deps = c.get_dependencies_transitive("app").unwrap();
        deps.sort();
        assert_eq!(deps, vec!["a", "b", "c"]);
    }

    #[test]
    fn get_dependencies_transitive_returns_none_for_unknown() {
        let c = make_crates(&[("app", &[])]);
        assert!(c.get_dependencies_transitive("nonexistent").is_none());
    }

    #[test]
    fn get_dependents_transitive_walks_chain() {
        // a -> b -> c (so dependents of a: b, c)
        let c = make_crates(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        let mut deps = c.get_dependents_transitive("a").unwrap();
        deps.sort();
        assert_eq!(deps, vec!["b", "c"]);
    }

    #[test]
    fn get_dependents_transitive_returns_none_for_unknown() {
        let c = make_crates(&[("app", &[])]);
        assert!(c.get_dependents_transitive("nonexistent").is_none());
    }

    #[test]
    fn len_returns_crate_count() {
        let c = make_crates(&[("a", &[]), ("b", &[]), ("c", &[])]);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn get_all_package_ids_returns_all() {
        let c = make_crates(&[("alpha", &[]), ("beta", &[])]);
        let mut names = c.get_all_package_ids();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }
}
