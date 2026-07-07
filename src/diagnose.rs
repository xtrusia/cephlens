use std::collections::HashMap;

use crate::{
    kfstrace::{KfsEvent, kfs_op_rows},
    model::{NodeSummary, Snapshot},
    radostrace::{RadosEvent, rados_pool_rows},
    trace::{TraceEvent, TraceGraphRow, dominant_component},
    util::short,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsightLevel {
    Ok,
    Info,
    Warn,
    Bad,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Insight {
    pub(crate) level: InsightLevel,
    pub(crate) text: String,
}

pub(crate) struct DiagnoseInput<'a> {
    pub(crate) snapshot: Option<&'a Snapshot>,
    pub(crate) admin_host: &'a str,
    pub(crate) node_summaries: &'a HashMap<String, NodeSummary>,
    pub(crate) stream_counts: Option<(usize, usize)>,
    pub(crate) trace_events: &'a [TraceEvent],
    pub(crate) trace_rows: &'a [TraceGraphRow],
    pub(crate) kfs_events: &'a [KfsEvent],
    pub(crate) rados_events: &'a [RadosEvent],
    pub(crate) idle_message: Option<&'a str>,
}

pub(crate) fn diagnose(input: DiagnoseInput<'_>) -> Vec<Insight> {
    let mut insights = Vec::new();

    if let Some(snapshot) = input.snapshot {
        if snapshot.cluster.health != "HEALTH_OK" {
            let level = if snapshot.cluster.health == "HEALTH_WARN" {
                InsightLevel::Warn
            } else {
                InsightLevel::Bad
            };
            insights.push(Insight {
                level,
                text: format!(
                    "cluster health {}; run ceph health detail on {}",
                    snapshot.cluster.health, input.admin_host
                ),
            });
        }
        if !snapshot.cluster.pg_states.contains("active+clean") {
            insights.push(Insight {
                level: InsightLevel::Warn,
                text: format!(
                    "PG state {}; trace latency may include recovery/peering",
                    snapshot.cluster.pg_states
                ),
            });
        }
    } else {
        insights.push(Insight {
            level: InsightLevel::Info,
            text: "waiting for first cluster snapshot".to_owned(),
        });
    }

    if let Some((live_streams, total_streams)) = input.stream_counts
        && total_streams > 0
        && live_streams < total_streams
    {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "ssh streams {live_streams}/{total_streams} live; check hosts marked retry/error"
            ),
        });
    }

    if let Some(error) = input
        .trace_events
        .iter()
        .rev()
        .find(|event| event.op == "error")
    {
        insights.push(Insight {
            level: InsightLevel::Bad,
            text: format!("trace error on {}: {}", error.host, short(&error.raw, 72)),
        });
    }

    let active_rows = input
        .trace_rows
        .iter()
        .filter(|row| row.ops > 0)
        .collect::<Vec<_>>();
    if !active_rows.is_empty() {
        insights.extend(osd_trace_insights(input.node_summaries, &active_rows));
    }
    insights.extend(kfs_insights(input.kfs_events));
    insights.extend(rados_insights(input.rados_events));
    if let Some(cross) = cross_source_insight(&active_rows, input.rados_events) {
        insights.push(cross);
    }
    if active_rows.is_empty() && input.kfs_events.is_empty() && input.rados_events.is_empty() {
        if let Some(message) = input.idle_message {
            insights.push(Insight {
                level: InsightLevel::Info,
                text: message.to_owned(),
            });
        }
    }

    insights
}

fn osd_trace_insights(
    node_summaries: &HashMap<String, NodeSummary>,
    active_rows: &[&TraceGraphRow],
) -> Vec<Insight> {
    let mut insights = Vec::new();
    let total_ops = active_rows.iter().map(|row| row.ops).sum::<u64>();
    let worst = active_rows
        .iter()
        .max_by(|left, right| {
            left.max_us
                .cmp(&right.max_us)
                .then_with(|| left.ops.cmp(&right.ops))
        })
        .copied()
        .expect("active_rows is not empty");
    let worst_level = insight_level_for_latency(worst.max_us);
    insights.push(Insight {
        level: worst_level,
        text: format!(
            "last 60s: {total_ops} ops on {} OSDs; worst {} max {} avg {}",
            active_rows.len(),
            worst.osd,
            format_latency_us(worst.max_us),
            format_latency_us(worst.avg_us)
        ),
    });

    let dominant = dominant_component(worst);
    if dominant.value_us > 0 {
        insights.push(Insight {
            level: insight_level_for_latency(dominant.value_us),
            text: format!(
                "dominant {} {}; suspect {}",
                dominant.name,
                format_latency_us(dominant.value_us),
                dominant.suspect
            ),
        });
    } else if worst.max_us >= 10_000 {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: "slow op seen, but parsed queue/store/network components are empty; inspect raw osdtrace"
                .to_owned(),
        });
    }

    if worst.hot_pg != "-" {
        insights.push(Insight {
            level: InsightLevel::Info,
            text: format!(
                "top PG on {}: {}; compare acting set if it stays hot",
                worst.osd, worst.hot_pg
            ),
        });
    }

    if let Some(node) = node_for_host(node_summaries, &worst.host) {
        if node.cpu_percent >= 85.0 {
            insights.push(Insight {
                level: InsightLevel::Bad,
                text: format!(
                    "{} CPU {}%; queue latency may include OSD worker/scheduler pressure",
                    worst.host,
                    format_percent(node.cpu_percent).trim()
                ),
            });
        } else if node.mem_percent >= 85.0 {
            insights.push(Insight {
                level: InsightLevel::Warn,
                text: format!(
                    "{} memory {}%; check OSD memory pressure before deeper trace",
                    worst.host,
                    format_percent(node.mem_percent).trim()
                ),
            });
        }
    }

    let slow_osds = active_rows
        .iter()
        .filter(|row| row.max_us >= 10_000)
        .count();
    if slow_osds >= 2 {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "{slow_osds} OSDs over 10ms; shared network/device/controller pressure is possible"
            ),
        });
    } else if worst.max_us < 10_000 {
        insights.push(Insight {
            level: InsightLevel::Ok,
            text: "no obvious slow OSD in the trace window; max latency is below 10ms".to_owned(),
        });
    }

    insights
}

