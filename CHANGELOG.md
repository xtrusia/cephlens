# Changelog

Notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-07-05

Initial release.

- Agentless Ceph investigation TUI over SSH: live cluster, OSD, and host status.
- Three eBPF trace sources driven through cephtrace: osdtrace (OSD server side),
  kfstrace (CephFS MDS client), and radostrace (RADOS client).
- Trace controls with view switching and start/stop confirmation, per-source and
  cross-source operator insights, in-TUI config editing, and session replay.
- Cross-platform controller (Linux, macOS, Windows) with cargo-dist release
  archives that bundle the cephtrace tracers.

[Unreleased]: https://github.com/xtrusia/cephlens/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/xtrusia/cephlens/releases/tag/v0.1.0
