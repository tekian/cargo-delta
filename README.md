# cargo-delta

[![crate.io](https://img.shields.io/crates/v/cargo-delta.svg)](https://crates.io/crates/cargo-delta)
[![CI](https://github.com/tekian/cargo-delta/workflows/main/badge.svg)](https://github.com/tekian/cargo-delta/actions)
[![Coverage](https://codecov.io/gh/tekian/cargo-delta/graph/badge.svg)](https://codecov.io/gh/tekian/cargo-delta)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

`cargo-delta` detects which crates in a Cargo workspace are impacted by changes in a Git feature branch. Build, test, and benchmark only the crates you need.

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

   By default this prints the full `Impact` JSON with all three tiers. Use the
   tier toggles (`--modified`, `--affected`, `--required`) to filter — when none
   are given, all three are included (back-compat). To plug the result straight
   into `cargo`, change the format:

   ```bash
   # One crate per line — good for xargs / shell loops.
   cargo delta impact --baseline main.json --current feature.json -f names --affected

   # `-p NAME` pairs — drop into any cargo invocation via $(...).
   cargo build $(cargo delta impact --baseline main.json --current feature.json -f cargo-args --affected)

   # JSON, but only the keys you care about:
   cargo delta impact --baseline main.json --current feature.json --required
   ```

   Combining tier toggles for `names` / `cargo-args` emits the **union** of the
   selected tiers (deduplicated, sorted). The human-readable summary is written
   to stderr, so `$(...)` capture stays clean.

   > The legacy subcommand names `analyze` (= `snapshot`) and `run` (= `impact`)
   > continue to work as hidden aliases for back-compat.

### CI/CD Integration

`cargo-delta` is designed to speed up PR builds by building and testing only impacted crates.
Since detection is best-effort, a **backstop build** must run separately to catch anything delta missed or was misconfigured for.

**PR pipeline** — snapshot both branches, then capture each tier into its own
variable. Different cargo commands need different tiers:

| Command | Tier | Reasoning |
|---|---|---|
| `cargo fmt --check`, `cargo clippy` | `--modified` | Lints and formatting only matter for code the PR actually touched. Untouched code already passed on `main`. |
| `cargo build`, `cargo test`, `cargo bench` | `--affected` | A modified crate can break a dependent's compile or behavior, so downstream needs to be built and tested too. |
| `cargo doc`, vendor verification | `--required` | Needs transitive dependencies in scope. |

```yaml
- name: Snapshot baseline (main)
  run: git checkout origin/main && cargo delta snapshot > baseline.json

- name: Snapshot current (PR)
  run: git checkout $PR_BRANCH && cargo delta snapshot > current.json

- name: Build, test, lint impacted crates
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

If any changed or deleted file matches a trip wire pattern, all crates are considered impacted.

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

- **files**: Nested tree of file dependencies as detected by all the heuristics.
- **crates**: Dependency relationships between crates within the workspace.

### Impact

`cargo delta impact` compares two snapshots plus the git diff and prints which
crates are impacted, in a JSON shape your CI/CD can consume.

- **Modified**: Crates directly modified by Git changes.
- **Affected**: Modified crates plus all their dependents, direct and indirect.
- **Required**: Affected crates plus all their dependencies, direct and indirect.


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

Modified      2 (Crates directly modified by Git changes.)
Affected      3 (Modified crates plus all their dependents, direct and indirect.)
Required      4 (Affected crates plus all their dependencies, direct and indirect.)
Total        15 (Total crates in this workspace.)
```

## Contributing

Fork the repository and submit a pull request.

## License

[MIT](LICENSE)
