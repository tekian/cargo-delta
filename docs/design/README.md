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
- `packages` maps each workspace package to its direct workspace dependencies.

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
      "app@1.0.0": ["core@1.0.0"],
      "core@1.0.0": []
    }
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

Managed comparison resolves `merge-base(HEAD, REF)` and compares that commit
with the current working tree. It includes committed, staged, unstaged,
deleted, and non-ignored untracked paths.

An explicit baseline defines the exact base commit from its embedded `HEAD`.
Explicit snapshots without a cache key are invalid, and an explicit baseline
with working-tree changes is rejected because its digest can validate state but
cannot reconstruct those changes for a Git comparison.

The baseline snapshot owns deleted-file lookups. The current snapshot owns
changed and newly discovered files and supplies the dependency graph used for
affected and required traversal.

## Snapshot cache

Generated snapshots are cached as `target/cargo-delta/baseline.json` and
`target/cargo-delta/current.json`. The ordinary snapshot JSON is extended with
a required `cache_key`, so there is no separate cache-entry artifact or
generation path. `cargo delta snapshot` and managed impact generation use the
same snapshot builder and produce directly interchangeable files.

Every key includes the cargo-delta/cache version, workspace path, and
configuration digest. Every snapshot source is represented uniformly as `HEAD`
plus a digest of tracked worktree changes and non-ignored untracked paths and
contents. The managed baseline is the merge-base commit with the empty
working-tree digest. Explicit snapshots are used as requested, but cargo-delta
warns when their embedded key does not match the state being compared or is
absent.

The internal cache directory is always excluded from snapshot state, including
when shell redirection hides the output path from cargo-delta. Exact explicit
snapshot and output paths are also excluded from managed current-state
calculation, so generated artifacts do not invalidate their own cache.

A baseline cache miss creates a detached temporary worktree at the merge base
and runs the existing snapshot builder at the corresponding workspace path.
The Cargo executable inherited through Cargo's `CARGO` environment variable is
used, so a different or unavailable `rust-toolchain.toml` in the baseline does
not change the selected toolchain. The temporary worktree is removed on both
success and failure.

If the workspace does not exist at the merge base, cargo-delta writes an empty
baseline snapshot and widens every impact tier to all current packages.

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
