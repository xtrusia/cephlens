# cephlens

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)

An SSH-driven Ceph investigation TUI with live cluster status, per-node
readiness, and osdtrace eBPF latency views.

cephlens runs on Windows, Linux, or macOS and talks to Ceph nodes over
persistent SSH streams. It is currently a lab-first prototype, not a packaged
production monitoring agent.

Trace collection uses temporary remote shell runners that start `osdtrace`,
stream its output back to the TUI, and remove themselves when tracing stops or
cephlens exits.

## Features

- Live cluster health, quorum, OSD counts, and IO throughput over a single SSH stream.
- Per-node readiness: connection state, OSD ids, CPU and memory percent, and microceph version.
- osdtrace eBPF latency tracing with per-OSD and per-PG breakdown of queue, BlueStore, and KV-commit latency.
- Agentless: no permanent daemon on the nodes; runner scripts remove themselves on stop, quit, or TTL expiry.
- Edit hosts and trace settings live in the TUI; changes apply to open SSH streams immediately.

## Requirements

Controller (where the TUI runs):

- Rust 1.85+ (edition 2024) to build.
- An OpenSSH client on `PATH`, with every host reachable over non-interactive SSH (key-based, no password prompt).

Ceph nodes:

- `ceph` and `rados` CLIs, plus passwordless `sudo -n` for the observed commands (see Access and sudo).
- For tracing, the `osdtrace` binary from [cephtrace](https://github.com/taodd/cephtrace). osdtrace is eBPF-based and needs a Linux 5.8+ kernel; cephlens installs a prebuilt binary automatically only on Debian/Ubuntu x86_64, otherwise install it yourself.

## Status

This project is suitable for lab clusters and experiments. Before using it on a
production cluster, review the sudo policy, osdtrace artifact source, and
cleanup behavior described below.

## Configuration

Copy the example config and edit it for your cluster:

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
machines that get persistent node-readiness SSH streams.

## Access and sudo

cephlens does not install a permanent agent on Ceph nodes. It opens SSH
connections from the machine running the TUI, so each configured host must be
reachable with non-interactive SSH:

```powershell
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

For production, prefer a dedicated user and a command whitelist. Adjust paths
with `command -v ceph`, `command -v rados`, and `command -v osdtrace` on your
hosts:

```sudoers
cephlens ALL=(root) NOPASSWD: /usr/bin/ceph, /usr/bin/rados, /usr/bin/osdtrace, /usr/bin/kill
```

The current prototype runs these privileged operations:

```text
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
  sudo -n osdtrace --list
  sudo -n ~/.cephlens/bin/osdtrace --list

trace runner:
  sudo -n osdtrace -a -l <latency_ms>
  sudo -n kill <osdtrace_pid> when cleanup cannot kill it as the SSH user
```

The runner script itself is written under
`~/.cache/cephlens/runner/cephlens-runner-*.sh` on each remote host and is
removed on stop, quit, or TTL expiry. The optional downloaded `osdtrace` binary
is stored under `~/.cephlens/bin/osdtrace`.

Automatic `osdtrace` download is disabled unless `osdtrace_url` is configured.
When a download is required, cephlens requires `osdtrace_sha256` and verifies the
download before installing it. `osdtrace_allow_unverified = true` bypasses that
check for lab use only; do not use it for production clusters.

## Quick start

```powershell
cargo run -- snapshot
cargo run -- probe
cargo run -- record --count 3 --interval-secs 2
cargo run -- tui --refresh-secs 1
cargo run -- bench --host ceph-node-1 --seconds 5
```

Create a fresh config template:

```powershell
cargo run -- init-config
```

Useful keys in the TUI:

```text
r      one-shot refresh
p      run a probe readiness check
c      edit config
i      install osdtrace
s      stop trace runners
t      start temp trace runners with latency threshold 1ms
0      start temp trace runners and show all observed ops
x      clear captured trace events
v      open the osdtrace targets view (Esc returns)
[/-    shrink event log
]/+    grow event log
Tab    focus next panel
Shift+Tab focus previous panel
Up/Down or j/k scroll focused panel
PgUp/PgDn scroll focused panel faster
Home/End jump focused panel to start/end
q/Esc  quit
```

Config screen keys:

```text
up/down  select admin_host, refresh_secs, or a host row
a        add host
e/Enter  edit selected row
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

The integrated trace panel currently focuses on `osdtrace` because the
prototype observes Ceph OSD nodes. It streams `op_r`, `op_w`, and `subop_w`
lines into the live dashboard and summarizes total, queue, and BlueStore
latency. On wide terminals it appears on the right; on tall terminals it appears
below the dashboard.
When `trace_auto_start` is true, cephlens starts trace runners as soon as the
TUI opens. The default config keeps it false so an operator explicitly starts
and stops tracing with `t`, `0`, and `s`.
If no events appear, the cluster may be idle or all observed operations may be
below the `t` key's 1ms latency threshold; press `0` to trace all observed ops
or run Ceph IO while the trace runners are active.

Live TUI mode keeps one SSH stream open for cluster status and one stream per
host for node readiness. Each stream emits data once per second by default and
the node table shows connection state (`live`, `dial`, `retry`, `error`), OSD
ids, CPU percentage, and memory percentage.

## License

MIT — see [LICENSE](LICENSE).

cephlens drives the `osdtrace` binary from the
[cephtrace](https://github.com/taodd/cephtrace) project, which is licensed
separately under GPL-2.0. cephlens runs it as an external command over SSH and
does not link against it.
