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
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::MainConfig;
use crate::crates::{PackageId, Packages, package_name};
use crate::files::FileNode;
use crate::git::GitDiff;

mod cargo;
mod config;
mod crates;
mod error;
mod files;
mod git;
mod host;
mod managed;
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

/// Identify impacted packages from git changes.
#[derive(Parser)]
#[command(name = "cargo-delta", author, version, long_about = None, display_name = "cargo-delta")]
#[command(about = "Identify impacted packages from git changes")]
struct Args {
    /// Path to configuration file (defaults to `delta.toml`)
    #[arg(short = 'c', long, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compute impacted packages from a pair of snapshots
    #[command(alias = "run")]
    Impact(ImpactCommand),
    /// Snapshot the current workspace into a JSON artifact
    #[command(alias = "analyze")]
    Snapshot(SnapshotCommand),
}

#[derive(Parser)]
struct ImpactCommand {
    /// Baseline workspace analysis JSON file (e.g., from main branch)
    #[arg(long, value_name = "PATH", required_unless_present = "base_ref", conflicts_with = "base_ref")]
    baseline: Option<PathBuf>,
    /// Current workspace analysis JSON file (e.g., from feature branch)
    #[arg(long, value_name = "PATH")]
    current: Option<PathBuf>,
    /// Compare HEAD with the merge base of this Git ref.
    #[arg(long, value_name = "REF")]
    base_ref: Option<String>,
    /// Write the selected format to this path instead of stdout.
    #[arg(long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// Output format on stdout. Non-json formats emit the union of the selected tiers and
    /// suppress the human-readable banner from stdout (it still goes to stderr), so the
    /// output can be captured directly, e.g. `cargo build $(cargo delta run ... -f cargo-args)`.
    #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::Json)]
    format: OutputFormat,
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
    /// One package name per line - convenient for `xargs` or shell loops.
    Names,
    /// Space-separated `-p NAME` arguments - drop straight into a `cargo` invocation
    /// via `$(cargo delta run ... -f cargo-args)`.
    CargoArgs,
    /// Space-separated `-p NAME@VERSION` arguments.
    CargoArgsVersioned,
    /// Space-separated `--exclude NAME` arguments for the *complement* of the selected
    /// tier(s) within the workspace. Use with `cargo --workspace` to scope to impacted
    /// packages without `-p` ambiguity (since `--exclude` matches workspace members only,
    /// it can't collide with same-named transitive registry deps). Empty when the
    /// selected tier covers (or exceeds) the workspace; combine with `-f names` for
    /// a "nothing impacted" check.
    CargoExcludes,
    /// Space-separated `--exclude NAME@VERSION` arguments for the workspace complement.
    CargoExcludesVersioned,
    /// Space-separated `--test-package NAME` arguments for `cargo gamma run`.
    GammaTestPackages,
    /// One canonical `name@version` package ID per line.
    Packages,
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
    /// Write the snapshot JSON to this path instead of stdout.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<managed::SnapshotCacheKey>,
    pub files: FileNode,
    pub packages: Packages,
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
        Commands::Impact(cmd) => impact(host, &config, cmd, cli.config.as_ref()),

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

    let caller_dir = match host.current_dir() {
        Ok(path) => path,
        Err(error) => {
            let _ = writeln!(host.error(), "Error getting current directory: {error}");
            host.exit(1);
            return;
        }
    };

    let metadata = match cargo::metadata(host, Some(&caller_dir)) {
        Ok(metadata) => metadata,
        Err(e) => {
            let _ = writeln!(host.error(), "Error getting cargo metadata: {e}");
            host.exit(1);
            return;
        }
    };

    let workspace_root = &metadata.workspace_root;

