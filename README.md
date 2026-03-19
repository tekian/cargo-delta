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
    - [Tripwire](#tripwire)
- [Output](#output)
    - [Analyze](#analyze)
    - [Run](#run)
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

1. **Analyze the baseline branch:**
   ```bash
   git checkout main
   cargo delta analyze > main.json
   ```

2. **Analyze the feature branch:**
   ```bash
   git checkout feature-branch
   cargo delta analyze > feature.json
   ```

3. **Compare to find impacted crates:**
   ```bash
   cargo delta run --baseline main.json --current feature.json
   ```

### CI/CD Integration

`cargo-delta` is designed to speed up PR builds by building and testing only impacted crates.
Since detection is best-effort, a **backstop build** must run separately to catch anything delta missed or was misconfigured for.

**PR pipeline** — runs delta, builds and tests only impacted crates:

```yaml
# 1. Analyze baseline (main branch)
- run: git checkout origin/main && cargo delta analyze > baseline.json

# 2. Analyze current (PR branch)
- run: git checkout $PR_BRANCH && cargo delta analyze > current.json

# 3. Determine impacted crates
- run: cargo delta run --baseline baseline.json --current current.json > delta.json

# 4. Build/test only impacted crates (use "Required" output)
- run: cargo test -p impacted-crate-a -p impacted-crate-b
```

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
cargo delta analyze -c config.toml # ...
cargo delta run -c config.toml # ...
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

### Tripwire

If any changed or deleted file matches a tripwire pattern, all crates are considered impacted.

Config default:

```toml
tripwire_patterns = []
```


Config example:

```toml
tripwire_patterns = [
    "Cargo.toml",       # top-level Cargo.toml
    "delta.toml"   # Delta config file
]
```

## Output

### Analyze

Analyze phase produces JSON file that's intended to be consumed by `run` phase.

- **files**: Nested tree of file dependencies as detected by all the heuristics.
- **crates**: Dependency relationships between crates within the workspace.

### Run

Run phase produces JSON file that's intended to be consumed by _your_ CI/CD.

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
$ cargo delta run --baseline main.json --current feature.json
Running delta..

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
