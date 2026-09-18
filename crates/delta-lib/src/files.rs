use glob::glob;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt, fs,
    path::{Path, PathBuf},
};
use syn::visit::Visit;

use crate::{
    cargo::{CargoMetadata, CargoPackage},
    config::{MainConfig, ParserConfig},
    crates::{PackageId, package_id},
    error::Result,
    host::Host,
    utils,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    Workspace,     // Top-level Cargo.toml with [workspace]
    Package,       // Package-level Cargo.toml
    Target,        // Target entry point (bin, lib, etc.)
    Module,        // File resolved by mod declaration
    ModulePath,    // File resolved by #[path = "..."]
    MacroInclude,  // File resolved by include! macro
    FileReference, // File resolved by method calls
    Assume,        // File resolved by assume pattern matching
    Unset,         // Unset kind, used for root nodes
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace => write!(f, "Workspace"),
            Self::Package => write!(f, "Package"),
            Self::Target => write!(f, "Target"),
            Self::Module => write!(f, "Module"),
            Self::ModulePath => write!(f, "ModulePath"),
            Self::MacroInclude => write!(f, "MacroInclude"),
            Self::FileReference => write!(f, "FileReference"),
            Self::Assume => write!(f, "Assume"),
            Self::Unset => write!(f, "Unset"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[expect(clippy::use_self, reason = "Self cannot be used in struct field definitions")]
pub struct FileNode {
    pub path: PathBuf,
    pub kind: FileKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageId>,
    pub children: Vec<FileNode>,
}

impl FileNode {
    pub const fn new(path: PathBuf, kind: FileKind) -> Self {
        Self {
            path,
            kind,
            package: None,
            children: Vec::new(),
        }
    }

    pub const fn for_package(path: PathBuf, package: PackageId) -> Self {
        Self {
            path,
            kind: FileKind::Package,
            package: Some(package),
            children: Vec::new(),
        }
    }

    pub fn add_child(&mut self, child: Self) {
        if !self.children.iter().any(|existing| existing.path == child.path) {
            self.children.push(child);
        }
    }

    pub fn make_relative_paths(&mut self, workspace_root: &Path) {
        let workspace_root = fs::canonicalize(workspace_root).unwrap_or_else(|_error| workspace_root.to_path_buf());
        self.make_relative_paths_from(&workspace_root);
    }

    fn make_relative_paths_from(&mut self, workspace_root: &Path) {
        let path = fs::canonicalize(&self.path).unwrap_or_else(|_error| self.path.clone());
        self.path = path
            .strip_prefix(workspace_root)
            .map_or_else(|_error| path.clone(), Path::to_path_buf);

        for child in &mut self.children {
            child.make_relative_paths_from(workspace_root);
        }
    }

    pub fn len(&self) -> usize {
        self.children.iter().map(|i| i.len() + 1).sum::<usize>() + 1
    }

    pub fn distinct(&self) -> HashSet<PathBuf> {
        let mut paths = HashSet::new();
        let _ = paths.insert(self.path.clone());

        for child in &self.children {
            paths.extend(child.distinct());
        }

        paths
    }

    pub fn find_packages_containing_file(&self, target_file: &PathBuf) -> Vec<PackageId> {
        fn visit(node: &FileNode, target_file: &PathBuf, current_package: Option<&PackageId>, results: &mut Vec<PackageId>) {
            let current_package = if matches!(node.kind, FileKind::Package) {
                node.package.as_ref()
            } else {
                current_package
            };

            if &node.path == target_file
                && let Some(package) = current_package
                && !results.contains(package)
            {
                results.push(package.clone());
            }

            for child in &node.children {
                visit(child, target_file, current_package, results);
            }
        }

        let mut results = Vec::new();
        visit(self, target_file, None, &mut results);
        results
    }
}

struct SourceVisitor<'a> {
    mods: Vec<String>,
    includes: Vec<String>,
    mod_paths: Vec<(String, String)>,
    nested_mods: Vec<(Vec<String>, String)>,
    current_path: Vec<String>,
    constants: HashMap<String, String>,
    file_refs: Vec<String>,
    config: &'a ParserConfig,
}

impl<'a> SourceVisitor<'a> {
    fn new(config: &'a ParserConfig) -> Self {
        Self {
            mods: Vec::new(),
            includes: Vec::new(),
            mod_paths: Vec::new(),
            nested_mods: Vec::new(),
            current_path: Vec::new(),
            constants: HashMap::new(),
            file_refs: Vec::new(),
            config,
        }
    }
}

impl<'ast> Visit<'ast> for SourceVisitor<'_> {
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        if let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(lit_str),
            ..
        }) = &*i.expr
        {
            let _ = self.constants.insert(i.ident.to_string(), lit_str.value());
        }
        syn::visit::visit_item_const(self, i);
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        if let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(lit_str),
            ..
        }) = &*i.expr
        {
            let _ = self.constants.insert(i.ident.to_string(), lit_str.value());
        }
        syn::visit::visit_item_static(self, i);
    }

    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        if !self.config.file_refs {
            syn::visit::visit_expr_call(self, i);
            return;
        }

        if let syn::Expr::Path(syn::ExprPath { path, .. }) = &*i.func
            && let Some(last) = path.segments.last()
        {
            let method = last.ident.to_string();
            if !self.config.file_methods.contains(&method) {
                syn::visit::visit_expr_call(self, i);
                return;
            }
            if let Some(first_arg) = i.args.first()
                && let Some(path) = self.expr_to_str(first_arg)
            {
                self.file_refs.push(path);
            }
        }

        syn::visit::visit_expr_call(self, i);
    }

    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        let mod_name = i.ident.to_string();
        self.current_path.push(mod_name.clone());

        if i.content.is_none() {
            if let Some(custom_path) = self.extract_path(&i.attrs) {
                self.mod_paths.push((mod_name, custom_path));
            } else if self.current_path.len() == 1 {
                self.mods.push(mod_name);
            } else {
                let parent = self.current_path.iter().take(self.current_path.len() - 1).cloned().collect();

                self.nested_mods.push((parent, mod_name));
            }
        }

        syn::visit::visit_item_mod(self, i);
        let _ = self.current_path.pop();
    }

    fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
        let Some(ident) = i.mac.path.get_ident() else {
            syn::visit::visit_item_macro(self, i);
            return;
        };

        let macro_name = ident.to_string();

        if self.config.mods && self.config.mod_macros.contains(&macro_name) {
            let tokens_str = i.mac.tokens.to_string();

            if let Some(first_arg) = tokens_str.split(',').next() {
                let mod_name = first_arg.trim().to_string();
                if !mod_name.is_empty() {
                    if self.current_path.is_empty() {
                        self.mods.push(mod_name);
                    } else {
                        let parent = self.current_path.clone();
                        self.nested_mods.push((parent, mod_name));
                    }
                }
            }
        }

        syn::visit::visit_item_macro(self, i);
    }

    fn visit_expr_macro(&mut self, i: &'ast syn::ExprMacro) {
        let Some(ident) = i.mac.path.get_ident() else {
            syn::visit::visit_expr_macro(self, i);
            return;
        };

        let macro_name = ident.to_string();

        if self.config.includes
            && self.config.include_macros.contains(&macro_name)
            && let Ok(syn::Lit::Str(lit_str)) = i.mac.parse_body::<syn::Lit>()
        {
            self.includes.push(lit_str.value());
        }

        syn::visit::visit_expr_macro(self, i);
    }
}