    let git_root = match git::get_top_level(host, Some(&caller_dir)) {
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

    let cache_key = match snapshot_cache_key(host, &metadata, &git_root, config_path, output) {
        Ok(key) => key,
        Err(error) => {
            let _ = writeln!(host.error(), "Error computing snapshot cache key: {error}");
            host.exit(1);
            return;
        }
    };
    let workspace_tree = build_snapshot(host, config, &metadata, &git_root, Some(cache_key));

    let _ = writeln!(host.error(), "Found {} package(s) in the workspace.", workspace_tree.packages.len());
    let _ = writeln!(host.error(), "Found {} file(s) in the workspace.", workspace_tree.files.len());
    let _ = writeln!(host.error());

    match serde_json::to_string_pretty(&workspace_tree) {
        Ok(json_output) => {
            if !write_output(host, output, &format!("{json_output}\n")) {
                return;
            }
        }
        Err(e) => {
            let _ = writeln!(host.error(), "Error serializing workspace tree to JSON: {e}");
            host.exit(1);
            return;
        }
    }

    let _ = writeln!(host.error());
    let excludes: Vec<PathBuf> = workspace_tree.files.distinct().into_iter().collect();

    let unrelated = utils::find_unrelated(&git_root, &excludes, &config.file_exclude_patterns, &config.trip_wire_patterns);

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

    let duration = start.elapsed();
    let _ = writeln!(host.error(), "\nSnapshot finished in {duration:.2?}");
}

fn snapshot_cache_key(
    host: &mut impl Host,
    metadata: &cargo::CargoMetadata,
    git_root: &Path,
    config_path: Option<&PathBuf>,
    output: Option<&Path>,
) -> error::Result<managed::SnapshotCacheKey> {
    let caller_dir = host
        .current_dir()
        .map_err(|error| error::Error::Other(format!("Failed to get current directory: {error}")))?;
    let mut excluded_paths = vec![metadata.target_directory.join("cargo-delta")];
    if let Some(path) = output {
        excluded_paths.push({
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                caller_dir.join(path)
            }
        });
    }
    managed::current_cache_key(host, metadata, git_root, config_path, true, &excluded_paths)
}

fn build_snapshot(
    host: &mut impl Host,
    config: &MainConfig,
    metadata: &cargo::CargoMetadata,
    git_root: &Path,
    cache_key: Option<managed::SnapshotCacheKey>,
) -> WorkspaceTree {
    let packages = cargo::get_workspace_packages(metadata);
    let mut files = files::build_tree(host, metadata, &packages, config);
    let packages = crates::parse(metadata);
    files.make_relative_paths(git_root);
    WorkspaceTree {
        cache_key,
        files,
        packages,
    }
}

fn explicit_baseline_head<'a>(path: &Path, tree: &'a WorkspaceTree) -> error::Result<&'a str> {
    managed::cache_key(path, tree, "baseline")?.clean_head().ok_or_else(|| {
        error::Error::Other(format!(
            "Explicit baseline snapshot '{}' contains working-tree changes and cannot define a Git diff base",
            path.display()
        ))
    })
}

