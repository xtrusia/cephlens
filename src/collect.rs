use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;

use crate::{
    config::ResolvedConfig,
    model::{ClusterSummary, NodeSummary, OsdSummary, Snapshot},
    ssh::ssh_capture,
    stream::NODE_FACTS_SNIPPET,
    util::{ptr_f64, ptr_i64, ptr_str, ptr_u64},
};

pub(crate) fn collect_snapshot(cfg: &ResolvedConfig) -> Result<Snapshot> {
    let status_out = ssh_capture(&cfg.admin_host, "sudo -n ceph -s --format json")?;
    let tree_out = ssh_capture(&cfg.admin_host, "sudo -n ceph osd tree --format json")?;
    let df_out = ssh_capture(&cfg.admin_host, "sudo -n ceph osd df --format json")?;

    let status: Value = serde_json::from_str(status_out.trim())
        .with_context(|| "failed to parse ceph status json")?;
    let tree: Value =
        serde_json::from_str(tree_out.trim()).with_context(|| "failed to parse osd tree json")?;
    let df: Value =
        serde_json::from_str(df_out.trim()).with_context(|| "failed to parse osd df json")?;

    let cluster = parse_cluster_summary(&status);
    let osds = parse_osds(&tree, &df);
    let nodes = cfg.hosts.iter().map(|host| collect_node(host)).collect();

    Ok(Snapshot {
        captured_at: Utc::now(),
        profile: cfg.profile.clone(),
        admin_host: cfg.admin_host.clone(),
        hosts: cfg.hosts.clone(),
        cluster,
        nodes,
        osds,
    })
}

