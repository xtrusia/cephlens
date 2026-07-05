use std::collections::HashMap;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};

use crate::app::{
    App, EVENT_LOG_MIN_HEIGHT, EVENT_LOG_RESERVED_ROWS, Mode, PanelFocus, StreamState,
    StreamStatus, live_streams_active,
};
use crate::editor::ConfigDraft;
use crate::model::NodeSummary;
use crate::trace::{TraceGraphRow, dominant_component, trace_graph_rows as build_trace_graph_rows};
use crate::util::{clamp_bottom_scroll, clamp_top_scroll, short};

const ACCENT: Color = Color::Rgb(93, 228, 199);
const BLUE: Color = Color::Rgb(130, 170, 255);
const OK: Color = Color::Rgb(195, 232, 141);
const WARN: Color = Color::Rgb(255, 203, 107);
const BAD: Color = Color::Rgb(255, 83, 112);
const MUTED: Color = Color::Rgb(91, 99, 112);
const TEXT: Color = Color::Rgb(198, 208, 219);

#[derive(Clone, Copy, Debug)]
enum InsightLevel {
    Ok,
    Info,
    Warn,
    Bad,
}

#[derive(Clone, Debug)]
struct Insight {
    level: InsightLevel,
    text: String,
}

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
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, app, chunks[0]);
    draw_body(frame, app, chunks[1]);
    draw_logs(frame, app, chunks[2]);
    draw_footer(frame, app, chunks[3]);
    if app.show_help {
        draw_help_overlay(frame, app, area);
    }
    if app.confirm_quit {
        draw_quit_confirm(frame, app, area);
    }
    if app.shutting_down {
        draw_shutdown(frame, app, area);
    }
}

fn event_log_height_for(area: Rect, preferred: u16) -> u16 {
    let terminal_limit = area
        .height
        .saturating_sub(EVENT_LOG_RESERVED_ROWS)
        .max(EVENT_LOG_MIN_HEIGHT);
    preferred.max(EVENT_LOG_MIN_HEIGHT).min(terminal_limit)
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

fn draw_shutdown(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let note = if app.trace_following || app.trace_active > 0 {
        "Stopping trace runners and cleaning up remote hosts..."
    } else {
        "Closing SSH streams..."
    };
    let modal = centered_rect(58, 5, area);
    let lines = vec![
        Line::styled("Shutting down", Style::default().fg(WARN).bold()),
        Line::raw(""),
        Line::styled(note, Style::default().fg(TEXT)),
    ];
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(panel(" cleaning up ")),
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
    let base_top = if show_insights {
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
        let top_height = clamp_overview_height(
            base_top,
            app.overview_offset,
            area.height,
            insight_height,
            trace_min_height,
        );
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(top_height),
                Constraint::Length(insight_height),
                Constraint::Min(trace_min_height),
            ])
            .split(area);

        draw_overview(frame, app, chunks[0]);
        draw_insights(frame, app, chunks[1]);
        draw_trace_events(frame, app, chunks[2]);
    } else {
        let top_height = clamp_overview_height(
            base_top,
            app.overview_offset,
            area.height,
            0,
            trace_min_height,
        );
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(top_height),
                Constraint::Min(trace_min_height),
            ])
            .split(area);

        draw_overview(frame, app, chunks[0]);
        draw_trace_events(frame, app, chunks[1]);
    }
}

fn clamp_overview_height(base: u16, offset: i16, total: u16, insight: u16, trace_min: u16) -> u16 {
    let max_top = total.saturating_sub(insight + trace_min + 1).max(5);
    ((base as i16) + offset).clamp(5, max_top as i16) as u16
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

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(command_spans(&footer_commands(app)));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().fg(TEXT)),
        area,
    );
}

