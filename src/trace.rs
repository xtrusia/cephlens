use std::collections::{BTreeMap, HashMap, VecDeque};

use anyhow::{Result, anyhow};
use chrono::Utc;

use crate::model::Snapshot;
use crate::util::shell_quote;

pub(crate) const TRACE_BUCKET_SECS: i64 = 2;
pub(crate) const TRACE_BUCKET_COUNT: usize = 30;

#[derive(Clone, Debug, Default)]
pub(crate) struct TraceTarget {
    pub(crate) host: String,
    pub(crate) installed: bool,
    pub(crate) binary: String,
    pub(crate) kernel: String,
    pub(crate) arch: String,
    pub(crate) os_id: String,
    pub(crate) os_like: String,
    pub(crate) os_version: String,
    pub(crate) osds: String,
    pub(crate) traceable: String,
    pub(crate) version: String,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct TraceEvent {
    pub(crate) host: String,
    pub(crate) osd: String,
    pub(crate) pg: String,
    pub(crate) op: String,
    pub(crate) op_lat_us: u64,
    pub(crate) throttle_lat_us: u64,
    pub(crate) recv_lat_us: u64,
    pub(crate) dispatch_lat_us: u64,
    pub(crate) queue_lat_us: u64,
    pub(crate) bluestore_lat_us: u64,
    pub(crate) kv_commit_us: u64,
    pub(crate) raw: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TracePgStats {
    pub(crate) ops: u64,
    pub(crate) op_sum_us: u64,
    pub(crate) op_max_us: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TraceBucket {
    pub(crate) bucket: i64,
    pub(crate) ops: u64,
    pub(crate) op_sum_us: u64,
    pub(crate) op_max_us: u64,
    pub(crate) throttle_max_us: u64,
    pub(crate) recv_max_us: u64,
    pub(crate) dispatch_max_us: u64,
    pub(crate) queue_max_us: u64,
    pub(crate) store_max_us: u64,
    pub(crate) kv_commit_max_us: u64,
    pub(crate) pgs: HashMap<String, TracePgStats>,
}

#[derive(Clone, Debug)]
pub(crate) struct TraceGraphRow {
    pub(crate) osd: String,
    pub(crate) host: String,
    pub(crate) ops: u64,
    pub(crate) avg_us: u64,
    pub(crate) max_us: u64,
    pub(crate) throttle_max_us: u64,
    pub(crate) recv_max_us: u64,
    pub(crate) dispatch_max_us: u64,
    pub(crate) queue_max_us: u64,
    pub(crate) store_max_us: u64,
    pub(crate) kv_commit_max_us: u64,
    pub(crate) pg_count: usize,
    pub(crate) hot_pg: String,
    pub(crate) points: Vec<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TraceComponent {
    pub(crate) name: &'static str,
    pub(crate) value_us: u64,
    pub(crate) suspect: &'static str,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TraceInstallConfig {
    pub(crate) url: Option<String>,
    pub(crate) sha256: Option<String>,
    pub(crate) allow_unverified: bool,
}

pub(crate) fn validate_sha256(value: &str) -> Result<()> {
    if is_valid_sha256(value) {
        Ok(())
    } else {
        Err(anyhow!(
            "osdtrace_sha256 must be exactly 64 hexadecimal characters"
        ))
    }
}

pub(crate) fn is_valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

pub(crate) fn trace_install_command(install: &TraceInstallConfig) -> String {
    let url = install.url.as_deref().unwrap_or_default();
    let sha256 = install.sha256.as_deref().unwrap_or_default();
    let allow_unverified = if install.allow_unverified {
        "yes"
    } else {
        "no"
    };
    format!(
        r#"
set -u
url={url}
sha256={sha256}
allow_unverified={allow_unverified}
kernel=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
os_id=unknown
os_like=none
os_version=unknown
if [ -r /etc/os-release ]; then
  . /etc/os-release
  os_id=${{ID:-unknown}}
  os_like=${{ID_LIKE:-none}}
  os_version=${{VERSION_ID:-unknown}}
fi
os_like=$(printf '%s' "$os_like" | tr ' ' ',')
echo "__CEPHLENS_PLATFORM__ kernel=$kernel arch=$arch os=$os_id like=$os_like version=$os_version"
bin=$(command -v osdtrace 2>/dev/null || true)
if [ -z "$bin" ] && [ -x "$HOME/.cephlens/bin/osdtrace" ]; then
  bin="$HOME/.cephlens/bin/osdtrace"
fi
if [ -n "$bin" ]; then
  echo "__CEPHLENS_BIN__=$bin"
  sudo -n "$bin" --list 2>&1
  exit 0
fi
case "$arch" in
  x86_64|amd64) supported_arch=yes ;;
  *) supported_arch=no ;;
esac
case "$os_id $os_like" in
  *ubuntu*|*debian*) supported_os=yes ;;
  *) supported_os=no ;;
esac
if [ "$kernel" != "Linux" ]; then
  echo "__CEPHLENS_ERROR__ unsupported kernel '$kernel'; osdtrace prebuilt is Linux-only"
  exit 0
fi
if [ "$supported_arch" != "yes" ]; then
  echo "__CEPHLENS_ERROR__ unsupported arch '$arch'; osdtrace prebuilt is x86_64/amd64 only"
  exit 0
fi
if [ "$supported_os" != "yes" ]; then
  echo "__CEPHLENS_ERROR__ unsupported Linux distribution '$os_id' like '$os_like'; automatic install is limited to Debian/Ubuntu family"
  exit 0
fi
if [ -z "$url" ]; then
  echo "__CEPHLENS_ERROR__ osdtrace download disabled; set osdtrace_url and osdtrace_sha256 in cephlens.toml"
  exit 0
fi
if [ -z "$sha256" ] && [ "$allow_unverified" != "yes" ]; then
  echo "__CEPHLENS_ERROR__ osdtrace_sha256 is required for automatic install"
  exit 0
fi
mkdir -p "$HOME/.cephlens/bin"
tmp="$HOME/.cephlens/bin/osdtrace.tmp.$$"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$url" -o "$tmp"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$tmp" "$url"
else
  echo "__CEPHLENS_ERROR__ curl or wget is required"
  exit 0
fi
if [ -n "$sha256" ]; then
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp" | awk '{{print $1}}')
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp" | awk '{{print $1}}')
  else
    rm -f "$tmp"
    echo "__CEPHLENS_ERROR__ sha256sum or shasum is required"
    exit 0
  fi
  if [ "$actual" != "$sha256" ]; then
    rm -f "$tmp"
    echo "__CEPHLENS_ERROR__ checksum mismatch expected=$sha256 actual=$actual"
    exit 0
  fi
else
  echo "__CEPHLENS_WARN__ unverified osdtrace download allowed by config"
fi
mv "$tmp" "$HOME/.cephlens/bin/osdtrace"
chmod +x "$HOME/.cephlens/bin/osdtrace"
echo "__CEPHLENS_BIN__=$HOME/.cephlens/bin/osdtrace"
sudo -n "$HOME/.cephlens/bin/osdtrace" --list 2>&1
"#,
        url = shell_quote(url),
        sha256 = shell_quote(sha256),
        allow_unverified = shell_quote(allow_unverified),
    )
}

