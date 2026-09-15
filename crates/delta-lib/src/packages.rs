use crate::cargo::CargoPackage;
use crate::error::{Error, Result};
use normpath::PathExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageId(String);

impl PackageId {
    pub fn new(name: &str, version: &str) -> Self {
        Self(format!("{name}@{version}"))
    }

    pub fn name(&self) -> Result<&str> {
        self.0
            .rsplit_once('@')
            .filter(|(name, version)| !name.is_empty() && !version.is_empty())
            .map(|(name, _)| name)
            .ok_or_else(|| Error::Other(format!("Invalid package ID '{}'; expected name@version", self.0)))
    }

    pub fn validate(&self) -> Result<()> {
        let _ = self.name()?;
        Ok(())
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Packages {
    packages: BTreeMap<PackageId, Vec<PackageId>>,
}

pub fn parse(workspace_packages: &[&CargoPackage]) -> Result<Packages> {
    let mut packages = BTreeMap::new();
    for package in workspace_packages {
        let package_id = PackageId::new(&package.name, &package.version);
        if packages.insert(package_id.clone(), Vec::new()).is_some() {
            return Err(Error::Other(format!("Workspace contains duplicate package ID '{package_id}'")));
        }
    }

    for package in workspace_packages {
        let package_id = PackageId::new(&package.name, &package.version);
        let package_dependencies = packages
            .get_mut(&package_id)
            .ok_or_else(|| Error::Other(format!("Package '{package_id}' is not a workspace member")))?;
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
                [dependency_package] => {
                    package_dependencies.push(PackageId::new(&dependency_package.name, &dependency_package.version));
                }
                _ => {
                    return Err(Error::Other(format!(
                        "Workspace dependency '{}' of package '{}' does not map to exactly one workspace package",
                        dependency.name, package.name
                    )));
                }
            }
        }
        package_dependencies.sort();
        package_dependencies.dedup();
    }

    Ok(Packages { packages })
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

impl Packages {
    pub fn validate(&self) -> Result<()> {
        for (package, dependencies) in &self.packages {
            package.validate()?;
            for dependency in dependencies {
                dependency.validate()?;
                if !self.packages.contains_key(dependency) {
                    return Err(Error::Other(format!("Package graph refers to unknown dependency '{dependency}'")));
                }
            }
        }
        Ok(())
    }

    pub fn contains(&self, package: &PackageId) -> bool {
        self.packages.contains_key(package)
    }

    pub fn get_dependencies(&self, package: &PackageId) -> Option<&Vec<PackageId>> {
        self.packages.get(package)
    }

    pub fn get_dependents(&self, package: &PackageId) -> Option<Vec<PackageId>> {
        if !self.packages.contains_key(package) {
            return None;
        }

        Some(
            self.packages
                .iter()
                .filter(|(_, dependencies)| dependencies.contains(package))
                .map(|(dependent, _)| dependent.clone())
                .collect(),
        )
    }