fn command_spans(commands: &[(&'static str, &'static str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (key, label_text)) in commands.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
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

fn footer_commands(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.mode {
        Mode::Live => vec![
            ("t", "trace"),
            ("0", "all"),
            ("c", "config"),
            ("Tab", "panel"),
            ("?", "more"),
            ("q", "quit"),
        ],
        Mode::Config if app.config_editor.input.is_some() => {
            vec![("Enter", "save"), ("Ctrl+U", "clear"), ("Esc", "cancel")]
        }
        Mode::Config => vec![
            ("Up/Dn", "select"),
            ("e", "edit"),
            ("a", "add"),
            ("d", "delete"),
            ("s", "save"),
            ("?", "more"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        Mode::Replay { .. } => vec![("Left/Right", "replay"), ("q", "quit")],
    }
}

fn help_commands(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.mode {
        Mode::Live => vec![
            ("c", "edit config"),
            ("p", "probe node + osdtrace readiness"),
            ("i", "install osdtrace"),
            ("t", "toggle trace (>=1ms)"),
            ("0", "trace all observed ops"),
            ("x", "clear captured trace"),
            ("Tab / Shift+Tab", "focus next / prev panel"),
            ("Up/Dn j/k", "scroll focused panel"),
            ("PgUp/PgDn", "scroll faster"),
            ("Home/End", "jump to start / end"),
            ("-/+", "resize focused panel"),
            ("q / Esc", "quit"),
        ],
        Mode::Config => vec![
            ("Up/Dn", "select field or host row"),
            ("e / Enter", "edit or toggle selected"),
            ("a", "add host"),
            ("d / Delete", "delete selected host row"),
            ("s", "save current profile"),
            ("Esc / c", "back to dashboard"),
            ("q", "quit"),
        ],
        Mode::Replay { .. } => vec![
            ("Left/Right", "previous / next snapshot"),
            ("q / Esc", "quit"),
        ],
    }
}

fn draw_help_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let commands = help_commands(app);
    let key_width = commands
        .iter()
        .map(|(key, _)| key.len())
        .max()
        .unwrap_or(3)
        .max(3);
    let mut lines = vec![
        Line::styled("keys", Style::default().fg(ACCENT).bold()),
        Line::raw(""),
    ];
    lines.extend(commands.iter().map(|(key, label_text)| {
        Line::from(vec![
            Span::styled(
                format!("{:<width$}", key, width = key_width),
                Style::default().fg(WARN).bold(),
            ),
            Span::raw("  "),
            Span::styled((*label_text).to_owned(), Style::default().fg(TEXT)),
        ])
    }));
    lines.push(Line::raw(""));
    if matches!(app.mode, Mode::Live) {
        lines.push(Line::styled("legend", Style::default().fg(ACCENT).bold()));
        lines.push(Line::from(vec![
            Span::styled("T   ", Style::default().fg(WARN).bold()),
            Span::styled(
                "● ready  ◐ warn  ○ missing  · unprobed",
                Style::default().fg(TEXT),
            ),
        ]));
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled("any key closes", Style::default().fg(MUTED)));

    let legend_width = if matches!(app.mode, Mode::Live) {
        44
    } else {
        0
    };
    let inner_width = commands
        .iter()
        .map(|(_, label)| key_width + 2 + label.len())
        .max()
        .unwrap_or(20)
        .max(legend_width)
        .clamp(20, 60) as u16;
    let modal = centered_rect(inner_width + 4, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .block(panel(" help ")),
        modal,
    );
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
                resize_hint(app, PanelFocus::Trace, area),
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
        config_row(
            editor.selected == 2,
            "trace_auto_start",
            bool_label(editor.draft.trace_auto_start),
            if editor.draft.trace_auto_start {
                OK
            } else {
                MUTED
            },
        ),
        config_row(
            editor.selected == 3,
            "trace_window_secs",
            editor.draft.trace_window_secs.to_string(),
            BLUE,
        ),
        config_row(
            editor.selected == 4,
            "trace_latency_ms",
            editor.draft.trace_latency_ms.to_string(),
            BLUE,
        ),
        config_row(
            editor.selected == 5,
            "trace_ttl_secs",
            editor.draft.trace_ttl_secs.to_string(),
            BLUE,
        ),
        config_row(
            editor.selected == 6,
            "osdtrace_url",
            optional_config_value(&editor.draft.osdtrace_url, 72),
            TEXT,
        ),
        config_row(
            editor.selected == 7,
            "osdtrace_sha256",
            optional_config_value(&editor.draft.osdtrace_sha256, 72),
            TEXT,
        ),
        config_row(
            editor.selected == 8,
            "osdtrace_allow_unverified",
            bool_label(editor.draft.osdtrace_allow_unverified),
            if editor.draft.osdtrace_allow_unverified {
                WARN
            } else {
                MUTED
            },
        ),
    ];
    rows.extend(editor.draft.hosts.iter().enumerate().map(|(index, host)| {
        let label = format!("host[{}]", index + 1);
        let color = if *host == editor.draft.admin_host {
            OK
        } else {
            TEXT
        };
        let row_index = ConfigDraft::FIXED_ROWS + index;
        config_row(editor.selected == row_index, label, host.clone(), color)
    }));
    let total_rows = rows.len();
    let visible_rows = table_visible_rows(chunks[1]);
    let scroll = editor
        .selected
        .saturating_sub(visible_rows.saturating_sub(1));
    let rows = rows
        .into_iter()
        .skip(scroll)
        .take(visible_rows)
        .collect::<Vec<_>>();

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Length(27),
                Constraint::Min(16),
            ],
        )
        .header(
            Row::new(["", "Field", "Value"])
                .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
        )
        .block(panel_with_style(
            format!(
                " live config{} ",
                scroll_suffix(total_rows, visible_rows, scroll, false)
            ),
            if editor.dirty { WARN } else { ACCENT },
        )),
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
                "Enter/e edits or toggles; a adds host; d deletes host rows; values apply immediately.",
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

fn bool_label(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn optional_config_value(value: &str, width: usize) -> String {
    if value.trim().is_empty() {
        "-".to_owned()
    } else {
        short(value.trim(), width)
    }
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

fn osdtrace_glyph(app: &App, host: &str) -> (&'static str, Color) {
    match app.trace_targets.iter().find(|target| target.host == host) {
        None => ("·", MUTED),
        Some(target) if target.installed && target.error.is_none() => ("●", OK),
        Some(target) if target.installed => ("◐", WARN),
        Some(_) => ("○", BAD),
    }
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
                Constraint::Length(2),
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Length(5),
            ],
        )
        .header(
            Row::new(["Host", "State", "T", "OSD", "CPU%", "MEM%"])
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
            resize_hint(app, PanelFocus::Nodes, area),
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
            let (glyph, glyph_color) = osdtrace_glyph(app, host);
            Row::new(vec![
                Cell::from(short(host, 9)).style(Style::default().fg(ACCENT).bold()),
                Cell::from(state).style(Style::default().fg(color).bold()),
                Cell::from(glyph).style(Style::default().fg(glyph_color).bold()),
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
            resize_hint(app, PanelFocus::Osds, area),
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
                resize_hint(app, PanelFocus::Logs, area),
            ))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn panel(title: &'static str) -> Block<'static> {
    panel_with_style(title.to_owned(), MUTED)
}

#[allow(clippy::too_many_arguments)]
fn scroll_panel(
    app: &App,
    focus: PanelFocus,
    title: &str,
    total: usize,
    visible: usize,
    scroll: usize,
    from_bottom: bool,
    resize: Option<u16>,
) -> Block<'static> {
    let focused = app.focused_panel == focus;
    let marker = if focused { ">" } else { " " };
    let suffix = scroll_suffix(total, visible, scroll, from_bottom);
    let border = if focused { WARN } else { MUTED };
    let mut block = panel_with_style(format!(" {marker} {title}{suffix} "), border);
    if let Some(rows) = resize {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" -/+ {rows} "),
                Style::default().fg(WARN),
            ))
            .right_aligned(),
        );
    }
    block
}

fn resize_hint(app: &App, focus: PanelFocus, area: Rect) -> Option<u16> {
    if app.focused_panel == focus && panel_resizable(&app.mode, focus) {
        Some(area.height)
    } else {
        None
    }
}

fn panel_resizable(mode: &Mode, focus: PanelFocus) -> bool {
    match focus {
        PanelFocus::Logs => true,
        PanelFocus::Nodes | PanelFocus::Osds => matches!(mode, Mode::Live),
        PanelFocus::Trace => matches!(mode, Mode::Live),
    }
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

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_compact_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}{}", bytes, UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

fn format_kb(kb: u64) -> String {
    format_bytes(kb.saturating_mul(1024))
}