pub(crate) fn kfstrace_run_command(latency_us: u64, ttl_secs: u64) -> String {
    format!(
        r#"
bin=$(command -v kfstrace 2>/dev/null || true)
if [ -z "$bin" ] && [ -x "$HOME/.cephlens/bin/kfstrace" ]; then
  bin="$HOME/.cephlens/bin/kfstrace"
fi
if [ -z "$bin" ]; then
  echo "__CEPHLENS_KFS_ERROR__ kfstrace missing"
  exit 127
fi
if ! sudo -n true 2>/dev/null; then
  echo "__CEPHLENS_KFS_ERROR__ sudo -n unavailable"
  exit 126
fi
exec sudo -n "$bin" -m mds -l {latency_us} -t {ttl_secs} 2>&1
"#
    )
}

pub(crate) fn radostrace_run_command(ttl_secs: u64) -> String {
    format!(
        r#"
bin=$(command -v radostrace 2>/dev/null || true)
if [ -z "$bin" ] && [ -x "$HOME/.cephlens/bin/radostrace" ]; then
  bin="$HOME/.cephlens/bin/radostrace"
fi
if [ -z "$bin" ]; then
  echo "__CEPHLENS_RADOS_ERROR__ radostrace missing"
  exit 127
fi
if ! sudo -n true 2>/dev/null; then
  echo "__CEPHLENS_RADOS_ERROR__ sudo -n unavailable"
  exit 126
fi
exec sudo -n "$bin" -t {ttl_secs} 2>&1
"#
    )
}

