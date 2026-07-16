use std::{collections::HashMap, process};

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;

use crate::{
    config::ResolvedConfig,
    model::{ClusterSummary, NodeSummary, OsdSummary, Snapshot},
    ssh::ssh_capture,
    stream::NODE_FACTS_SNIPPET,
    util::{ptr_f64, ptr_i64, ptr_str, ptr_u64, shell_quote},
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
printf 'ceph_version=%s\n' "$ceph_version"
printf 'deployment=%s\n' "$deployment"
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
                ceph_version: map.get("ceph_version").cloned().unwrap_or_default(),
                deployment: map.get("deployment").cloned().unwrap_or_default(),
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

pub(crate) fn run_bench(
    host: &str,
    seconds: u64,
    session: Option<&str>,
    keep_pool: bool,
) -> Result<String> {
    let session = session.map(ToOwned::to_owned).unwrap_or_else(|| {
        Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
            .to_string()
    });
    let pool = bench_pool_name(&session, process::id());
    ssh_capture(host, &bench_command(&pool, seconds, keep_pool))
}

fn bench_pool_name(session: &str, pid: u32) -> String {
    format!("cephlens-test-{session}-{pid}")
}

fn bench_command(pool: &str, seconds: u64, keep_pool: bool) -> String {
    let pool = shell_quote(pool);
    let keep_pool = if keep_pool { "yes" } else { "no" };
    format!(
        r#"
set -eu
pool={pool}
keep_pool={keep_pool}
pool_created=no
cleanup() {{
  code=$?
  trap - INT TERM HUP EXIT
  if [ "$pool_created" = yes ]; then
    if ! sudo -n rados -p "$pool" cleanup >/dev/null 2>&1; then
      echo "cephlens bench cleanup failed for pool $pool" >&2
      if [ "$code" -eq 0 ]; then code=1; fi
    fi
    if [ "$keep_pool" = no ] && ! sudo -n ceph osd pool delete "$pool" "$pool" --yes-i-really-really-mean-it >/dev/null 2>&1; then
      echo "cephlens bench pool deletion failed for $pool" >&2
      if [ "$code" -eq 0 ]; then code=1; fi
    fi
  fi
  exit "$code"
}}
trap cleanup INT TERM HUP EXIT
sudo -n ceph osd pool create "$pool" 32 >/dev/null
pool_created=yes
sudo -n ceph osd pool application enable "$pool" rados >/dev/null
echo "cephlens bench pool=$pool"
sudo -n rados -p "$pool" bench {seconds} write -b 4096 -t 4 --no-cleanup
"#
    )
}

pub(crate) fn run_probe(hosts: &[String]) -> String {
    let mut output = String::new();
    for host in hosts {
        let command = r#"
printf '--- %s ---\n' "$(hostname)"
printf 'kernel='; uname -r
printf 'sudo='; if sudo -n true 2>/dev/null; then echo ok; else echo needs_password; fi
printf 'ceph_version='
ceph_version=$(ceph --version 2>/dev/null | head -1)
if [ -n "$ceph_version" ]; then echo "$ceph_version"; else echo missing; fi
printf 'deployment='
micro=$(snap list microceph 2>/dev/null | awk 'NR==2 {print $2" "$4" "$6; found=1}')
if [ -n "$micro" ]; then
  echo "microceph $micro"
elif command -v cephadm >/dev/null 2>&1; then
  echo cephadm
elif [ -d /var/lib/rook ]; then
  echo rook
else
  echo generic
fi
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_command_cleans_up_unique_pool_on_exit() {
        let pool = bench_pool_name("20260716-120000", 42);
        let command = bench_command(&pool, 5, false);

        assert_eq!(pool, "cephlens-test-20260716-120000-42");
        assert!(command.contains("pool='cephlens-test-20260716-120000-42'"));
        assert!(command.contains("trap cleanup INT TERM HUP EXIT"));
        assert!(command.contains("cephlens bench pool=$pool"));
        assert!(command.contains("rados -p \"$pool\" cleanup"));
        assert!(
            command
                .contains("ceph osd pool delete \"$pool\" \"$pool\" --yes-i-really-really-mean-it")
        );
    }

    #[test]
    fn bench_command_can_retain_pool() {
        let command = bench_command("cephlens-test-20260716-120000-42", 5, true);

        assert!(command.contains("keep_pool=yes"));
    }
}
