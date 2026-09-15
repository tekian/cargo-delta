#![doc(hidden)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! This is an implementation detail of the cargo-delta tool. Do not take a dependency on this crate
//! as it may change in incompatible ways without warning.

use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::MainConfig;
use crate::files::{FileKind, FileNode};
use crate::git::GitDiff;
use crate::packages::{PackageId, Packages};

mod artifacts;
mod cargo;
mod config;
mod error;
mod files;
mod git;
mod host;
mod output;
mod packages;
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

/// Identify impacted Cargo packages from Git changes.
#[derive(Parser)]
#[command(name = "cargo-delta", author, version, long_about = None, display_name = "cargo-delta")]
#[command(about = "Identify impacted Cargo packages from Git changes")]
struct Args {
    /// Path to configuration file (defaults to `delta.toml`)
    #[arg(short = 'c', long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compute impacted Cargo packages from a pair of snapshots
    #[command(alias = "run")]
    Impact(ImpactCommand),
    /// Snapshot the current workspace into a JSON artifact
    #[command(alias = "analyze")]
    Snapshot(SnapshotCommand),
}

#[derive(Parser)]
struct ImpactCommand {
    /// Baseline workspace analysis JSON file (e.g., from main branch)
    #[arg(long, value_name = "PATH", required_unless_present = "output_dir", conflicts_with = "output_dir")]
    baseline: Option<PathBuf>,
    /// Current workspace analysis JSON file (e.g., from feature branch)
    #[arg(long, value_name = "PATH", required_unless_present = "output_dir", conflicts_with = "output_dir")]
    current: Option<PathBuf>,
    /// Compare HEAD with the merge base of this Git ref instead of using Git configuration.
    #[arg(long, value_name = "REF", conflicts_with = "changed_files")]
    base_ref: Option<String>,
    /// Read changed and deleted paths from a UTF-8 JSON manifest instead of invoking Git diff.
    #[arg(long, value_name = "PATH", conflicts_with = "base_ref")]
    changed_files: Option<PathBuf>,
    /// Atomically write the selected format to this path instead of stdout.
    #[arg(long, value_name = "PATH", conflicts_with = "output_dir")]
    output: Option<PathBuf>,
    /// Generate the complete ref-to-artifacts directory instead of using explicit snapshots.
    #[arg(
        long,
        value_name = "DIR",
        requires = "base_ref",
        conflicts_with_all = ["baseline", "current", "changed_files", "format", "modified", "affected", "required"]
    )]
    output_dir: Option<PathBuf>,
    /// Policy for tracked or non-ignored untracked workspace changes.
    #[arg(long, value_enum, value_name = "POLICY", requires = "output_dir")]
    dirty: Option<DirtyPolicy>,
    /// Output format for stdout or `--output`. Non-json formats emit the union of the
    /// selected tiers; diagnostics remain on stderr, so stdout can be captured directly,
    /// e.g. `cargo build $(cargo delta run ... -f cargo-args)`.
    #[arg(short = 'f', long, value_enum)]
    format: Option<OutputFormat>,
    /// Include packages directly modified by Git changes. If none of `--modified`,
    /// `--affected`, `--required` are given, all three are included (default).
    #[arg(long)]
    modified: bool,
    /// Include modified packages plus their transitive dependents. If none of `--modified`,
    /// `--affected`, `--required` are given, all three are included (default).
    #[arg(long)]
    affected: bool,
    /// Include affected packages plus their transitive dependencies. If none of `--modified`,
    /// `--affected`, `--required` are given, all three are included (default).
    #[arg(long)]
    required: bool,
}

/// Output format for the `run` subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// `Impact` JSON object containing only the selected tiers (default emits all three,
    /// backward compatible).
    Json,
    /// One bare package name per line - convenient for `xargs` or shell loops.
    Names,
    /// Space-separated `-p NAME` arguments - drop straight into a `cargo` invocation
    /// via `$(cargo delta run ... -f cargo-args)`.
    CargoArgs,
    /// Space-separated `--exclude NAME` arguments for the *complement* of the selected
    /// tier(s) within the workspace. Use with `cargo --workspace` to scope to impacted
    /// packages without `-p` ambiguity (since `--exclude` matches workspace members only,
    /// it can't collide with same-named transitive registry deps). Empty when the
    /// selected tier covers (or exceeds) the workspace; combine with `-f names` for
    /// a "nothing impacted" check.
    CargoExcludes,
    /// One canonical `name@version` Cargo package spec per line.
    Packages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DirtyPolicy {
    Error,
    Workspace,
}

/// Bit-mask of which impact tiers to emit. `none()` means "all on" (default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TierMask {
    modified: bool,
    affected: bool,
    required: bool,
}

impl TierMask {
    /// Resolve the user-provided flags into the effective mask (no flags equals all on).
    const fn resolve(modified: bool, affected: bool, required: bool) -> Self {
        if !modified && !affected && !required {
            Self {
                modified: true,
                affected: true,
                required: true,
            }
        } else {
            Self {
                modified,
                affected,
                required,
            }
        }
    }
}

