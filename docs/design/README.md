# cargo-delta design

## Purpose

`cargo-delta` identifies modified Cargo workspace packages, their transitive
dependents, and the dependencies required to build those packages. It snapshots
file ownership and workspace dependency relationships, then combines two
snapshots with a Git change set.

## Snapshot model

```text
cargo delta snapshot [--output PATH]
```

The snapshot keeps the original two-part structure:

- `files` is the detected input tree;
- `packages` maps each workspace package to a record containing:
  - direct workspace dependencies;
  - transitive external dependencies, stopping at another workspace package;
  - canonical effective direct dependency declarations.

Snapshot generation requires a Git worktree. Git supplies the root for portable
relative paths and the checkout identity embedded in the snapshot key; without
Git, the command fails without producing an artifact.

Package IDs are stable `name@version` strings. Each package node in `files`
records its owning package ID because a package name does not have to match its
directory name.

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

Without `--output`, JSON is written to stdout. With `--output`, the same text
is written directly to the requested file, creating missing parent directories.

## Impact model

```text
cargo delta impact \
  (--baseline PATH | --base-ref REF) \
  [--current PATH] \
  [--output PATH] \
  [-f FORMAT]
```

Snapshots describe ownership and dependencies, not file contents. The Git diff
provides the changed and deleted paths.

An explicit `--baseline` is mutually exclusive with `--base-ref`. An explicit
`--current` independently overrides current-snapshot generation. When either
side is omitted, cargo-delta uses the cached snapshot when its embedded key is
current and regenerates it otherwise.

Cached comparison resolves `merge-base(HEAD, REF)` and compares that commit
with the current working tree. It includes committed, staged, unstaged,
deleted, and non-ignored untracked paths.

An explicit baseline defines the exact base commit from its embedded `HEAD`.
Explicit snapshots without a cache key are invalid, and an explicit baseline
with working-tree changes is rejected because its digest can validate state but
cannot reconstruct those changes for a Git comparison.

The baseline snapshot owns deleted-file lookups. The current snapshot owns
changed and newly discovered files and supplies the dependency graph used for
affected and required traversal.

### Cargo input changes

Git remains the only change detector. Cargo-specific processing runs only when
the Git change set contains the workspace `Cargo.lock` or root `Cargo.toml`.

For a changed lockfile, cargo-delta reads the baseline content with `git show`
and the current working-tree content from disk. It parses both files into maps
keyed by package name, version, and source; checksum and normalized dependency
lists are compared values. Removed or changed baseline identities are looked up
in the baseline package records, and added or changed current identities in the
current records. The union of nearest workspace consumers becomes modified.

External traversal stops when it reaches another workspace package. If
`app -> core -> external`, `core` records `external` while `app` records only
its workspace edge to `core`; an external change therefore makes `core`
modified and `app` affected.

For a changed root manifest, cargo-delta parses both TOML documents and removes
only `workspace.dependencies`, `workspace.members`, and `workspace.exclude`
before comparing the remaining values. If anything remains changed, the root
manifest keeps its full-workspace trip-wire behavior. Otherwise effective
dependency declarations and package membership are compared to seed modified
packages and surviving dependents of removed packages.

Snapshot construction first uses no-deps metadata. When `Cargo.lock` already
exists it additionally runs `cargo metadata --all-features --locked` to build
the resolved external graph without modifying the lockfile. Without a lockfile,
the snapshot remains usable but records incomplete resolution, so a later
lockfile change cannot bypass its conservative trip wire.

Trip-wire patterns use path-component matching. A pattern such as `*.just`
matches only the repository root; matching nested paths requires `**`.

## Snapshot cache

Generated snapshots are cached as `target/cargo-delta/baseline.json` and
`target/cargo-delta/current.json`. The ordinary snapshot JSON is extended with
a required `cache_key`, so there is no separate cache-entry artifact or
generation path. `cargo delta snapshot` and cached impact generation use the
same snapshot builder and produce directly interchangeable files.

Every key includes the cargo-delta version, workspace path, and configuration
digest. Every snapshot source is represented uniformly as `HEAD`
plus a digest of tracked worktree changes and non-ignored untracked paths and
contents. The cached baseline is the merge-base commit with the empty
working-tree digest. Explicit snapshots are used as requested, but cargo-delta
warns when their embedded key does not match the state being compared. A
missing key makes the snapshot invalid.

The internal cache directory is always excluded from snapshot state, including
when shell redirection hides the output path from cargo-delta. Exact explicit
snapshot and output paths are also excluded from cached current-state
calculation, so generated artifacts do not invalidate their own cache.

A baseline cache miss creates a detached temporary worktree at the merge base
and runs the existing snapshot builder at the corresponding workspace path.
The Cargo executable inherited through Cargo's `CARGO` environment variable is
used, so a different or unavailable `rust-toolchain.toml` in the baseline does
not change the selected toolchain. The temporary worktree is removed on both
success and failure.

If the workspace does not exist at the merge base, cargo-delta writes an empty
baseline snapshot and widens every impact tier to all current packages.

## Implementation boundaries

- `git` resolves the base commit and returns one `GitComparison` containing the
  base checkout identity, current checkout identity, and changed paths. The
  untracked-file query contributes to both current identity and changed paths.
- `snapshot` defines, constructs, loads, and validates one snapshot and its
  checkout key.
- `snapshot_cache` persists snapshots and exposes only `current(state)` and
  `baseline(state)`. Its baseline method privately owns temporary-worktree
  creation and cleanup.
- The impact command selects explicit snapshots or cache methods, then combines
  the resulting pair with `GitComparison.diff`. Cache details and temporary
  worktrees do not enter the impact calculation.
- `Host::write_output` owns destination resolution and output I/O. Command code
  owns user-facing error reporting and exit behavior.

`-f packages` emits the selected union as sorted `name@version` package IDs,
one per line. `cargo-args-versioned` and `cargo-excludes-versioned` place those
IDs after `-p` and `--exclude`; the existing variants continue to emit package
names. `gamma-test-packages` emits the repeated `--test-package NAME` arguments
accepted by `cargo gamma run`; cargo-gamma resolves these as workspace package
names rather than Cargo package specs. `--output` writes whichever format was
selected to a file instead of stdout. An empty selection produces an empty
output file.

## Compatibility

The `analyze` and `run` aliases remain available. Omitting the new options
preserves existing branch discovery, stdout output, and format behavior.
