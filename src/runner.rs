use std::{process::Command as ProcessCommand, thread};

use crate::{
    ssh::ssh_capture,
    trace::{TraceInstallConfig, TraceTarget, parse_trace_target, trace_install_command},
    util::{shell_quote, short},
};

#[derive(Clone, Debug)]
pub(crate) struct CleanupResult {
    pub(crate) host: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

pub(crate) fn cleanup_trace_runners_async(hosts: Vec<String>, session: Option<String>) {
    let Some(session) = session else {
        return;
    };
    for host in hosts {
        let session = session.clone();
        thread::spawn(move || {
            let _ = cleanup_trace_runner_on_host(host, session);
        });
    }
}

pub(crate) fn cleanup_trace_runners_wait(
    hosts: Vec<String>,
    session: Option<String>,
) -> Vec<CleanupResult> {
    let Some(session) = session else {
        return Vec::new();
    };
    let handles = hosts
        .into_iter()
        .map(|host| {
            let session = session.clone();
            thread::spawn(move || cleanup_trace_runner_on_host(host, session))
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .map(|handle| match handle.join() {
            Ok(result) => result,
            Err(_) => CleanupResult {
                host: "-".to_owned(),
                ok: false,
                detail: "cleanup worker panicked".to_owned(),
            },
        })
        .collect()
}

fn cleanup_trace_runner_on_host(host: String, session: String) -> CleanupResult {
    let command = trace_runner_cleanup_command(&session);
    let remote = format!("sh -c {}", shell_quote(&command));
    match ProcessCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=2",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg(&host)
        .arg(remote)
        .output()
    {
        Ok(output) if output.status.success() => CleanupResult {
            host,
            ok: true,
            detail: "cleaned".to_owned(),
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = if !stderr.trim().is_empty() {
                stderr.trim().to_owned()
            } else if !stdout.trim().is_empty() {
                stdout.trim().to_owned()
            } else {
                format!("ssh exited with {}", output.status)
            };
            CleanupResult {
                host,
                ok: false,
                detail: short(&detail, 160),
            }
        }
        Err(err) => CleanupResult {
            host,
            ok: false,
            detail: format!("failed to start cleanup ssh: {err}"),
        },
    }
}

pub(crate) fn report_cleanup_results(results: &[CleanupResult]) {
    for result in results.iter().filter(|result| !result.ok) {
        eprintln!("cephlens cleanup {}: {}", result.host, result.detail);
    }
}

fn trace_runner_cleanup_command(session: &str) -> String {
    let safe_session = safe_session_id(session);
    format!(
        r#"
runner="$HOME/.cache/cephlens/runner/cephlens-runner-{safe_session}.sh"
pidfile="$HOME/.cache/cephlens/runner/cephlens-runner-{safe_session}.pid"
pids=""
if [ -f "$pidfile" ]; then
  pids=$(cat "$pidfile" 2>/dev/null || true)
fi
if [ -n "$pids" ]; then
  for pid in $pids; do
    children=$(pgrep -P "$pid" 2>/dev/null || true)
    if [ -n "$children" ]; then
      kill -TERM $children 2>/dev/null || true
    fi
    kill -TERM "$pid" 2>/dev/null || true
  done
  sleep 1
  for pid in $pids; do
    children=$(pgrep -P "$pid" 2>/dev/null || true)
    if [ -n "$children" ]; then
      kill -KILL $children 2>/dev/null || true
    fi
    kill -KILL "$pid" 2>/dev/null || true
  done
fi
rm -f "$runner" "$pidfile" 2>/dev/null || true
"#
    )
}

pub(crate) fn probe_trace_host(host: &str) -> TraceTarget {
    let command = r#"
kernel=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
os_id=unknown
os_like=none
os_version=unknown
if [ -r /etc/os-release ]; then
  . /etc/os-release
  os_id=${ID:-unknown}
  os_like=${ID_LIKE:-none}
  os_version=${VERSION_ID:-unknown}
fi
os_like=$(printf '%s' "$os_like" | tr ' ' ',')
echo "__CEPHLENS_PLATFORM__ kernel=$kernel arch=$arch os=$os_id like=$os_like version=$os_version"
bin=$(command -v osdtrace 2>/dev/null || true)
if [ -z "$bin" ] && [ -x "$HOME/.cephlens/bin/osdtrace" ]; then
  bin="$HOME/.cephlens/bin/osdtrace"
fi
if [ -z "$bin" ]; then
  echo "__CEPHLENS_STATUS__ missing"
  exit 0
fi
echo "__CEPHLENS_BIN__=$bin"
sudo -n "$bin" --list 2>&1
"#;
    match ssh_capture(host, command) {
        Ok(output) => parse_trace_target(host, &output),
        Err(err) => TraceTarget {
            host: host.to_owned(),
            error: Some(format!("{err:#}")),
            ..TraceTarget::default()
        },
    }
}

pub(crate) fn install_trace_host(host: &str, install: &TraceInstallConfig) -> TraceTarget {
    let command = trace_install_command(install);
    match ssh_capture(host, &command) {
        Ok(output) => parse_trace_target(host, &output),
        Err(err) => TraceTarget {
            host: host.to_owned(),
            error: Some(format!("{err:#}")),
            ..TraceTarget::default()
        },
    }
}

pub(crate) fn trace_threshold_label(latency_ms: u64) -> String {
    if latency_ms == 0 {
        "all ops".to_owned()
    } else {
        format!("latency>={latency_ms}ms")
    }
}

pub(crate) fn trace_runner_install_command(
    session: &str,
    latency_ms: u64,
    ttl_secs: u64,
) -> String {
    let safe_session = safe_session_id(session);
    format!(
        r#"
set -eu
dir="$HOME/.cache/cephlens/runner"
mkdir -p "$dir"
runner="$dir/cephlens-runner-{safe_session}.sh"
pidfile="$dir/cephlens-runner-{safe_session}.pid"
cat > "$runner"
chmod 700 "$runner"
echo "__CEPHLENS_RUNNER__ installed $runner"
exec "$runner" {latency_ms} {ttl_secs} "$pidfile"
"#
    )
}

pub(crate) fn trace_runner_script() -> &'static str {
    r#"#!/bin/sh
latency_ms="${1:-1}"
ttl_secs="${2:-1800}"
pidfile="${3:-}"
runner_path="$0"
trace_pid=""
ttl_pid=""

if [ -n "$pidfile" ]; then
  printf '%s\n' "$$" > "$pidfile"
fi

cleanup() {
  code=$?
  trap - INT TERM HUP EXIT
  if [ -n "$ttl_pid" ]; then
    kill "$ttl_pid" 2>/dev/null || true
  fi
  if [ -n "$trace_pid" ]; then
    kill "$trace_pid" 2>/dev/null || sudo -n kill "$trace_pid" 2>/dev/null || true
    wait "$trace_pid" 2>/dev/null || true
  fi
  rm -f "$runner_path" 2>/dev/null || true
  if [ -n "$pidfile" ]; then
    rm -f "$pidfile" 2>/dev/null || true
  fi
  exit "$code"
}

trap cleanup INT TERM HUP EXIT
echo "__CEPHLENS_RUNNER__ starting ttl=${ttl_secs}s latency_ms=${latency_ms}"

if ! sudo -n true 2>/dev/null; then
  echo "__CEPHLENS_TRACE_ERROR__ sudo -n unavailable"
  exit 126
fi

bin=$(command -v osdtrace 2>/dev/null || true)
if [ -z "$bin" ] && [ -x "$HOME/.cephlens/bin/osdtrace" ]; then
  bin="$HOME/.cephlens/bin/osdtrace"
fi
if [ -z "$bin" ]; then
  echo "__CEPHLENS_TRACE_ERROR__ osdtrace missing"
  exit 127
fi

(
  sleep "$ttl_secs"
  echo "__CEPHLENS_RUNNER__ ttl expired"
  kill -TERM $$
) &
ttl_pid=$!

sudo -n "$bin" -a -l "$latency_ms" 2>&1 &
trace_pid=$!
echo "__CEPHLENS_RUNNER__ osdtrace_pid=$trace_pid"
wait "$trace_pid"
status=$?
trace_pid=""
echo "__CEPHLENS_RUNNER__ osdtrace exited status=$status"
exit "$status"
"#
}

fn safe_session_id(session: &str) -> String {
    session
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .collect()
}
