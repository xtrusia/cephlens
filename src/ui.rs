use std::collections::HashMap;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};

use crate::model::NodeSummary;
use crate::trace::{
    TraceGraphRow, dominant_component, trace_graph_rows as build_trace_graph_rows,
    trace_platform_label,
};
use crate::util::{clamp_bottom_scroll, clamp_top_scroll, short};
use crate::{
    ACCENT, App, BAD, BLUE, EVENT_LOG_MAX_HEIGHT, EVENT_LOG_MIN_HEIGHT, Insight, InsightLevel,
    MUTED, Mode, OK, PanelFocus, StreamState, StreamStatus, TEXT, WARN, format_bytes,
    format_compact_bytes, format_kb, live_streams_active,
};
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let log_height = event_log_height_for(area, app.event_log_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(log_height),
        ])
        .split(area);

    draw_header(frame, app, chunks[0]);
    draw_body(frame, app, chunks[1]);
    draw_logs(frame, app, chunks[2]);
    if app.confirm_quit {
        draw_quit_confirm(frame, app, area);
    }
}

fn event_log_height_for(area: Rect, preferred: u16) -> u16 {
    let terminal_limit = area
        .height
        .saturating_sub(9)
        .clamp(EVENT_LOG_MIN_HEIGHT, EVENT_LOG_MAX_HEIGHT);
    preferred
        .clamp(EVENT_LOG_MIN_HEIGHT, EVENT_LOG_MAX_HEIGHT)
        .min(terminal_limit)
}

fn draw_quit_confirm(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let trace_note = if app.trace_following || app.trace_active > 0 {
        "Trace runner will be stopped and cleaned up."
    } else {
        "SSH streams will be closed."
    };
    let modal = centered_rect(52, 7, area);
    let lines = vec![
        Line::styled("Quit cephlens?", Style::default().fg(WARN).bold()),
        Line::raw(""),
        Line::styled(trace_note, Style::default().fg(TEXT)),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Enter/y", Style::default().fg(WARN).bold()),
            Span::raw(" quit    "),
            Span::styled("Esc/n/q", Style::default().fg(WARN).bold()),
            Span::raw(" cancel"),
        ]),
    ];
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(panel(" confirm ")),
        modal,
    );
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2).max(1));
    let height = height.min(area.height.saturating_sub(2).max(1));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mode = match &app.mode {
        Mode::Live => "LIVE",
        Mode::Config => "CONFIG",
        Mode::Trace => "TRACE",
        Mode::Replay { index, snapshots } => {
            return frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" cephlens ", Style::default().fg(ACCENT).bold()),
                    Span::styled("REPLAY ", Style::default().fg(BLUE).bold()),
                    Span::styled(
                        format!("snapshot {}/{}", index + 1, snapshots.len()),
                        Style::default().fg(WARN),
                    ),
                    Span::styled("  left/right move  q quit", Style::default().fg(MUTED)),
                ]))
                .block(panel(" replay deck ")),
                area,
            );
        }
    };

    let session = app
        .session_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let status = if live_streams_active(app) {
        let (live, total) = stream_counts(app);
        if total == 0 {
            "starting".to_owned()
        } else if live == total {
            format!("streaming {live}/{total}")
        } else {
            format!("reconnecting {live}/{total}")
        }
    } else if app.collecting {
        "collecting".to_owned()
    } else {
        "idle".to_owned()
    };
    let health = app
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.cluster.health.as_str())
        .unwrap_or("warming up");
    let osd = app
        .snapshot
        .as_ref()
        .map(|snapshot| {
            format!(
                "{}/{} up/in",
                snapshot.cluster.osds_up, snapshot.cluster.osds_in
            )
        })
        .unwrap_or_else(|| "-/- up/in".to_owned());
    let io = app
        .snapshot
        .as_ref()
        .map(|snapshot| {
            format!(
                "rd {} {}/s  wr {} {}/s",
                snapshot.cluster.read_ops_sec,
                format_bytes(snapshot.cluster.read_bytes_sec),
                snapshot.cluster.write_ops_sec,
                format_bytes(snapshot.cluster.write_bytes_sec)
            )
        })
        .unwrap_or_else(|| "rd 0 0 B/s  wr 0 0 B/s".to_owned());
    let mut spans = vec![
        Span::styled(" cephlens ", Style::default().fg(ACCENT).bold()),
        Span::styled(mode, Style::default().fg(BLUE).bold()),
        Span::raw("  "),
        pill(health, health_color(health)),
        Span::styled(format!("  osd {osd}"), Style::default().fg(TEXT)),
        Span::styled(format!("  {io}"), Style::default().fg(TEXT)),
    ];
    if area.width >= 110 {
        spans.extend([
            Span::styled("  profile=", Style::default().fg(MUTED)),
            Span::styled(app.profile.clone(), Style::default().fg(OK)),
            Span::styled("  admin=", Style::default().fg(MUTED)),
            Span::styled(app.admin_host.clone(), Style::default().fg(OK)),
        ]);
    }
    if area.width >= 132 {
        spans.extend([
            Span::styled("  status=", Style::default().fg(MUTED)),
            Span::styled(status, Style::default().fg(WARN)),
        ]);
    }
    if area.width >= 168 && !session.is_empty() {
        spans.extend([
            Span::styled("  session=", Style::default().fg(MUTED)),
            Span::styled(short(&session, 20), Style::default().fg(MUTED)),
        ]);
    }
    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line).block(panel(" cephlens ")), area);
}