impl SourceVisitor<'_> {
    fn expr_to_str(&self, expr: &syn::Expr) -> Option<String> {
        match expr {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(lit_str),
                ..
            }) => Some(lit_str.value()),
            syn::Expr::Path(syn::ExprPath { path, .. }) => self.constants.get(&path.get_ident()?.to_string()).cloned(),
            _ => None,
        }
    }

    fn extract_path(&self, attrs: &[syn::Attribute]) -> Option<String> {
        for attr in attrs {
            if !attr.path().is_ident("path") {
                continue;
            }

            if let Ok(meta) = attr.meta.require_name_value()
                && let Some(path) = self.expr_to_str(&meta.value)
            {
                return Some(path);
            }
        }
        None
    }
}

fn parse_rust<'a>(path: &Path, config: &'a ParserConfig) -> Result<SourceVisitor<'a>> {
    let content = fs::read_to_string(path)?;
    let syntax = syn::parse_file(&content)?;

    let mut visitor = SourceVisitor::new(config);
    visitor.visit_file(&syntax);

    Ok(visitor)
}

fn resolve_mod_files(base: &Path, mods: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for module in mods {
        let mod_rs_path = base.join(format!("{module}/mod.rs"));
        let direct_rs_path = base.join(format!("{module}.rs"));

        if mod_rs_path.exists() {
            files.push(mod_rs_path);
            continue;
        }

        if direct_rs_path.exists() {
            files.push(direct_rs_path);
            continue;
        }

        let mod_dir = base.join(module);
        if !mod_dir.exists() || !mod_dir.is_dir() {
            continue;
        }

        let Ok(entries) = fs::read_dir(&mod_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|i| i.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files
}

fn build_file_node(
    host: &mut impl Host,
    file_path: &Path,
    visited: &mut HashSet<PathBuf>,
    workspace_root: Option<&Path>,
    main_config: &MainConfig,
    crate_name: &str,
) -> FileNode {
    let mut node = FileNode::new(file_path.to_path_buf(), FileKind::Unset);

    if visited.contains(file_path) {
        return node; // Avoid infinite recursion
    }

    let _ = visited.insert(file_path.to_path_buf());

    let config = main_config.crate_config(crate_name);
    let Ok(visitor) = parse_rust(file_path, &config) else {
        return node;
    };

    let Some(base_dir) = file_path.parent() else {
        return node;
    };

    let file_stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let maybe_mod_dir = base_dir.join(file_stem);
    let actual_base = if maybe_mod_dir.exists() && maybe_mod_dir.is_dir() {
        maybe_mod_dir
    } else {
        base_dir.to_path_buf()
    };

    if config.mods {
        let mod_files = resolve_mod_files(&actual_base, &visitor.mods);

        for mod_file in mod_files {
            let mut child_node = build_file_node(host, &mod_file, visited, workspace_root, main_config, crate_name);

            child_node.kind = FileKind::Module;
            node.add_child(child_node);
        }

        for (parent_path, nested_mod_name) in &visitor.nested_mods {
            let mut parent_dir = actual_base.clone();
            for component in parent_path {
                parent_dir = parent_dir.join(component);
            }

            let nested_mod_files = resolve_mod_files(&parent_dir, core::slice::from_ref(nested_mod_name));

            for mod_file in nested_mod_files {
                let mut child_node = build_file_node(host, &mod_file, visited, workspace_root, main_config, crate_name);

                child_node.kind = FileKind::Module;
                node.add_child(child_node);
            }
        }

        for (_, custom_path) in &visitor.mod_paths {
            if let Some(path) = utils::resolve(file_path, custom_path) {
                let child = FileNode::new(path, FileKind::ModulePath);
                node.add_child(child);
            }
        }
    }

    let includes = utils::resolve_includes(host, file_path, &visitor.includes);

    for include in includes {
        node.add_child(FileNode::new(include, FileKind::MacroInclude));
    }

    for file_ref in &visitor.file_refs {
        let maybe_path = utils::resolve(file_path, file_ref);
        let resolved_path = maybe_path.or_else(|| utils::resolve_workspace_relative(workspace_root?, file_ref));

        if let Some(path) = resolved_path {
            node.add_child(FileNode::new(path, FileKind::FileReference));
        }
    }

    node
}

fn find_assume_files(crate_root: &Path, patterns: &HashSet<String>) -> Vec<PathBuf> {
    let mut found_files = Vec::new();
    for pattern in patterns {
        let full_pattern = crate_root.join("**").join(pattern);
        if let Ok(paths) = glob(&full_pattern.to_string_lossy()) {
            for path_result in paths.flatten() {
                if path_result.is_file() {
                    found_files.push(path_result);
                }
            }
        }
    }

    found_files.sort();
    found_files.dedup();
    found_files
}

pub fn build_tree(host: &mut impl Host, metadata: &CargoMetadata, packages: &[&CargoPackage], config: &MainConfig) -> FileNode {
    let mut visited = HashSet::new();

    let root_path = metadata.workspace_root.join("Cargo.toml");
    let root_kind = FileKind::Workspace;

    let mut root_node = FileNode::new(root_path, root_kind);

    for package in packages {
        let mut node = FileNode::for_package(package.manifest_path.clone(), package_id(&package.name, &package.version));

        for target in &package.targets {
            let mut target_node = FileNode::new(target.src_path.clone(), FileKind::Target);

            let source_tree = build_file_node(
                host,
                &target.src_path,
                &mut visited,
                Some(&metadata.workspace_root),
                config,
                &package.name,
            );

            for child in source_tree.children {
                target_node.add_child(child);
            }

            node.add_child(target_node);
        }

        let parser_config = config.crate_config(&package.name);
        if parser_config.assume
            && !parser_config.assume_patterns.is_empty()
            && let Some(crate_root) = package.manifest_path.parent()
        {
            let assume_files = find_assume_files(crate_root, &parser_config.assume_patterns);

            for assume_file in assume_files {
                let assume_node = FileNode::new(assume_file, FileKind::Assume);

                node.add_child(assume_node);
            }
        }

        root_node.add_child(node);
    }

    root_node
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestHost;

    #[test]
    fn new_file_node_has_no_children() {
        let node = FileNode::new(PathBuf::from("src/main.rs"), FileKind::Target);
        assert_eq!(node.path, PathBuf::from("src/main.rs"));
        assert_eq!(node.kind, FileKind::Target);
        assert!(node.children.is_empty());
    }

    #[test]
    fn add_child_appends() {
        let mut parent = FileNode::new(PathBuf::from("root"), FileKind::Workspace);
        let child = FileNode::new(PathBuf::from("child.rs"), FileKind::Module);
        parent.add_child(child);
        assert_eq!(parent.children.len(), 1);
        assert_eq!(parent.children[0].path, PathBuf::from("child.rs"));
    }

    #[test]
    fn add_child_deduplicates_by_path() {
        let mut parent = FileNode::new(PathBuf::from("root"), FileKind::Workspace);
        parent.add_child(FileNode::new(PathBuf::from("a.rs"), FileKind::Module));
        parent.add_child(FileNode::new(PathBuf::from("a.rs"), FileKind::Target));
        parent.add_child(FileNode::new(PathBuf::from("b.rs"), FileKind::Module));
        assert_eq!(parent.children.len(), 2);
    }

    #[test]
    fn len_counts_self_and_all_descendants() {
        let mut root = FileNode::new(PathBuf::from("root"), FileKind::Workspace);
        let mut child = FileNode::new(PathBuf::from("child"), FileKind::Package);
        child.add_child(FileNode::new(PathBuf::from("grandchild"), FileKind::Module));
        root.add_child(child);
        // Each node contributes (children_sum + 1), where each child contributes (child.len() + 1)
        // grandchild: 1, child: (1+1)+1=3, root: (3+1)+1=5
        assert_eq!(root.len(), 5);
    }

    #[test]
    fn len_leaf_is_one() {
        let leaf = FileNode::new(PathBuf::from("leaf.rs"), FileKind::Module);
        assert_eq!(leaf.len(), 1);
    }

    #[test]
    fn distinct_collects_unique_paths() {
        let mut root = FileNode::new(PathBuf::from("root"), FileKind::Workspace);
        let mut child = FileNode::new(PathBuf::from("a.rs"), FileKind::Package);
        child.add_child(FileNode::new(PathBuf::from("b.rs"), FileKind::Module));
        root.add_child(child);
        root.add_child(FileNode::new(PathBuf::from("c.rs"), FileKind::Module));

        let paths = root.distinct();
        assert_eq!(paths.len(), 4);
        assert!(paths.contains(&PathBuf::from("root")));
        assert!(paths.contains(&PathBuf::from("a.rs")));
        assert!(paths.contains(&PathBuf::from("b.rs")));
        assert!(paths.contains(&PathBuf::from("c.rs")));
    }

    #[test]
    fn make_relative_paths_strips_prefix() {
        let ws = PathBuf::from("/workspace");
        let mut root = FileNode::new(PathBuf::from("/workspace/Cargo.toml"), FileKind::Workspace);
        root.add_child(FileNode::new(PathBuf::from("/workspace/src/main.rs"), FileKind::Target));

        root.make_relative_paths(&ws);

        assert_eq!(root.path, PathBuf::from("Cargo.toml"));
        assert_eq!(root.children[0].path, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn make_relative_paths_preserves_unrelated() {
        let ws = PathBuf::from("/workspace");
        let mut node = FileNode::new(PathBuf::from("/other/file.rs"), FileKind::Module);
        node.make_relative_paths(&ws);
        assert_eq!(node.path, PathBuf::from("/other/file.rs"));
    }

    #[test]
    fn make_relative_paths_resolves_equivalent_filesystem_paths() {
        let root = std::env::temp_dir().join(format!("cargo-delta-relative-paths-{}", std::process::id()));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("file.rs"), "").unwrap();
        let mut node = FileNode::new(nested.join("../file.rs"), FileKind::Module);

        node.make_relative_paths(&root);

        assert_eq!(node.path, PathBuf::from("file.rs"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn find_packages_containing_file_finds_match() {
        let mut root = FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace);
        let package = package_id("package-name", "0.1.0");
        let mut package_node = FileNode::for_package(PathBuf::from("folder/Cargo.toml"), package.clone());
        package_node.add_child(FileNode::new(PathBuf::from("folder/src/lib.rs"), FileKind::Target));
        root.add_child(package_node);

        let target = PathBuf::from("folder/src/lib.rs");
        let packages = root.find_packages_containing_file(&target);
        assert_eq!(packages, vec![package]);
    }

    #[test]
    fn find_packages_containing_file_returns_empty_for_no_match() {
        let root = FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace);
        let target = PathBuf::from("nonexistent.rs");
        let packages = root.find_packages_containing_file(&target);
        assert!(packages.is_empty());
    }

    #[test]
    fn build_tree_adds_files_matching_nonempty_assume_patterns() {
        let root = std::env::temp_dir().join(format!("cargo-delta-assume-patterns-{}", std::process::id()));
        let package_root = root.join("package");
        fs::create_dir_all(&package_root).unwrap();
        let assumed_file = package_root.join("schema.proto");
        fs::write(&assumed_file, "message Example {}").unwrap();
        let metadata = CargoMetadata {
            packages: vec![CargoPackage {
                id: "package".to_string(),
                name: "package".to_string(),
                version: "0.1.0".to_string(),
                source: None,
                targets: Vec::new(),
                manifest_path: package_root.join("Cargo.toml"),
                dependencies: Vec::new(),
            }],
            workspace_root: root.clone(),
            target_directory: root.join("target"),
            workspace_members: vec!["package".to_string()],
            resolve: None,
        };
        let config = MainConfig {
            parser: ParserConfig {
                assume: true,
                assume_patterns: HashSet::from(["*.proto".to_string()]),
                ..ParserConfig::default()
            },
            ..MainConfig::default()
        };

        let tree = build_tree(&mut TestHost::new(), &metadata, &[&metadata.packages[0]], &config);

        assert_eq!(tree.children[0].children.len(), 1);
        assert_eq!(tree.children[0].children[0].kind, FileKind::Assume);
        assert_eq!(tree.children[0].children[0].path, assumed_file);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_kind_display() {
        assert_eq!(FileKind::Workspace.to_string(), "Workspace");
        assert_eq!(FileKind::Module.to_string(), "Module");
        assert_eq!(FileKind::Unset.to_string(), "Unset");
    }
}
