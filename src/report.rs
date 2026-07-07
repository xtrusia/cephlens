use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::{
    diagnose::{DiagnoseInput, InsightLevel, diagnose, format_latency_us},
    kfstrace::{KfsEvent, kfs_op_rows, parse_kfs_event},
    model::{NodeSummary, Snapshot},
    radostrace::{RadosEvent, parse_rados_event, rados_pool_rows},
    session::{TRACE_KFS_LOG, TRACE_OSD_LOG, TRACE_RADOS_LOG, load_snapshots},
    trace::{
        TRACE_BUCKET_SECS, TraceBucket, TraceEvent, parse_trace_event, record_trace_event_at,
        trace_graph_rows_at,
    },
};

#[derive(Default)]
struct TraceLogs {
    osd_events: Vec<TraceEvent>,
    osd_series: HashMap<String, VecDeque<TraceBucket>>,
    osd_now_bucket: Option<i64>,
    kfs_events: Vec<KfsEvent>,
    rados_events: Vec<RadosEvent>,
}

pub(crate) fn build_report(path: &Path) -> Result<String> {
    let snapshots = load_snapshots(path)?;
    let first = snapshots
        .first()
        .expect("load_snapshots returns at least one snapshot");
    let last = snapshots
        .last()
        .expect("load_snapshots returns at least one snapshot");
    let logs = load_trace_logs(trace_log_dir(path).as_deref())?;
    let node_summaries = node_summary_map(last);
    let now_bucket = logs
        .osd_now_bucket
        .unwrap_or_else(|| Utc::now().timestamp() / TRACE_BUCKET_SECS);
    let osd_rows = trace_graph_rows_at(
        Some(last),
        &logs.osd_events,
        &logs.osd_series,
        usize::MAX,
        now_bucket,
    );
    let insights = diagnose(DiagnoseInput {
        snapshot: Some(last),
        admin_host: &last.admin_host,
        node_summaries: &node_summaries,
        stream_counts: None,
        trace_events: &logs.osd_events,
        trace_rows: &osd_rows,
        kfs_events: &logs.kfs_events,
        rados_events: &logs.rados_events,
        idle_message: Some("no trace data recorded in session"),
    });

    let mut out = String::new();
    push_line(&mut out, "# cephlens report");
    push_line(&mut out, "");
    push_line(&mut out, &format!("Session: `{}`", path.display()));
    push_line(&mut out, &format!("Snapshots: {}", snapshots.len()));
    push_line(
        &mut out,
        &format!(
            "Captured: `{}` -> `{}`",
            first.captured_at, last.captured_at
        ),
    );
    push_line(&mut out, &format!("Profile: `{}`", last.profile));
    push_line(&mut out, &format!("Admin host: `{}`", last.admin_host));
    push_line(&mut out, &format!("Hosts: {}", last.hosts.join(", ")));
    push_line(&mut out, "");

    push_cluster(&mut out, last);
    push_nodes(&mut out, &last.nodes);
    push_insights(&mut out, &insights);
    push_osd_trace(&mut out, &osd_rows);
    push_kfs_trace(&mut out, &logs.kfs_events);
    push_rados_trace(&mut out, &logs.rados_events);

    Ok(out)
}

fn load_trace_logs(dir: Option<&Path>) -> Result<TraceLogs> {
    let Some(dir) = dir else {
        return Ok(TraceLogs::default());
    };
    let mut logs = TraceLogs::default();
    load_trace_payloads(&dir.join(TRACE_OSD_LOG), |stamp, host, payload| {
        if let Some(event) = parse_trace_event(host, payload) {
            if let Some(bucket) = trace_bucket(stamp) {
                record_trace_event_at(&mut logs.osd_series, &event, bucket);
                logs.osd_now_bucket =
                    Some(logs.osd_now_bucket.map_or(bucket, |last| last.max(bucket)));
            }
            logs.osd_events.push(event);
        }
    })?;
    load_trace_payloads(&dir.join(TRACE_KFS_LOG), |_, _, payload| {
        if let Some(event) = parse_kfs_event(payload) {
            logs.kfs_events.push(event);
        }
    })?;
    load_trace_payloads(&dir.join(TRACE_RADOS_LOG), |_, _, payload| {
        if let Some(event) = parse_rados_event(payload) {
            logs.rados_events.push(event);
        }
    })?;
    Ok(logs)
}