fn draw_body(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if matches!(app.mode, Mode::Config) {
        draw_config(frame, app, area);
        return;
    }
    if matches!(app.mode, Mode::Trace) {
        draw_trace(frame, app, area);
        return;
    }

    draw_live_body(frame, app, area);
}

fn draw_live_body(frame: &mut Frame<'_>, app: &App, area: Rect) {
    draw_dashboard(frame, app, area);
}

fn draw_dashboard(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.height < 12 {
        draw_overview(frame, app, area);
        return;
    }

    let show_insights = area.height >= 15;
    let top_height = if show_insights {
        if area.height >= 30 {
            11
        } else if area.height >= 22 {
            8
        } else {
            5
        }
    } else if area.height >= 17 {
        8
    } else {
        5
    };
    let trace_min_height = if show_insights {
        if area.height >= 30 {
            7
        } else if area.height >= 22 {
            6
        } else {
            4
        }
    } else if area.height >= 17 {
        7
    } else {
        4
    };
    if show_insights {
        let insight_height = if area.height >= 22 { 5 } else { 3 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(top_height),
                Constraint::Length(3),
                Constraint::Length(insight_height),
                Constraint::Min(trace_min_height),
            ])
            .split(area);

        draw_overview(frame, app, chunks[0]);
        draw_command_bar(frame, app, chunks[1]);
        draw_insights(frame, app, chunks[2]);
        draw_trace_events(frame, app, chunks[3]);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(top_height),
                Constraint::Length(3),
                Constraint::Min(trace_min_height),
            ])
            .split(area);

        draw_overview(frame, app, chunks[0]);
        draw_command_bar(frame, app, chunks[1]);
        draw_trace_events(frame, app, chunks[2]);
    }
}

fn draw_overview(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.width >= 142 && area.height >= 8 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(34),
                Constraint::Length(32),
                Constraint::Min(60),
            ])
            .split(area);
        draw_cluster(frame, app, chunks[0]);
        draw_nodes(frame, app, chunks[1]);
        draw_osds(frame, app, chunks[2]);
    } else if area.width >= 82 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(area);
        draw_cluster(frame, app, chunks[0]);
        draw_osds(frame, app, chunks[1]);
    } else if area.height >= 12 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(7), Constraint::Min(5)])
            .split(area);
        draw_cluster(frame, app, chunks[0]);
        draw_osds(frame, app, chunks[1]);
    } else {
        draw_osds(frame, app, area);
    }
}

