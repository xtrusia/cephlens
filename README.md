# cephlens

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)
[![CI](https://github.com/xtrusia/cephlens/actions/workflows/ci.yml/badge.svg)](https://github.com/xtrusia/cephlens/actions/workflows/ci.yml)

An SSH-driven Ceph investigation TUI with live cluster status, per-node
readiness, and osdtrace/kfstrace/radostrace eBPF latency views.

cephlens runs on Windows, Linux, or macOS and talks to Ceph nodes over
persistent SSH streams. It is currently a lab-first prototype, not a packaged
production monitoring agent.

Trace collection runs cephtrace tracers over SSH. `osdtrace` uses a temporary
remote runner script that removes itself when tracing stops or cephlens exits;
`kfstrace` and `radostrace` run directly on configured client hosts.

## Features

- Live cluster health, quorum, OSD counts, and IO throughput over a single SSH stream.
- Per-node readiness: connection state, OSD ids, CPU and memory percent, and microceph version.
- osdtrace eBPF latency tracing with per-OSD and per-PG breakdown of queue, BlueStore, and KV-commit latency.
- No standing agent: no permanent daemon on the nodes; the osdtrace runner script removes itself on stop, quit, or TTL expiry. (The cephtrace tracer binaries you deploy do persist under `~/.cephlens/bin/`.)
- Edit hosts and trace settings live in the TUI; changes apply to open SSH streams immediately.

## Requirements

Controller (where the TUI runs):

- Rust 1.85+ (edition 2024) to build.
- An OpenSSH client on `PATH`, with every host reachable over non-interactive SSH (key-based, no password prompt). Windows 10/11 ship this as the optional OpenSSH Client feature; macOS and Linux include it by default.

Ceph nodes:

- `ceph` and `rados` CLIs, plus passwordless `sudo -n` for the observed commands (see Access and sudo).
- For tracing, the `osdtrace`, `kfstrace`, and `radostrace` binaries from [cephtrace](https://github.com/taodd/cephtrace). They are eBPF-based and need a Linux 5.8+ kernel on the Ceph hosts. Release archives bundle them (see Install); a source build does not, so place them under `~/.cephlens/bin/` or `PATH` yourself. cephlens can also install osdtrace from a pinned URL (see Configuration).

## Install

Prebuilt binaries for Linux, macOS, and Windows are attached to each
[release](https://github.com/xtrusia/cephlens/releases). The archives bundle the
cephtrace tracers, so a downloaded build is self-contained — useful for
air-gapped clusters.

Install script (picks the right binary for your platform):

```sh
# Linux / macOS
curl --proto '=https' --tlsv1.2 -LsSf https://cephlens.seyeong.kim/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://cephlens.seyeong.kim/install.ps1 | iex
```

Or download a `cephlens-<target>.tar.xz` / `.zip` archive and extract it. Each
archive holds the `cephlens` binary plus a `cephtrace/` directory with the
`osdtrace` / `kfstrace` / `radostrace` binaries; deploy those to your Ceph hosts
under `~/.cephlens/bin/` or `PATH`.

### From source

```sh
cargo install --git https://github.com/xtrusia/cephlens
# or, in a clone:
cargo build --release
```

A source build does not bundle cephtrace — supply the tracers on the hosts
yourself (see Requirements). The `cargo run --` examples below become `cephlens`
with an installed binary.

## Status

This project is suitable for lab clusters and experiments. Before using it on a
production cluster, review the sudo policy, osdtrace artifact source, and
cleanup behavior described below.

## Configuration

Copy the example config and edit it for your cluster.

Linux and macOS:

```sh
cp cephlens.example.toml cephlens.toml
```

Windows (PowerShell):

```powershell
Copy-Item cephlens.example.toml cephlens.toml
```

`cephlens.toml` defines the Ceph hosts to observe and is intentionally ignored
by git because it may contain site-specific hostnames:

```toml
default_profile = "example"

[profiles.example]
admin_host = "ceph-admin"
hosts = ["ceph-admin", "ceph-node-1", "ceph-node-2", "ceph-node-3"]
client_hosts = ["ceph-client-1"]
refresh_secs = 1
trace_auto_start = false
trace_window_secs = 10
trace_latency_ms = 1
trace_ttl_secs = 1800

# Optional automatic osdtrace install. Keep this disabled unless you pin an
# artifact and verify it with SHA256.
# osdtrace_url = "https://example.invalid/artifacts/osdtrace-linux-amd64"
# osdtrace_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
# osdtrace_allow_unverified = false
```

`admin_host` is the host where cephlens runs Ceph admin commands such as
`ceph -s`, `ceph osd tree`, and `ceph osd df`. The `hosts` list is the set of
machines that get persistent node-readiness SSH streams and osdtrace runners.
`client_hosts` is optional; when set, it is where kfstrace and radostrace run.
Leave it out if you only want the OSD-side osdtrace view.

## Access and sudo

cephlens does not install a permanent agent on Ceph nodes. It opens SSH
connections from the machine running the TUI, so each configured host must be
reachable with non-interactive SSH:

```sh
ssh ceph-admin hostname
ssh ceph-node-1 hostname
```

Host aliases and usernames are resolved by OpenSSH. For example, put this in
`~/.ssh/config` if you want `ssh ceph-admin` to mean a specific user and address:

```sshconfig
Host ceph-admin
  HostName 203.0.113.10
  User cephlens
```

Remote commands use `sudo -n` where root privileges are required. The `-n`
flag means "non-interactive": fail immediately instead of waiting for a password
prompt. This prevents the TUI from hanging behind an invisible sudo prompt.

For a lab, a broad passwordless sudo rule is convenient:

```sudoers
cephlens ALL=(root) NOPASSWD: ALL
```

For production, prefer a dedicated user and a command whitelist. This sudoers
example assumes the bundled tracers are copied under
`/home/cephlens/.cephlens/bin/`. Adjust the other paths with
`command -v true ceph rados kill` on your hosts. If you install tracers on
`PATH` instead, replace the tracer paths with `command -v osdtrace kfstrace
radostrace` results:

```sudoers
cephlens ALL=(root) NOPASSWD: /usr/bin/true, /usr/bin/ceph, /usr/bin/rados, /usr/bin/kill, /home/cephlens/.cephlens/bin/osdtrace, /home/cephlens/.cephlens/bin/kfstrace, /home/cephlens/.cephlens/bin/radostrace
```

The current prototype runs these privileged operations:

```text
availability checks:
  sudo -n true

admin host:
  sudo -n ceph -s --format json
  sudo -n ceph osd tree --format json
  sudo -n ceph osd df --format json

bench command:
  sudo -n ceph osd pool create ...
  sudo -n ceph osd pool application enable ...
  sudo -n rados -p cephlens-test bench ...
  sudo -n rados -p cephlens-test cleanup

trace install / probe:
  sudo -n <osdtrace_path> --list

osdtrace runner on hosts:
  sudo -n <osdtrace_path> -a -l <latency_ms>

kfstrace runner on client_hosts:
  sudo -n <kfstrace_path> -m mds -l <latency_us> -t <ttl_secs>

radostrace runner on client_hosts:
  sudo -n <radostrace_path> -t <ttl_secs>

trace cleanup:
  sudo -n kill <osdtrace_pid> when osdtrace runner cleanup cannot kill it as the SSH user

trace path placeholders:
  <osdtrace_path> is osdtrace from PATH or ~/.cephlens/bin/osdtrace
  <kfstrace_path> is kfstrace from PATH or ~/.cephlens/bin/kfstrace
  <radostrace_path> is radostrace from PATH or ~/.cephlens/bin/radostrace
```

The osdtrace runner script is written under
`~/.cache/cephlens/runner/cephlens-runner-*.sh` on each remote host and is
removed on stop, quit, or TTL expiry. The optional downloaded `osdtrace` binary
is stored under `~/.cephlens/bin/osdtrace`.

Automatic `osdtrace` download is disabled unless `osdtrace_url` is configured.
When a download is required, cephlens requires `osdtrace_sha256` and verifies the
download before installing it. `osdtrace_allow_unverified = true` bypasses that
check for lab use only; do not use it for production clusters.

## Quick start

```sh
cargo run -- snapshot
cargo run -- probe
cargo run -- record --count 3 --interval-secs 2
cargo run -- tui --refresh-secs 1
cargo run -- bench --host ceph-node-1 --seconds 5
```

Create a fresh config template:

```sh
cargo run -- init-config
```

Useful keys in the TUI (the dashboard auto-refreshes every `refresh_secs`):

```text
p          run a probe readiness check
c          edit config
t/f/r      view osdtrace / kfstrace / radostrace; press again to start or stop (confirmed)
a          start or stop all trace sources (confirmed)
i          install osdtrace
x          clear captured trace events
?          toggle the help overlay
[/-        shrink event log
]/+        grow event log
Tab        focus next panel
Shift+Tab  focus previous panel
Up/Down or j/k    scroll focused panel
PgUp/PgDn  scroll focused panel faster
Home/End   jump focused panel to start/end
q/Esc      quit
```

Config screen keys:

```text
up/down  select a config or host row
a        add host
e/Enter  edit or toggle selected row
d/Delete delete selected host row
s        save current profile again
Esc/c    return to live dashboard
```

Config edits are written to `cephlens.toml` and applied to the live SSH streams
immediately after the edit is confirmed.

Before installing, cephlens checks the remote kernel, architecture, and
`/etc/os-release`. The automatic install path only runs when the target is
Linux, x86_64/amd64, and in the Debian/Ubuntu family. Other platforms are shown
as unsupported instead of being blindly overwritten.

The integrated trace panel can show osdtrace, kfstrace, or radostrace data.
The osdtrace view observes Ceph OSD nodes. It streams `op_r`, `op_w`, and
`subop_w` lines into the live dashboard and summarizes total, queue, and
BlueStore latency. The kfstrace and radostrace views run on `client_hosts`.
On wide terminals the trace panel appears on the right; on tall terminals it
appears below the dashboard.
When `trace_auto_start` is true, cephlens starts osdtrace runners as soon as the
TUI opens. The default config keeps it false so an operator explicitly starts
and stops tracing with `t`, `f`, `r`, or `a`.
If no events appear, the cluster may be idle or all observed operations may be
below the configured 1ms latency threshold. Set `trace_latency_ms = 0` to trace
all observed osdtrace ops, or run Ceph IO while the trace runners are active.

Live TUI mode keeps one SSH stream open for cluster status and one stream per
host for node readiness. Each stream emits data once per second by default and
the node table shows connection state (`live`, `dial`, `retry`, `error`), OSD
ids, CPU percentage, and memory percentage.

## License

MIT — see [LICENSE](LICENSE).

cephlens drives the `osdtrace`, `kfstrace`, and `radostrace` binaries from the
[cephtrace](https://github.com/taodd/cephtrace) project, which is licensed
separately under GPL-2.0. cephlens runs them as external commands over SSH and
does not link against them.

Release archives bundle those cephtrace binaries so installation is
self-contained (including for air-gapped clusters). That bundling is mere
aggregation and does not change cephlens's MIT license; the GPL-2.0 license text
and attribution are in [`third_party/cephtrace`](third_party/cephtrace), and the
corresponding source is at the URL above. Building from source does not bundle
them — supply the tracers yourself (`~/.cephlens/bin/<tool>` or `PATH`).