fn load_trace_payloads(path: &Path, mut handle: impl FnMut(&str, &str, &str)) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        let Some((stamp, rest)) = line.split_once('\t') else {
            continue;
        };
        let Some((host, payload)) = rest.split_once('\t') else {
            continue;
        };
        handle(stamp, host, payload);
    }
    Ok(())
}

fn trace_bucket(stamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|time| time.timestamp() / TRACE_BUCKET_SECS)
}

fn trace_log_dir(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        Some(path.to_path_buf())
    } else {
        path.parent().map(Path::to_path_buf)
    }
}

fn node_summary_map(snapshot: &Snapshot) -> HashMap<String, NodeSummary> {
    snapshot
        .nodes
        .iter()
        .map(|node| (node.host.clone(), node.clone()))
        .collect()
}

fn push_cluster(out: &mut String, snapshot: &Snapshot) {
    let cluster = &snapshot.cluster;
    push_line(out, "## cluster");
    push_line(out, "");
    push_line(out, &format!("- Health: `{}`", cluster.health));
    push_line(
        out,
        &format!(
            "- OSDs: `{}` total, `{}` up, `{}` in",
            cluster.osds_total, cluster.osds_up, cluster.osds_in
        ),
    );
    push_line(out, &format!("- PGs: `{}`", cluster.pg_states));
    push_line(
        out,
        &format!(
            "- IO: `{}` read ops/s, `{}` write ops/s",
            cluster.read_ops_sec, cluster.write_ops_sec
        ),
    );
    push_line(out, "");
}

fn push_nodes(out: &mut String, nodes: &[NodeSummary]) {
    push_line(out, "## nodes");
    push_line(out, "");
    if nodes.is_empty() {
        push_line(out, "No node readiness data was recorded.");
        push_line(out, "");
        return;
    }
    push_line(
        out,
        "| Host | Hostname | Sudo | OSDs | CPU | Mem | Deployment | Ceph version | Error |",
    );
    push_line(
        out,
        "| --- | --- | --- | --- | ---: | ---: | --- | --- | --- |",
    );
    for node in nodes {
        push_line(
            out,
            &format!(
                "| {} | {} | {} | {} | {:.1}% | {:.1}% | {} | {} | {} |",
                md_cell(&node.host),
                md_cell(value_or_dash(&node.hostname)),
                md_cell(value_or_dash(&node.sudo)),
                md_cell(value_or_dash(&node.osd_ids)),
                node.cpu_percent,
                node.mem_percent,
                md_cell(value_or_dash(&node.deployment)),
                md_cell(value_or_dash(&node.ceph_version)),
                md_cell(node.error.as_deref().unwrap_or("-"))
            ),
        );
    }
    push_line(out, "");
}

fn push_insights(out: &mut String, insights: &[crate::diagnose::Insight]) {
    push_line(out, "## insights");
    push_line(out, "");
    if insights.is_empty() {
        push_line(out, "- info no findings from the recorded data");
    } else {
        for insight in insights {
            push_line(
                out,
                &format!("- {} {}", insight_label(insight.level), insight.text),
            );
        }
    }
    push_line(out, "");
}

fn push_osd_trace(out: &mut String, rows: &[crate::trace::TraceGraphRow]) {
    push_line(out, "## osd trace");
    push_line(out, "");
    let active_rows = rows.iter().filter(|row| row.ops > 0).collect::<Vec<_>>();
    if active_rows.is_empty() {
        push_line(
            out,
            "No osdtrace events were recorded in the final trace window.",
        );
        push_line(out, "");
        return;
    }
    push_line(
        out,
        "| OSD | Host | Ops | Avg | Max | Queue | Store | KV commit | Hot PG |",
    );
    push_line(
        out,
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    );
    for row in active_rows.into_iter().take(20) {
        push_line(
            out,
            &format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                md_cell(&row.osd),
                md_cell(&row.host),
                row.ops,
                format_latency_us(row.avg_us),
                format_latency_us(row.max_us),
                format_latency_us(row.queue_max_us),
                format_latency_us(row.store_max_us),
                format_latency_us(row.kv_commit_max_us),
                md_cell(&row.hot_pg)
            ),
        );
    }
    push_line(out, "");
}