pub(crate) fn tracer_probe_command(tool: &str) -> String {
    format!(
        r#"
bin=$(command -v {tool} 2>/dev/null || true)
if [ -z "$bin" ] && [ -x "$HOME/.cephlens/bin/{tool}" ]; then
  bin="$HOME/.cephlens/bin/{tool}"
fi
if [ -z "$bin" ]; then
  echo "__CEPHLENS_STATUS__ missing"
  exit 0
fi
echo "__CEPHLENS_BIN__=$bin"
if ! sudo -n true 2>/dev/null; then
  echo "__CEPHLENS_ERROR__ sudo -n unavailable"
  exit 0
fi
"$bin" --version 2>/dev/null | head -1 || true
"#
    )
}

pub(crate) fn parse_trace_target(host: &str, output: &str) -> TraceTarget {
    let mut target = TraceTarget {
        host: host.to_owned(),
        ..TraceTarget::default()
    };
    let mut osds = Vec::new();
    let mut traceable = Vec::new();
    let mut versions = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if let Some(binary) = line.strip_prefix("__CEPHLENS_BIN__=") {
            target.binary = binary.to_owned();
            target.installed = true;
            continue;
        }
        if let Some(platform) = line.strip_prefix("__CEPHLENS_PLATFORM__ ") {
            apply_trace_platform(&mut target, platform);
            continue;
        }
        if line.starts_with("__CEPHLENS_STATUS__ missing") {
            target.error = Some("osdtrace missing".to_owned());
            continue;
        }
        if let Some(error) = line.strip_prefix("__CEPHLENS_ERROR__ ") {
            target.error = Some(error.to_owned());
            continue;
        }

        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() >= 5 && parts[0].chars().all(|ch| ch.is_ascii_digit()) {
            osds.push(parts[1].to_owned());
            traceable.push(parts[3].to_owned());
            versions.push(parts[4..].join(" "));
        }
    }

    if target.installed && osds.is_empty() && target.error.is_none() {
        target.error = Some("no ceph-osd process listed".to_owned());
    }
    target.osds = if osds.is_empty() {
        "-".to_owned()
    } else {
        osds.join(",")
    };
    target.traceable = summarize_traceable(&traceable);
    target.version = versions.first().cloned().unwrap_or_else(|| "-".to_owned());
    target
}

fn apply_trace_platform(target: &mut TraceTarget, fields: &str) {
    for field in fields.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "kernel" => target.kernel = value.to_owned(),
            "arch" => target.arch = value.to_owned(),
            "os" => target.os_id = value.to_owned(),
            "like" => target.os_like = value.replace(',', " "),
            "version" => target.os_version = value.to_owned(),
            _ => {}
        }
    }
}

fn summarize_traceable(values: &[String]) -> String {
    if values.is_empty() {
        return "-".to_owned();
    }
    let yes = values
        .iter()
        .filter(|value| value.eq_ignore_ascii_case("yes"))
        .count();
    format!("{yes}/{}", values.len())
}

pub(crate) fn parse_trace_event(host: &str, line: &str) -> Option<TraceEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("PID")
        || trimmed.starts_with("---")
        || trimmed.contains("Traceable")
        || trimmed.contains("Ceph Version")
    {
        return None;
    }
    if let Some(error) = trimmed.strip_prefix("__CEPHLENS_TRACE_ERROR__") {
        return Some(TraceEvent {
            host: host.to_owned(),
            osd: "-".to_owned(),
            pg: "-".to_owned(),
            op: "error".to_owned(),
            op_lat_us: 0,
            throttle_lat_us: 0,
            recv_lat_us: 0,
            dispatch_lat_us: 0,
            queue_lat_us: 0,
            bluestore_lat_us: 0,
            kv_commit_us: 0,
            raw: error.trim().to_owned(),
        });
    }
    let op = trimmed
        .split_whitespace()
        .find(|token| matches!(*token, "op_r" | "op_w" | "subop_w"))?;
    let osd = token_after(trimmed, "osd").unwrap_or("-").to_owned();
    let pg = token_after(trimmed, "pg").unwrap_or("-").to_owned();
    Some(TraceEvent {
        host: host.to_owned(),
        osd,
        pg,
        op: op.to_owned(),
        op_lat_us: token_after(trimmed, "op_lat")
            .or_else(|| token_after(trimmed, "subop_lat"))
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        throttle_lat_us: token_after(trimmed, "throttle_lat")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        recv_lat_us: token_after(trimmed, "recv_lat")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        dispatch_lat_us: token_after(trimmed, "dispatch_lat")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        queue_lat_us: token_after(trimmed, "queue_lat")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        bluestore_lat_us: token_after(trimmed, "bluestore_lat")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        kv_commit_us: token_after(trimmed, "kv_commit")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        raw: trimmed.to_owned(),
    })
}

