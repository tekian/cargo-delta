//! Cargo package dependency graph.

use crate::cargo::CargoMetadata;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub type PackageId = String;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Packages {
    packages: HashMap<PackageId, Vec<PackageId>>,
}

pub fn package_id(name: &str, version: &str) -> PackageId {
    format!("{name}@{version}")
}

pub fn package_name(package: &PackageId) -> &str {
    package.rsplit_once('@').map_or(package, |(name, _)| name)
}

pub fn parse(metadata: &CargoMetadata) -> Packages {
    let mut workspace = HashMap::new();
    let mut dependencies = HashMap::new();

    for package in &metadata.packages {
        if package.source.is_some() {
            continue;
        }
        let id = package_id(&package.name, &package.version);
        let _ = workspace.insert(package.name.clone(), id.clone());
        let _ = dependencies.insert(id, Vec::new());
    }

    for package in &metadata.packages {
        if package.source.is_some() {
            continue;
        }

        for dep in &package.dependencies {
            let Some(dependency_id) = workspace.get(&dep.name) else {
                continue;
            };
            if dep.source.is_some() {
                continue;
            }

            let id = package_id(&package.name, &package.version);
            let package_deps = dependencies.get_mut(&id).unwrap();

            if !package_deps.contains(dependency_id) {
                package_deps.push(dependency_id.clone());
            }
        }
    }

    Packages { packages: dependencies }
}

impl Packages {
    pub fn get_dependencies(&self, package: &PackageId) -> Option<&Vec<PackageId>> {
        self.packages.get(package)
    }

    pub fn get_dependents(&self, package: &PackageId) -> Option<Vec<PackageId>> {
        if !self.packages.contains_key(package) {
            return None;
        }

        let mut dependents = Vec::new();

        for (name, deps) in &self.packages {
            if deps.contains(package) {
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
            let _ = packages.insert(id(name), dep_list.iter().map(|dependency| id(dependency)).collect());
        }
        Packages { packages }
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