fn push_kfs_trace(out: &mut String, events: &[KfsEvent]) {
    push_line(out, "## kfs trace");
    push_line(out, "");
    let rows = kfs_op_rows(events);
    if rows.is_empty() {
        push_line(out, "No kfstrace events were recorded.");
        push_line(out, "");
        return;
    }
    push_line(out, "| Op | Count | Avg | Max | Unsafe |");
    push_line(out, "| --- | ---: | ---: | ---: | ---: |");
    for row in rows.into_iter().take(20) {
        push_line(
            out,
            &format!(
                "| {} | {} | {} | {} | {} |",
                md_cell(&row.op),
                row.count,
                format_latency_us(row.avg_us),
                format_latency_us(row.max_us),
                row.unsafe_count
            ),
        );
    }
    push_line(out, "");
}

fn push_rados_trace(out: &mut String, events: &[RadosEvent]) {
    push_line(out, "## rados trace");
    push_line(out, "");
    let rows = rados_pool_rows(events);
    if rows.is_empty() {
        push_line(out, "No radostrace events were recorded.");
        push_line(out, "");
        return;
    }
    push_line(out, "| Pool | Count | Avg | Max | Writes | Reads |");
    push_line(out, "| --- | ---: | ---: | ---: | ---: | ---: |");
    for row in rows.into_iter().take(20) {
        push_line(
            out,
            &format!(
                "| {} | {} | {} | {} | {} | {} |",
                md_cell(&row.pool),
                row.count,
                format_latency_us(row.avg_us),
                format_latency_us(row.max_us),
                row.writes,
                row.reads
            ),
        );
    }
    push_line(out, "");
}

fn insight_label(level: InsightLevel) -> &'static str {
    match level {
        InsightLevel::Ok => "ok",
        InsightLevel::Info => "info",
        InsightLevel::Warn => "warn",
        InsightLevel::Bad => "bad",
    }
}

fn md_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn value_or_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::{
        model::{ClusterSummary, OsdSummary},
        session::{append_snapshot, append_trace_line, session_snapshot_path},
    };

    fn temp_session_dir() -> PathBuf {
        let id = Utc::now()
            .timestamp_nanos_opt()
            .expect("test timestamp should fit");
        std::env::temp_dir().join(format!("cephlens-report-test-{id}"))
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            captured_at: Utc::now(),
            profile: "test".to_owned(),
            admin_host: "admin".to_owned(),
            hosts: vec!["node-a".to_owned()],
            cluster: ClusterSummary {
                health: "HEALTH_OK".to_owned(),
                osds_total: 1,
                osds_up: 1,
                osds_in: 1,
                pg_states: "1 active+clean".to_owned(),
                ..ClusterSummary::default()
            },
            nodes: vec![NodeSummary {
                host: "node-a".to_owned(),
                hostname: "node-a".to_owned(),
                sudo: "ok".to_owned(),
                ceph_version: "ceph version test".to_owned(),
                deployment: "generic".to_owned(),
                cpu_percent: 90.0,
                ..NodeSummary::default()
            }],
            osds: vec![OsdSummary {
                id: 1,
                name: "osd.1".to_owned(),
                host: "node-a".to_owned(),
                ..OsdSummary::default()
            }],
        }
    }

    #[test]
    fn report_uses_recorded_trace_logs() {
        let dir = temp_session_dir();
        fs::create_dir_all(&dir).unwrap();
        append_snapshot(&session_snapshot_path(&dir), &snapshot()).unwrap();
        append_trace_line(
            &dir,
            TRACE_OSD_LOG,
            "node-a",
            "123 op_w osd 1 pg 1.a queue_lat 20000 op_lat 25000",
        )
        .unwrap();

        let report = build_report(&dir).unwrap();

        assert!(report.contains("dominant queue 20.0ms"));
        assert!(report.contains(
            "| node-a | node-a | ok | - | 90.0% | 0.0% | generic | ceph version test | - |"
        ));
        assert!(report.contains("| osd.1 | node-a | 1 | 25.0ms | 25.0ms |"));
        let _ = fs::remove_dir_all(dir);
    }
}