fn resolve_impact_inputs(
    host: &mut impl Host,
    config: &MainConfig,
    command: &ImpactCommand,
    config_path: Option<&PathBuf>,
) -> error::Result<Option<managed::ManagedImpact>> {
    let caller_dir = host
        .current_dir()
        .map_err(|error| error::Error::Other(format!("Failed to get current directory: {error}")))?;
    let git_root = git::get_top_level(host, Some(&caller_dir))?;

    if let (Some(baseline_path), Some(current_path)) = (command.baseline.as_deref(), command.current.as_deref()) {
        let baseline: WorkspaceTree = utils::deser_json(baseline_path)?;
        let current: WorkspaceTree = utils::deser_json(current_path)?;
        let _ = managed::cache_key(current_path, &current, "current")?;
        let excluded_paths = [command.baseline.clone(), command.current.clone(), command.output.clone()]
            .into_iter()
            .flatten()
            .map(|path| if path.is_absolute() { path } else { caller_dir.join(path) })
            .collect::<Vec<_>>();
        let base_commit = explicit_baseline_head(baseline_path, &baseline)?;
        let comparison = git::compare_from_commit(host, &git_root, base_commit, true, &excluded_paths)?;
        if comparison.diff.changed.is_empty() && comparison.diff.deleted.is_empty() {
            return Ok(None);
        }
        let metadata = cargo::metadata(host, Some(&caller_dir))
            .map_err(|error| error::Error::Other(format!("Failed to read current Cargo metadata: {error}")))?;
        let mut excluded_paths = excluded_paths;
        excluded_paths.push(metadata.target_directory.join("cargo-delta"));
        let current_key = managed::current_cache_key(host, &metadata, &git_root, config_path, true, &excluded_paths)?;
        let baseline_key = managed::baseline_cache_key(&metadata, &git_root, config_path, &comparison.base_commit, true)?;
        managed::warn_if_stale(host, baseline_path, &baseline, &baseline_key, "baseline")?;
        managed::warn_if_stale(host, current_path, &current, &current_key, "current")?;
        return Ok(Some(managed::ManagedImpact {
            baseline,
            current,
            diff: comparison.diff,
            widen: false,
        }));
    }

    let metadata = cargo::metadata(host, Some(&caller_dir))
        .map_err(|error| error::Error::Other(format!("Failed to read current Cargo metadata: {error}")))?;
    let mut excluded_paths = [command.baseline.clone(), command.current.clone(), command.output.clone()]
        .into_iter()
        .flatten()
        .map(|path| if path.is_absolute() { path } else { caller_dir.join(path) })
        .collect::<Vec<_>>();
    excluded_paths.push(metadata.target_directory.join("cargo-delta"));
    let baseline = match command.baseline.as_deref() {
        Some(path) => Some(managed::ExplicitSnapshot {
            path,
            tree: utils::deser_json(path)?,
        }),
        None => None,
    };
    let comparison = match baseline.as_ref() {
        Some(explicit) => {
            let base_commit = explicit_baseline_head(explicit.path, &explicit.tree)?;
            git::compare_from_commit(host, &git_root, base_commit, true, &excluded_paths)?
        }
        None => git::compare(
            host,
            &git_root,
            config.git.as_ref(),
            command.base_ref.as_deref(),
            true,
            &excluded_paths,
        )?,
    };
    managed::resolve(
        host,
        config,
        managed::ResolveContext {
            config_path,
            comparison,
            git_root: &git_root,
            current_metadata: &metadata,
            baseline,
            current_path: command.current.as_deref(),
            excluded_paths: &excluded_paths,
        },
    )
    .map(Some)
}