fn token_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == key {
            return tokens.next();
        }
    }
    None
}

pub(crate) fn normalize_osd_name(osd: &str) -> String {
    let osd = osd.trim();
    if osd.is_empty() || osd == "-" {
        "-".to_owned()
    } else if osd.starts_with("osd.") {
        osd.to_owned()
    } else if osd.chars().all(|ch| ch.is_ascii_digit()) {
        format!("osd.{osd}")
    } else {
        osd.to_owned()
    }
}

pub(crate) fn normalize_pg_name(pg: &str) -> String {
    let pg = pg.trim();
    if pg.is_empty() || pg == "-" {
        "-".to_owned()
    } else {
        pg.to_owned()
    }
}

pub(crate) fn dominant_component(row: &TraceGraphRow) -> TraceComponent {
    [
        TraceComponent {
            name: "kv_commit",
            value_us: row.kv_commit_max_us,
            suspect: "RocksDB/BlueFS commit or DB/WAL device latency",
        },
        TraceComponent {
            name: "bluestore",
            value_us: row.store_max_us,
            suspect: "BlueStore backend or block device latency",
        },
        TraceComponent {
            name: "queue",
            value_us: row.queue_max_us,
            suspect: "OSD queue wait or worker saturation",
        },
        TraceComponent {
            name: "recv",
            value_us: row.recv_max_us,
            suspect: "network receive path, bandwidth, or packet loss",
        },
        TraceComponent {
            name: "dispatch",
            value_us: row.dispatch_max_us,
            suspect: "messenger dispatch or OSD thread scheduling",
        },
        TraceComponent {
            name: "throttle",
            value_us: row.throttle_max_us,
            suspect: "throttling/backpressure before OSD processing",
        },
    ]
    .into_iter()
    .max_by_key(|component| component.value_us)
    .unwrap_or(TraceComponent {
        name: "unknown",
        value_us: 0,
        suspect: "raw trace inspection needed",
    })
}

pub(crate) fn trace_graph_rows(
    snapshot: Option<&Snapshot>,
    trace_events: &[TraceEvent],
    trace_series: &HashMap<String, VecDeque<TraceBucket>>,
    limit: usize,
) -> Vec<TraceGraphRow> {
    let now_bucket = Utc::now().timestamp() / TRACE_BUCKET_SECS;
    trace_graph_rows_at(snapshot, trace_events, trace_series, limit, now_bucket)
}

