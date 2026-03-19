#![doc(hidden)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! This is an implementation detail of the cargo-delta tool. Do not take a dependency on this crate
//! as it may change in incompatible ways without warning.

use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::MainConfig;
use crate::crates::Crates;
use crate::files::FileNode;
use crate::git::GitDiff;

mod cargo;
mod config;
mod crates;
mod error;
mod files;
mod git;
mod host;
mod utils;

pub use host::Host;

const CLAP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

/// Top-level CLI wrapper for `cargo delta`.
#[derive(Parser)]
#[command(name = "cargo-delta", bin_name = "cargo", version, about, author, styles = CLAP_STYLES)]
struct Cli {
    #[command(subcommand)]
    command: CargoSubcommand,
}

#[derive(Subcommand)]
enum CargoSubcommand {
    Delta(Args),
}

/// Identify impacted crates from git changes.
#[derive(Parser)]
#[command(name = "cargo-delta", author, version, long_about = None, display_name = "cargo-delta")]
#[command(about = "Identify impacted crates from git changes")]
struct Args {
    /// Path to configuration file (defaults to `delta.toml`)
    #[arg(short = 'c', long, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run delta and show impacted crates
    Run(RunCommand),
    /// Analyze current workspace and produce JSON output
    Analyze(AnalyzeCommand),
}

#[derive(Parser)]
struct RunCommand {
    /// Baseline workspace analysis JSON file (e.g., from main branch)
    #[arg(long, value_name = "PATH")]
    baseline: PathBuf,
    /// Current workspace analysis JSON file (e.g., from feature branch)
    #[arg(long, value_name = "PATH")]
    current: PathBuf,
}

