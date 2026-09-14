# cargo-delta design

## Status

This document describes the implemented portable snapshot, low-level impact,
and high-level ref-to-artifacts surfaces.

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

Snapshot schema 1 contains:

```json
{
  "schema": 1,
  "packages": [{
    "id": "path+file:///repo/crates/foo#foo@1.2.3",
    "name": "foo",
    "version": "1.2.3",
    "manifest_path": "crates/foo/Cargo.toml"
  }],
  "files": {
    "path": "Cargo.toml",
    "kind": "Workspace",
    "children": []
  },
  "dependencies": {
    "path+file:///repo/crates/foo#foo@1.2.3": []
  }
}
```

The ID is Cargo's package ID. Manifest and file paths are Git-root-relative and
serialize with `/` separators. File ownership and workspace dependency edges
reference package IDs; package names and target names are not used as graph
identity. A package-root manifest has file kind `Package`; the Rust crate
targets built by that package have file kind `Target`.

Snapshots are derived cache artifacts. The reader accepts only schema 1;
unversioned cargo-delta 0.3 snapshots and unknown future schemas fail with
guidance to regenerate both inputs.

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

## High-level ref-to-artifacts command

```text
cargo delta impact \
  --base-ref REF \
  --output-dir DIR \
  [--dirty error|workspace] \
  [-c PATH]
```

This mode owns the complete comparison lifecycle. `--output-dir` requires
`--base-ref` and is mutually exclusive with the low-level snapshot paths,
change manifest, single output path, tier switches, and format selection. The
default dirty policy is `error`.

The command performs only direct `git` and `cargo` process invocations. It does
not invoke a shell, fetch, mutate remotes, change the caller's branch or index,
or provide a user-command execution facility.

### Git resolution and dirty state

The command resolves `REF^{commit}` and `HEAD^{commit}` from the local object
database, computes their merge base, and compares `MERGE_BASE..HEAD`. A missing
ref fails without fetching. Missing merge-base history reports that the
histories may be unrelated or the clone may be shallow.

Before consulting snapshot caches, the command checks `git status` for tracked
changes and non-ignored untracked paths. Git-ignored paths and paths beneath
Cargo's effective target directory are excluded. If the output directory is
inside the repository, that exact normalized subtree is also excluded so a
completed generation does not make the next invocation dirty. Its parents and
similarly named siblings remain subject to dirty detection.

- `--dirty error` rejects a dirty workspace before creating or replacing
  artifacts.
- `--dirty workspace` continues, selects every current workspace package in
  all three tiers, and records `widened: true` in the manifest.

Dirty and Git-change validation always run, including on a snapshot cache hit.

### Output directory placement

A relative `--output-dir` is resolved against the invocation working directory
and normalized before validation, dirty filtering, cache lookup, or writes. An
output directory outside the repository is allowed unless it is the repository
root's ancestor.

Inside the repository, the output location must be a dedicated subtree with no
tracked content. The command rejects a location that:

- is the Git root or one of its ancestors;
- is inside `.git`;
- traverses a symlink within the repository; or
- contains a tracked path or is nested beneath a tracked file or Gitlink.

Therefore an untracked artifact directory may be placed beneath a source
parent, but it cannot overlap tracked source. Only the exact output subtree is
reserved and excluded from dirty detection; an unrelated untracked path still
causes `--dirty error` to fail or `--dirty workspace` to widen.

### Commit snapshots and widening

Each uncached snapshot is generated from a unique detached temporary Git
worktree for its exact commit. The current snapshot therefore always
corresponds to `HEAD`, even under `--dirty workspace`; caller working-tree
content never enters it. Temporary worktrees are removed on both success and
failure. A cleanup error is surfaced, and when generation already failed the
primary error is retained alongside the cleanup error.

The corresponding Cargo workspace is found by its Git-root-relative path. If
the merge base predates that workspace, the baseline artifact is an empty
schema-1 snapshot and every current package is selected in all tiers with
`widened: true`. Other baseline Cargo metadata failures remain errors.

### Snapshot cache

`DIR/snapshots/baseline.json` and `DIR/snapshots/current.json` are reused only
when the previous manifest matches the snapshot's:

- exact commit;
- canonical effective configuration content;
- Cargo workspace path;
- snapshot schema; and
- cargo-delta version.

The cached bytes must also match the size and SHA-256 recorded by the previous
manifest and deserialize as a valid supported snapshot. Otherwise the snapshot
is regenerated.

### Artifact directory

The command writes:

```text
DIR/
  impact.json
  modified.packages
  affected.packages
  required.packages
  snapshots/
    baseline.json
    current.json
  manifest.json
```

`impact.json` contains all three sorted name-based JSON tiers.
`*.packages` contains sorted canonical `name@version` specs with one LF
newline per entry; an empty tier is a present zero-byte file. JSON artifacts
are pretty-printed and LF-terminated. Repository-relative paths and manifest
file keys always use `/`.

Every artifact is atomically replaced. `manifest.json` is replaced last and
records the resolved commits, merge base, dirty policy, widening state,
snapshot cache identities, deterministic generation identity, and SHA-256 plus
byte length for every other artifact. A reader must read the manifest and
verify those file records. During concurrent replacement it may observe a hash
mismatch and retry, but it cannot mistake a partially generated directory for
the published generation. A failure before the final replacement never
publishes a new manifest.

## Compatibility and errors

- The `analyze` and `run` aliases remain available.
- Omitting `--output` preserves stdout behavior.
- Operational, snapshot compatibility, identity, path-validation, and atomic
  output failures exit with status 1 and write diagnostics to stderr.
- Clap usage errors, including conflicting change-source options, use Clap's
  usage-error status.
- Only snapshot schema 1 is accepted. Missing, older, or newer schemas fail
  with a command that regenerates the input.