pub(crate) fn trace_graph_rows_at(
    snapshot: Option<&Snapshot>,
    trace_events: &[TraceEvent],
    trace_series: &HashMap<String, VecDeque<TraceBucket>>,
    limit: usize,
    now_bucket: i64,
) -> Vec<TraceGraphRow> {
    let mut hosts = BTreeMap::new();
    if let Some(snapshot) = snapshot {
        for osd in &snapshot.osds {
            hosts.insert(normalize_osd_name(&osd.name), osd.host.clone());
        }
    }
    for event in trace_events {
        let osd = normalize_osd_name(&event.osd);
        if osd != "-" {
            hosts.entry(osd).or_insert_with(|| event.host.clone());
        }
    }
    for osd in trace_series.keys() {
        hosts.entry(osd.clone()).or_insert_with(|| "-".to_owned());
    }

    let first_bucket = now_bucket - TRACE_BUCKET_COUNT as i64 + 1;
    let mut rows = hosts
        .into_iter()
        .map(|(osd, host)| {
            let series = trace_series.get(&osd);
            let mut ops = 0u64;
            let mut sum_us = 0u64;
            let mut max_us = 0u64;
            let mut throttle_max_us = 0u64;
            let mut recv_max_us = 0u64;
            let mut dispatch_max_us = 0u64;
            let mut queue_max_us = 0u64;
            let mut store_max_us = 0u64;
            let mut kv_commit_max_us = 0u64;
            let mut pg_stats = HashMap::<String, TracePgStats>::new();

            if let Some(series) = series {
                for bucket in series
                    .iter()
                    .filter(|bucket| bucket.bucket >= first_bucket && bucket.bucket <= now_bucket)
                {
                    ops = ops.saturating_add(bucket.ops);
                    sum_us = sum_us.saturating_add(bucket.op_sum_us);
                    max_us = max_us.max(bucket.op_max_us);
                    throttle_max_us = throttle_max_us.max(bucket.throttle_max_us);
                    recv_max_us = recv_max_us.max(bucket.recv_max_us);
                    dispatch_max_us = dispatch_max_us.max(bucket.dispatch_max_us);
                    queue_max_us = queue_max_us.max(bucket.queue_max_us);
                    store_max_us = store_max_us.max(bucket.store_max_us);
                    kv_commit_max_us = kv_commit_max_us.max(bucket.kv_commit_max_us);
                    for (pg, stats) in &bucket.pgs {
                        let aggregate = pg_stats.entry(pg.clone()).or_default();
                        aggregate.ops = aggregate.ops.saturating_add(stats.ops);
                        aggregate.op_sum_us = aggregate.op_sum_us.saturating_add(stats.op_sum_us);
                        aggregate.op_max_us = aggregate.op_max_us.max(stats.op_max_us);
                    }
                }
            }

            let pg_count = pg_stats.len();
            let hot_pg = hot_pg_label(pg_stats);
            TraceGraphRow {
                osd,
                host,
                ops,
                avg_us: sum_us.checked_div(ops).unwrap_or(0),
                max_us,
                throttle_max_us,
                recv_max_us,
                dispatch_max_us,
                queue_max_us,
                store_max_us,
                kv_commit_max_us,
                pg_count,
                hot_pg,
                points: trace_points(series, first_bucket, now_bucket),
            }
        })
        .collect::<Vec<_>>();

    rows.sort_by(|left, right| {
        right
            .ops
            .cmp(&left.ops)
            .then_with(|| right.max_us.cmp(&left.max_us))
            .then_with(|| osd_sort_key(&left.osd).cmp(&osd_sort_key(&right.osd)))
            .then_with(|| left.osd.cmp(&right.osd))
    });

    if limit > 0 && rows.len() > limit {
        rows.truncate(limit);
    }
    rows
}

pub(crate) fn record_trace_event_at(
    trace_series: &mut HashMap<String, VecDeque<TraceBucket>>,
    event: &TraceEvent,
    bucket_id: i64,
) {
    let osd = normalize_osd_name(&event.osd);
    if osd == "-" || event.op == "error" {
        return;
    }

    let series = trace_series.entry(osd).or_default();
    let needs_new_bucket = series
        .back()
        .map(|bucket| bucket.bucket != bucket_id)
        .unwrap_or(true);
    if needs_new_bucket {
        series.push_back(TraceBucket {
            bucket: bucket_id,
            ..TraceBucket::default()
        });
    }
    while series.len() > TRACE_BUCKET_COUNT {
        series.pop_front();
    }

    if let Some(bucket) = series.back_mut() {
        bucket.ops += 1;
        bucket.op_sum_us = bucket.op_sum_us.saturating_add(event.op_lat_us);
        bucket.op_max_us = bucket.op_max_us.max(event.op_lat_us);
        bucket.throttle_max_us = bucket.throttle_max_us.max(event.throttle_lat_us);
        bucket.recv_max_us = bucket.recv_max_us.max(event.recv_lat_us);
        bucket.dispatch_max_us = bucket.dispatch_max_us.max(event.dispatch_lat_us);
        bucket.queue_max_us = bucket.queue_max_us.max(event.queue_lat_us);
        bucket.store_max_us = bucket.store_max_us.max(event.bluestore_lat_us);
        bucket.kv_commit_max_us = bucket.kv_commit_max_us.max(event.kv_commit_us);
        let pg = normalize_pg_name(&event.pg);
        if pg != "-" {
            let pg_stats = bucket.pgs.entry(pg).or_default();
            pg_stats.ops += 1;
            pg_stats.op_sum_us = pg_stats.op_sum_us.saturating_add(event.op_lat_us);
            pg_stats.op_max_us = pg_stats.op_max_us.max(event.op_lat_us);
        }
    }
}

