# cargo-delta design

## Status

This document describes the currently implemented portable snapshot and
low-level impact artifact surface. The final section lists follow-up work that
is intentionally not implemented yet.

## Purpose

`cargo-delta` compares Cargo workspace snapshots and changed Git paths to
identify three package sets:

- **modified** packages directly own a changed input;
- **affected** packages are modified packages plus transitive workspace
  dependents;
- **required** packages are affected packages plus transitive workspace
  dependencies.

The tool computes selections and writes artifacts. It does not run build, test,
lint, or other user commands.

## Snapshot command

```text
cargo delta snapshot [--output PATH] [-c PATH]
```

Without `--output`, the snapshot JSON is written to stdout. With `--output`,
the same bytes are written to a temporary sibling, flushed, and atomically
replace `PATH`. An error before replacement leaves an existing artifact
unchanged.

Snapshot schema 1 records every workspace member using:

```json
{
  "id": "path+file:///repo/crates/foo#foo@1.2.3",
  "name": "foo",
  "version": "1.2.3",
  "manifest_path": "crates/foo/Cargo.toml"
}
```

The ID is Cargo's package ID. Manifest and file paths are Git-root-relative and
serialize with `/` separators. File ownership and workspace dependency edges
reference package IDs; package names and target names are not used as graph
identity.

The reader accepts the unversioned cargo-delta 0.3 snapshot shape. Legacy
snapshots continue to support the existing name-based formats. A legacy
current snapshot cannot produce `packages` output because it has no package
versions. When a legacy package name maps to multiple current package
identities, impact computation fails rather than guessing.

## Low-level impact command

```text
cargo delta impact \
  --baseline PATH \
  --current PATH \
  [--base-ref REF | --changed-files PATH] \
  [--output PATH] \
  [existing tier and format options]
```

The two explicit change sources are mutually exclusive:

- `--base-ref REF` runs `git merge-base HEAD REF`, then obtains name-only
  changes from `MERGE_BASE..HEAD`.
- `--changed-files PATH` reads a UTF-8 JSON manifest and does not invoke Git for
  comparison:

  ```json
  {
    "changed": ["crates/a/src/lib.rs"],
    "deleted": ["crates/b/src/old.rs"]
  }
  ```

  Entries must be non-empty, `/`-separated paths relative to the Git root.
  Absolute paths, `.` or `..` components, empty components, backslashes, and a
  path listed as both changed and deleted are rejected. Repeated entries with
  the same disposition are deduplicated.

When neither option is present, the existing configuration-driven Git branch
selection remains in effect. Explicit command-line change sources take
precedence over `[git].remote_branch`.

Without `--output`, the selected format is written to stdout as before. With
`--output`, those bytes atomically replace the requested file. Diagnostics
remain on stderr. A computed empty byte stream creates or replaces the output
with a present zero-byte file.

## Output formats and identity

The existing `json`, `names`, `cargo-args`, and `cargo-excludes` formats remain
available. The additive `packages` format emits the union of the selected tiers
as one canonical `name@version` Cargo package spec per line, sorted by package
name and version.

Every selected internal package ID must map to exactly one package in the
current snapshot. A current workspace containing duplicate `name@version`
specs is rejected because a package file could not identify those members
unambiguously.

Existing JSON and human-oriented formats continue to emit package names where
possible. The current snapshot remains authoritative for emitted selections;
packages deleted from the baseline are not emitted as current packages.

## Compatibility and errors

- The `analyze` and `run` aliases remain available.
- Omitting `--output` preserves stdout behavior.
- Operational, snapshot compatibility, identity, path-validation, and atomic
  output failures exit with status 1 and write diagnostics to stderr.
- Clap usage errors, including conflicting change-source options, use Clap's
  usage-error status.
- Snapshot schema 1 and the unversioned 0.3 schema are accepted. Other schema
  versions fail with a message listing the supported versions.

## Follow-up work (not implemented)

The higher-level ref-to-artifacts lifecycle is intentionally deferred. This
change does **not** add `--output-dir`, temporary baseline worktrees, snapshot
caches, dirty-tree policy, generation manifests, or multi-tier artifact
directories. Those features can build on the atomic output, explicit change
sources, canonical package identity, and package-file format defined here.