#[derive(Parser)]
struct SnapshotCommand {
    /// Atomically write the snapshot to this path instead of stdout.
    #[arg(long, value_name = "PATH")]
    output: Option<PathBuf>,
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Impact {
    #[serde(rename = "Modified")]
    pub modified: HashSet<PackageId>,
    #[serde(rename = "Affected")]
    pub affected: HashSet<PackageId>,
    #[serde(rename = "Required")]
    pub required: HashSet<PackageId>,
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceTree {
    #[serde(default)]
    pub schema: u32,
    pub files: FileNode,
    pub packages: Packages,
}

const SNAPSHOT_SCHEMA: u32 = 1;

impl WorkspaceTree {
    fn validate(&self) -> error::Result<()> {
        if self.schema != SNAPSHOT_SCHEMA {
            return Err(error::Error::Other(format!(
                "Unsupported snapshot schema {}; cargo-delta requires schema {SNAPSHOT_SCHEMA}. Regenerate this snapshot with `cargo delta snapshot`",
                self.schema
            )));
        }

        self.packages.validate()?;
        validate_file_tree(&self.files, &self.packages)
    }
}

fn portable_relative_path(path: &Path) -> error::Result<String> {
    let mut components = Vec::new();
    for (index, component) in path.components().enumerate() {
        let std::path::Component::Normal(component) = component else {
            return Err(error::Error::Other(format!(
                "Path '{}' is not a repository-relative path",
                path.display()
            )));
        };
        let component = component
            .to_str()
            .ok_or_else(|| error::Error::Other(format!("Path '{}' contains non-UTF-8 text", path.display())))?;
        if component.contains('\\')
            || (index == 0 && component.len() >= 2 && component.as_bytes()[0].is_ascii_alphabetic() && component.as_bytes()[1] == b':')
        {
            return Err(error::Error::Other(format!(
                "Path '{}' is not portable across supported platforms",
                path.display()
            )));
        }
        components.push(component);
    }
    if components.is_empty() {
        return Err(error::Error::Other("Package manifest path must not be empty".to_string()));
    }
    Ok(components.join("/"))
}

fn validate_file_tree(node: &FileNode, packages: &Packages) -> error::Result<()> {
    let _ = portable_relative_path(&node.path)?;
    if matches!(node.kind, FileKind::Package) {
        let package = node
            .package
            .as_ref()
            .ok_or_else(|| error::Error::Other(format!("Package node '{}' has no package ID", node.path.display())))?;
        if !packages.contains(package) {
            return Err(error::Error::Other(format!(
                "Snapshot file ownership refers to unknown package '{package}'"
            )));
        }
    }
    for child in &node.children {
        validate_file_tree(child, packages)?;
    }
    Ok(())
}

fn load_snapshot(path: &Path) -> error::Result<WorkspaceTree> {
    let tree: WorkspaceTree = utils::deser_json(path)?;
    tree.validate()?;
    Ok(tree)
}

fn write_artifact(host: &mut impl Host, output: Option<&Path>, contents: &[u8]) -> bool {
    let result = output.map_or_else(|| host.output().write_all(contents), |path| output::atomic_write(path, contents));

    if let Err(error) = result {
        let destination = output.map_or_else(|| "stdout".to_string(), |path| path.display().to_string());
        let _ = writeln!(host.error(), "Error writing output to {destination}: {error}");
        host.exit(1);
        false
    } else {
        true
    }
}

fn lines(values: &[String]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut rendered = values.join("\n").into_bytes();
    rendered.push(b'\n');
    rendered
}

fn with_optional_newline(value: String) -> Vec<u8> {
    if value.is_empty() {
        Vec::new()
    } else {
        let mut value = value.into_bytes();
        value.push(b'\n');
        value
    }
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
        Commands::Impact(cmd) => {
            if let Some(output_dir) = cmd.output_dir.as_deref() {
                let base_ref = cmd
                    .base_ref
                    .as_deref()
                    .unwrap_or_else(|| unreachable!("clap requires --base-ref with --output-dir"));
                artifacts::impact_from_ref(
                    host,
                    &config,
                    cli.config.as_ref(),
                    base_ref,
                    output_dir,
                    cmd.dirty.unwrap_or(DirtyPolicy::Error),
                );
            } else {
                impact(host, &config, cmd, cli.config.as_ref());
            }
        }

        Commands::Snapshot(cmd) => snapshot(host, &config, cli.config.as_ref(), cmd.output.as_deref()),
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
fn snapshot(host: &mut impl Host, config: &MainConfig, config_path: Option<&PathBuf>, output: Option<&Path>) {
    let start = Instant::now();
    let _ = writeln!(host.error(), "Snapshotting workspace..");
    print_common_props(host, config_path);

    let metadata = match cargo::metadata(host, None) {
        Ok(metadata) => metadata,
        Err(e) => {
            let _ = writeln!(host.error(), "Error getting cargo metadata: {e}");
            host.exit(1);
            return;
        }
    };

    let workspace_root = &metadata.workspace_root;

    let git_root = match git::get_top_level(host, None) {
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

    let workspace_tree = match build_workspace_tree(host, config, &metadata, &git_root) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = writeln!(host.error(), "Error creating workspace snapshot: {error}");
            host.exit(1);
            return;
        }
    };

    let _ = writeln!(host.error(), "Found {} package(s) in the workspace.", workspace_tree.packages.len());
    let _ = writeln!(host.error(), "Found {} file(s) in the workspace.", workspace_tree.files.len());
    let _ = writeln!(host.error());

    let snapshot_bytes = match snapshot_bytes(&workspace_tree) {
        Ok(json_output) => json_output,
        Err(e) => {
            let _ = writeln!(host.error(), "Error serializing workspace tree to JSON: {e}");
            host.exit(1);
            return;
        }
    };

    if !write_artifact(host, output, &snapshot_bytes) {
        return;
    }

    report_snapshot_file_coverage(host, config, &git_root, &workspace_tree);

    let duration = start.elapsed();
    let _ = writeln!(host.error(), "\nSnapshot finished in {duration:.2?}");
}

fn build_workspace_tree(
    host: &mut impl Host,
    config: &MainConfig,
    metadata: &cargo::CargoMetadata,
    git_root: &Path,
) -> error::Result<WorkspaceTree> {
    let mut workspace_packages = cargo::get_workspace_packages(metadata);
    workspace_packages.sort_by(|left, right| left.id.cmp(&right.id));
    let mut files = files::build_tree(host, metadata, &workspace_packages, config);
    let packages = packages::parse(&workspace_packages)?;
    files.make_relative_paths(git_root)?;

    let workspace_tree = WorkspaceTree {
        schema: SNAPSHOT_SCHEMA,
        files,
        packages,
    };
    workspace_tree.validate()?;
    Ok(workspace_tree)
}

fn snapshot_bytes(workspace_tree: &WorkspaceTree) -> error::Result<Vec<u8>> {
    let mut json_output = serde_json::to_vec_pretty(workspace_tree)?;
    json_output.push(b'\n');
    Ok(json_output)
}

fn report_snapshot_file_coverage(host: &mut impl Host, config: &MainConfig, git_root: &Path, workspace_tree: &WorkspaceTree) {
    let _ = writeln!(host.error());
    let excludes: Vec<PathBuf> = workspace_tree.files.distinct().into_iter().collect();
    let unrelated = utils::find_unrelated(git_root, &excludes, &config.file_exclude_patterns, &config.trip_wire_patterns);

    if !config.file_exclude_patterns.is_empty() {
        let _ = writeln!(
            host.error(),
            "Excluded patterns       : {}",
            config.file_exclude_patterns.join(", ")
        );
    }
    if !config.trip_wire_patterns.is_empty() {
        let _ = writeln!(host.error(), "Trip wire patterns      : {}", config.trip_wire_patterns.join(", "));
    }
    if !unrelated.filtered.is_empty() {
        let _ = writeln!(host.error());
        let _ = writeln!(host.error(), "Excluded file(s): (filtered out by exclude patterns)");
        for file in &unrelated.filtered {
            let _ = writeln!(host.error(), "  {}", file.display());
        }
    }
    if !unrelated.trip_wire.is_empty() {
        let _ = writeln!(host.error());
        let _ = writeln!(host.error(), "Trip wire file(s): (changes to these trigger a full rebuild)");
        for file in &unrelated.trip_wire {
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
}

#[doc(hidden)]
fn impact(host: &mut impl Host, config: &MainConfig, command: &ImpactCommand, config_path: Option<&PathBuf>) {
    let _ = writeln!(host.error(), "Computing impact..");
    print_common_props(host, config_path);
    let tiers = TierMask::resolve(command.modified, command.affected, command.required);

    // Get git root to ensure we're working with consistent path bases
    let git_root = match git::get_top_level(host, None) {
        Ok(root) => root,
        Err(e) => {
            let _ = writeln!(host.error(), "Error getting git root: {e}");
            host.exit(1);
            return;
        }
    };

    let _ = writeln!(host.error(), "Looking up changes..");

    let diff = match (command.base_ref.as_deref(), command.changed_files.as_deref()) {
        (Some(base_ref), None) => git::diff_for_ref(host, &git_root, base_ref),
        (None, Some(changed_files)) => git::diff_from_file(changed_files, &git_root),
        (None, None) => git::diff(host, &git_root, config.git.as_ref()),
        (Some(_), Some(_)) => unreachable!("clap prevents conflicting change sources"),
    };
    let diff = match diff {
        Ok(i) => i,
        Err(e) => {
            let _ = writeln!(host.error(), "Error creating diff: {e}");
            host.exit(1);
            return;
        }
    };

    if diff.changed.is_empty() && diff.deleted.is_empty() {
        let _ = writeln!(host.error(), "No file has been changed or deleted, quitting.");
        if !write_artifact(host, command.output.as_deref(), b"") {
            return;
        }
        host.exit(0);
        return;
    }

    for changed in &diff.changed {
        let _ = writeln!(host.error(), "Changed file: {}", &changed.display());
    }

    for deleted in &diff.deleted {
        let _ = writeln!(host.error(), "Deleted file: {}", &deleted.display());
    }

    let (baseline, current) = low_level_snapshot_paths(command);

    let _ = writeln!(host.error());
    let _ = writeln!(host.error(), "Using baseline analysis : {}", baseline.display());
    let _ = writeln!(host.error(), "Using current analysis  : {}", current.display());
    let _ = writeln!(host.error());

    let baseline_tree = match load_snapshot(baseline) {
        Ok(tree) => tree,
        Err(e) => {
            let _ = writeln!(host.error(), "Error loading baseline workspace tree: {e}");
            host.exit(1);
            return;
        }
    };

    let current_tree = match load_snapshot(current) {
        Ok(tree) => tree,
        Err(e) => {
            let _ = writeln!(host.error(), "Error loading current workspace tree: {e}");
            host.exit(1);
            return;
        }
    };

    let result = get_impacted_packages(host, &baseline_tree, &current_tree, &diff, config);

    let rendered = match emit_result(&result, &current_tree, command.format.unwrap_or(OutputFormat::Json), tiers) {
        Ok(rendered) => rendered,
        Err(error) => {
            let _ = writeln!(host.error(), "Error rendering impact: {error}");
            host.exit(1);
            return;
        }
    };

    if !write_artifact(host, command.output.as_deref(), &rendered) {
        return;
    }

    let total_packages = current_tree.packages.len();

    let required_packages_len = result.required.len();
    let affected_packages_len = result.affected.len();
    let modified_packages_len = result.modified.len();

    let _ = writeln!(
        host.error(),
        "Modified    {modified_packages_len:>3} (Packages directly modified by Git changes.)"
    );
    let _ = writeln!(
        host.error(),
        "Affected    {affected_packages_len:>3} (Modified packages plus all their dependents, direct and indirect.)"
    );
    let _ = writeln!(
        host.error(),
        "Required    {required_packages_len:>3} (Affected packages plus all their dependencies, direct and indirect.)"
    );
    let _ = writeln!(host.error(), "Total       {total_packages:>3} (Total packages in this workspace.)");
    let _ = writeln!(host.error());
}

fn low_level_snapshot_paths(command: &ImpactCommand) -> (&Path, &Path) {
    let (Some(baseline), Some(current)) = (&command.baseline, &command.current) else {
        unreachable!("clap requires --baseline and --current without --output-dir");
    };
    (baseline, current)
}

#[doc(hidden)]
fn emit_result(result: &Impact, workspace: &WorkspaceTree, format: OutputFormat, tiers: TierMask) -> error::Result<Vec<u8>> {
    match format {
        OutputFormat::Json => {
            let mut obj = serde_json::Map::new();
            if tiers.modified {
                let _ = obj.insert("Modified".to_string(), json!(workspace.packages.names(&result.modified)?));
            }
            if tiers.affected {
                let _ = obj.insert("Affected".to_string(), json!(workspace.packages.names(&result.affected)?));
            }
            if tiers.required {
                let _ = obj.insert("Required".to_string(), json!(workspace.packages.names(&result.required)?));
            }
            let mut rendered = serde_json::to_vec_pretty(&serde_json::Value::Object(obj))?;
            rendered.push(b'\n');
            Ok(rendered)
        }
        OutputFormat::Names => {
            let names = workspace.packages.names(&union_of_tiers(result, tiers).into_iter().collect())?;
            Ok(lines(&names))
        }
        OutputFormat::CargoArgs => {
            let joined = workspace
                .packages
                .names(&union_of_tiers(result, tiers).into_iter().collect())?
                .iter()
                .map(|n| format!("-p {n}"))
                .collect::<Vec<_>>()
                .join(" ");
            // Empty tier ⇒ emit nothing at all (not even a newline). Otherwise
            // `echo "Impacted: $(...)"` would print a stray trailing space, and
            // `cargo build $(...)` callers can't easily tell "no packages" from
            // "blank line". `[ -z "$VAR" ]` then works as expected.
            Ok(with_optional_newline(joined))
        }
        OutputFormat::CargoExcludes => {
            let selected: HashSet<PackageId> = union_of_tiers(result, tiers).into_iter().collect();
            let unselected: HashSet<PackageId> = workspace
                .packages
                .get_all()
                .into_iter()
                .filter(|package| !selected.contains(package))
                .collect();
            let joined = workspace
                .packages
                .names(&unselected)?
                .into_iter()
                .map(|name| format!("--exclude {name}"))
                .collect::<Vec<_>>()
                .join(" ");
            // Empty ⇒ selected covers the workspace (or exceeds it); the caller's
            // `cargo --workspace` already does the right thing with no exclusions.
            Ok(with_optional_newline(joined))
        }
        OutputFormat::Packages => {
            let selected: HashSet<PackageId> = union_of_tiers(result, tiers).into_iter().collect();
            let specs = workspace.packages.specs(&selected)?;
            Ok(lines(&specs))
        }
    }
}

/// Union of the selected tiers, deduplicated and sorted. For non-json formats this is what
/// the user actually wants - listing both `--affected` and `--required` shouldn't print
/// the same package twice.
#[doc(hidden)]
fn union_of_tiers(result: &Impact, tiers: TierMask) -> Vec<PackageId> {
    let mut union: HashSet<&PackageId> = HashSet::new();
    if tiers.modified {
        union.extend(result.modified.iter());
    }
    if tiers.affected {
        union.extend(result.affected.iter());
    }
    if tiers.required {
        union.extend(result.required.iter());
    }
    let mut packages: Vec<PackageId> = union.into_iter().cloned().collect();
    packages.sort();
    packages
}

fn trip_wire_impact(host: &mut impl Host, current_tree: &WorkspaceTree, git_diff: &GitDiff, config: &MainConfig) -> Option<Impact> {
    use glob::Pattern;

    if config.trip_wire_patterns.is_empty() {
        return None;
    }

    let trip_wire_patterns: Vec<Pattern> = config
        .trip_wire_patterns
        .iter()
        .filter_map(|pattern| Pattern::new(pattern).ok())
        .collect();
    let mut tripped_files = Vec::new();

    for file in git_diff.deleted.iter().chain(&git_diff.changed) {
        let file = file.to_string_lossy();
        if trip_wire_patterns.iter().any(|pattern| pattern.matches(&file)) {
            tripped_files.push(file.to_string());
        }
    }

    if tripped_files.is_empty() {
        let _ = writeln!(host.error(), "Trip wire is enabled, but no matching files were found, good.");
        let _ = writeln!(host.error());
        return None;
    }

    let _ = writeln!(
        host.error(),
        "WARNING: Trip wire activated due to changes in the following file(s):"
    );
    for file in &tripped_files {
        let _ = writeln!(host.error(), "- {file}");
    }
    let _ = writeln!(host.error());

    let all_packages: HashSet<PackageId> = current_tree.packages.get_all().into_iter().collect();
    Some(Impact {
        modified: all_packages.clone(),
        affected: all_packages.clone(),
        required: all_packages,
    })
}

#[doc(hidden)]
fn get_impacted_packages(
    host: &mut impl Host,
    baseline_tree: &WorkspaceTree,
    current_tree: &WorkspaceTree,
    git_diff: &GitDiff,
    config: &MainConfig,
) -> Impact {
    let mut modified = HashSet::new();
    let mut affected_seeds = HashSet::new();

    if let Some(impact) = trip_wire_impact(host, current_tree, git_diff, config) {
        return impact;
    }

    for deleted_file in &git_diff.deleted {
        let packages = baseline_tree.files.find_packages_containing_file(deleted_file);

        for package in packages {
            if current_tree.packages.contains(&package) {
                let _ = modified.insert(package);
                continue;
            }

            if let Some(dependents) = baseline_tree.packages.get_dependents_transitive(&package) {
                for dependent in dependents {
                    if current_tree.packages.contains(&dependent) {
                        let _ = affected_seeds.insert(dependent);
                    }
                }
            }
        }
    }

    for changed_file in &git_diff.changed {
        modified.extend(current_tree.files.find_packages_containing_file(changed_file));
    }

    let main_files = baseline_tree.files.distinct();
    let branch_files = current_tree.files.distinct();

    for new_file in branch_files.difference(&main_files) {
        modified.extend(current_tree.files.find_packages_containing_file(new_file));
    }

    // Affected = Modified + all their dependents
    let mut affected = modified.clone();
    affected.extend(affected_seeds);
    for package in affected.clone() {
        if let Some(transitive_dependents) = current_tree.packages.get_dependents_transitive(&package) {
            for dependent in transitive_dependents {
                let _ = affected.insert(dependent);
            }
        }
    }

    // Required = Affected + all their dependencies
    let mut required = affected.clone();
    for package in &affected {
        if let Some(transitive_deps) = current_tree.packages.get_dependencies_transitive(package) {
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
    use crate::cargo::{CargoDependency, CargoMetadata, CargoPackage, CargoTarget};
    use crate::test_helpers::*;

    type PackageDef<'a> = (&'a str, &'a str, &'a str, &'a [&'a str], &'a [&'a str]);

    fn id(name: &str) -> PackageId {
        PackageId::new(name, "0.1.0")
    }

    fn make_metadata(package_dependencies: &[(&str, &[&str])]) -> CargoMetadata {
        let workspace_root = PathBuf::from("/workspace");
        let mut packages = Vec::new();
        for (name, dependencies) in package_dependencies {
            packages.push(CargoPackage {
                id: name.to_string(),
                name: name.to_string(),
                version: "0.1.0".to_string(),
                source: None,
                targets: vec![CargoTarget {
                    name: name.to_string(),
                    kind: vec!["lib".to_string()],
                    src_path: workspace_root.join(name).join("src/lib.rs"),
                }],
                manifest_path: workspace_root.join(name).join("Cargo.toml"),
                dependencies: dependencies
                    .iter()
                    .map(|dependency| CargoDependency {
                        name: (*dependency).to_string(),
                        source: None,
                        path: Some(workspace_root.join(dependency)),
                    })
                    .collect(),
            });
        }
        let workspace_members = packages.iter().map(|package| package.id.clone()).collect();
        CargoMetadata {
            packages,
            workspace_members,
            workspace_root: workspace_root.clone(),
            target_directory: workspace_root.join("target"),
        }
    }

    fn make_file_tree(package_files: &[(&str, &[&str])]) -> FileNode {
        let mut root = FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace);
        for (package_name, files) in package_files {
            let manifest = PathBuf::from(format!("{package_name}/Cargo.toml"));
            let mut package_node = FileNode::for_package(manifest, id(package_name));
            for file in *files {
                package_node.add_child(FileNode::new(PathBuf::from(*file), FileKind::Target));
            }
            root.add_child(package_node);
        }
        root
    }

    fn make_workspace(package_definitions: &[(&str, &[&str], &[&str])]) -> WorkspaceTree {
        let dependencies: Vec<(&str, &[&str])> = package_definitions.iter().map(|(name, _, deps)| (*name, *deps)).collect();
        let package_files: Vec<(&str, &[&str])> = package_definitions.iter().map(|(name, files, _)| (*name, *files)).collect();

        let metadata = make_metadata(&dependencies);
        let files = make_file_tree(&package_files);
        let workspace_packages = cargo::get_workspace_packages(&metadata);
        let packages = packages::parse(&workspace_packages).unwrap();

        WorkspaceTree {
            schema: SNAPSHOT_SCHEMA,
            files,
            packages,
        }
    }

    fn make_schema1_workspace(package_defs: &[PackageDef<'_>]) -> WorkspaceTree {
        let workspace_root = PathBuf::from("/workspace");
        let metadata = CargoMetadata {
            packages: package_defs
                .iter()
                .enumerate()
                .map(|(index, (name, version, manifest_path, _, dependencies))| CargoPackage {
                    id: format!("fixture-package-{index}"),
                    name: (*name).to_string(),
                    version: (*version).to_string(),
                    source: None,
                    targets: Vec::new(),
                    manifest_path: workspace_root.join(manifest_path),
                    dependencies: dependencies
                        .iter()
                        .filter_map(|dependency_manifest| {
                            package_defs
                                .iter()
                                .find(|(_, _, manifest_path, _, _)| manifest_path == dependency_manifest)
                                .map(|(dependency_name, _, dependency_manifest, _, _)| CargoDependency {
                                    name: (*dependency_name).to_string(),
                                    source: None,
                                    path: workspace_root.join(dependency_manifest).parent().map(Path::to_path_buf),
                                })
                        })
                        .collect(),
                })
                .collect(),
            workspace_members: (0..package_defs.len()).map(|index| format!("fixture-package-{index}")).collect(),
            workspace_root: workspace_root.clone(),
            target_directory: workspace_root.join("target"),
        };
        let mut files = FileNode::new(PathBuf::from("Cargo.toml"), FileKind::Workspace);
        for (name, version, manifest_path, package_files, _) in package_defs {
            let mut package_node = FileNode::for_package(PathBuf::from(manifest_path), PackageId::new(name, version));
            for file in *package_files {
                package_node.add_child(FileNode::new(PathBuf::from(file), FileKind::Target));
            }
            files.add_child(package_node);
        }
        let workspace_packages = cargo::get_workspace_packages(&metadata);
        let packages = packages::parse(&workspace_packages).unwrap();

        WorkspaceTree {
            schema: SNAPSHOT_SCHEMA,
            files,
            packages,
        }
    }

    // --- get_impacted_packages tests ---

    #[test]
    fn no_changes_produces_empty_impact() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &["lib"]), ("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![],
            deleted: vec![],
        };
        let config = MainConfig::default();

        let result = get_impacted_packages(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.is_empty());
        assert!(result.affected.is_empty());
        assert!(result.required.is_empty());
    }

    #[test]
    fn changed_file_marks_package_modified() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &[]), ("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("lib/src/lib.rs")],
            deleted: vec![],
        };
        let config = MainConfig::default();