fn hot_pg_label(pg_stats: HashMap<String, TracePgStats>) -> String {
    if pg_stats.is_empty() {
        return "-".to_owned();
    }

    let mut stats = pg_stats.into_iter().collect::<Vec<_>>();
    stats.sort_by(|(left_pg, left), (right_pg, right)| {
        right
            .ops
            .cmp(&left.ops)
            .then_with(|| right.op_max_us.cmp(&left.op_max_us))
            .then_with(|| pg_sort_key(left_pg).cmp(&pg_sort_key(right_pg)))
            .then_with(|| left_pg.cmp(right_pg))
    });

    stats
        .into_iter()
        .take(2)
        .map(|(pg, stats)| format!("{pg}:{}", stats.ops))
        .collect::<Vec<_>>()
        .join(" ")
}

fn pg_sort_key(pg: &str) -> (u64, u64) {
    let Some((pool, ps)) = pg.split_once('.') else {
        return (u64::MAX, u64::MAX);
    };
    let pool = pool.parse::<u64>().unwrap_or(u64::MAX);
    let ps = u64::from_str_radix(ps, 16).unwrap_or(u64::MAX);
    (pool, ps)
}

fn trace_points(
    series: Option<&VecDeque<TraceBucket>>,
    first_bucket: i64,
    now_bucket: i64,
) -> Vec<u64> {
    let width = (now_bucket - first_bucket + 1).max(0) as usize;
    let mut points = vec![0; width];
    let Some(series) = series else {
        return points;
    };

    for bucket in series {
        if bucket.bucket < first_bucket || bucket.bucket > now_bucket {
            continue;
        }
        let index = (bucket.bucket - first_bucket) as usize;
        if let Some(point) = points.get_mut(index) {
            *point = bucket.op_max_us;
        }
    }
    points
}