#[doc(hidden)]
fn impact(host: &mut impl Host, config: &MainConfig, command: &ImpactCommand, config_path: Option<&PathBuf>) {
    let _ = writeln!(host.error(), "Computing impact..");
    print_common_props(host, config_path);
    let tiers = TierMask::resolve(command.modified, command.affected, command.required);

    let _ = writeln!(host.error(), "Looking up git changes..");
    let managed::ManagedImpact {
        baseline: baseline_tree,
        current: current_tree,
        diff,
        widen,
    } = match resolve_impact_inputs(host, config, command, config_path) {
        Ok(Some(inputs)) => inputs,
        Ok(None) => {
            let _ = writeln!(host.error(), "No file has been changed or deleted, quitting.");
            if !write_output(host, command.output.as_deref(), "") {
                return;
            }
            host.exit(0);
            return;
        }
        Err(error) => {
            let _ = writeln!(host.error(), "Error resolving impact inputs: {error}");
            host.exit(1);
            return;
        }
    };

    if diff.changed.is_empty() && diff.deleted.is_empty() && !widen {
        let _ = writeln!(host.error(), "No file has been changed or deleted, quitting.");
        if !write_output(host, command.output.as_deref(), "") {
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
    let _ = writeln!(host.error());

    let result = if widen {
        let packages: HashSet<PackageId> = current_tree.packages.get_all_package_ids().into_iter().collect();
        Impact {
            modified: packages.clone(),
            affected: packages.clone(),
            required: packages,
        }
    } else {
        get_impacted_packages(host, &baseline_tree, &current_tree, &diff, config)
    };

    if !emit_result(
        host,
        &result,
        &current_tree.packages,
        command.format,
        tiers,
        command.output.as_deref(),
    ) {
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

#[doc(hidden)]
fn emit_result(
    host: &mut impl Host,
    result: &Impact,
    workspace: &Packages,
    format: OutputFormat,
    tiers: TierMask,
    output: Option<&Path>,
) -> bool {
    let selected = union_of_tiers(result, tiers);
    let text = match format {
        OutputFormat::Json => {
            let mut obj = serde_json::Map::new();
            if tiers.modified {
                let _ = obj.insert("Modified".to_string(), json!(sorted_names(&result.modified)));
            }
            if tiers.affected {
                let _ = obj.insert("Affected".to_string(), json!(sorted_names(&result.affected)));
            }
            if tiers.required {
                let _ = obj.insert("Required".to_string(), json!(sorted_names(&result.required)));
            }
            match serde_json::to_string_pretty(&serde_json::Value::Object(obj)) {
                Ok(json_output) => format!("{json_output}\n"),
                Err(e) => {
                    let _ = writeln!(host.error(), "Error serializing result to JSON: {e}");
                    host.exit(1);
                    return false;
                }
            }
        }
        OutputFormat::Names => lines(selected.iter().map(package_name)),
        OutputFormat::CargoArgs | OutputFormat::CargoArgsVersioned => {
            let versioned = format == OutputFormat::CargoArgsVersioned;
            let joined = selected
                .iter()
                .map(|package| {
                    let package = if versioned { package.as_str() } else { package_name(package) };
                    format!("-p {package}")
                })
                .collect::<Vec<_>>()
                .join(" ");
            lines(core::iter::once(joined).filter(|value| !value.is_empty()))
        }
        OutputFormat::CargoExcludes | OutputFormat::CargoExcludesVersioned => {
            let versioned = format == OutputFormat::CargoExcludesVersioned;
            let selected: HashSet<PackageId> = selected.into_iter().collect();
            let mut unselected = workspace
                .get_all_package_ids()
                .iter()
                .filter(|package| !selected.contains(*package))
                .map(|package| {
                    if versioned {
                        package.clone()
                    } else {
                        package_name(package).to_string()
                    }
                })
                .collect::<Vec<_>>();
            unselected.sort();
            let joined = unselected
                .into_iter()
                .map(|name| format!("--exclude {name}"))
                .collect::<Vec<_>>()
                .join(" ");
            lines(core::iter::once(joined).filter(|value| !value.is_empty()))
        }
        OutputFormat::GammaTestPackages => {
            let joined = selected
                .iter()
                .map(|package| format!("--test-package {}", package_name(package)))
                .collect::<Vec<_>>()
                .join(" ");
            lines(core::iter::once(joined).filter(|value| !value.is_empty()))
        }
        OutputFormat::Packages => lines(selected.iter().map(String::as_str)),
    };
    write_output(host, output, &text)
}

fn lines<T: AsRef<str>>(values: impl IntoIterator<Item = T>) -> String {
    let mut rendered = String::new();
    for value in values {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(value.as_ref());
    }
    if !rendered.is_empty() {
        rendered.push('\n');
    }
    rendered
}

fn write_output(host: &mut impl Host, output: Option<&Path>, text: &str) -> bool {
    let result = match output {
        Some(path) => {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                match host.current_dir() {
                    Ok(current_dir) => current_dir.join(path),
                    Err(error) => {
                        let _ = writeln!(host.error(), "Error getting current directory: {error}");
                        host.exit(1);
                        return false;
                    }
                }
            };
            path.parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::write(path, text))
        }
        None => host.output().write_all(text.as_bytes()),
    };
    if let Err(error) = result {
        let destination = output.map_or_else(|| "stdout".to_string(), |path| path.display().to_string());
        let _ = writeln!(host.error(), "Error writing output to {destination}: {error}");
        host.exit(1);
        return false;
    }
    true
}

fn sorted_names(set: &HashSet<PackageId>) -> Vec<&str> {
    let mut names: Vec<&str> = set.iter().map(package_name).collect();
    names.sort_unstable();
    names
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

    if !config.trip_wire_patterns.is_empty() {
        use glob::Pattern;

        let trip_wire_patterns: Vec<Pattern> = config
            .trip_wire_patterns
            .iter()
            .filter_map(|pattern| Pattern::new(pattern).ok())
            .collect();

        let mut tripped_files = Vec::new();

        for deleted_file in &git_diff.deleted {
            let file_str = deleted_file.to_string_lossy();
            if trip_wire_patterns.iter().any(|pattern| pattern.matches(&file_str)) {
                tripped_files.push(file_str.to_string());
            }
        }

        for changed_file in &git_diff.changed {
            let file_str = changed_file.to_string_lossy();
            if trip_wire_patterns.iter().any(|pattern| pattern.matches(&file_str)) {
                tripped_files.push(file_str.to_string());
            }
        }

        if !tripped_files.is_empty() {
            let _ = writeln!(
                host.error(),
                "WARNING: Trip wire activated due to changes in the following file(s):"
            );
            for file in &tripped_files {
                let _ = writeln!(host.error(), "- {file}");
            }
            let _ = writeln!(host.error());

            let all_packages: HashSet<PackageId> = current_tree.packages.get_all_package_ids().into_iter().collect();

            return Impact {
                modified: all_packages.clone(),
                affected: all_packages.clone(),
                required: all_packages,
            };
        }

        let _ = writeln!(host.error(), "Trip wire is enabled, but no matching files were found, good.");
        let _ = writeln!(host.error());
    }

    for deleted_file in &git_diff.deleted {
        let packages_for_file = baseline_tree.files.find_packages_containing_file(deleted_file);

        for package in packages_for_file {
            if let Some(current_package) = current_tree.packages.find_by_name(&package) {
                let _ = modified.insert(current_package);
                continue;
            }
            if let Some(dependents) = baseline_tree.packages.get_dependents_transitive(&package) {
                for dependent in dependents {
                    if let Some(current_dependent) = current_tree.packages.find_by_name(&dependent) {
                        let _ = affected_seeds.insert(current_dependent);
                    }
                }
            }
        }
    }

    for changed_file in &git_diff.changed {
        let packages_for_file = current_tree.files.find_packages_containing_file(changed_file);

        for package in packages_for_file {
            let _ = modified.insert(package);
        }
    }

    let main_files = baseline_tree.files.distinct();
    let branch_files = current_tree.files.distinct();

    for new_file in branch_files.difference(&main_files) {
        let packages_for_file = current_tree.files.find_packages_containing_file(new_file);

        for package in packages_for_file {
            let _ = modified.insert(package);
        }
    }

    // Affected = Modified + all their dependents
    let mut affected = modified.clone();
    affected.extend(affected_seeds);
    for package in &modified {
        if let Some(transitive_dependents) = current_tree.packages.get_dependents_transitive(package) {
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
    use crate::crates::package_id;
    use crate::files::FileKind;
    use crate::test_helpers::*;

    fn id(name: &str) -> PackageId {
        package_id(name, "0.1.0")
    }

    fn make_metadata(package_deps: &[(&str, &[&str])]) -> CargoMetadata {
        let mut packages = Vec::new();
        for (name, deps) in package_deps {
            packages.push(CargoPackage {
                name: name.to_string(),
                version: "0.1.0".to_string(),
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

    fn make_workspace(package_defs: &[(&str, &[&str], &[&str])]) -> WorkspaceTree {
        let deps: Vec<(&str, &[&str])> = package_defs.iter().map(|(name, _, deps)| (*name, *deps)).collect();
        let package_files: Vec<(&str, &[&str])> = package_defs.iter().map(|(name, files, _)| (*name, *files)).collect();

        let metadata = make_metadata(&deps);
        let files = make_file_tree(&package_files);
        let packages = crates::parse(&metadata);

        WorkspaceTree {
            cache_key: None,
            files,
            packages,
        }
    }

    fn make_keyed_workspace(package_defs: &[(&str, &[&str], &[&str])], head: &str) -> WorkspaceTree {
        let mut tree = make_workspace(package_defs);
        tree.cache_key = Some(managed::SnapshotCacheKey::clean_test_key(head));
        tree
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

    fn sample_workspace() -> Packages {
        make_workspace(&[
            ("a", &["a/src/lib.rs"], &[]),
            ("b", &["b/src/lib.rs"], &[]),
            ("c", &["c/src/lib.rs"], &[]),
            ("d", &["d/src/lib.rs"], &[]),
            ("e", &["e/src/lib.rs"], &[]),
        ])
        .packages
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
    fn managed_and_explicit_snapshot_options_have_expected_relationships() {
        let _managed = Cli::try_parse_from(["cargo", "delta", "impact", "--base-ref", "origin/main"]).unwrap();
        let _generated_current = Cli::try_parse_from(["cargo", "delta", "impact", "--baseline", "base.json"]).unwrap();
        let _explicit_current =
            Cli::try_parse_from(["cargo", "delta", "impact", "--base-ref", "origin/main", "--current", "current.json"]).unwrap();
        let _conflict = Cli::try_parse_from(["cargo", "delta", "impact", "--baseline", "base.json", "--base-ref", "origin/main"])
            .err()
            .expect("baseline and base-ref must conflict");
        let _missing_baseline = Cli::try_parse_from(["cargo", "delta", "impact", "--current", "current.json"])
            .err()
            .expect("baseline or base-ref must be required");
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
        let mut host = TestHost::new();
        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::Json,
            all_tiers(),
            None,
        );
        assert!(ok);
        let stdout = host.stdout_str();
        assert!(stdout.contains("\"Modified\""));
        assert!(stdout.contains("\"Affected\""));
        assert!(stdout.contains("\"Required\""));
        // Values should now be sorted arrays, not unordered HashSet dumps.
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(parsed["Required"], serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn emit_result_json_filter_omits_unselected_keys() {
        let mut host = TestHost::new();
        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::Json,
            TierMask::resolve(false, true, false),
            None,
        );
        assert!(ok);
        let parsed: serde_json::Value = serde_json::from_str(&host.stdout_str()).unwrap();
        assert!(parsed.get("Modified").is_none());
        assert!(parsed.get("Required").is_none());
        assert_eq!(parsed["Affected"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn emit_result_names_emits_union_one_per_line() {
        let mut host = TestHost::new();
        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::Names,
            TierMask::resolve(true, true, false),
            None,
        );
        assert!(ok);
        // Modified ∪ Affected = {a, b}
        assert_eq!(host.stdout_str(), "a\nb\n");
    }

    #[test]
    fn emit_result_cargo_args_emits_dash_p_pairs_for_union() {
        let mut host = TestHost::new();
        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::CargoArgs,
            all_tiers(),
            None,
        );
        assert!(ok);
        // Union of all tiers == required == {a, b, c}
        assert_eq!(host.stdout_str(), "-p a -p b -p c\n");
    }

    #[test]
    fn emit_result_versioned_cargo_args_include_versions() {
        let mut host = TestHost::new();

        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::CargoArgsVersioned,
            all_tiers(),
            None,
        );

        assert!(ok);
        assert_eq!(host.stdout_str(), "-p a@0.1.0 -p b@0.1.0 -p c@0.1.0\n");
    }

    #[test]
    fn emit_result_cargo_args_empty_tier_emits_blank_line() {
        let mut host = TestHost::new();
        let empty = Impact {
            modified: HashSet::new(),
            affected: HashSet::new(),
            required: HashSet::new(),
        };
        let ok = emit_result(&mut host, &empty, &sample_workspace(), OutputFormat::CargoArgs, all_tiers(), None);
        assert!(ok);
        // Empty set ⇒ truly empty output (no trailing newline) so shell
        // callers can use `[ -z "$VAR" ]` to detect "nothing impacted".
        assert_eq!(host.stdout_str(), "");
    }

    #[test]
    fn emit_result_cargo_excludes_emits_complement_of_union() {
        let mut host = TestHost::new();
        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::CargoExcludes,
            all_tiers(),
            None,
        );
        assert!(ok);
        // Union (all tiers) = {a, b, c}; workspace = {a..e}; complement = {d, e}.
        assert_eq!(host.stdout_str(), "--exclude d --exclude e\n");
    }

    #[test]
    fn emit_result_versioned_cargo_excludes_include_versions() {
        let mut host = TestHost::new();

        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::CargoExcludesVersioned,
            all_tiers(),
            None,
        );

        assert!(ok);
        assert_eq!(host.stdout_str(), "--exclude d@0.1.0 --exclude e@0.1.0\n");
    }

    #[test]
    fn emit_result_cargo_excludes_filtered_tier_emits_wider_complement() {
        let mut host = TestHost::new();
        // Only --modified selected: union = {a}; complement = {b, c, d, e}.
        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::CargoExcludes,
            TierMask::resolve(true, false, false),
            None,
        );
        assert!(ok);
        assert_eq!(host.stdout_str(), "--exclude b --exclude c --exclude d --exclude e\n");
    }

    #[test]
    fn emit_result_cargo_excludes_full_workspace_selection_emits_nothing() {
        let mut host = TestHost::new();
        // Selection covers the whole workspace (e.g. trip wire fired) → no excludes.
        let full = Impact {
            modified: sample_workspace().get_all_package_ids().into_iter().collect(),
            affected: sample_workspace().get_all_package_ids().into_iter().collect(),
            required: sample_workspace().get_all_package_ids().into_iter().collect(),
        };
        let ok = emit_result(
            &mut host,
            &full,
            &sample_workspace(),
            OutputFormat::CargoExcludes,
            all_tiers(),
            None,
        );
        assert!(ok);
        assert_eq!(host.stdout_str(), "");
    }

    #[test]
    fn emit_result_cargo_excludes_empty_selection_excludes_entire_workspace() {
        let mut host = TestHost::new();
        // Nothing impacted ⇒ excludes the whole workspace. Callers should detect this
        // via a separate `-f names` check and skip the cargo invocation entirely.
        let empty = Impact {
            modified: HashSet::new(),
            affected: HashSet::new(),
            required: HashSet::new(),
        };
        let ok = emit_result(
            &mut host,
            &empty,
            &sample_workspace(),
            OutputFormat::CargoExcludes,
            all_tiers(),
            None,
        );
        assert!(ok);
        assert_eq!(host.stdout_str(), "--exclude a --exclude b --exclude c --exclude d --exclude e\n");
    }

    #[test]
    fn emit_result_packages_emits_name_and_version() {
        let mut host = TestHost::new();

        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::Packages,
            TierMask::resolve(false, false, true),
            None,
        );

        assert!(ok);
        assert_eq!(host.stdout_str(), "a@0.1.0\nb@0.1.0\nc@0.1.0\n");
    }

    #[test]
    fn emit_result_gamma_test_packages_emits_repeated_package_arguments() {
        let mut host = TestHost::new();

        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::GammaTestPackages,
            all_tiers(),
            None,
        );

        assert!(ok);
        assert_eq!(host.stdout_str(), "--test-package a --test-package b --test-package c\n");
    }

    #[test]
    fn snapshot_uses_package_ids_for_graph_and_ownership() {
        let snapshot = serde_json::to_value(make_workspace(&[
            ("app", &["app/src/main.rs"], &["lib"]),
            ("lib", &["lib/src/lib.rs"], &[]),
        ]))
        .unwrap();

        assert!(snapshot.get("crates").is_none());
        assert_eq!(snapshot["packages"]["packages"]["app@0.1.0"], serde_json::json!(["lib@0.1.0"]));
        assert_eq!(snapshot["files"]["children"][0]["package"], "app@0.1.0");
    }

    #[test]
    fn output_path_receives_content_instead_of_stdout() {
        let path = std::env::temp_dir().join(format!("cargo-delta-output-{}.txt", std::process::id()));
        let mut host = TestHost::new();

        let ok = emit_result(
            &mut host,
            &sample_impact(),
            &sample_workspace(),
            OutputFormat::Packages,
            TierMask::resolve(true, false, false),
            Some(&path),
        );

        assert!(ok);
        assert!(host.stdout.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "a@0.1.0\n");
        let _ = fs::remove_file(path);
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
    fn snapshot_stops_after_output_write_failure() {
        let root = std::env::temp_dir().join(format!("cargo-delta-snapshot-write-failure-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let metadata = CargoMetadata {
            packages: Vec::new(),
            workspace_root: root.clone(),
            target_directory: root.join("target"),
        };
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output(&serde_json::to_string(&metadata).unwrap())),
            Ok(success_output(&format!("{}\n", root.display()))),
            Ok(success_output("head\n")),
            Ok(success_output("")),
            Ok(success_output("")),
        ]);

        run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "snapshot".to_string(),
                "--output".to_string(),
                root.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr_str().contains("Error writing output"));
        assert!(!host.stderr_str().contains("Snapshot finished"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn snapshot_requires_a_git_worktree() {
        let root = std::env::temp_dir().join(format!("cargo-delta-snapshot-no-git-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let output = root.join("snapshot.json");
        let metadata = CargoMetadata {
            packages: Vec::new(),
            workspace_root: root.clone(),
            target_directory: root.join("target"),
        };
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output(&serde_json::to_string(&metadata).unwrap())),
            Ok(failure_output("fatal: not a git repository")),
        ]);

        run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "snapshot".to_string(),
                "--output".to_string(),
                output.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr_str().contains("not a git repository"));
        assert!(!output.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn run_subcommand_no_changes_exits_zero() {
        let tmp = std::env::temp_dir().join(format!("cargo-delta-test-run-no-changes-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let snapshot = serde_json::to_string_pretty(&make_keyed_workspace(&[], "abc123")).unwrap();
        let baseline = tmp.join("baseline.json");
        let current = tmp.join("current.json");
        fs::write(&baseline, &snapshot).unwrap();
        fs::write(&current, snapshot).unwrap();
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output(&format!("{}\n", tmp.display()))), // git rev-parse
            Ok(success_output("")),                              // git diff (no changes)
            Ok(success_output("")),                              // untracked files
        ]);

        // Uses the legacy "run" alias to guard against accidental alias removal.
        run(
            &mut host,
            [
                "cargo".to_string(),
                "delta".to_string(),
                "run".to_string(),
                "--baseline".to_string(),
                baseline.display().to_string(),
                "--current".to_string(),
                current.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, Some(0), "{}", host.stderr_str());
        assert!(host.stderr_str().contains("No file has been changed"));
        fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn impact_subcommand_canonical_name_works() {
        let tmp = std::env::temp_dir().join(format!("cargo-delta-test-impact-name-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let snapshot = serde_json::to_string_pretty(&make_keyed_workspace(&[], "abc123")).unwrap();
        let baseline = tmp.join("baseline.json");
        let current = tmp.join("current.json");
        fs::write(&baseline, &snapshot).unwrap();
        fs::write(&current, snapshot).unwrap();
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output(&format!("{}\n", tmp.display()))),
            Ok(success_output("")),
            Ok(success_output("")),
        ]);

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
            ],
        );

        assert_eq!(host.exit_code, Some(0), "{}", host.stderr_str());
        assert!(host.stderr_str().contains("Computing impact"));
        fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn run_subcommand_with_changes_produces_output() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_run_changes");
        let _ = fs::create_dir_all(&tmp);

        let tree = make_keyed_workspace(
            &[("app", &["app/src/main.rs"], &["lib"]), ("lib", &["lib/src/lib.rs"], &[])],
            "abc123",
        );
        let json = serde_json::to_string_pretty(&tree).unwrap();
        let baseline_path = tmp.join("baseline.json");
        let current_path = tmp.join("current.json");
        fs::write(&baseline_path, &json).unwrap();
        fs::write(&current_path, &json).unwrap();

        let metadata = CargoMetadata {
            packages: Vec::new(),
            workspace_root: tmp.clone(),
            target_directory: tmp.join("target"),
        };
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output(&format!("{}\n", tmp.display()))), // git rev-parse
            Ok(success_output("D\0lib/src/lib.rs\0")),           // git diff
            Ok(success_output("")),                              // untracked files
            Ok(success_output(&serde_json::to_string(&metadata).unwrap())),
            Ok(success_output("head\n")),
            Ok(success_output("")),
            Ok(success_output("")),
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

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn impact_stops_after_output_write_failure() {
        let root = std::env::temp_dir().join(format!("cargo-delta-impact-write-failure-{}", std::process::id()));
        fs::create_dir_all(root.join("lib/src")).unwrap();
        fs::write(root.join("lib/src/lib.rs"), "pub fn value() {}\n").unwrap();
        let tree = make_keyed_workspace(&[("lib", &["lib/src/lib.rs"], &[])], "abc123");
        let snapshot = serde_json::to_string_pretty(&tree).unwrap();
        let baseline = root.join("baseline.json");
        let current = root.join("current.json");
        fs::write(&baseline, &snapshot).unwrap();
        fs::write(&current, &snapshot).unwrap();
        let metadata = CargoMetadata {
            packages: Vec::new(),
            workspace_root: root.clone(),
            target_directory: root.join("target"),
        };
        let mut host = TestHost::new().with_commands(vec![
            Ok(success_output(&format!("{}\n", root.display()))),
            Ok(success_output("M\0lib/src/lib.rs\0")),
            Ok(success_output("")),
            Ok(success_output(&serde_json::to_string(&metadata).unwrap())),
            Ok(success_output("head\n")),
            Ok(success_output("")),
            Ok(success_output("")),
        ]);

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
                "--output".to_string(),
                root.display().to_string(),
            ],
        );

        assert_eq!(host.exit_code, Some(1));
        assert!(host.stderr_str().contains("Error writing output"));
        assert!(!host.stderr_str().contains("Modified"));
        fs::remove_dir_all(root).unwrap();
    }
}
