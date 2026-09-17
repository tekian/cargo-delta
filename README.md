# cargo-delta

[![crate.io](https://img.shields.io/crates/v/cargo-delta.svg)](https://crates.io/crates/cargo-delta)
[![CI](https://github.com/tekian/cargo-delta/workflows/main/badge.svg)](https://github.com/tekian/cargo-delta/actions)
[![Coverage](https://codecov.io/gh/tekian/cargo-delta/graph/badge.svg)](https://codecov.io/gh/tekian/cargo-delta)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

`cargo-delta` detects which packages in a Cargo workspace are impacted by changes in a Git feature branch. Build, test, and benchmark only the packages you need.

- [Installation](#installation)
- [Usage](#usage)
    - [Quick Start](#quick-start)
    - [CI/CD Integration](#cicd-integration)
- [Configuration](#configuration)
- [Detection Methods](#detection-methods)
    - [Module Traversal](#module-traversal)
    - [Mod Macros](#mod-macros)
    - [Include Macros](#include-macros)
    - [Pattern-based Assumptions](#pattern-based-assumptions)
    - [File Method Matching](#file-method-matching)
- [File Control](#file-control)
    - [File Exclusion](#file-exclusion)
    - [Trip Wire](#trip-wire)
- [Output](#output)
    - [Snapshot](#snapshot)
    - [Impact](#impact)
- [Limitations](#limitations)
- [Example](#example)
- [Contributing](#contributing)
- [License](#license)

## Installation

```bash
cargo install cargo-delta
```

## Usage

### Quick Start

Run impact analysis directly from the feature branch:

```bash
cargo delta impact --base-ref origin/main
```

Cargo-delta resolves the merge base, snapshots both it and the current working
tree, and caches the snapshots under `target/cargo-delta/`. Later invocations
reuse each snapshot while its embedded state key still matches. The comparison
includes committed, staged, unstaged, deleted, and non-ignored untracked files.

By default the command prints the full `Impact` JSON with all three tiers. Use
the tier toggles (`--modified`, `--affected`, `--required`) to filter. To plug
the result straight into `cargo`, change the format:

```bash
# One package per line — good for xargs / shell loops.
cargo delta impact --base-ref origin/main -f names --affected

# `-p NAME` pairs — drop into any cargo invocation via $(...).
cargo build $(cargo delta impact --base-ref origin/main -f cargo-args --affected)

# `--exclude NAME` for the workspace complement of the selected tier.
cargo build --workspace $(cargo delta impact --base-ref origin/main -f cargo-excludes --affected)

# JSON, but only the keys you care about:
cargo delta impact --base-ref origin/main --required
```

Combining tier toggles for non-JSON formats emits the **union** of the selected
tiers (deduplicated, sorted). The human-readable summary is written to stderr,
so `$(...)` capture stays clean.

#### Explicit snapshot generation

Use `cargo delta snapshot` when snapshots must be generated or transferred
separately:

```bash
git checkout main
cargo delta snapshot --output main.json

git checkout feature-branch
cargo delta snapshot --output feature.json

cargo delta impact --baseline main.json --current feature.json
```

These are the same cache-keyed snapshot artifacts used by managed mode. They can
also prepopulate `target/cargo-delta/baseline.json` and
`target/cargo-delta/current.json`; a baseline cache entry must describe the
exact merge-base checkout. Prefer `--output` for arbitrary destinations so the
output file is excluded from its own working-tree digest.

> The legacy subcommand names `analyze` (= `snapshot`) and `run` (= `impact`)
> continue to work as hidden aliases for back-compat.

### CI/CD Integration

`cargo-delta` is designed to speed up PR builds by building and testing only impacted packages.
Since detection is best-effort, a **backstop build** must run separately to catch anything delta missed or was misconfigured for.

**PR pipeline** — compare the PR working tree directly with the target branch.
Different cargo commands need different tiers:

| Command | Tier | Reasoning |
|---|---|---|
| `cargo fmt --check`, `cargo clippy` | `--modified` | Lints and formatting only matter for code the PR actually touched. Untouched code already passed on `main`. |
| `cargo build`, `cargo test`, `cargo bench` | `--affected` | A modified package can break a dependent's compile or behavior, so downstream needs to be built and tested too. |
| `cargo doc`, vendor verification | `--required` | Needs transitive dependencies in scope. |

```yaml
- name: Build, test, lint impacted packages
  run: |
    MODIFIED=$(cargo delta impact --base-ref origin/main -f cargo-args --modified)
    AFFECTED=$(cargo delta impact --base-ref origin/main -f cargo-args --affected)
    echo "Modified: $MODIFIED"
    echo "Affected: $AFFECTED"

    # Lint only what changed.
    cargo fmt --check $MODIFIED
    cargo clippy $MODIFIED -- -D warnings

    # Build & test what could be impacted by the change.
    cargo build $AFFECTED
    cargo test  $AFFECTED
```

Notes:
- The variables are **unquoted** on purpose — that's what lets the shell split
  `-p foo -p bar` into separate cargo arguments.
- If a tier ends up empty, the corresponding cargo command falls back to its
  workspace default. Add `[ -z "$AFFECTED" ] && exit 0` if you'd rather skip.
- The first call generates any missing or stale snapshot. Later calls reuse the
  matching cache entries and only repeat the inexpensive impact calculation.

**Backstop pipeline** — full build without delta, runs post-merge and/or on a nightly schedule:

```yaml
# Full workspace build and test, no delta
- run: cargo build --workspace
- run: cargo test --workspace
```

The backstop ensures correctness. If it fails on code that passed the delta-optimized PR build,
it indicates a gap in detection or a misconfigured delta — adjust the [configuration](#configuration) accordingly.

## Configuration

You can customize `cargo-delta` by providing a `-c config.toml` argument to the command.

```bash
cargo delta snapshot -c config.toml # ...
cargo delta impact -c config.toml # ...
```

Configuration options can be set globally and overridden per package. For example:

```toml
[parser]
foo = true
foo_patterns = ["*.foo", "*.bar"]

[parser.my-crate]
foo_patterns = ["*.baz"] # Override for a specific package
```

Default settings are provided in [`config.toml.example`](./config.toml.example).

## Detection Methods

### Module Traversal

Follows `mod` declarations and `#[path]` attributes to discover all Rust modules in the workspace.

### Mod Macros

Discovers modules declared via custom macros (e.g., `my_mod!`), assuming first argument is the name of the module.

Config default:

```toml
[parser]
mod_macros = []
```

Config example:

```toml
[parser]
mod_macros = ["my_mod"]  # my_mod!(foo)
```

### Include Macros

Detects files included via macros such as `include_str!` and `include_bytes!`, assuming the first argument is the name of the file.

Config default:

```toml
[parser]
includes = true
include_macros = [
    "include_str",   # include_str!("file.txt")
    "include_bytes"  # include_bytes!("file.bin")
]
```

### Pattern-based Assumptions

Assumes certain files are dependencies based on glob patterns (e.g., `*.proto`, `*.snap`).

Config default:

```toml
[parser]
assume = false
assume_patterns = []
```

Config example:

```toml
[parser.grpc_crate]
assume = true
assume_patterns = [".proto"]
```

### File Method Matching

Detects files loaded at runtime by matching method names (e.g., `from_file`, `load`, `open`), assuming the first argument is the name of the file.

Config default:

```toml
[parser]
file_refs = true
file_methods = [
    "file",       # ::file(path, ...)
    "from_file",  # ::from_file(path, ...)
    "load",       # ::load(path, ...)
    "open",       # ::open(path, ...)
    "read",       # ::read(path, ...)
    "load_from"   # ::load_from(path, ...)
]
```

## File Control

### File Exclusion

Exclude files and folders from analysis using glob patterns.

Config default:

```toml
file_exclude_patterns = ["target/**", "*.tmp"]
```

### Trip Wire

If any changed or deleted file matches a trip wire pattern, all packages are considered impacted.

Config default:

```toml
trip_wire_patterns = []
```


Config example:

```toml
trip_wire_patterns = [
    "Cargo.toml",       # top-level Cargo.toml
    "delta.toml"   # Delta config file
]
```

## Output

### Snapshot

`cargo delta snapshot` writes the same cache-keyed JSON artifact that cached
impact generation stores under `target/cargo-delta/`. It describes the workspace
at the current checkout and can be used as an explicit impact input or placed
directly at a cache path.

Snapshot generation requires the Cargo workspace to be inside a Git worktree.
The snapshot records the resolved `HEAD` commit and uses Git-root-relative paths;
outside Git, the command exits with an error and does not write a snapshot.

- **`files`** is the nested tree of detected inputs. Each package manifest node
  records its `name@version` package ID.
- **`packages`** maps each workspace package ID to its direct workspace
  dependencies, transitive external dependencies up to the next workspace
  package, and effective direct dependency declarations.

For example:

```json
{
  "files": {
    "path": "Cargo.toml",
    "kind": "Workspace",
    "children": [
      {
        "path": "crates/app/Cargo.toml",
        "kind": "Package",
        "package": "app@1.0.0",
        "children": []
      }
    ]
  },
  "packages": {
    "packages": {
      "app@1.0.0": {
        "workspace_dependencies": ["core@1.0.0"],
        "external_dependencies": [],
        "dependency_declarations": []
      },
      "core@1.0.0": {
        "workspace_dependencies": [],
        "external_dependencies": [],
        "dependency_declarations": []
      }
    },
    "resolution_complete": true
  }
}
```

Write the JSON to stdout or directly to a file:

```bash
cargo delta snapshot > snapshot.json
cargo delta snapshot --output snapshot.json

# Prepopulate the managed current cache entry, creating its directory if needed.
cargo delta snapshot --output target/cargo-delta/current.json

# Shell redirection produces the same entry when the directory already exists.
cargo delta snapshot > target/cargo-delta/current.json
```

The cache directory is always excluded from the snapshot's working-tree
digest, so both `current.json` commands produce a directly reusable cache entry.
For other destinations, prefer `--output` so cargo-delta can create parent
directories and exclude the output file from the digest.

### Impact

`cargo delta impact` combines the Git change set with the baseline and current
snapshots. The snapshots map changed, deleted, and newly discovered inputs to
packages and provide the dependency graph used to calculate impact.

#### How impact is calculated

Each invocation performs these steps:

1. Determine the base commit. `--base-ref` uses its merge base with `HEAD`; a
   cache-keyed explicit baseline uses the exact `HEAD` commit recorded when it
   was generated.
2. Ask Git for tracked files changed or deleted between that commit and the
   current working tree. This includes committed, staged, and unstaged changes.
3. Query non-ignored untracked files separately, because `git diff` does not
   report them, and treat those paths as changed.
4. Use the current snapshot to map changed and newly discovered paths to
   packages. Use the baseline snapshot for deleted paths, which no longer exist
   in the current workspace.
5. Expand the modified packages through the current dependency graph to produce
   the affected and required tiers.

When Git reports the workspace `Cargo.lock` changed, cargo-delta compares each
workspace package's baseline and current `external_dependencies` graph.
External records contain package identity, version, source, and direct resolved
dependencies. Packages whose external graph changed are modified; workspace
dependents become affected through the existing local graph. Cargo-delta does
not parse the lockfile.

Lockfile formatting, unused entries, and checksum-only edits do not change
package-build scope when Cargo reports the same resolution. Cargo and unscoped
lock-integrity checks remain responsible for validating those changes.

When Git reports the root `Cargo.toml` changed, cargo-delta compares the
manifests semantically. Changes confined to `[workspace.dependencies]`,
`workspace.members`, or `workspace.exclude` are mapped through effective package
declarations and package-set changes. Any other root setting remains a
full-workspace trip wire, including profiles, workspace lints, resolver,
workspace package defaults, patches, and unknown settings.

Full external resolution is collected with `cargo metadata --all-features
--locked` when a lockfile exists. A workspace without a lockfile remains
supported, but lockfile scoping is unavailable and therefore conservative.

Snapshot caching avoids rebuilding file ownership and dependency information,
but it does not cache the Git change set. Every impact invocation reruns the Git
queries. It also fingerprints changes relative to `HEAD`—including untracked
paths and contents—to verify whether the current snapshot cache entry is still
valid. The same untracked-file listing is reused for change detection and the
fingerprint.

- **Modified**: Packages directly modified by Git changes.
- **Affected**: Modified packages plus all their dependents, direct and indirect.
- **Required**: Affected packages plus all their dependencies, direct and indirect.

Use `--base-ref REF` for the primary cached workflow:

```bash
cargo delta impact --base-ref origin/main
```

An explicit baseline generated by the current cargo-delta version supplies the
exact Git commit used for the change set:

```bash
cargo delta impact --baseline main.json --current feature.json
```

The baseline must describe a clean checkout because its digest cannot reconstruct
uncommitted baseline content. Explicit baseline and current snapshots must both
contain a `cache_key`; older keyless artifacts are rejected as invalid.

`-f`/`--format` controls the emitted representation:

| Format | Output |
|---|---|
| `json` | Selected impact tiers as package-name arrays. |
| `names` | One package name per line. |
| `cargo-args` | One line of `-p NAME` arguments. |
| `cargo-args-versioned` | One line of `-p NAME@VERSION` arguments. |
| `cargo-excludes` | One line of `--exclude NAME` arguments for unselected workspace packages. |
| `cargo-excludes-versioned` | One line of `--exclude NAME@VERSION` arguments for unselected workspace packages. |
| `gamma-test-packages` | One line of repeated `--test-package NAME` arguments for `cargo gamma run`. |
| `packages` | One `name@version` package ID per line. |

Use `--output PATH` to write the same output directly instead of relying on
shell redirection:

```bash
cargo delta impact --baseline main.json --current feature.json --affected -f packages
cargo delta impact --baseline main.json --current feature.json --affected -f packages --output affected.packages
```

`--current` is optional and overrides current-snapshot generation. Supplying an
explicit `--baseline` is mutually exclusive with `--base-ref`; when
`--baseline` is used alone, the current snapshot is still managed:

```bash
cargo delta impact --base-ref origin/main --affected -f cargo-args-versioned
cargo delta impact --baseline main.json --affected -f cargo-args-versioned
```

Managed snapshots are stored under Cargo's target directory in
`cargo-delta/baseline.json` and `cargo-delta/current.json`. Their embedded
`cache_key` records the checkout commit and working-tree content digest,
workspace path, configuration digest, and cargo-delta version. A matching
snapshot is reused regardless of whether cargo-delta generated it implicitly or
`cargo delta snapshot` wrote it explicitly. An explicit stale snapshot is
accepted with a warning.

The working-tree comparison includes committed, staged, unstaged, deleted, and
non-ignored untracked files. If the merge base predates the Cargo workspace,
all current packages are selected.


## Limitations

This tool is **best-effort** and may not detect all dependencies:

- Dynamic file paths computed at runtime
- Conditional compilation dependencies
- Other dependencies not captured by the heuristics


## Example

```bash
$ cargo delta impact --base-ref origin/main
Computing impact..
Looking up git changes..

Changed file: "src/api/mod.rs"
Changed file: "src/utils.rs"

{
  "Modified": [
    "my-api",
    "my-utils"
  ],
  "Affected": [
    "my-api",
    "my-utils",
    "my-app"
  ],
  "Required": [
    "my-api",
    "my-utils",
    "my-app",
    "common-lib"
  ]
}

Modified      2 (Packages directly modified by Git changes.)
Affected      3 (Modified packages plus all their dependents, direct and indirect.)
Required      4 (Affected packages plus all their dependencies, direct and indirect.)
Total        15 (Total packages in this workspace.)
```

## Contributing

Fork the repository and submit a pull request.

## License

[MIT](LICENSE)