pub(crate) fn parse_cluster_summary(status: &Value) -> ClusterSummary {
    let pg_states = status
        .pointer("/pgmap/pgs_by_state")
        .and_then(Value::as_array)
        .map(|states| {
            states
                .iter()
                .map(|state| {
                    format!(
                        "{} {}",
                        ptr_u64(state, "/count"),
                        ptr_str(state, "/state_name")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    ClusterSummary {
        fsid: ptr_str(status, "/fsid"),
        health: ptr_str(status, "/health/status"),
        quorum: status
            .pointer("/quorum_names")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        mon_count: ptr_u64(status, "/monmap/num_mons"),
        mgr_available: status
            .pointer("/mgrmap/available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        mgr_standbys: ptr_u64(status, "/mgrmap/num_standbys"),
        osds_total: ptr_u64(status, "/osdmap/num_osds"),
        osds_up: ptr_u64(status, "/osdmap/num_up_osds"),
        osds_in: ptr_u64(status, "/osdmap/num_in_osds"),
        pools: ptr_u64(status, "/pgmap/num_pools"),
        pgs: ptr_u64(status, "/pgmap/num_pgs"),
        objects: ptr_u64(status, "/pgmap/num_objects"),
        bytes_used: ptr_u64(status, "/pgmap/bytes_used"),
        bytes_total: ptr_u64(status, "/pgmap/bytes_total"),
        read_bytes_sec: ptr_u64(status, "/pgmap/read_bytes_sec"),
        write_bytes_sec: ptr_u64(status, "/pgmap/write_bytes_sec"),
        read_ops_sec: ptr_u64(status, "/pgmap/read_op_per_sec"),
        write_ops_sec: ptr_u64(status, "/pgmap/write_op_per_sec"),
        pg_states,
    }
}

pub(crate) fn parse_osds(tree: &Value, df: &Value) -> Vec<OsdSummary> {
    let mut host_by_osd = HashMap::new();
    let mut status_by_osd = HashMap::new();

    if let Some(nodes) = tree.pointer("/nodes").and_then(Value::as_array) {
        for node in nodes {
            if ptr_str(node, "/type") == "host" {
                let host = ptr_str(node, "/name");
                if let Some(children) = node.pointer("/children").and_then(Value::as_array) {
                    for child in children {
                        if let Some(id) = child.as_i64() {
                            host_by_osd.insert(id, host.clone());
                        }
                    }
                }
            } else if ptr_str(node, "/type") == "osd" {
                let id = ptr_i64(node, "/id");
                status_by_osd.insert(id, ptr_str(node, "/status"));
            }
        }
    }

    let mut osds = Vec::new();
    if let Some(nodes) = df.pointer("/nodes").and_then(Value::as_array) {
        for node in nodes {
            let id = ptr_i64(node, "/id");
            osds.push(OsdSummary {
                id,
                name: ptr_str(node, "/name"),
                host: host_by_osd.get(&id).cloned().unwrap_or_default(),
                status: status_by_osd
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| ptr_str(node, "/status")),
                reweight: ptr_f64(node, "/reweight"),
                utilization: ptr_f64(node, "/utilization"),
                pgs: ptr_u64(node, "/pgs"),
                used_kb: ptr_u64(node, "/kb_used"),
                avail_kb: ptr_u64(node, "/kb_avail"),
            });
        }
    }
    osds.sort_by_key(|osd| osd.id);
    osds
}

fn collect_node(host: &str) -> NodeSummary {
    let command = format!(
        r#"
{facts}
read _ user1 nice1 system1 idle1 iowait1 irq1 softirq1 steal1 _ _ < /proc/stat
sleep 0.2
read _ user2 nice2 system2 idle2 iowait2 irq2 softirq2 steal2 _ _ < /proc/stat
idle_a=$((idle1 + iowait1))
idle_b=$((idle2 + iowait2))
non_idle_a=$((user1 + nice1 + system1 + irq1 + softirq1 + steal1))
non_idle_b=$((user2 + nice2 + system2 + irq2 + softirq2 + steal2))
total_a=$((idle_a + non_idle_a))
total_b=$((idle_b + non_idle_b))
diff_total=$((total_b - total_a))
diff_idle=$((idle_b - idle_a))
cpu_pct=$(awk -v total="$diff_total" -v idle="$diff_idle" 'BEGIN {{if (total > 0) printf "%.1f", (total-idle)*100/total; else printf "0.0"}}')
printf 'hostname=%s\n' "$hostname"
printf 'sudo=%s\n' "$sudo_state"
printf 'microceph=%s\n' "$micro"
printf 'ceph_osd_processes=%s\n' "$count"
printf 'osd_ids=%s\n' "$ids"
printf 'cpu_percent=%s\n' "$cpu_pct"
printf 'mem_percent=%s\n' "$mem_pct"
"#,
        facts = NODE_FACTS_SNIPPET
    );
    match ssh_capture(host, &command) {
        Ok(output) => {
            let map = parse_key_values(&output);
            NodeSummary {
                host: host.to_owned(),
                hostname: map.get("hostname").cloned().unwrap_or_default(),
                sudo: map.get("sudo").cloned().unwrap_or_default(),
                microceph: map.get("microceph").cloned().unwrap_or_default(),
                ceph_osd_processes: map
                    .get("ceph_osd_processes")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                osd_ids: map.get("osd_ids").cloned().unwrap_or_default(),
                cpu_percent: map
                    .get("cpu_percent")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                mem_percent: map
                    .get("mem_percent")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                error: None,
            }
        }
        Err(err) => NodeSummary {
            host: host.to_owned(),
            error: Some(format!("{err:#}")),
            ..NodeSummary::default()
        },
    }
}

pub(crate) fn run_bench(host: &str, seconds: u64) -> Result<String> {
    let command = format!(
        r#"
set -e
pool=cephlens-test
sudo -n ceph osd pool create "$pool" 32 >/dev/null 2>&1 || true
sudo -n ceph osd pool application enable "$pool" rados >/dev/null 2>&1 || true
sudo -n rados -p "$pool" bench {seconds} write -b 4096 -t 4 --no-cleanup
sudo -n rados -p "$pool" cleanup >/dev/null 2>&1 || true
"#
    );
    ssh_capture(host, &command)
}

pub(crate) fn run_probe(hosts: &[String]) -> String {
    let mut output = String::new();
    for host in hosts {
        let command = r#"
printf '--- %s ---\n' "$(hostname)"
printf 'kernel='; uname -r
printf 'sudo='; if sudo -n true 2>/dev/null; then echo ok; else echo needs_password; fi
printf 'microceph='; snap list microceph 2>/dev/null | awk 'NR==2 {print $2" "$4" "$6; found=1} END {if (!found) print "missing"}'
printf 'ceph_osd='; pgrep -af '[c]eph-osd --cluster ceph' || true
printf 'osdtrace='
bin=$(command -v osdtrace 2>/dev/null || true)
if [ -z "$bin" ] && [ -x "$HOME/.cephlens/bin/osdtrace" ]; then
  bin="$HOME/.cephlens/bin/osdtrace"
fi
if [ -n "$bin" ]; then
  "$bin" --version 2>/dev/null | head -1 | awk -v bin="$bin" '{print bin" "$0; found=1} END {if (!found) print bin}'
else
  echo missing
fi
"#;
        match ssh_capture(host, command) {
            Ok(s) => {
                output.push_str(&format!("probe {host}: ok\n{s}\n"));
            }
            Err(err) => {
                output.push_str(&format!("probe {host}: {err:#}\n"));
            }
        }
    }
    output
}

fn parse_key_values(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}