fn draw_trace(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let show_insights = area.height >= 16;
    let outer = if show_insights {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(5),
                Constraint::Min(6),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(6)])
            .split(area)
    };

    draw_command_bar(frame, app, outer[0]);
    let body_area = if show_insights {
        draw_insights(frame, app, outer[1]);
        outer[2]
    } else {
        outer[1]
    };

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(body_area);

    let target_rows = app
        .trace_targets
        .iter()
        .map(|target| {
            let state = if target.installed && target.error.is_none() {
                "ready"
            } else if target.installed {
                "warn"
            } else {
                "missing"
            };
            let color = if target.installed && target.error.is_none() {
                OK
            } else if target.installed {
                WARN
            } else {
                BAD
            };
            let detail = target.error.clone().unwrap_or_else(|| {
                format!(
                    "osd {} trace {} {}",
                    target.osds, target.traceable, target.version
                )
            });
            Row::new(vec![
                Cell::from(short(&target.host, 9)).style(Style::default().fg(ACCENT).bold()),
                Cell::from(state).style(Style::default().fg(color).bold()),
                Cell::from(short(&trace_platform_label(target), 13))
                    .style(Style::default().fg(BLUE)),
                Cell::from(short(&detail, 22)).style(Style::default().fg(MUTED)),
            ])
        })
        .collect::<Vec<_>>();
    let target_visible = table_visible_rows(chunks[0]);
    let target_scroll = clamp_top_scroll(app.targets_scroll, target_rows.len(), target_visible);
    let target_total = target_rows.len();

    frame.render_widget(
        Table::new(
            target_rows
                .into_iter()
                .skip(target_scroll)
                .take(target_visible),
            [
                Constraint::Length(9),
                Constraint::Length(6),
                Constraint::Length(13),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new(["Host", "Tool", "Platform", "Detail"])
                .style(Style::default().fg(MUTED).bold()),
        )
        .block(scroll_panel(
            app,
            PanelFocus::Targets,
            "osdtrace targets",
            target_total,
            target_visible,
            target_scroll,
            false,
        )),
        chunks[0],
    );

    draw_trace_events(frame, app, chunks[1]);
}

fn draw_command_bar(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let commands = command_help(app, area.width);
    frame.render_widget(
        Paragraph::new(Line::from(commands))
            .style(Style::default().fg(TEXT))
            .block(panel(" commands ")),
        area,
    );
}

