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
is written directly to the requested file.

## Impact model

```text
cargo delta impact \
  --baseline PATH \
  --current PATH \
  [--base-ref REF] \
  [--output PATH] \
  [-f FORMAT]
```

Snapshots describe ownership and dependencies, not file contents. The Git diff
provides the changed and deleted paths. `--base-ref REF` explicitly selects the
ref used to compute `merge-base(HEAD, REF)..HEAD`; it overrides
`[git].remote_branch` and automatic primary-branch discovery.

The baseline snapshot owns deleted-file lookups. The current snapshot owns
changed and newly discovered files and supplies the dependency graph used for
affected and required traversal.

`-f packages` emits the selected union as sorted `name@version` package IDs,
one per line. `cargo-args-versioned` and `cargo-excludes-versioned` place those
IDs after `-p` and `--exclude`; the existing variants continue to emit package
names. `--output` writes whichever format was selected to a file instead of
stdout. An empty selection produces an empty output file.

## Compatibility

The `analyze` and `run` aliases remain available. Omitting the new options
preserves existing branch discovery, stdout output, and format behavior.