fn kfs_insights(events: &[KfsEvent]) -> Vec<Insight> {
    let mut insights = Vec::new();
    let rows = kfs_op_rows(events);
    let Some(worst) = rows.iter().max_by_key(|row| row.max_us) else {
        return insights;
    };
    let total: u64 = rows.iter().map(|row| row.count).sum();
    insights.push(Insight {
        level: insight_level_for_latency(worst.max_us),
        text: format!(
            "kfstrace: {total} MDS ops; slowest {} max {} avg {}",
            worst.op,
            format_latency_us(worst.max_us),
            format_latency_us(worst.avg_us)
        ),
    });
    let unsafe_total: u64 = rows.iter().map(|row| row.unsafe_count).sum();
    if unsafe_total > 0 {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "kfstrace: {unsafe_total} unsafe metadata ops awaiting MDS journal commit"
            ),
        });
    }
    insights
}

fn rados_insights(events: &[RadosEvent]) -> Vec<Insight> {
    let mut insights = Vec::new();
    let rows = rados_pool_rows(events);
    let Some(worst) = rows.iter().max_by_key(|row| row.max_us) else {
        return insights;
    };
    let total: u64 = rows.iter().map(|row| row.count).sum();
    insights.push(Insight {
        level: insight_level_for_latency(worst.max_us),
        text: format!(
            "radostrace: {total} client ops; pool {} max {} avg {} ({}W/{}R)",
            worst.pool,
            format_latency_us(worst.max_us),
            format_latency_us(worst.avg_us),
            worst.writes,
            worst.reads
        ),
    });
    insights
}

fn cross_source_insight(
    osd_active: &[&TraceGraphRow],
    rados_events: &[RadosEvent],
) -> Option<Insight> {
    let rados = rados_pool_rows(rados_events);
    let client_max = rados.iter().map(|row| row.max_us).max().unwrap_or(0);
    let server_max = osd_active.iter().map(|row| row.max_us).max().unwrap_or(0);
    if client_max == 0 || server_max == 0 {
        return None;
    }
    let client = format_latency_us(client_max);
    let server = format_latency_us(server_max);
    if client_max > server_max.saturating_mul(2) {
        Some(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "cross: rados client {client} vs osd server {server}; gap = network/messenger/queue"
            ),
        })
    } else {
        Some(Insight {
            level: InsightLevel::Info,
            text: format!(
                "cross: rados client {client} vs osd server {server}; client tracks server"
            ),
        })
    }
}

pub(crate) fn insight_level_for_latency(latency_us: u64) -> InsightLevel {
    if latency_us >= 100_000 {
        InsightLevel::Bad
    } else if latency_us >= 10_000 {
        InsightLevel::Warn
    } else if latency_us > 0 {
        InsightLevel::Ok
    } else {
        InsightLevel::Info
    }
}

pub(crate) fn format_latency_us(value: u64) -> String {
    if value >= 1000 {
        format!("{:.1}ms", value as f64 / 1000.0)
    } else if value == 0 {
        "-".to_owned()
    } else {
        format!("{value}us")
    }
}

fn format_percent(value: f64) -> String {
    format!("{value:>4.1}")
}

fn node_for_host<'a>(
    node_summaries: &'a HashMap<String, NodeSummary>,
    host: &str,
) -> Option<&'a NodeSummary> {
    node_summaries.get(host).or_else(|| {
        node_summaries
            .values()
            .find(|node| node.host == host || node.hostname == host)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(osd: &str, host: &str, max_us: u64, queue_us: u64) -> TraceGraphRow {
        TraceGraphRow {
            osd: osd.to_owned(),
            host: host.to_owned(),
            ops: 2,
            avg_us: max_us / 2,
            max_us,
            throttle_max_us: 0,
            recv_max_us: 0,
            dispatch_max_us: 0,
            queue_max_us: queue_us,
            store_max_us: 0,
            kv_commit_max_us: 0,
            pg_count: 1,
            hot_pg: "1.a:2".to_owned(),
            points: Vec::new(),
        }
    }

    #[test]
    fn osd_insights_flag_dominant_queue_and_cpu_pressure() {
        let rows = vec![row("osd.1", "node-a", 25_000, 20_000)];
        let mut nodes = HashMap::new();
        nodes.insert(
            "node-a".to_owned(),
            NodeSummary {
                host: "node-a".to_owned(),
                cpu_percent: 90.0,
                ..NodeSummary::default()
            },
        );

        let insights = diagnose(DiagnoseInput {
            snapshot: None,
            admin_host: "admin",
            node_summaries: &nodes,
            stream_counts: None,
            trace_events: &[],
            trace_rows: &rows,
            kfs_events: &[],
            rados_events: &[],
            idle_message: None,
        });

        assert!(
            insights
                .iter()
                .any(|insight| insight.text.contains("dominant queue 20.0ms"))
        );
        assert!(
            insights
                .iter()
                .any(|insight| insight.text.contains("node-a CPU 90.0%"))
        );
    }
}