fn command_help(app: &App, width: u16) -> Vec<Span<'static>> {
    let compact = width < 96;
    let raw = match app.mode {
        Mode::Live if compact => vec![
            ("r", "ref"),
            ("c", "cfg"),
            ("p", "probe"),
            ("i", "inst"),
            ("t", "trace"),
            ("0", "all"),
            ("s", "stop"),
            ("x", "clr"),
            ("[ ]", "log"),
            ("Tab", "pan"),
            ("Up/Dn", "scr"),
            ("q", "quit"),
        ],
        Mode::Live => vec![
            ("r", "refresh"),
            ("c", "config"),
            ("p", "probe"),
            ("i", "install"),
            ("t", "trace"),
            ("0", "all"),
            ("s", "stop"),
            ("x", "clear"),
            ("[ ]", "log"),
            ("Tab", "panel"),
            ("Up/Dn", "scroll"),
            ("q", "quit"),
        ],
        Mode::Trace if compact => vec![
            ("p", "probe"),
            ("i", "inst"),
            ("r", "trace"),
            ("0", "all"),
            ("s", "stop"),
            ("x", "clr"),
            ("Tab", "pan"),
            ("Up/Dn", "scr"),
            ("[ ]", "log"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        Mode::Trace => vec![
            ("p", "probe"),
            ("i", "install"),
            ("r", "trace"),
            ("0", "all"),
            ("s", "stop"),
            ("x", "clear"),
            ("Tab", "panel"),
            ("Up/Dn", "scroll"),
            ("[ ]", "log"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        Mode::Config if app.config_editor.input.is_some() => {
            vec![("Enter", "save"), ("Ctrl+U", "clear"), ("Esc", "cancel")]
        }
        Mode::Config if compact => vec![
            ("Up/Dn", "sel"),
            ("a", "add"),
            ("e", "edit"),
            ("d", "delete"),
            ("s", "save"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        Mode::Config => vec![
            ("Up/Dn", "select"),
            ("a", "add"),
            ("e", "edit"),
            ("d", "delete"),
            ("s", "save"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        Mode::Replay { .. } => vec![("Left/Right", "replay"), ("q", "quit")],
    };

    let mut spans = Vec::new();
    for (index, (key, label_text)) in raw.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(if compact { "  " } else { "   " }));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(WARN).bold(),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            (*label_text).to_owned(),
            Style::default().fg(TEXT),
        ));
    }
    spans
}

fn draw_insights(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let visible = area.height.saturating_sub(2).max(1) as usize;
    let lines = operator_insights(app)
        .into_iter()
        .take(visible)
        .map(insight_line)
        .collect::<Vec<_>>();

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(panel(" insights "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn operator_insights(app: &App) -> Vec<Insight> {
    let mut insights = Vec::new();

    if let Some(snapshot) = &app.snapshot {
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
                    snapshot.cluster.health, app.admin_host
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

    let (live_streams, total_streams) = stream_counts(app);
    if total_streams > 0 && live_streams < total_streams {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "ssh streams {live_streams}/{total_streams} live; check hosts marked retry/error"
            ),
        });
    }

    if let Some(error) = app
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

    let rows = trace_graph_rows(app, usize::MAX);
    let active_rows = rows.iter().filter(|row| row.ops > 0).collect::<Vec<_>>();
    let trace_active = app.trace_following || app.trace_active > 0;

    if active_rows.is_empty() {
        insights.push(Insight {
            level: InsightLevel::Info,
            text: if trace_active {
                "trace listening; no OSD ops observed yet. Generate Ceph IO or press 0 for all ops"
                    .to_owned()
            } else {
                "trace idle; press t for >=1ms ops or 0 for all observed ops".to_owned()
            },
        });
        return insights;
    }

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

    if let Some(node) = node_for_host(app, &worst.host) {
        if node.cpu_percent >= 85.0 {
            insights.push(Insight {
                level: InsightLevel::Bad,
                text: format!(
                    "{} CPU {}%; queue latency may include OSD worker/scheduler pressure",
                    worst.host,
                    percent_label(node.cpu_percent).trim()
                ),
            });
        } else if node.mem_percent >= 85.0 {
            insights.push(Insight {
                level: InsightLevel::Warn,
                text: format!(
                    "{} memory {}%; check OSD memory pressure before deeper trace",
                    worst.host,
                    percent_label(node.mem_percent).trim()
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

fn insight_level_for_latency(latency_us: u64) -> InsightLevel {
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

fn node_for_host<'a>(app: &'a App, host: &str) -> Option<&'a NodeSummary> {
    app.node_summaries.get(host).or_else(|| {
        app.node_summaries
            .values()
            .find(|node| node.host == host || node.hostname == host)
    })
}

fn insight_line(insight: Insight) -> Line<'static> {
    let (label_text, color) = match insight.level {
        InsightLevel::Ok => ("ok", OK),
        InsightLevel::Info => ("info", BLUE),
        InsightLevel::Warn => ("warn", WARN),
        InsightLevel::Bad => ("bad", BAD),
    };
    Line::from(vec![
        Span::styled(
            format!("{label_text:<5}"),
            Style::default().fg(color).bold(),
        ),
        Span::styled(insight.text, Style::default().fg(TEXT)),
    ])
}

fn draw_trace_events(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let visible = table_visible_rows(area);
    let compact = area.width < 104;
    let trace_active = app.trace_following || app.trace_active > 0;
    let graph_rows = trace_graph_rows(app, usize::MAX);
    let graph_total = graph_rows.len();
    let trace_scroll = clamp_top_scroll(app.trace_scroll, graph_total, visible);
    let rows: Vec<Row<'static>> = if graph_rows.is_empty() {
        let hint = if app.trace_active > 0 {
            "waiting for snapshot and matching OSD ops"
        } else {
            "press t or 0, then generate Ceph IO"
        };
        if compact {
            vec![Row::new(vec![
                Cell::from("-"),
                Cell::from("0").style(Style::default().fg(MUTED)),
                Cell::from("-"),
                Cell::from("0").style(Style::default().fg(MUTED)),
                Cell::from("-"),
                Cell::from(hint).style(Style::default().fg(MUTED)),
            ])]
        } else {
            vec![Row::new(vec![
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("0").style(Style::default().fg(MUTED)),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("0").style(Style::default().fg(MUTED)),
                Cell::from("-"),
                Cell::from(hint).style(Style::default().fg(MUTED)),
            ])]
        }
    } else {
        graph_rows
            .iter()
            .skip(trace_scroll)
            .take(visible)
            .map(|row| {
                let max_color = latency_color(row.max_us);
                let graph_width = if compact {
                    area.width.saturating_sub(46) as usize
                } else {
                    area.width.saturating_sub(92) as usize
                }
                .max(12);
                let graph = trace_sparkline(&row.points, graph_width, trace_active);
                if compact {
                    Row::new(vec![
                        Cell::from(row.osd.clone()).style(Style::default().fg(ACCENT).bold()),
                        Cell::from(row.ops.to_string()).style(trace_ops_style(row.ops)),
                        Cell::from(format_latency_us(row.max_us))
                            .style(Style::default().fg(max_color)),
                        Cell::from(row.pg_count.to_string()).style(trace_ops_style(row.ops)),
                        Cell::from(short(&row.hot_pg, 15)).style(Style::default().fg(BLUE)),
                        Cell::from(graph).style(Style::default().fg(max_color)),
                    ])
                } else {
                    Row::new(vec![
                        Cell::from(row.osd.clone()).style(Style::default().fg(ACCENT).bold()),
                        Cell::from(short(&row.host, 12)).style(Style::default().fg(TEXT)),
                        Cell::from(row.ops.to_string()).style(trace_ops_style(row.ops)),
                        Cell::from(format_latency_us(row.avg_us)),
                        Cell::from(format_latency_us(row.max_us))
                            .style(Style::default().fg(max_color)),
                        Cell::from(format_latency_us(row.queue_max_us)),
                        Cell::from(format_latency_us(row.store_max_us)),
                        Cell::from(row.pg_count.to_string()).style(trace_ops_style(row.ops)),
                        Cell::from(short(&row.hot_pg, 21)).style(Style::default().fg(BLUE)),
                        Cell::from(graph).style(Style::default().fg(max_color)),
                    ])
                }
            })
            .collect()
    };

    let (widths, header) = if compact {
        (
            vec![
                Constraint::Length(7),
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(5),
                Constraint::Length(15),
                Constraint::Min(12),
            ],
            Row::new(["OSD", "Ops", "Max", "PGs", "Top PG", "Max/2s"]),
        )
    } else {
        (
            vec![
                Constraint::Length(7),
                Constraint::Length(10),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(5),
                Constraint::Length(21),
                Constraint::Min(16),
            ],
            Row::new([
                "OSD", "Host", "Ops", "Avg", "Max", "Queue", "Store", "PGs", "Top PG", "Max/2s",
            ]),
        )
    };

    frame.render_widget(
        Table::new(rows, widths)
            .header(header.style(Style::default().fg(MUTED).bold()))
            .block(scroll_panel(
                app,
                PanelFocus::Trace,
                trace_panel_title(app),
                graph_total,
                visible,
                trace_scroll,
                false,
            )),
        area,
    );
}

fn trace_panel_title(app: &App) -> &'static str {
    if app.trace_following {
        "trace graph: following"
    } else if app.trace_active > 0 {
        "trace graph: running"
    } else {
        "trace graph"
    }
}

fn trace_graph_rows(app: &App, limit: usize) -> Vec<TraceGraphRow> {
    build_trace_graph_rows(
        app.snapshot.as_ref(),
        &app.trace_events,
        &app.trace_series,
        limit,
    )
}

fn trace_sparkline(points: &[u64], width: usize, active: bool) -> String {
    let width = width.clamp(8, 96);
    if points.is_empty() {
        return trace_idle_line(width, active);
    }

    let mut samples = Vec::with_capacity(width);
    for column in 0..width {
        let start = column * points.len() / width;
        let mut end = (column + 1) * points.len() / width;
        if end <= start {
            end = (start + 1).min(points.len());
        }
        let value = points[start..end].iter().copied().max().unwrap_or_default();
        samples.push(value);
    }

    let max = samples.iter().copied().max().unwrap_or_default();
    if max == 0 {
        return trace_idle_line(width, active);
    }

    const LEVELS: [char; 9] = ['·', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    samples
        .into_iter()
        .map(|value| {
            if value == 0 {
                LEVELS[0]
            } else {
                let level =
                    ((value as f64 / max as f64) * (LEVELS.len() - 1) as f64).ceil() as usize;
                LEVELS[level.clamp(1, LEVELS.len() - 1)]
            }
        })
        .collect()
}

fn trace_idle_line(width: usize, active: bool) -> String {
    let label = if active { "listening" } else { "no samples" };
    if width <= label.len() {
        return short(label, width);
    }
    format!("{label} {}", "─".repeat(width - label.len() - 1))
}

fn trace_ops_style(ops: u64) -> Style {
    if ops == 0 {
        Style::default().fg(MUTED)
    } else {
        Style::default().fg(WARN).bold()
    }
}

fn latency_color(latency_us: u64) -> Color {
    if latency_us >= 100_000 {
        BAD
    } else if latency_us >= 10_000 {
        WARN
    } else if latency_us > 0 {
        OK
    } else {
        MUTED
    }
}

fn draw_config(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(6),
            Constraint::Length(if app.config_editor.input.is_some() {
                5
            } else {
                4
            }),
        ])
        .split(area);

    let config_path = app
        .config_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_owned());
    let dirty = if app.config_editor.dirty {
        "pending"
    } else {
        "synced"
    };
    let summary = vec![
        Line::from(vec![
            label("profile"),
            Span::styled(
                &app.config_editor.draft.profile,
                Style::default().fg(ACCENT).bold(),
            ),
            Span::styled("  config ", Style::default().fg(MUTED)),
            Span::styled(config_path, Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            label("state"),
            Span::styled(
                dirty,
                Style::default().fg(if app.config_editor.dirty { WARN } else { OK }),
            ),
            Span::styled(
                "  edits are saved and applied to live ssh streams",
                Style::default().fg(MUTED),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(summary)
            .style(Style::default().fg(TEXT))
            .block(panel(" config target ")),
        chunks[0],
    );

    let editor = &app.config_editor;
    let mut rows = vec![
        config_row(
            editor.selected == 0,
            "admin_host",
            editor.draft.admin_host.clone(),
            ACCENT,
        ),
        config_row(
            editor.selected == 1,
            "refresh_secs",
            editor.draft.refresh_secs.to_string(),
            BLUE,
        ),
    ];
    rows.extend(editor.draft.hosts.iter().enumerate().map(|(index, host)| {
        let label = format!("host[{}]", index + 1);
        let color = if *host == editor.draft.admin_host {
            OK
        } else {
            TEXT
        };
        config_row(editor.selected == index + 2, label, host.clone(), color)
    }));

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Length(14),
                Constraint::Min(16),
            ],
        )
        .header(
            Row::new(["", "Field", "Value"])
                .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
        )
        .block(panel(" live config ")),
        chunks[1],
    );

    let footer = if let Some(input) = &editor.input {
        vec![
            Line::from(vec![
                Span::styled(
                    format!("{}: ", input.label),
                    Style::default().fg(WARN).bold(),
                ),
                Span::styled(&input.buffer, Style::default().fg(TEXT)),
            ]),
            Line::styled(
                "Enter applies immediately; Esc cancels this input",
                Style::default().fg(MUTED),
            ),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled("message ", Style::default().fg(MUTED)),
                Span::styled(&editor.message, Style::default().fg(TEXT)),
            ]),
            Line::styled(
                "Rows update cephlens.toml and restart live ssh streams after apply.",
                Style::default().fg(MUTED),
            ),
        ]
    };
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(TEXT))
            .block(panel(" apply ")),
        chunks[2],
    );
}

fn config_row(
    selected: bool,
    field: impl Into<String>,
    value: impl Into<String>,
    value_color: Color,
) -> Row<'static> {
    let marker = if selected { ">" } else { " " };
    let style = if selected {
        Style::default().bg(Color::Rgb(39, 45, 56))
    } else {
        Style::default()
    };
    Row::new(vec![
        Cell::from(marker).style(Style::default().fg(WARN).bold()),
        Cell::from(field.into()).style(Style::default().fg(MUTED)),
        Cell::from(value.into()).style(Style::default().fg(value_color).bold()),
    ])
    .style(style)
}

