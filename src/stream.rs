use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::{
    model::NodeSummary,
    util::{ptr_f64, ptr_str, ptr_u64},
};

pub(crate) fn cluster_stream_command(interval_secs: u64) -> String {
    format!(
        r#"
while true; do
  status=$(sudo -n ceph -s --format json 2>/dev/null | tr -d '\n')
  tree=$(sudo -n ceph osd tree --format json 2>/dev/null | tr -d '\n')
  df=$(sudo -n ceph osd df --format json 2>/dev/null | tr -d '\n')
  if [ -n "$status" ] && [ -n "$tree" ] && [ -n "$df" ]; then
    printf '{{"type":"status","status":%s,"tree":%s,"df":%s}}\n' "$status" "$tree" "$df"
  else
    printf '{{"type":"error","message":"ceph command failed"}}\n'
  fi
  sleep {interval_secs}
done
"#
    )
}

// Shared node facts collection: sets hostname, sudo_state, ceph_version,
// deployment, count, ids, and mem_pct shell variables. Used by both the
// one-shot probe in collect.rs and the streaming loop below.
pub(crate) const NODE_FACTS_SNIPPET: &str = r#"hostname=$(hostname)
if sudo -n true 2>/dev/null; then sudo_state=ok; else sudo_state=needs_password; fi
ceph_version=$(ceph --version 2>/dev/null | head -1)
if [ -z "$ceph_version" ]; then ceph_version=missing; fi
deployment=generic
micro=$(snap list microceph 2>/dev/null | awk 'NR==2 {print $2" "$4" "$6; found=1}')
if [ -n "$micro" ]; then
  deployment="microceph $micro"
elif command -v cephadm >/dev/null 2>&1; then
  deployment=cephadm
elif [ -d /var/lib/rook ]; then
  deployment=rook
fi
count=$(pgrep -c '[c]eph-osd' 2>/dev/null || echo 0)
ids=$(pgrep -af '[c]eph-osd --cluster ceph' 2>/dev/null | sed -n 's/.*--id \([0-9][0-9]*\).*/\1/p' | paste -sd, -)
mem_pct=$(awk '/MemTotal:/ {total=$2} /MemAvailable:/ {avail=$2} END {if (total > 0) printf "%.1f", (total-avail)*100/total; else printf "0.0"}' /proc/meminfo)"#;

pub(crate) fn node_stream_command(interval_secs: u64) -> String {
    format!(
        r#"
prev_total=0
prev_idle=0
while true; do
{facts}
  read _ user nice system idle iowait irq softirq steal _ _ < /proc/stat
  idle_all=$((idle + iowait))
  non_idle=$((user + nice + system + irq + softirq + steal))
  total=$((idle_all + non_idle))
  if [ "$prev_total" -gt 0 ]; then
    diff_total=$((total - prev_total))
    diff_idle=$((idle_all - prev_idle))
    if [ "$diff_total" -gt 0 ]; then
      cpu_pct=$(awk -v total="$diff_total" -v idle="$diff_idle" 'BEGIN {{ printf "%.1f", (total-idle)*100/total }}')
    else
      cpu_pct=0.0
    fi
  else
    cpu_pct=0.0
  fi
  prev_total=$total
  prev_idle=$idle_all
  printf '{{"type":"node","hostname":"%s","sudo":"%s","ceph_version":"%s","deployment":"%s","ceph_osd_processes":%s,"osd_ids":"%s","cpu_percent":%s,"mem_percent":%s}}\n' "$hostname" "$sudo_state" "$ceph_version" "$deployment" "$count" "$ids" "$cpu_pct" "$mem_pct"
  sleep {interval_secs}
done
"#,
        facts = NODE_FACTS_SNIPPET,
    )
}

pub(crate) fn parse_node_stream_payload(host: &str, payload: &str) -> Result<NodeSummary> {
    let value: Value = serde_json::from_str(payload)
        .with_context(|| format!("invalid node stream payload from {host}"))?;
    if value.pointer("/type").and_then(Value::as_str) == Some("error") {
        return Err(anyhow!(
            "{}",
            value
                .pointer("/message")
                .and_then(Value::as_str)
                .unwrap_or("remote node probe failed")
        ));
    }
    Ok(NodeSummary {
        host: host.to_owned(),
        hostname: ptr_str(&value, "/hostname"),
        sudo: ptr_str(&value, "/sudo"),
        ceph_version: ptr_str(&value, "/ceph_version"),
        deployment: ptr_str(&value, "/deployment"),
        ceph_osd_processes: ptr_u64(&value, "/ceph_osd_processes"),
        osd_ids: ptr_str(&value, "/osd_ids"),
        cpu_percent: ptr_f64(&value, "/cpu_percent"),
        mem_percent: ptr_f64(&value, "/mem_percent"),
        error: None,
    })
}