    pub fn get_dependencies_transitive(&self, package: &PackageId) -> Option<Vec<PackageId>> {
        if !self.packages.contains_key(package) {
            return None;
        }

        let mut all_dependencies = HashSet::new();
        let mut to_visit = vec![package.clone()];
        let mut visited = HashSet::new();

        while let Some(current_package) = to_visit.pop() {
            if !visited.insert(current_package.clone()) {
                continue;
            }

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

    pub fn get_dependents_transitive(&self, package: &PackageId) -> Option<Vec<PackageId>> {
        if !self.packages.contains_key(package) {
            return None;
        }

        let mut all_dependents = HashSet::new();
        let mut to_visit = vec![package.clone()];
        let mut visited = HashSet::new();

        while let Some(current_package) = to_visit.pop() {
            if !visited.insert(current_package.clone()) {
                continue;
            }

            if let Some(dependents) = self.get_dependents(&current_package) {
                for dependent in dependents {
                    if all_dependents.insert(dependent.clone()) {
                        to_visit.push(dependent);
                    }
                }
            }
        }

        Some(all_dependents.into_iter().collect())
    }

    pub fn names(&self, selected: &HashSet<PackageId>) -> Result<Vec<String>> {
        let mut names = self
            .selected(selected)?
            .into_iter()
            .map(PackageId::name)
            .collect::<Result<HashSet<_>>>()?
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        names.sort();
        Ok(names)
    }

    pub fn specs(&self, selected: &HashSet<PackageId>) -> Result<Vec<String>> {
        Ok(self.selected(selected)?.into_iter().map(ToString::to_string).collect())
    }

    fn selected(&self, selected: &HashSet<PackageId>) -> Result<Vec<&PackageId>> {
        let mut packages = selected
            .iter()
            .map(|package| {
                self.packages
                    .get_key_value(package)
                    .map(|(package, _)| package)
                    .ok_or_else(|| Error::Other(format!("Impact result refers to unknown package '{package}'")))
            })
            .collect::<Result<Vec<_>>>()?;
        packages.sort();
        Ok(packages)
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    pub fn get_all(&self) -> Vec<PackageId> {
        self.packages.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> PackageId {
        PackageId::new(name, "0.1.0")
    }

    fn make_packages(dependencies: &[(&str, &[&str])]) -> Packages {
        Packages {
            packages: dependencies
                .iter()
                .map(|(name, dependencies)| (id(name), dependencies.iter().map(|dependency| id(dependency)).collect()))
                .collect(),
        }
    }

    #[test]
    fn package_id_exposes_name() {
        assert_eq!(PackageId::new("my-package", "1.2.3").name().unwrap(), "my-package");
    }

    #[test]
    fn package_id_rejects_invalid_value() {
        assert!(PackageId("missing-version".to_string()).validate().is_err());
    }

    #[test]
    fn get_dependencies_returns_direct_dependencies() {
        let packages = make_packages(&[("app", &["lib-a", "lib-b"]), ("lib-a", &[]), ("lib-b", &[])]);
        assert_eq!(packages.get_dependencies(&id("app")).unwrap(), &[id("lib-a"), id("lib-b")]);
    }

    #[test]
    fn get_dependencies_returns_none_for_unknown() {
        assert!(make_packages(&[("app", &[])]).get_dependencies(&id("missing")).is_none());
    }

    #[test]
    fn get_dependents_finds_reverse_dependencies() {
        let packages = make_packages(&[("app", &["lib"]), ("cli", &["lib"]), ("lib", &[])]);
        let mut dependents = packages.get_dependents(&id("lib")).unwrap();
        dependents.sort();
        assert_eq!(dependents, vec![id("app"), id("cli")]);
    }

    #[test]
    fn get_dependents_returns_none_for_unknown() {
        assert!(make_packages(&[("app", &[])]).get_dependents(&id("missing")).is_none());
    }

    #[test]
    fn get_dependents_returns_empty_for_root() {
        assert!(
            make_packages(&[("app", &["lib"]), ("lib", &[])])
                .get_dependents(&id("app"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn get_dependencies_transitive_walks_chain() {
        let packages = make_packages(&[("app", &["lib-a"]), ("lib-a", &["lib-b"]), ("lib-b", &["lib-c"]), ("lib-c", &[])]);
        let mut dependencies = packages.get_dependencies_transitive(&id("app")).unwrap();
        dependencies.sort();
        assert_eq!(dependencies, vec![id("lib-a"), id("lib-b"), id("lib-c")]);
    }

    #[test]
    fn get_dependencies_transitive_handles_diamond() {
        let packages = make_packages(&[("app", &["a", "b"]), ("a", &["c"]), ("b", &["c"]), ("c", &[])]);
        let mut dependencies = packages.get_dependencies_transitive(&id("app")).unwrap();
        dependencies.sort();
        assert_eq!(dependencies, vec![id("a"), id("b"), id("c")]);
    }

    #[test]
    fn get_dependencies_transitive_returns_none_for_unknown() {
        assert!(make_packages(&[("app", &[])]).get_dependencies_transitive(&id("missing")).is_none());
    }

    #[test]
    fn get_dependents_transitive_walks_chain() {
        let packages = make_packages(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        let mut dependents = packages.get_dependents_transitive(&id("a")).unwrap();
        dependents.sort();
        assert_eq!(dependents, vec![id("b"), id("c")]);
    }

    #[test]
    fn get_dependents_transitive_returns_none_for_unknown() {
        assert!(make_packages(&[("app", &[])]).get_dependents_transitive(&id("missing")).is_none());
    }

    #[test]
    fn len_returns_package_count() {
        assert_eq!(make_packages(&[("a", &[]), ("b", &[]), ("c", &[])]).len(), 3);
    }

    #[test]
    fn get_all_returns_all_packages() {
        let mut packages = make_packages(&[("alpha", &[]), ("beta", &[])]).get_all();
        packages.sort();
        assert_eq!(packages, vec![id("alpha"), id("beta")]);
    }
}