fn osd_sort_key(osd: &str) -> i64 {
    osd.strip_prefix("osd.")
        .and_then(|id| id.parse::<i64>().ok())
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket_with_pg(bucket: i64, ops: u64, op_max_us: u64, pg: &str) -> TraceBucket {
        let mut pgs = HashMap::new();
        pgs.insert(
            pg.to_owned(),
            TracePgStats {
                ops,
                op_sum_us: op_max_us.saturating_mul(ops),
                op_max_us,
            },
        );
        TraceBucket {
            bucket,
            ops,
            op_sum_us: op_max_us.saturating_mul(ops),
            op_max_us,
            pgs,
            ..TraceBucket::default()
        }
    }

    #[test]
    fn parse_trace_event_reads_osdtrace_fields() {
        let event = parse_trace_event(
            "node-a",
            "123 op_w osd 2 pg 2.e throttle_lat 7 recv_lat 11 dispatch_lat 13 queue_lat 17 bluestore_lat 19 kv_commit 23 op_lat 101",
        )
        .expect("trace event should parse");

        assert_eq!(event.host, "node-a");
        assert_eq!(event.osd, "2");
        assert_eq!(event.pg, "2.e");
        assert_eq!(event.op, "op_w");
        assert_eq!(event.op_lat_us, 101);
        assert_eq!(event.throttle_lat_us, 7);
        assert_eq!(event.recv_lat_us, 11);
        assert_eq!(event.dispatch_lat_us, 13);
        assert_eq!(event.queue_lat_us, 17);
        assert_eq!(event.bluestore_lat_us, 19);
        assert_eq!(event.kv_commit_us, 23);
    }

    #[test]
    fn parse_trace_event_supports_subop_latency_and_errors() {
        let event = parse_trace_event("node-a", "subop_w osd 3 pg 2.14 subop_lat 42")
            .expect("subop event should parse");
        assert_eq!(event.op, "subop_w");
        assert_eq!(event.op_lat_us, 42);

        let error = parse_trace_event("node-a", "__CEPHLENS_TRACE_ERROR__ sudo -n unavailable")
            .expect("runner error should parse");
        assert_eq!(error.op, "error");
        assert_eq!(error.raw, "sudo -n unavailable");
    }

    #[test]
    fn parse_trace_event_ignores_headers_and_empty_lines() {
        assert!(parse_trace_event("node-a", "").is_none());
        assert!(parse_trace_event("node-a", "PID something").is_none());
        assert!(parse_trace_event("node-a", "Ceph Version 18.2.0").is_none());
    }

    #[test]
    fn sha256_validation_requires_exact_hex_digest() {
        assert!(is_valid_sha256(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_valid_sha256("0123"));
        assert!(!is_valid_sha256(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg"
        ));
    }

    #[test]
    fn trace_install_command_requires_checksum_by_default() {
        let command = trace_install_command(&TraceInstallConfig {
            url: Some("https://example.invalid/osdtrace".to_owned()),
            sha256: None,
            allow_unverified: false,
        });

        assert!(command.contains("osdtrace_sha256 is required"));
        assert!(command.contains("allow_unverified='no'"));
    }

    #[test]
    fn trace_install_command_quotes_url_and_sha() {
        let command = trace_install_command(&TraceInstallConfig {
            url: Some("https://example.invalid/osd'trace".to_owned()),
            sha256: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            ),
            allow_unverified: false,
        });

        assert!(command.contains("url='https://example.invalid/osd'\\''trace'"));
        assert!(
            command.contains(
                "sha256='0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'"
            )
        );
        assert!(command.contains("checksum mismatch expected=$sha256 actual=$actual"));
    }

    #[test]
    fn hot_pg_label_orders_by_ops_then_latency_then_pg() {
        let mut stats = HashMap::new();
        stats.insert(
            "2.14".to_owned(),
            TracePgStats {
                ops: 80,
                op_sum_us: 0,
                op_max_us: 400,
            },
        );
        stats.insert(
            "2.e".to_owned(),
            TracePgStats {
                ops: 100,
                op_sum_us: 0,
                op_max_us: 100,
            },
        );
        stats.insert(
            "2.f".to_owned(),
            TracePgStats {
                ops: 80,
                op_sum_us: 0,
                op_max_us: 500,
            },
        );

        assert_eq!(hot_pg_label(stats), "2.e:100 2.f:80");
    }

    #[test]
    fn trace_graph_rows_aggregates_and_sorts_osds() {
        let now_bucket = Utc::now().timestamp() / TRACE_BUCKET_SECS;
        let mut series_by_osd = HashMap::new();
        let mut osd1 = VecDeque::new();
        osd1.push_back(bucket_with_pg(now_bucket, 1, 25_000, "2.e"));
        let mut osd2 = VecDeque::new();
        osd2.push_back(bucket_with_pg(now_bucket, 5, 12_000, "2.14"));
        let mut osd3 = VecDeque::new();
        osd3.push_back(bucket_with_pg(now_bucket, 0, 100_000, "2.99"));
        series_by_osd.insert("osd.1".to_owned(), osd1);
        series_by_osd.insert("osd.2".to_owned(), osd2);
        series_by_osd.insert("osd.3".to_owned(), osd3);

        let rows = trace_graph_rows(None, &[], &series_by_osd, usize::MAX);

        assert_eq!(rows[0].osd, "osd.2");
        assert_eq!(rows[0].ops, 5);
        assert_eq!(rows[0].avg_us, 12_000);
        assert_eq!(rows[0].hot_pg, "2.14:5");
        assert_eq!(rows[1].osd, "osd.1");
        assert_eq!(rows[1].max_us, 25_000);
        assert_eq!(rows[2].osd, "osd.3");
    }

    #[test]
    fn trace_graph_rows_respects_limit() {
        let now_bucket = Utc::now().timestamp() / TRACE_BUCKET_SECS;
        let mut series_by_osd = HashMap::new();
        for id in 0..4 {
            let mut series = VecDeque::new();
            series.push_back(bucket_with_pg(now_bucket, id + 1, 100, "1.a"));
            series_by_osd.insert(format!("osd.{id}"), series);
        }

        assert_eq!(trace_graph_rows(None, &[], &series_by_osd, 2).len(), 2);
    }
}
