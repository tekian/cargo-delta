# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-09-22

### Added

- Add `--output PATH` to `snapshot` and `impact`.
- Add `packages`, `cargo-args-versioned`, `cargo-excludes-versioned`, and
  `gamma-test-packages` output formats.
- Add cached, single-invocation impact analysis with `impact --base-ref REF`.

### Changed

- Identify packages as `name@version` in snapshots and package-oriented output.
- Scope Git-reported `Cargo.lock` changes to packages whose resolved external
  dependency graphs changed.
- Scope root `Cargo.toml` changes limited to workspace dependencies, members, or
  exclusions; other workspace-wide settings remain full-workspace trip wires.
- Match trip-wire globs by path component so root patterns such as `*.just` do
  not match nested files.

### Fixed

- Normalize Windows ownership paths so short and long path spellings map to the
  same package.

## [0.3.1] - 2026-04-24

### Added

- Add `-f cargo-excludes` output format that emits `--exclude NAME` arguments for the workspace complement of the selected tier(s). Combined with `cargo --workspace`, this scopes invocations without `-p` ambiguity (since `--exclude` only matches workspace members, it cannot collide with same-named transitive registry deps).

## [0.3.0] - 2026-04-23

### Added

- Add `--format / -f {json|names|cargo-args}` flag to the impact subcommand (#24)
- Add tier-toggle flags `--modified`, `--affected`, `--required` (#24)

### Changed

- Rename subcommands `analyze` → `snapshot` and `run` → `impact`; legacy names kept as hidden aliases (#24)
- Sort impact JSON arrays for deterministic output (#24)
- `-f cargo-args` on an empty tier emits no output (no trailing newline) (#24)

### Fixed

- Minor banner wording and spacing polish (#24)

## [0.2.1] - 2026-02-25

### Added

- Add filters to standard error output (#10)
- Add a baseline set of unit tests (#5)
- Add rich CI functionality (build, clippy, spell check)
- Integrate cargo-make for build orchestration (#11)

### Changed

- Renamed project to cargo-delta (#9)
- Move crates to `crates/` workspace layout (#11)
- Make mutation analysis optional (#11)
- Improve command-line processing with clap (#7)
- Split into bin+lib to enable integration testing (#8)

### Fixed

- Attempt to determine git origin base branch automatically (#3)
- Fix build, clippy, and spell check warnings

### Dependencies

- Bump taiki-e/upload-rust-binary-action from 1.27.0 to 1.28.0 (#6)

[0.4.0]: https://github.com/tekian/cargo-delta/compare/v0.3.1...v0.4.0
[0.3.0]: https://github.com/tekian/cargo-delta/compare/0.2.1...0.3.0
[0.2.1]: https://github.com/tekian/cargo-delta/compare/0.1...0.2.1
