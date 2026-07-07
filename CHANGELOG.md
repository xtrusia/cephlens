# Changelog

Notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.4] - 2026-07-07

### Added

- Added `cephlens report <session>` to export recorded sessions as Markdown.
- Live TUI sessions now write `report.md` on exit when snapshots were recorded.
- Added `cephlens doctor` for SSH, sudo, Ceph CLI, and tracer preflight checks.
- Added `cephlens lab` to run a short benchmark with optional trace capture and
  write a session report.

### Changed

- Moved diagnostic insight rules out of the TUI so CLI reports reuse the same
  checks.
- Replaced MicroCeph-specific node readiness wording with Ceph
  version/deployment data.

## [0.1.3] - 2026-07-06

### Added

- Recorded raw trace logs under live TUI session directories.
- Documented sudo whitelist setup for trace and install commands.

## [0.1.2] - 2026-07-05

### Added

- Added `cephlens --version` so installed binaries report the Cargo package
  version.

## [0.1.1] - 2026-07-05

### Security

- Hardened SSH destination validation to reject option-like, empty, or
  whitespace-containing host values before invoking `ssh`.
- Pinned bundled cephtrace artifacts to `v1.6` and verified their SHA256
  digests during release builds.

### Changed

- Improved bundled cephtrace GPL notice and release artifact attribution.

## [0.1.0] - 2026-07-05

Initial release.

- SSH-driven Ceph investigation TUI: live cluster, OSD, and host status.
- Three eBPF trace sources driven through cephtrace: osdtrace (OSD server side),
  kfstrace (CephFS MDS client), and radostrace (RADOS client).
- Trace controls with view switching and start/stop confirmation, per-source and
  cross-source operator insights, in-TUI config editing, and session replay.
- Cross-platform controller (Linux, macOS, Windows) with cargo-dist release
  archives that bundle the cephtrace tracers.

[Unreleased]: https://github.com/xtrusia/cephlens/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/xtrusia/cephlens/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/xtrusia/cephlens/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/xtrusia/cephlens/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/xtrusia/cephlens/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/xtrusia/cephlens/releases/tag/v0.1.0
