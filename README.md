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
    - [Impact output formats](#impact-output-formats)
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

The high-level mode resolves both commits locally, snapshots them without
changing your checkout, and writes the complete artifact set:

```bash
cargo delta impact \
  --base-ref origin/main \
  --output-dir target/cargo-delta
```

The output directory contains `impact.json`, one package file for each impact
tier, both snapshots, and `manifest.json`. Package files contain sorted
`name@version` specs and are zero bytes when their tier is empty. The manifest
is published last and includes hashes for concurrent-reader validation.

The workspace must be clean by default. To conservatively continue from a
dirty checkout, widen every tier to the full current workspace:

```bash
cargo delta impact \
  --base-ref origin/main \
  --output-dir target/cargo-delta \
  --dirty workspace
```

The command never fetches, so `origin/main` (or another requested ref) and its
merge-base history must already exist locally.

`--output-dir` is normalized relative to the invocation directory. When it is
inside the repository, cargo-delta excludes only that exact subtree from dirty
checking, so repeated runs work even when the directory is not ignored. The
directory must not be the repository root, Git metadata, a symlinked path, or
overlap tracked content; unrelated untracked files still follow the selected
dirty policy.

The low-level snapshot workflow remains available when snapshots or changed
paths are managed externally:

1. **Snapshot the baseline branch:**
   ```bash
   git checkout main
   cargo delta snapshot > main.json
   ```

2. **Snapshot the feature branch:**
   ```bash
   git checkout feature-branch
   cargo delta snapshot > feature.json
   ```

3. **Compute the impact:**
   ```bash
   cargo delta impact --baseline main.json --current feature.json
   ```

   By default, both commands write their machine-readable JSON to stdout. Use
   `--output PATH` to atomically write each result to a file instead of relying
   on shell redirection:

   ```bash
   git checkout main
   cargo delta snapshot --output main.json

   git checkout feature-branch
   cargo delta snapshot --output feature.json

   cargo delta impact \
     --baseline main.json \
     --current feature.json \
     --base-ref origin/main \
     --affected \
     --format packages \
     --output affected.packages
   ```

   `affected.packages` contains one sorted `name@version` Cargo package spec per
   line. If the selection is empty, the file is still created and has zero
   bytes. `--changed-files PATH` can replace `--base-ref` when CI already has an
   authoritative UTF-8 JSON change manifest with `changed` and `deleted`
   arrays; the two options are mutually exclusive.

   By default this prints the full `Impact` JSON with all three tiers. Use the
   tier toggles (`--modified`, `--affected`, `--required`) to filter — when none
   are given, all three are included (back-compat). To plug the result straight
   into `cargo`, change the format:

   ```bash
   # One bare package name per line — good for xargs / shell loops.
   cargo delta impact --baseline main.json --current feature.json -f names --affected

   # One unambiguous Cargo package spec per line.
   cargo delta impact --baseline main.json --current feature.json -f packages --affected

   # `-p NAME` pairs — drop into any cargo invocation via $(...).
   cargo build $(cargo delta impact --baseline main.json --current feature.json -f cargo-args --affected)

   # `--exclude NAME` for the workspace complement of the selected tier — combine
   # with `cargo --workspace` to scope without `-p` ambiguity (since `--exclude`
   # only matches workspace members, it can't collide with same-named registry
   # deps). Empty when the selected tier covers the workspace.
   cargo build --workspace $(cargo delta impact --baseline main.json --current feature.json -f cargo-excludes --affected)

   # JSON, but only the keys you care about:
   cargo delta impact --baseline main.json --current feature.json --required
   ```

   Every non-JSON format emits the **union** of the selected tiers,
   deduplicated and sorted. The human-readable summary is written to stderr, so
   stdout and `--output` contain only the selected machine-readable format.

   > The legacy subcommand names `analyze` (= `snapshot`) and `run` (= `impact`)
   > continue to work as hidden aliases for back-compat.

### CI/CD Integration

`cargo-delta` is designed to speed up PR builds by building and testing only impacted packages.
Since detection is best-effort, a **backstop build** must run separately to catch anything delta missed or was misconfigured for.

**PR pipeline** — snapshot both branches, then capture each tier into its own
variable. Different cargo commands need different tiers:

| Command | Tier | Reasoning |
|---|---|---|
| `cargo fmt --check`, `cargo clippy` | `--modified` | Lints and formatting only matter for code the PR actually touched. Untouched code already passed on `main`. |
| `cargo build`, `cargo test`, `cargo bench` | `--affected` | A modified package can break a dependent's compile or behavior, so downstream needs to be built and tested too. |
| `cargo doc`, vendor verification | `--required` | Needs transitive dependencies in scope. |

```yaml
- name: Snapshot baseline (main)
  run: git checkout origin/main && cargo delta snapshot > baseline.json

- name: Snapshot current (PR)
  run: git checkout $PR_BRANCH && cargo delta snapshot > current.json

- name: Build, test, lint impacted packages
  run: |
    MODIFIED=$(cargo delta impact --baseline baseline.json --current current.json -f cargo-args --modified)
    AFFECTED=$(cargo delta impact --baseline baseline.json --current current.json -f cargo-args --affected)
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
- The two `cargo delta impact` calls are cheap — they re-read the same snapshots
  and do the same set computation; no need to deduplicate.

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

Configuration options can be set globally and overridden per crate. For example:

```toml
[parser]
foo = true
foo_patterns = ["*.foo", "*.bar"]

[parser.my-crate]
foo_patterns = ["*.baz"] # Override for a specific crate
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

`cargo delta snapshot` writes a JSON artifact describing the workspace at the
current checkout. It's the input to `cargo delta impact`.

Cargo's unit of workspace membership is a **package**: one `Cargo.toml` with a
name and version. A package may build multiple Rust crates or targets, but
cargo-delta computes impact between packages because that is the unit Cargo's
`-p`/`--package` interface can consume.

Schema `1` has three data fields:

- **`packages`** is the canonical identity table for workspace members. Each
  record contains Cargo's package ID, package name, version, and
  Git-root-relative manifest path.
- **`files`** is the recursive input tree. Its nodes represent manifests,
  Cargo targets, Rust modules, `include!` inputs, configured file references,
  and assumed inputs. Each package-root node carries the owning package ID.
- **`dependencies`** maps each package ID to its direct workspace dependency
  package IDs. cargo-delta traverses it in both directions to compute affected
  and required sets.

For example, a two-package workspace snapshot starts like this:

```json
{
  "schema": 1,
  "packages": [
    {
      "id": "path+file:///repo/crates/app#app@1.0.0",
      "name": "app",
      "version": "1.0.0",
      "manifest_path": "crates/app/Cargo.toml"
    },
    {
      "id": "path+file:///repo/crates/core#core@1.0.0",
      "name": "core",
      "version": "1.0.0",
      "manifest_path": "crates/core/Cargo.toml"
    }
  ],
  "files": {
    "path": "Cargo.toml",
    "kind": "Workspace",
    "children": [
      {
        "path": "crates/app/Cargo.toml",
        "kind": "Crate",
        "package_id": "path+file:///repo/crates/app#app@1.0.0",
        "children": []
      }
    ]
  },
  "dependencies": {
    "path+file:///repo/crates/app#app@1.0.0": [
      "path+file:///repo/crates/core#core@1.0.0"
    ],
    "path+file:///repo/crates/core#core@1.0.0": []
  }
}
```

Snapshots are derived artifacts rather than a long-lived interchange format.
Only schema `1` is accepted; regenerate older or unversioned snapshots with
`cargo delta snapshot`.

Use `--output PATH` to atomically replace a snapshot file. Without it, snapshot
JSON is written to stdout as before.

### Impact

`cargo delta impact` compares two snapshots plus the Git change set and reports
which workspace packages are impacted.

- **Modified**: Packages that directly own a changed input.
- **Affected**: Modified packages plus all their dependents, direct and indirect.
- **Required**: Affected packages plus all their dependencies, direct and indirect.

Use `--base-ref REF` for an explicit merge-base comparison, or
`--changed-files PATH` to supply `{"changed":[],"deleted":[]}` paths directly.
Use `--output PATH` to atomically write any format instead of stdout.

### Impact output formats

Select the format with `-f FORMAT` or `--format FORMAT`. The default is
`json`. With no tier flag, all three tiers are selected. For every non-JSON
format, selecting multiple tiers emits their sorted, deduplicated union.

| Format | Output | Typical use |
| --- | --- | --- |
| `json` | JSON object with one array for each selected tier (`Modified`, `Affected`, `Required`). Values are bare package names. Unlike other formats, tiers remain separate. | Durable reports and structured CI processing. |
| `names` | One bare package name per line. | `xargs`, display, or checking whether a tier is empty. |
| `packages` | One canonical `name@version` Cargo package spec per line. | Passing a selection to tools that read package files without risking same-name ambiguity. |
| `cargo-args` | One space-separated line of `-p NAME` pairs. | Shell expansion into Cargo commands, for example `cargo test $(cargo delta impact ... -f cargo-args)`. |
| `cargo-excludes` | One space-separated line containing `--exclude NAME` for every workspace package outside the selected union. | Combine with Cargo's `--workspace` flag when exclusion is safer than positive package selection. |

`names`, `packages`, and `cargo-args` produce zero bytes for an empty
selection. `cargo-excludes` instead lists the whole workspace when the
selection is empty, and produces zero bytes when the selection already covers
the whole workspace.

Alternatively, pair `--base-ref REF` with `--output-dir DIR` and omit
`--baseline`/`--current` to generate both exact-commit snapshots, all tier
package files, `impact.json`, and a hash-bearing `manifest.json` in one
invocation. Snapshot cache hits still revalidate the ref, merge base, changes,
and dirty workspace state.


## Limitations

This tool is **best-effort** and may not detect all dependencies:

- Dynamic file paths computed at runtime
- Conditional compilation dependencies
- Other dependencies not captured by the heuristics


## Example

```bash
$ cargo delta impact --baseline main.json --current feature.json
Computing impact..
Looking up git changes..

Changed file: "src/api/mod.rs"
Changed file: "src/utils.rs"

Using baseline analysis : main.json
Using current analysis  : feature.json

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