        let result = get_impacted_packages(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains(&id("lib")));
        assert!(!result.modified.contains(&id("app")));
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

        let result = get_impacted_packages(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains(&id("lib")));
        assert!(result.affected.contains(&id("lib")));
        assert!(result.affected.contains(&id("app")));
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

        let result = get_impacted_packages(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains(&id("middleware")));
        assert!(result.affected.contains(&id("app")));
        assert!(result.affected.contains(&id("middleware")));
        assert!(result.required.contains(&id("core")));
        assert!(result.required.contains(&id("middleware")));
        assert!(result.required.contains(&id("app")));
    }

    #[test]
    fn deleted_file_marks_package_modified() {
        let mut host = TestHost::new();
        let baseline = make_workspace(&[("lib", &["lib/src/lib.rs", "lib/src/old.rs"], &[])]);
        let current = make_workspace(&[("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![],
            deleted: vec![PathBuf::from("lib/src/old.rs")],
        };
        let config = MainConfig::default();

        let result = get_impacted_packages(&mut host, &baseline, &current, &diff, &config);

        assert!(result.modified.contains(&id("lib")));
    }

    #[test]
    fn new_file_in_branch_marks_package_modified() {
        let mut host = TestHost::new();
        let baseline = make_workspace(&[("lib", &["lib/src/lib.rs"], &[])]);
        let current = make_workspace(&[("lib", &["lib/src/lib.rs", "lib/src/new.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![],
            deleted: vec![],
        };
        let config = MainConfig::default();

        let result = get_impacted_packages(&mut host, &baseline, &current, &diff, &config);

        assert!(result.modified.contains(&id("lib")));
    }

    #[test]
    fn package_ids_drive_ownership_and_dependency_edges() {
        const APP_MANIFEST: &str = "tools/app-folder/Cargo.toml";
        const LIB_MANIFEST: &str = "components/not-the-package-name/Cargo.toml";
        let tree = make_schema1_workspace(&[
            (
                "application",
                "2.0.0",
                APP_MANIFEST,
                &["tools/app-folder/src/main.rs"],
                &[LIB_MANIFEST],
            ),
            (
                "package-name",
                "1.2.3",
                LIB_MANIFEST,
                &["components/not-the-package-name/src/lib.rs"],
                &[],
            ),
        ]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("components/not-the-package-name/src/lib.rs")],
            deleted: Vec::new(),
        };

        let result = get_impacted_packages(&mut TestHost::new(), &tree, &tree, &diff, &MainConfig::default());

        let app = PackageId::new("application", "2.0.0");
        let library = PackageId::new("package-name", "1.2.3");
        assert_eq!(result.modified, HashSet::from([library.clone()]));
        assert_eq!(result.affected, HashSet::from([library, app]));
    }

    #[test]
    fn deleted_baseline_package_is_not_emitted_but_surviving_dependent_is_affected() {
        const APP_MANIFEST: &str = "app/Cargo.toml";
        const DELETED_LIB_MANIFEST: &str = "lib/Cargo.toml";
        let baseline = make_schema1_workspace(&[
            ("app", "1.0.0", APP_MANIFEST, &["app/src/main.rs"], &[DELETED_LIB_MANIFEST]),
            ("removed-lib", "1.0.0", DELETED_LIB_MANIFEST, &["lib/src/lib.rs"], &[]),
        ]);
        let current = make_schema1_workspace(&[("app", "1.0.0", APP_MANIFEST, &["app/src/main.rs"], &[])]);
        let diff = GitDiff {
            changed: Vec::new(),
            deleted: vec![PathBuf::from("lib/src/lib.rs")],
        };

        let result = get_impacted_packages(&mut TestHost::new(), &baseline, &current, &diff, &MainConfig::default());

        assert!(result.modified.is_empty());
        assert_eq!(result.affected, HashSet::from([PackageId::new("app", "1.0.0")]));
        assert_eq!(result.required, HashSet::from([PackageId::new("app", "1.0.0")]));
    }

    #[test]
    fn trip_wire_activated_returns_all_packages() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &[]), ("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("Cargo.lock")],
            deleted: vec![],
        };
        let config = MainConfig {
            trip_wire_patterns: vec!["Cargo.lock".to_string()],
            ..MainConfig::default()
        };

        let result = get_impacted_packages(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains(&id("app")));
        assert!(result.modified.contains(&id("lib")));
        assert!(result.affected.contains(&id("app")));
        assert!(result.affected.contains(&id("lib")));
        assert!(host.stderr_str().contains("Trip wire activated"));
    }

    #[test]
    fn trip_wire_enabled_no_match() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("lib", &["lib/src/lib.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![PathBuf::from("lib/src/lib.rs")],
            deleted: vec![],
        };
        let config = MainConfig {
            trip_wire_patterns: vec!["Cargo.lock".to_string()],
            ..MainConfig::default()
        };

        let result = get_impacted_packages(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains(&id("lib")));
        assert!(host.stderr_str().contains("no matching files were found"));
    }

    #[test]
    fn trip_wire_on_deleted_file() {
        let mut host = TestHost::new();
        let tree = make_workspace(&[("app", &["app/src/main.rs"], &[])]);
        let diff = GitDiff {
            changed: vec![],
            deleted: vec![PathBuf::from("Cargo.lock")],
        };
        let config = MainConfig {
            trip_wire_patterns: vec!["Cargo.lock".to_string()],
            ..MainConfig::default()
        };

        let result = get_impacted_packages(&mut host, &tree, &tree, &diff, &config);

        assert!(result.modified.contains(&id("app")));
        assert!(host.stderr_str().contains("Trip wire activated"));
    }

    // --- emit_result / TierMask tests ---

    fn sample_impact() -> Impact {
        Impact {
            modified: core::iter::once(id("a")).collect(),
            affected: [id("a"), id("b")].into_iter().collect(),
            required: [id("a"), id("b"), id("c")].into_iter().collect(),
        }
    }

    fn sample_workspace_packages() -> Vec<PackageId> {
        // A workspace of 5 packages; impact above touches a/b/c, leaves d/e untouched.
        ["a", "b", "c", "d", "e"].into_iter().map(id).collect()
    }

    fn sample_workspace() -> WorkspaceTree {
        make_workspace(&[
            ("a", &["a/src/lib.rs"], &[]),
            ("b", &["b/src/lib.rs"], &[]),
            ("c", &["c/src/lib.rs"], &[]),
            ("d", &["d/src/lib.rs"], &[]),
            ("e", &["e/src/lib.rs"], &[]),
        ])
    }

    fn all_tiers() -> TierMask {
        TierMask::resolve(false, false, false)
    }

    #[test]
    fn tier_mask_resolve_no_flags_enables_all() {
        let m = TierMask::resolve(false, false, false);
        assert!(m.modified && m.affected && m.required);
    }

    #[test]
    fn tier_mask_resolve_any_flag_acts_as_filter() {
        let m = TierMask::resolve(true, false, false);
        assert!(m.modified && !m.affected && !m.required);

        let m = TierMask::resolve(false, true, true);
        assert!(!m.modified && m.affected && m.required);
    }

    #[test]
    fn union_of_tiers_dedupes_and_sorts() {
        let impact = sample_impact();
        // Affected ⊇ Modified, Required ⊇ Affected — union with all three == required.
        assert_eq!(union_of_tiers(&impact, all_tiers()), vec![id("a"), id("b"), id("c")]);
        assert_eq!(union_of_tiers(&impact, TierMask::resolve(true, false, false)), vec![id("a")]);
        assert_eq!(
            union_of_tiers(&impact, TierMask::resolve(false, true, false)),
            vec![id("a"), id("b")]
        );
    }

    #[test]
    fn emit_result_json_default_emits_all_three_keys_sorted() {
        let output = emit_result(&sample_impact(), &sample_workspace(), OutputFormat::Json, all_tiers()).unwrap();
        let stdout = String::from_utf8(output).unwrap();
        assert!(stdout.contains("\"Modified\""));
        assert!(stdout.contains("\"Affected\""));
        assert!(stdout.contains("\"Required\""));
        // Values should now be sorted arrays, not unordered HashSet dumps.
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(parsed["Required"], serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn emit_result_json_filter_omits_unselected_keys() {
        let output = emit_result(
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::Json,
            TierMask::resolve(false, true, false),
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert!(parsed.get("Modified").is_none());
        assert!(parsed.get("Required").is_none());
        assert_eq!(parsed["Affected"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn emit_result_names_emits_union_one_per_line() {
        let output = emit_result(
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::Names,
            TierMask::resolve(true, true, false),
        )
        .unwrap();
        // Modified ∪ Affected = {a, b}
        assert_eq!(output, b"a\nb\n");
    }

    #[test]
    fn emit_result_cargo_args_emits_dash_p_pairs_for_union() {
        let output = emit_result(&sample_impact(), &sample_workspace(), OutputFormat::CargoArgs, all_tiers()).unwrap();
        // Union of all tiers == required == {a, b, c}
        assert_eq!(output, b"-p a -p b -p c\n");
    }

    #[test]
    fn emit_result_cargo_args_empty_tier_emits_blank_line() {
        let empty = Impact {
            modified: HashSet::new(),
            affected: HashSet::new(),
            required: HashSet::new(),
        };
        let output = emit_result(&empty, &sample_workspace(), OutputFormat::CargoArgs, all_tiers()).unwrap();
        // Empty set ⇒ truly empty output (no trailing newline) so shell
        // callers can use `[ -z "$VAR" ]` to detect "nothing impacted".
        assert!(output.is_empty());
    }

    #[test]
    fn emit_result_cargo_excludes_emits_complement_of_union() {
        let output = emit_result(&sample_impact(), &sample_workspace(), OutputFormat::CargoExcludes, all_tiers()).unwrap();
        // Union (all tiers) = {a, b, c}; workspace = {a..e}; complement = {d, e}.
        assert_eq!(output, b"--exclude d --exclude e\n");
    }

    #[test]
    fn emit_result_cargo_excludes_filtered_tier_emits_wider_complement() {
        // Only --modified selected: union = {a}; complement = {b, c, d, e}.
        let output = emit_result(
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::CargoExcludes,
            TierMask::resolve(true, false, false),
        )
        .unwrap();
        assert_eq!(output, b"--exclude b --exclude c --exclude d --exclude e\n");
    }

    #[test]
    fn emit_result_cargo_excludes_full_workspace_selection_emits_nothing() {
        // Selection covers the whole workspace (e.g. trip wire fired) → no excludes.
        let full = Impact {
            modified: sample_workspace_packages().into_iter().collect(),
            affected: sample_workspace_packages().into_iter().collect(),
            required: sample_workspace_packages().into_iter().collect(),
        };
        let output = emit_result(&full, &sample_workspace(), OutputFormat::CargoExcludes, all_tiers()).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn emit_result_cargo_excludes_empty_selection_excludes_entire_workspace() {
        // Nothing impacted ⇒ excludes the whole workspace. Callers should detect this
        // via a separate `-f names` check and skip the cargo invocation entirely.
        let empty = Impact {
            modified: HashSet::new(),
            affected: HashSet::new(),
            required: HashSet::new(),
        };
        let output = emit_result(&empty, &sample_workspace(), OutputFormat::CargoExcludes, all_tiers()).unwrap();
        assert_eq!(output, b"--exclude a --exclude b --exclude c --exclude d --exclude e\n");
    }

    #[test]
    fn emit_result_packages_emits_sorted_canonical_specs() {
        let workspace = make_schema1_workspace(&[
            ("zeta", "2.0.0", "z/Cargo.toml", &["z/src/lib.rs"], &[]),
            ("alpha", "1.0.0", "a/Cargo.toml", &["a/src/lib.rs"], &[]),
        ]);
        let impact = Impact {
            modified: HashSet::new(),
            affected: HashSet::new(),
            required: workspace.packages.get_all().into_iter().collect(),
        };

        let output = emit_result(&impact, &workspace, OutputFormat::Packages, TierMask::resolve(false, false, true)).unwrap();

        assert_eq!(output, b"alpha@1.0.0\nzeta@2.0.0\n");
    }

    #[test]
    fn emit_result_packages_empty_selection_is_zero_bytes() {
        let workspace = make_schema1_workspace(&[("alpha", "1.0.0", "a/Cargo.toml", &["a/src/lib.rs"], &[])]);
        let empty = Impact {
            modified: HashSet::new(),
            affected: HashSet::new(),
            required: HashSet::new(),
        };

        let output = emit_result(&empty, &workspace, OutputFormat::Packages, all_tiers()).unwrap();

        assert!(output.is_empty());
    }

    #[test]
    fn unversioned_snapshot_is_rejected_with_regeneration_guidance() {
        let directory = test_directory("unversioned-snapshot");
        let path = directory.join("snapshot.json");
        let snapshot = make_workspace(&[("alpha", &["alpha/src/lib.rs"], &[])]);
        let mut snapshot = serde_json::to_value(snapshot).unwrap();
        let _removed_schema = snapshot.as_object_mut().unwrap().remove("schema");
        std::fs::write(&path, serde_json::to_vec_pretty(&snapshot).unwrap()).unwrap();

        let error = load_snapshot(&path).unwrap_err();

        assert!(error.to_string().contains("Unsupported snapshot schema 0"));
        assert!(error.to_string().contains("Regenerate this snapshot"));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn unsupported_snapshot_schema_is_rejected_clearly() {
        let mut snapshot = make_schema1_workspace(&[("alpha", "1.0.0", "a/Cargo.toml", &["a/src/lib.rs"], &[])]);
        snapshot.schema = SNAPSHOT_SCHEMA + 1;

        let error = snapshot.validate().unwrap_err();

        assert!(error.to_string().contains("Unsupported snapshot schema"));
    }

    #[test]
    fn explicit_change_sources_are_mutually_exclusive() {
        let error = Cli::try_parse_from([
            "cargo",
            "delta",
            "impact",
            "--baseline",
            "baseline.json",
            "--current",
            "current.json",
            "--base-ref",
            "origin/main",
            "--changed-files",
            "changed.json",
        ])
        .err()
        .expect("conflicting arguments should be rejected");

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn output_dir_requires_base_ref() {
        let error = Cli::try_parse_from(["cargo", "delta", "impact", "--output-dir", "artifacts"])
            .err()
            .expect("--output-dir without --base-ref should be rejected");

        assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn high_level_and_low_level_impact_options_are_mutually_exclusive() {
        for low_level in [
            ["--baseline", "baseline.json"],
            ["--current", "current.json"],
            ["--output", "impact.json"],
            ["--format", "packages"],
            ["--modified", ""],
        ] {
            let mut arguments = vec![
                "cargo",
                "delta",
                "impact",
                "--base-ref",
                "main",
                "--output-dir",
                "artifacts",
                low_level[0],
            ];
            if !low_level[1].is_empty() {
                arguments.push(low_level[1]);
            }
            let error = Cli::try_parse_from(arguments)
                .err()
                .expect("high-level and low-level options should conflict");
            assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    #[test]
    fn low_level_impact_still_requires_both_snapshots() {
        let error = Cli::try_parse_from(["cargo", "delta", "impact", "--baseline", "baseline.json"])
            .err()
            .expect("--current should remain required");

        assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument);
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

        // Uses the legacy "analyze" alias to guard against accidental alias removal.
        run(&mut host, ["cargo", "delta", "analyze"].iter().map(ToString::to_string));

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr_str().contains("Error getting cargo metadata"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn snapshot_subcommand_canonical_name_works() {
        let mut host = TestHost::new().with_commands(vec![Ok(failure_output("error: could not find Cargo.toml"))]);

        run(&mut host, ["cargo", "delta", "snapshot"].iter().map(ToString::to_string));

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr_str().contains("Snapshotting workspace"));
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

        // Uses the legacy "run" alias to guard against accidental alias removal.
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
    fn impact_subcommand_canonical_name_works() {
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output("/fake/root\n")),
            Ok(success_output("abc\trefs/heads/master\n")),
            Ok(success_output("abc123\n")),
            Ok(success_output("")),
        ]);

        run(
            &mut host,
            ["cargo", "delta", "impact", "--baseline", "fake.json", "--current", "fake.json"]
                .iter()
                .map(ToString::to_string),
        );

        assert_eq!(host.exit_code, Some(0));
        assert!(host.stderr_str().contains("Computing impact"));
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

    #[test]
    #[cfg_attr(miri, ignore)]
    fn snapshot_output_matches_stdout_and_uses_package_ids() {
        let directory = test_directory("snapshot-output");
        let package_directory = directory.join("portable");
        std::fs::create_dir_all(package_directory.join("src")).unwrap();
        std::fs::write(
            package_directory.join("Cargo.toml"),
            "[package]\nname='portable'\nversion='1.2.3'\n",
        )
        .unwrap();
        std::fs::write(package_directory.join("src").join("lib.rs"), "pub fn portable() {}\n").unwrap();

        let cargo_package_id = "path+file:///repo/portable#portable@1.2.3";
        let metadata = CargoMetadata {
            packages: vec![CargoPackage {
                id: cargo_package_id.to_string(),
                name: "portable".to_string(),
                version: "1.2.3".to_string(),
                source: None,
                targets: vec![CargoTarget {
                    name: "renamed_library".to_string(),
                    kind: vec!["lib".to_string()],
                    src_path: package_directory.join("src").join("lib.rs"),
                }],
                manifest_path: package_directory.join("Cargo.toml"),
                dependencies: Vec::new(),
            }],
            workspace_members: vec![cargo_package_id.to_string()],
            workspace_root: directory.clone(),
            target_directory: directory.join("target"),
        };
        let metadata_json = serde_json::to_string(&metadata).unwrap();
        let git_root = directory.to_string_lossy();
        let destination = directory.join("snapshot.json");
        std::fs::write(&destination, "previous snapshot").unwrap();

        let mut file_host = TestHost::new().with_commands(vec![
            Ok(success_output(&metadata_json)),
            Ok(success_output(&format!("{git_root}\n"))),
        ]);
        run(
            &mut file_host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "snapshot".to_string(),
                "--output".to_string(),
                destination.display().to_string(),
            ],
        );

        assert!(file_host.stdout.is_empty());
        assert!(file_host.exit_code.is_none());
        let file_bytes = std::fs::read(&destination).unwrap();
        let snapshot_json: serde_json::Value = serde_json::from_slice(&file_bytes).unwrap();
        assert!(snapshot_json.get("dependencies").is_none());
        assert!(snapshot_json.get("crates").is_none());
        assert_eq!(snapshot_json["packages"]["portable@1.2.3"], serde_json::json!([]));
        assert_eq!(
            snapshot_json["files"]["children"][0]["package"],
            serde_json::json!("portable@1.2.3")
        );
        let snapshot: WorkspaceTree = serde_json::from_slice(&file_bytes).unwrap();
        assert_eq!(snapshot.schema, SNAPSHOT_SCHEMA);
        assert!(snapshot.packages.contains(&PackageId::new("portable", "1.2.3")));
        assert_eq!(snapshot.files.children[0].path, PathBuf::from("portable/Cargo.toml"));

        let mut stdout_host = TestHost::new().with_commands(vec![
            Ok(success_output(&metadata_json)),
            Ok(success_output(&format!("{git_root}\n"))),
        ]);
        run(&mut stdout_host, ["cargo", "delta", "snapshot"].iter().map(ToString::to_string));
        assert_eq!(stdout_host.stdout, file_bytes);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn impact_changed_files_writes_package_specs_without_stdout() {
        let directory = test_directory("impact-output");
        let workspace = make_schema1_workspace(&[("portable-lib", "1.2.3", "lib/Cargo.toml", &["lib/src/lib.rs"], &[])]);
        let baseline = directory.join("baseline.json");
        let current = directory.join("current.json");
        let changes = directory.join("changes.json");
        let output = directory.join("modified.packages");
        let snapshot_json = serde_json::to_vec_pretty(&workspace).unwrap();
        std::fs::write(&baseline, &snapshot_json).unwrap();
        std::fs::write(&current, &snapshot_json).unwrap();
        std::fs::write(&changes, r#"{"changed":["lib/src/lib.rs"],"deleted":[]}"#).unwrap();
        std::fs::write(&output, "old output").unwrap();

        let mut host = TestHost::new().with_commands(vec![Ok(success_output(&format!("{}\n", directory.display())))]);
        run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "impact".to_string(),
                "--baseline".to_string(),
                baseline.display().to_string(),
                "--current".to_string(),
                current.display().to_string(),
                "--changed-files".to_string(),
                changes.display().to_string(),
                "--format".to_string(),
                "packages".to_string(),
                "--modified".to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert!(host.exit_code.is_none());
        assert!(host.stdout.is_empty());
        assert_eq!(std::fs::read(&output).unwrap(), b"portable-lib@1.2.3\n");
        assert_eq!(host.command_calls.len(), 1);
        assert_eq!(host.command_calls[0].args, ["rev-parse", "--show-toplevel"]);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn impact_no_changes_atomically_replaces_output_with_zero_byte_file() {
        let directory = test_directory("impact-empty-output");
        let changes = directory.join("changes.json");
        let output = directory.join("modified.packages");
        std::fs::write(&changes, r#"{"changed":[],"deleted":[]}"#).unwrap();
        std::fs::write(&output, "stale selection").unwrap();

        let mut host = TestHost::new().with_commands(vec![Ok(success_output(&format!("{}\n", directory.display())))]);
        run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "impact".to_string(),
                "--baseline".to_string(),
                "not-read-baseline.json".to_string(),
                "--current".to_string(),
                "not-read-current.json".to_string(),
                "--changed-files".to_string(),
                changes.display().to_string(),
                "--format".to_string(),
                "packages".to_string(),
                "--modified".to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, Some(0));
        assert!(host.stdout.is_empty());
        assert_eq!(std::fs::metadata(&output).unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(directory);
    }
}