fn draw_cluster(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(snapshot) = &app.snapshot else {
        frame.render_widget(
            Paragraph::new("Waiting for first snapshot...")
                .style(Style::default().fg(MUTED))
                .block(panel(" vitals ")),
            area,
        );
        return;
    };

    let c = &snapshot.cluster;
    let lines = vec![
        Line::from(vec![
            label("health"),
            pill(&c.health, health_color(&c.health)),
        ]),
        kv_line("fsid", short(&c.fsid, 8), BLUE),
        kv_line(
            "mon/mgr",
            format!("{} / +{}", c.mon_count, c.mgr_standbys),
            TEXT,
        ),
        kv_line("osd", format!("{}/{} up/in", c.osds_up, c.osds_in), OK),
        Line::from(vec![
            label("data"),
            Span::styled(
                format!(
                    "{} / {}",
                    format_compact_bytes(c.bytes_used),
                    format_compact_bytes(c.bytes_total)
                ),
                Style::default().fg(TEXT),
            ),
        ]),
        kv_line("pg", &c.pg_states, TEXT),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(panel(" vitals ")),
        area,
    );
}

fn draw_nodes(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = node_rows(app);
    let visible = table_visible_rows(area);
    let total = rows.len();
    let scroll = clamp_top_scroll(app.nodes_scroll, total, visible);
    frame.render_widget(
        Table::new(
            rows.into_iter().skip(scroll).take(visible),
            [
                Constraint::Length(9),
                Constraint::Length(6),
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Length(5),
            ],
        )
        .header(
            Row::new(["Host", "State", "OSD", "CPU%", "MEM%"])
                .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
        )
        .block(scroll_panel(
            app,
            PanelFocus::Nodes,
            "nodes",
            total,
            visible,
            scroll,
            false,
        )),
        area,
    );
}