#[derive(Parser)]
struct AnalyzeCommand;

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Impact {
    #[serde(rename = "Modified")]
    pub modified: HashSet<String>,
    #[serde(rename = "Affected")]
    pub affected: HashSet<String>,
    #[serde(rename = "Required")]
    pub required: HashSet<String>,
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceTree {
    pub files: FileNode,
    pub crates: Crates,
}

/// Run the cargo-delta tool with the given command-line arguments.
pub fn run(host: &mut impl Host, args: impl IntoIterator<Item = String>) {
    let CargoSubcommand::Delta(cli) = Cli::parse_from(args).command;

    let config = match config::load_config(cli.config.clone()) {
        Ok(i) => i,
        Err(e) => {
            let _ = writeln!(host.error(), "Error loading config: {e}");
            host.exit(1);
            return;
        }
    };

    match &cli.command {
        Commands::Run(run_cmd) => run_command(host, &config, &run_cmd.baseline, &run_cmd.current, cli.config.as_ref()),

        Commands::Analyze(_) => analyze(host, &config, cli.config.as_ref()),
    }
}

#[doc(hidden)]
fn print_common_props(host: &mut impl Host, config_path: Option<&PathBuf>) {
    if let Some(config_path) = config_path {
        let _ = writeln!(host.error());
        let _ = writeln!(host.error(), "Using config file  : {}", config_path.display());
    }
}

#[doc(hidden)]
fn analyze(host: &mut impl Host, config: &MainConfig, config_path: Option<&PathBuf>) {
    let start = Instant::now();
    let _ = writeln!(host.error(), "Analyzing workspace..");
    print_common_props(host, config_path);

    let metadata = match cargo::metadata(host) {
        Ok(metadata) => metadata,
        Err(e) => {
            let _ = writeln!(host.error(), "Error getting cargo metadata: {e}");
            host.exit(1);
            return;
        }
    };

    let workspace_root = &metadata.workspace_root;

    let git_root = match git::get_top_level(host) {
        Ok(root) => root,
        Err(e) => {
            let _ = writeln!(host.error(), "Error getting git root: {e}");
            host.exit(1);
            return;
        }
    };

    let _ = writeln!(host.error());
    let _ = writeln!(host.error(), "Detected Git root        : {}", git_root.display());
    let _ = writeln!(host.error(), "Detected Cargo workspace : {}", workspace_root.display());
    let _ = writeln!(host.error());

    let crates = cargo::get_workspace_crates(&metadata);
    let mut files = files::build_tree(host, &metadata, &crates, config);
    let crates = crates::parse(&metadata);

    files.make_relative_paths(&git_root);

    let _ = writeln!(host.error(), "Found {} crate(s) in the workspace.", crates.len());
    let _ = writeln!(host.error(), "Found {} file(s) in the workspace.", files.len());
    let _ = writeln!(host.error());

    let workspace_tree = WorkspaceTree { files, crates };

    match serde_json::to_string_pretty(&workspace_tree) {
        Ok(json_output) => {
            let _ = writeln!(host.output(), "{json_output}");
        }
        Err(e) => {
            let _ = writeln!(host.error(), "Error serializing workspace tree to JSON: {e}");
            host.exit(1);
            return;
        }
    }

    let _ = writeln!(host.error());
    let excludes: Vec<PathBuf> = workspace_tree.files.distinct().into_iter().collect();

    let unrelated = utils::find_unrelated(&git_root, &excludes, &config.file_exclude_patterns, &config.tripwire_patterns);

    if !config.file_exclude_patterns.is_empty() {
        let _ = writeln!(
            host.error(),
            "Excluded patterns       : {}",
            config.file_exclude_patterns.join(", ")
        );
    }

    if !config.tripwire_patterns.is_empty() {
        let _ = writeln!(host.error(), "Tripwire patterns       : {}", config.tripwire_patterns.join(", "));
    }

    if !unrelated.filtered.is_empty() {
        let _ = writeln!(host.error());
        let _ = writeln!(host.error(), "Excluded file(s): (filtered out by exclude patterns)");
        for file in &unrelated.filtered {
            let _ = writeln!(host.error(), "  {}", file.display());
        }
    }

    if !unrelated.tripwire.is_empty() {
        let _ = writeln!(host.error());
        let _ = writeln!(host.error(), "Tripwire file(s): (changes to these trigger a full rebuild)");
        for file in &unrelated.tripwire {
            let _ = writeln!(host.error(), "  {}", file.display());
        }
    }

    if !unrelated.unaccounted.is_empty() {
        let _ = writeln!(host.error());
        let _ = writeln!(host.error(), "Needs triage: (unknown impact, not matched by any rule)");
        for file in &unrelated.unaccounted {
            let _ = writeln!(host.error(), "  {}", file.display());
        }
    }

    let duration = start.elapsed();
    let _ = writeln!(host.error(), "\nAnalysis finished in {duration:.2?}");
}

#[doc(hidden)]
fn run_command(host: &mut impl Host, config: &MainConfig, baseline: &Path, current: &Path, config_path: Option<&PathBuf>) {
    let _ = writeln!(host.error(), "Running delta..\n");
    print_common_props(host, config_path);

    // Get git root to ensure we're working with consistent path bases
    let git_root = match git::get_top_level(host) {
        Ok(root) => root,
        Err(e) => {
            let _ = writeln!(host.error(), "Error getting git root: {e}");
            host.exit(1);
            return;
        }
    };

    let _ = writeln!(host.error(), "Looking up git changes..");

    let diff = match git::diff(host, &git_root, config.git.as_ref()) {
        Ok(i) => i,
        Err(e) => {
            let _ = writeln!(host.error(), "Error creating diff: {e}");
            host.exit(1);
            return;
        }
    };

    if diff.changed.is_empty() && diff.deleted.is_empty() {
        let _ = writeln!(host.error(), "No file has been changed or deleted, quitting.");
        host.exit(0);
        return;
    }

    for changed in &diff.changed {
        let _ = writeln!(host.error(), "Changed file: {}", &changed.display());
    }

    for deleted in &diff.deleted {
        let _ = writeln!(host.error(), "Deleted file: {}", &deleted.display());
    }

    let _ = writeln!(host.error());
    let _ = writeln!(host.error(), "Using baseline analysis : {}", baseline.display());
    let _ = writeln!(host.error(), "Using current analysis  : {}", current.display());
    let _ = writeln!(host.error());

    let baseline_tree: WorkspaceTree = match utils::deser_json(baseline) {
        Ok(tree) => tree,
        Err(e) => {
            let _ = writeln!(host.error(), "Error loading current workspace tree: {e}");
            host.exit(1);
            return;
        }
    };

    let current_tree: WorkspaceTree = match utils::deser_json(current) {
        Ok(tree) => tree,
        Err(e) => {
            let _ = writeln!(host.error(), "Error loading branch workspace tree: {e}");
            host.exit(1);
            return;
        }
    };

    let result = get_impacted_crates(host, &baseline_tree, &current_tree, &diff, config);

    match serde_json::to_string_pretty(&result) {
        Ok(json_output) => {
            let _ = writeln!(host.output(), "{json_output}");
        }
        Err(e) => {
            let _ = writeln!(host.error(), "Error serializing result to JSON: {e}");
            host.exit(1);
            return;
        }
    }

    let total_crates = current_tree.crates.len();

    let required_crates_len = result.required.len();
    let affected_crates_len = result.affected.len();
    let modified_crates_len = result.modified.len();

    let _ = writeln!(
        host.error(),
        "Modified    {modified_crates_len:>3} (Crates directly modified by Git changes.)"
    );
    let _ = writeln!(
        host.error(),
        "Affected    {affected_crates_len:>3} (Modified crates plus all their dependents, direct and indirect.)"
    );
    let _ = writeln!(
        host.error(),
        "Required    {required_crates_len:>3} (Affected crates plus all their dependencies, direct and indirect.)"
    );
    let _ = writeln!(host.error(), "Total       {total_crates:>3} (Total crates in this workspace.)");
    let _ = writeln!(host.error());
}

#[doc(hidden)]
fn get_impacted_crates(
    host: &mut impl Host,
    baseline_tree: &WorkspaceTree,
    current_tree: &WorkspaceTree,
    git_diff: &GitDiff,
    config: &MainConfig,
) -> Impact {
    let mut modified = HashSet::new();

    if !config.tripwire_patterns.is_empty() {
        use glob::Pattern;

        let tripwire_patterns: Vec<Pattern> = config
            .tripwire_patterns
            .iter()
            .filter_map(|pattern| Pattern::new(pattern).ok())
            .collect();

        let mut tripped_files = Vec::new();

        for deleted_file in &git_diff.deleted {
            let file_str = deleted_file.to_string_lossy();
            if tripwire_patterns.iter().any(|pattern| pattern.matches(&file_str)) {
                tripped_files.push(file_str.to_string());
            }
        }

        for changed_file in &git_diff.changed {
            let file_str = changed_file.to_string_lossy();
            if tripwire_patterns.iter().any(|pattern| pattern.matches(&file_str)) {
                tripped_files.push(file_str.to_string());
            }
        }

        if !tripped_files.is_empty() {
            let _ = writeln!(
                host.error(),
                "WARNING: Tripwire activated due to changes in the following file(s):"
            );
            for file in &tripped_files {
                let _ = writeln!(host.error(), "- {file}");
            }
            let _ = writeln!(host.error());

            let all_crates: HashSet<String> = current_tree.crates.get_all_crate_names().into_iter().collect();

            return Impact {
                modified: all_crates.clone(),
                affected: all_crates.clone(),
                required: all_crates,
            };
        }

        let _ = writeln!(host.error(), "Tripwire is enabled, but no matching files were found, good.");
        let _ = writeln!(host.error());
    }

    for deleted_file in &git_diff.deleted {
        let crates_for_file = baseline_tree.files.find_crates_containing_file(deleted_file);

        for crate_name in crates_for_file {
            let _ = modified.insert(crate_name);
        }
    }

    for changed_file in &git_diff.changed {
        let crates_for_file = current_tree.files.find_crates_containing_file(changed_file);

        for crate_name in crates_for_file {
            let _ = modified.insert(crate_name);
        }
    }

    let main_files = baseline_tree.files.distinct();
    let branch_files = current_tree.files.distinct();

    for new_file in branch_files.difference(&main_files) {
        let crates_for_file = current_tree.files.find_crates_containing_file(new_file);

        for crate_name in crates_for_file {
            let _ = modified.insert(crate_name);
        }
    }

    // Affected = Modified + all their dependents
    let mut affected = modified.clone();
    for crate_name in &modified {
        if let Some(transitive_dependents) = current_tree.crates.get_dependents_transitive(crate_name) {
            for dependent in transitive_dependents {
                let _ = affected.insert(dependent);
            }
        }
    }

    // Required = Affected + all their dependencies
    let mut required = affected.clone();
    for crate_name in &affected {
        if let Some(transitive_deps) = current_tree.crates.get_dependencies_transitive(crate_name) {
            for dependency in transitive_deps {
                let _ = required.insert(dependency);
            }
        }
    }

    Impact {
        modified,
        affected,
        required,
    }
}

#[cfg(test)]
pub(crate) mod test_helpers;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::{CargoCrate, CargoDependency, CargoMetadata, CargoTarget};
    use crate::files::FileKind;
    use crate::test_helpers::*;

    fn make_metadata(crate_deps: &[(&str, &[&str])]) -> CargoMetadata {
        let mut packages = Vec::new();
        for (name, deps) in crate_deps {
            packages.push(CargoCrate {
                name: name.to_string(),
                source: None,
                targets: vec![CargoTarget {
                    name: name.to_string(),
                    kind: vec!["lib".to_string()],
                    src_path: PathBuf::from(format!("{name}/src/lib.rs")),
                }],
                manifest_path: PathBuf::from(format!("{name}/Cargo.toml")),
                dependencies: deps
                    .iter()
                    .map(|d| CargoDependency {
                        name: d.to_string(),
                        source: None,
                    })
                    .collect(),
            });
        }
        CargoMetadata {
            packages,
            workspace_root: PathBuf::from("/workspace"),
            target_directory: PathBuf::from("/workspace/target"),
        }
    }

    fn make_file_tree(crate_files: &[(&str, &[&str])]) -> FileNode {
        let mut root = FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace);
        for (crate_name, files) in crate_files {
            let manifest = PathBuf::from(format!("{crate_name}/Cargo.toml"));
            let mut crate_node = FileNode::new(manifest, FileKind::Crate);
            for file in *files {
                crate_node.add_child(FileNode::new(PathBuf::from(*file), FileKind::Target));
            }
            root.add_child(crate_node);
        }
        root
    }

    fn make_workspace(crate_defs: &[(&str, &[&str], &[&str])]) -> WorkspaceTree {
        let deps: Vec<(&str, &[&str])> = crate_defs.iter().map(|(n, _, d)| (*n, *d)).collect();
        let crate_files: Vec<(&str, &[&str])> = crate_defs.iter().map(|(n, f, _)| (*n, *f)).collect();

        let metadata = make_metadata(&deps);
        let files = make_file_tree(&crate_files);
        let crates_graph = crates::parse(&metadata);

        WorkspaceTree {
            files,
            crates: crates_graph,
        }
    }

    // --- get_impacted_crates tests ---

    #[test]
    fn no_changes_produces_empty_impact() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &["lib"]), ("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![],
            deleted: vec![],
        };
        let config = MainConfig::default();

        let result = get_impacted_crates(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.is_empty());
        assert!(result.affected.is_empty());
        assert!(result.required.is_empty());
    }

    #[test]
    fn changed_file_marks_crate_modified() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &[]), ("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("lib/src/lib.rs")],
            deleted: vec![],
        };
        let config = MainConfig::default();

        let result = get_impacted_crates(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains("lib"));
        assert!(!result.modified.contains("app"));
    }

    #[test]
    fn changed_file_propagates_to_dependents() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &["lib"]), ("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("lib/src/lib.rs")],
            deleted: vec![],
        };
        let config = MainConfig::default();

        let result = get_impacted_crates(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains("lib"));
        assert!(result.affected.contains("lib"));
        assert!(result.affected.contains("app"));
    }

    #[test]
    fn required_includes_dependencies_of_affected() {
        let mut host = TestHost::new();
        // app -> middleware -> core
        let tree = make_workspace(&[
            ("app", &["app/src/main.rs"], &["middleware"]),
            ("middleware", &["middleware/src/lib.rs"], &["core"]),
            ("core", &["core/src/lib.rs"], &[]),
        ]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("middleware/src/lib.rs")],
            deleted: vec![],
        };
        let config = MainConfig::default();

        let result = get_impacted_crates(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains("middleware"));
        assert!(result.affected.contains("app"));
        assert!(result.affected.contains("middleware"));
        assert!(result.required.contains("core"));
        assert!(result.required.contains("middleware"));
        assert!(result.required.contains("app"));
    }

    #[test]
    fn deleted_file_marks_crate_modified() {
        let mut host = TestHost::new();
        let baseline = make_workspace(&[("lib", &["lib/src/lib.rs", "lib/src/old.rs"], &[])]);
        let current = make_workspace(&[("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![],
            deleted: vec![PathBuf::from("lib/src/old.rs")],
        };
        let config = MainConfig::default();

        let result = get_impacted_crates(&mut host, &baseline, &current, &diff, &config);

        assert!(result.modified.contains("lib"));
    }

    #[test]
    fn new_file_in_branch_marks_crate_modified() {
        let mut host = TestHost::new();
        let baseline = make_workspace(&[("lib", &["lib/src/lib.rs"], &[])]);
        let current = make_workspace(&[("lib", &["lib/src/lib.rs", "lib/src/new.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![],
            deleted: vec![],
        };
        let config = MainConfig::default();

        let result = get_impacted_crates(&mut host, &baseline, &current, &diff, &config);

        assert!(result.modified.contains("lib"));
    }

    #[test]
    fn tripwire_activated_returns_all_crates() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &[]), ("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.lock")],
            deleted: vec![],
        };
        let config = MainConfig {
            tripwire_patterns: vec!["Cargo.lock".to_string()],
            ..MainConfig::default()
        };

        let result = get_impacted_crates(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains("app"));
        assert!(result.modified.contains("lib"));
        assert!(result.affected.contains("app"));
        assert!(result.affected.contains("lib"));
        assert!(host.stderr_str().contains("Tripwire activated"));
    }

    #[test]
    fn tripwire_enabled_no_match() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("lib/src/lib.rs")],
            deleted: vec![],
        };
        let config = MainConfig {
            tripwire_patterns: vec!["Cargo.lock".to_string()],
            ..MainConfig::default()
        };

        let result = get_impacted_crates(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains("lib"));
        assert!(host.stderr_str().contains("no matching files were found"));
    }

    #[test]
    fn tripwire_on_deleted_file() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![],
            deleted: vec![PathBuf::from("Cargo.lock")],
        };
        let config = MainConfig {
            tripwire_patterns: vec!["Cargo.lock".to_string()],
            ..MainConfig::default()
        };

        let result = get_impacted_crates(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains("app"));
        assert!(host.stderr_str().contains("Tripwire activated"));
    }

    // --- print_common_props tests ---

    #[test]
    fn print_common_props_with_path() {
        let mut host = TestHost::new();
        let path = PathBuf::from("my-config.toml");
        print_common_props(&mut host, Some(&path));
        assert!(host.stderr_str().contains("Using config file"));
        assert!(host.stderr_str().contains("my-config.toml"));
    }

    #[test]
    fn print_common_props_without_path() {
        let mut host = TestHost::new();
        print_common_props(&mut host, None);
        assert!(host.stderr_str().is_empty());
    }

    // --- run() integration tests ---

    #[test]
    fn run_bad_config_exits_with_error() {
        let mut host = TestHost::new();
        run(
            &mut host,
            ["cargo", "delta", "-c", "nonexistent-config-xyz.toml", "analyze"]
                .iter()
                .map(ToString::to_string),
        );
        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr_str().contains("Error loading config"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn run_analyze_cargo_metadata_failure_exits() {
        let mut host = TestHost::new().with_commands(vec![Ok(failure_output("error: could not find Cargo.toml"))]);

        run(&mut host, ["cargo", "delta", "analyze"].iter().map(ToString::to_string));

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr_str().contains("Error getting cargo metadata"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn run_subcommand_no_changes_exits_zero() {
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("/fake/root\n")),             // git rev-parse
            Ok(success_output("abc\trefs/heads/master\n")), // git ls-remote (master found)
            Ok(success_output("abc123\n")),                 // git merge-base
            Ok(success_output("")),                         // git diff (no changes)
        ]);

        run(
            &mut host,
            ["cargo", "delta", "run", "--baseline", "fake.json", "--current", "fake.json"]
                .iter()
                .map(ToString::to_string),
        );

        assert_eq!(host.exit_code, Some(0));
        assert!(host.stderr_str().contains("No file has been changed"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn run_subcommand_with_changes_produces_output() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_run_changes");
        let _ = std::fs::create_dir_all(&tmp);

        let tree = make_workspace(&[("app", &["app/src/main.rs"], &["lib"]), ("lib", &["lib/src/lib.rs"], &[])]);
        let json = serde_json::to_string_pretty(&tree).unwrap();
        let baseline_path = tmp.join("baseline.json");
        let current_path = tmp.join("current.json");
        std::fs::write(&baseline_path, &json).unwrap();
        std::fs::write(&current_path, &json).unwrap();

        let git_root = tmp.to_string_lossy().to_string();
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output(&format!("{git_root}\n"))),   // git rev-parse
            Ok(success_output("abc\trefs/heads/master\n")), // git ls-remote
            Ok(success_output("abc123\n")),                 // git merge-base
            Ok(success_output("lib/src/lib.rs\n")),         // git diff (one file)
        ]);

        run(
            &mut host,
            [
                "cargo",
                "delta",
                "run",
                "--baseline",
                &baseline_path.to_string_lossy(),
                "--current",
                &current_path.to_string_lossy(),
            ]
            .iter()
            .map(ToString::to_string),
        );

        // File doesn't exist under git_root, so treated as deleted → lib is modified
        assert!(host.exit_code.is_none());
        let stdout = host.stdout_str();
        assert!(stdout.contains("Modified"));
        assert!(stdout.contains("lib"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