fn node_rows(app: &App) -> Vec<Row<'static>> {
    let replay_nodes = app
        .snapshot
        .as_ref()
        .map(|snapshot| {
            snapshot
                .nodes
                .iter()
                .map(|node| (node.host.clone(), node.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    app.hosts
        .iter()
        .map(|host| {
            let node = app
                .node_summaries
                .get(host)
                .or_else(|| replay_nodes.get(host));
            let stream_id = format!("node:{host}");
            let (state, color) = if live_streams_active(app) {
                connection_label(app.stream_statuses.get(&stream_id))
            } else {
                ("record".to_owned(), MUTED)
            };
            let osds = node
                .map(|node| node.osd_ids.clone())
                .filter(|ids| !ids.is_empty())
                .unwrap_or_else(|| "-".to_owned());
            let cpu = node
                .map(|node| percent_label(node.cpu_percent))
                .unwrap_or_else(|| "-".to_owned());
            let mem = node
                .map(|node| percent_label(node.mem_percent))
                .unwrap_or_else(|| "-".to_owned());
            Row::new(vec![
                Cell::from(short(host, 9)).style(Style::default().fg(ACCENT).bold()),
                Cell::from(state).style(Style::default().fg(color).bold()),
                Cell::from(osds),
                Cell::from(cpu).style(Style::default().fg(metric_color(
                    node.map(|node| node.cpu_percent).unwrap_or_default(),
                ))),
                Cell::from(mem).style(Style::default().fg(metric_color(
                    node.map(|node| node.mem_percent).unwrap_or_default(),
                ))),
            ])
        })
        .collect()
}

fn draw_osds(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let osds = app
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.osds.as_slice())
        .unwrap_or(&[]);

    let max_pgs = osds.iter().map(|osd| osd.pgs).max().unwrap_or(1).max(1);
    let compact = area.width < 72;
    let visible = table_visible_rows(area);
    let total = osds.len();
    let scroll = clamp_top_scroll(app.osds_scroll, total, visible);
    let rows = osds.iter().skip(scroll).take(visible).map(|osd| {
        let status_style = Style::default()
            .fg(if osd.status == "up" { OK } else { BAD })
            .add_modifier(Modifier::BOLD);
        let pg_bar = bar(
            osd.pgs as f64 / max_pgs as f64,
            if compact { 10 } else { 16 },
            BLUE,
        );
        if compact {
            Row::new(vec![
                Cell::from(osd.name.clone()).style(Style::default().fg(ACCENT).bold()),
                Cell::from(osd.host.clone()).style(Style::default().fg(TEXT)),
                Cell::from(osd.status.clone()).style(status_style),
                Cell::from(osd.pgs.to_string()),
                Cell::from(pg_bar).style(Style::default().fg(BLUE)),
            ])
        } else {
            Row::new(vec![
                Cell::from(osd.name.clone()).style(Style::default().fg(ACCENT).bold()),
                Cell::from(osd.host.clone()).style(Style::default().fg(TEXT)),
                Cell::from(osd.status.clone()).style(status_style),
                Cell::from(format!("{:.3}%", osd.utilization)),
                Cell::from(osd.pgs.to_string()),
                Cell::from(pg_bar).style(Style::default().fg(BLUE)),
                Cell::from(format_kb(osd.used_kb)),
                Cell::from(format_kb(osd.avail_kb)),
            ])
        }
    });

    let (widths, header) = if compact {
        (
            vec![
                Constraint::Length(7),
                Constraint::Length(12),
                Constraint::Length(7),
                Constraint::Length(5),
                Constraint::Length(10),
            ],
            Row::new(vec!["OSD", "Host", "State", "PGs", "PG load"]),
        )
    } else {
        (
            vec![
                Constraint::Length(7),
                Constraint::Length(12),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(5),
                Constraint::Length(16),
                Constraint::Length(9),
                Constraint::Length(9),
            ],
            Row::new(vec![
                "OSD", "Host", "State", "Util", "PGs", "PG load", "Used", "Avail",
            ]),
        )
    };

    let table = Table::new(rows, widths)
        .header(header.style(Style::default().fg(MUTED).bold()))
        .style(Style::default().fg(TEXT))
        .block(scroll_panel(
            app,
            PanelFocus::Osds,
            "osd map",
            total,
            visible,
            scroll,
            false,
        ))
        .row_highlight_style(Style::default().reversed());

    frame.render_widget(table, area);
}

fn draw_logs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let total = app.logs.len();
    let scroll = clamp_bottom_scroll(app.logs_scroll, total, visible);
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(visible);
    let lines = if app.logs.is_empty() {
        vec![Line::styled("no events yet", Style::default().fg(MUTED))]
    } else {
        app.logs[start..end]
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(scroll_panel(
                app,
                PanelFocus::Logs,
                "event log",
                total,
                visible,
                scroll,
                true,
            ))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn panel(title: &'static str) -> Block<'static> {
    panel_with_style(title.to_owned(), MUTED)
}

fn scroll_panel(
    app: &App,
    focus: PanelFocus,
    title: &str,
    total: usize,
    visible: usize,
    scroll: usize,
    from_bottom: bool,
) -> Block<'static> {
    let focused = app.focused_panel == focus;
    let marker = if focused { ">" } else { " " };
    let suffix = scroll_suffix(total, visible, scroll, from_bottom);
    let border = if focused { WARN } else { MUTED };
    panel_with_style(format!(" {marker} {title}{suffix} "), border)
}

fn scroll_suffix(total: usize, visible: usize, scroll: usize, from_bottom: bool) -> String {
    if total == 0 || total <= visible.max(1) {
        return String::new();
    }

    let visible = visible.max(1);
    if from_bottom {
        let scroll = clamp_bottom_scroll(scroll, total, visible);
        let end = total.saturating_sub(scroll);
        let start = end.saturating_sub(visible).saturating_add(1);
        let tail = if scroll == 0 { " tail" } else { "" };
        format!(" {start}-{end}/{total}{tail}")
    } else {
        let scroll = clamp_top_scroll(scroll, total, visible);
        let start = scroll.saturating_add(1);
        let end = scroll.saturating_add(visible).min(total);
        format!(" {start}-{end}/{total}")
    }
}

fn table_visible_rows(area: Rect) -> usize {
    area.height.saturating_sub(3).max(1) as usize
}

fn panel_with_style(title: String, border: Color) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
}

fn stream_counts(app: &App) -> (usize, usize) {
    let total = app.stream_statuses.len();
    let live = app
        .stream_statuses
        .values()
        .filter(|status| status.state == StreamState::Live)
        .count();
    (live, total)
}

fn connection_label(status: Option<&StreamStatus>) -> (String, Color) {
    match status.map(|status| &status.state) {
        Some(StreamState::Live) => ("live".to_owned(), OK),
        Some(StreamState::Connecting) => ("dial".to_owned(), WARN),
        Some(StreamState::Reconnecting) => ("retry".to_owned(), WARN),
        Some(StreamState::Error) => ("error".to_owned(), BAD),
        None => ("wait".to_owned(), MUTED),
    }
}

fn health_color(health: &str) -> Color {
    match health {
        "HEALTH_OK" => OK,
        "HEALTH_WARN" => WARN,
        _ => BAD,
    }
}

fn metric_color(value: f64) -> Color {
    if value >= 85.0 {
        BAD
    } else if value >= 65.0 {
        WARN
    } else {
        OK
    }
}

fn percent_label(value: f64) -> String {
    format!("{value:>4.1}")
}

fn format_latency_us(value: u64) -> String {
    if value >= 1000 {
        format!("{:.1}ms", value as f64 / 1000.0)
    } else if value == 0 {
        "-".to_owned()
    } else {
        format!("{value}us")
    }
}

fn pill(text: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default()
            .fg(Color::Black)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

fn label(text: &'static str) -> Span<'static> {
    Span::styled(format!("{text:<8}"), Style::default().fg(MUTED))
}

fn kv_line(label_text: &'static str, value: impl Into<String>, color: Color) -> Line<'static> {
    Line::from(vec![
        label(label_text),
        Span::styled(value.into(), Style::default().fg(color)),
    ])
}

fn bar(ratio: f64, width: usize, _color: Color) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}
