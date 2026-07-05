use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command as ProcessCommand, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use serde_json::Value;

use crate::{
    collect::{parse_cluster_summary, parse_osds, run_probe},
    editor::ConfigEditor,
    model::{NodeSummary, Snapshot},
    runner::{
        CleanupResult, cleanup_trace_runners_async, cleanup_trace_runners_wait, install_trace_host,
        probe_trace_host, trace_runner_install_command, trace_runner_script, trace_threshold_label,
    },
    session::append_snapshot,
    stream::{cluster_stream_command, node_stream_command, parse_node_stream_payload},
    trace::{
        TRACE_BUCKET_COUNT, TRACE_BUCKET_SECS, TraceBucket, TraceEvent, TraceInstallConfig,
        TraceTarget, normalize_osd_name, normalize_pg_name, parse_trace_event,
    },
    util::shell_quote,
};

pub(crate) const EVENT_LOG_MIN_HEIGHT: u16 = 3;
pub(crate) const EVENT_LOG_DEFAULT_HEIGHT: u16 = 6;
// Rows reserved for the header, footer, and a usable body when the event log
// grows. The log is capped at terminal_height - EVENT_LOG_RESERVED_ROWS.
pub(crate) const EVENT_LOG_RESERVED_ROWS: u16 = 10;
const SESSION_SNAPSHOT_LIMIT: usize = 10_000;

#[derive(Clone, Debug)]
pub(crate) enum WorkerMsg {
    Probe(String),
    Stream(StreamMsg),
    TraceProbe(Vec<TraceTarget>),
    TraceInstall(Vec<TraceTarget>),
    TraceLine { host: String, line: String },
    TraceDone { host: String, message: String },
}

#[derive(Clone, Debug)]
pub(crate) enum StreamMsg {
    Connecting {
        id: String,
    },
    Connected {
        id: String,
    },
    Line {
        id: String,
        payload: String,
    },
    Error {
        id: String,
        message: String,
    },
    Disconnected {
        id: String,
        message: String,
        retry_secs: u64,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct StreamStatus {
    pub(crate) state: StreamState,
    pub(crate) last_seen: Option<DateTime<Utc>>,
    pub(crate) detail: String,
    pub(crate) reconnects: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StreamState {
    Connecting,
    Live,
    Reconnecting,
    Error,
}

#[derive(Clone, Debug)]
pub(crate) enum Mode {
    Live,
    Config,
    Trace,
    Replay {
        index: usize,
        snapshots: Vec<Snapshot>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelFocus {
    Nodes,
    Osds,
    Trace,
    Logs,
    Targets,
}

pub(crate) struct App {
    pub(crate) profile: String,
    pub(crate) hosts: Vec<String>,
    pub(crate) admin_host: String,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) config_editor: ConfigEditor,
    pub(crate) refresh: Duration,
    pub(crate) mode: Mode,
    pub(crate) snapshot: Option<Snapshot>,
    pub(crate) confirm_quit: bool,
    pub(crate) shutting_down: bool,
    pub(crate) tx: Sender<WorkerMsg>,
    pub(crate) rx: Receiver<WorkerMsg>,
    pub(crate) logs: Vec<String>,
    pub(crate) event_log_height: u16,
    pub(crate) terminal_height: u16,
    pub(crate) overview_offset: i16,
    pub(crate) insights_offset: i16,
    pub(crate) show_help: bool,
    pub(crate) focused_panel: PanelFocus,
    pub(crate) nodes_scroll: usize,
    pub(crate) osds_scroll: usize,
    pub(crate) trace_scroll: usize,
    pub(crate) logs_scroll: usize,
    pub(crate) targets_scroll: usize,
    pub(crate) node_summaries: HashMap<String, NodeSummary>,
    pub(crate) stream_statuses: HashMap<String, StreamStatus>,
    pub(crate) trace_targets: Vec<TraceTarget>,
    pub(crate) trace_events: Vec<TraceEvent>,
    pub(crate) trace_series: HashMap<String, VecDeque<TraceBucket>>,
    pub(crate) trace_active: usize,
    pub(crate) trace_following: bool,
    pub(crate) trace_session: Option<String>,
    pub(crate) trace_auto_start: bool,
    pub(crate) trace_window_secs: u64,
    pub(crate) trace_latency_ms: u64,
    pub(crate) trace_ttl_secs: u64,
    pub(crate) trace_install: TraceInstallConfig,
    pub(crate) trace_stop: Arc<AtomicBool>,
    pub(crate) stream_stop: Arc<AtomicBool>,
    pub(crate) session_path: Option<PathBuf>,
    pub(crate) session_records: usize,
}

impl App {
    pub(crate) fn log(&mut self, message: impl Into<String>) {
        let stamp = Local::now().format("%H:%M:%S");
        self.logs
            .push(format!("[{stamp}] {}", message.into().replace('\n', " ")));
        if self.logs.len() > 400 {
            self.logs.drain(0..100);
        }
    }
}

pub(crate) fn request_quit(app: &mut App) {
    app.confirm_quit = true;
}

pub(crate) fn start_live_streams(app: &mut App) {
    let interval_secs = app.refresh.as_secs().max(1);
    app.log(format!(
        "opening persistent ssh streams: {interval_secs}s tick"
    ));
    let cluster_id = format!("cluster:{}", app.admin_host);
    spawn_persistent_ssh_stream(
        cluster_id,
        app.admin_host.clone(),
        cluster_stream_command(interval_secs),
        app.tx.clone(),
        app.stream_stop.clone(),
    );
    for host in app.hosts.clone() {
        spawn_persistent_ssh_stream(
            format!("node:{host}"),
            host,
            node_stream_command(interval_secs),
            app.tx.clone(),
            app.stream_stop.clone(),
        );
    }
}

pub(crate) fn shutdown_streams(app: &App, wait_for_cleanup: bool) -> Vec<CleanupResult> {
    app.trace_stop.store(true, Ordering::SeqCst);
    if live_streams_active(app) {
        app.stream_stop.store(true, Ordering::SeqCst);
    }
    let cleanup = if wait_for_cleanup {
        cleanup_trace_runners_wait(app.hosts.clone(), app.trace_session.clone())
    } else {
        cleanup_trace_runners_async(app.hosts.clone(), app.trace_session.clone());
        Vec::new()
    };
    // Only the quit path may block; stream threads need this window to kill
    // their ssh children before the process exits.
    if wait_for_cleanup && live_streams_active(app) {
        thread::sleep(Duration::from_millis(1200));
    }
    cleanup
}

pub(crate) fn live_streams_active(app: &App) -> bool {
    matches!(app.mode, Mode::Live | Mode::Config | Mode::Trace)
}

fn spawn_persistent_ssh_stream(
    id: String,
    host: String,
    command: String,
    tx: Sender<WorkerMsg>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let retry_secs = 2;
        while !stop.load(Ordering::SeqCst) {
            let _ = tx.send(WorkerMsg::Stream(StreamMsg::Connecting { id: id.clone() }));
            let remote = format!("sh -c {}", shell_quote(&command));
            let child_result = ProcessCommand::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=8",
                    "-o",
                    "ServerAliveInterval=5",
                    "-o",
                    "ServerAliveCountMax=2",
                ])
                .arg(&host)
                .arg(remote)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            let mut child = match child_result {
                Ok(child) => child,
                Err(err) => {
                    let _ = tx.send(WorkerMsg::Stream(StreamMsg::Disconnected {
                        id: id.clone(),
                        message: format!("failed to start ssh: {err}"),
                        retry_secs,
                    }));
                    sleep_with_stop(&stop, Duration::from_secs(retry_secs));
                    continue;
                }
            };

            let _ = tx.send(WorkerMsg::Stream(StreamMsg::Connected { id: id.clone() }));

            if let Some(stderr) = child.stderr.take() {
                let err_tx = tx.clone();
                let err_id = id.clone();
                let err_stop = stop.clone();
                thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        if err_stop.load(Ordering::SeqCst) {
                            break;
                        }
                        if !line.trim().is_empty() {
                            let _ = err_tx.send(WorkerMsg::Stream(StreamMsg::Error {
                                id: err_id.clone(),
                                message: line,
                            }));
                        }
                    }
                });
            }

            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    if stop.load(Ordering::SeqCst) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                    match line {
                        Ok(payload) if !payload.trim().is_empty() => {
                            let _ = tx.send(WorkerMsg::Stream(StreamMsg::Line {
                                id: id.clone(),
                                payload,
                            }));
                        }
                        Ok(_) => {}
                        Err(err) => {
                            let _ = tx.send(WorkerMsg::Stream(StreamMsg::Error {
                                id: id.clone(),
                                message: format!("stdout read failed: {err}"),
                            }));
                            break;
                        }
                    }
                }
            }

            let status = child.wait();
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let message = match status {
                Ok(status) => format!("ssh exited with {status}"),
                Err(err) => format!("ssh wait failed: {err}"),
            };
            let _ = tx.send(WorkerMsg::Stream(StreamMsg::Disconnected {
                id: id.clone(),
                message,
                retry_secs,
            }));
            sleep_with_stop(&stop, Duration::from_secs(retry_secs));
        }
    });
}

fn sleep_with_stop(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) fn drain_worker_messages(app: &mut App) {
    while let Ok(msg) = app.rx.try_recv() {
        match msg {
            WorkerMsg::Probe(output) => {
                for line in output.lines() {
                    app.log(line.to_owned());
                }
            }
            WorkerMsg::Stream(msg) => handle_stream_msg(app, msg),
            WorkerMsg::TraceProbe(targets) => {
                app.trace_active = 0;
                let ready = targets.iter().filter(|target| target.installed).count();
                app.log(format!(
                    "osdtrace probe complete: {ready}/{} ready",
                    targets.len()
                ));
                app.trace_targets = targets;
            }
            WorkerMsg::TraceInstall(targets) => {
                app.trace_active = 0;
                let ready = targets.iter().filter(|target| target.installed).count();
                app.log(format!(
                    "osdtrace install complete: {ready}/{} ready",
                    targets.len()
                ));
                app.trace_targets = targets;
            }
            WorkerMsg::TraceLine { host, line } => {
                if let Some(message) = line.trim().strip_prefix("__CEPHLENS_RUNNER__") {
                    app.log(format!("runner {host}: {}", message.trim()));
                    continue;
                }
                if let Some(event) = parse_trace_event(&host, &line) {
                    if event.op == "error" {
                        app.log(format!("trace {host}: {}", event.raw));
                    }
                    record_trace_event(app, &event);
                    app.trace_events.push(event);
                    if app.trace_events.len() > 400 {
                        app.trace_events.drain(0..100);
                    }
                }
            }
            WorkerMsg::TraceDone { host, message } => {
                app.trace_active = app.trace_active.saturating_sub(1);
                app.log(format!("trace {host}: {message}"));
                if app.trace_active == 0 {
                    app.trace_following = false;
                    app.trace_session = None;
                }
            }
        }
    }
}

fn handle_stream_msg(app: &mut App, msg: StreamMsg) {
    match msg {
        StreamMsg::Connecting { id } => {
            set_stream_state(app, &id, StreamState::Connecting, "dialing")
        }
        StreamMsg::Connected { id } => set_stream_state(app, &id, StreamState::Live, "connected"),
        StreamMsg::Error { id, message } => set_stream_state(app, &id, StreamState::Error, message),
        StreamMsg::Disconnected {
            id,
            message,
            retry_secs,
        } => set_stream_state(
            app,
            &id,
            StreamState::Reconnecting,
            format!("{message}; retry in {retry_secs}s"),
        ),
        StreamMsg::Line { id, payload } => {
            set_stream_seen(app, &id);
            if let Err(err) = handle_stream_payload(app, &id, &payload) {
                set_stream_state(app, &id, StreamState::Error, format!("{err:#}"));
            }
        }
    }
}

fn handle_stream_payload(app: &mut App, id: &str, payload: &str) -> Result<()> {
    if id.starts_with("cluster:") {
        let value: Value = serde_json::from_str(payload)
            .with_context(|| format!("invalid cluster stream payload from {id}"))?;
        if value.pointer("/type").and_then(Value::as_str) == Some("error") {
            return Err(anyhow!(
                "{}",
                value
                    .pointer("/message")
                    .and_then(Value::as_str)
                    .unwrap_or("remote ceph command failed")
            ));
        }
        let status = value
            .pointer("/status")
            .ok_or_else(|| anyhow!("cluster stream missing status"))?;
        let tree = value
            .pointer("/tree")
            .ok_or_else(|| anyhow!("cluster stream missing tree"))?;
        let df = value
            .pointer("/df")
            .ok_or_else(|| anyhow!("cluster stream missing df"))?;
        let snapshot = Snapshot {
            captured_at: Utc::now(),
            profile: app.profile.clone(),
            admin_host: app.admin_host.clone(),
            hosts: app.hosts.clone(),
            cluster: parse_cluster_summary(status),
            nodes: ordered_nodes(app),
            osds: parse_osds(tree, df),
        };
        record_session_snapshot(app, &snapshot);
        app.snapshot = Some(snapshot);
    } else if let Some(host) = id.strip_prefix("node:") {
        let node = parse_node_stream_payload(host, payload)?;
        app.node_summaries.insert(host.to_owned(), node);
        let nodes = ordered_nodes(app);
        if let Some(snapshot) = app.snapshot.as_mut() {
            snapshot.nodes = nodes;
        }
    }
    Ok(())
}

fn record_session_snapshot(app: &mut App, snapshot: &Snapshot) {
    let Some(path) = app.session_path.clone() else {
        return;
    };
    if app.session_records >= SESSION_SNAPSHOT_LIMIT {
        return;
    }
    match append_snapshot(&path, snapshot) {
        Ok(()) => {
            app.session_records += 1;
            if app.session_records == SESSION_SNAPSHOT_LIMIT {
                app.log(format!(
                    "session recording stopped after {SESSION_SNAPSHOT_LIMIT} snapshots"
                ));
            }
        }
        Err(err) => app.log(format!("record failed: {err:#}")),
    }
}

fn ordered_nodes(app: &App) -> Vec<NodeSummary> {
    app.hosts
        .iter()
        .map(|host| {
            app.node_summaries
                .get(host)
                .cloned()
                .or_else(|| {
                    app.snapshot.as_ref().and_then(|snapshot| {
                        snapshot
                            .nodes
                            .iter()
                            .find(|node| node.host == *host)
                            .cloned()
                    })
                })
                .unwrap_or_else(|| NodeSummary {
                    host: host.clone(),
                    error: Some("waiting for stream".to_owned()),
                    ..NodeSummary::default()
                })
        })
        .collect()
}

fn set_stream_seen(app: &mut App, id: &str) {
    let status = app
        .stream_statuses
        .entry(id.to_owned())
        .or_insert_with(|| StreamStatus {
            state: StreamState::Live,
            last_seen: None,
            detail: String::new(),
            reconnects: 0,
        });
    status.state = StreamState::Live;
    status.last_seen = Some(Utc::now());
    status.detail = "streaming".to_owned();
}

fn set_stream_state(app: &mut App, id: &str, state: StreamState, detail: impl Into<String>) {
    let detail = detail.into();
    let status = app
        .stream_statuses
        .entry(id.to_owned())
        .or_insert_with(|| StreamStatus {
            state: state.clone(),
            last_seen: None,
            detail: detail.clone(),
            reconnects: 0,
        });
    let changed = status.state != state || status.detail != detail;
    if state == StreamState::Reconnecting && changed {
        status.reconnects += 1;
    }
    status.state = state.clone();
    status.detail = detail.clone();
    if state == StreamState::Live {
        status.last_seen = Some(Utc::now());
    }
    if changed {
        app.log(format!("{id} {state:?}: {detail}"));
    }
}

pub(crate) fn spawn_probe(app: &mut App) {
    app.log("probe readiness check requested");
    let tx = app.tx.clone();
    let hosts = app.hosts.clone();
    thread::spawn(move || {
        let _ = tx.send(WorkerMsg::Probe(run_probe(&hosts)));
    });
}

pub(crate) fn spawn_trace_probe(app: &mut App) {
    if app.trace_active > 0 {
        return;
    }
    app.trace_following = false;
    app.trace_active = app.hosts.len().max(1);
    app.log("osdtrace probe requested");
    let tx = app.tx.clone();
    let hosts = app.hosts.clone();
    thread::spawn(move || {
        let targets = hosts.iter().map(|host| probe_trace_host(host)).collect();
        let _ = tx.send(WorkerMsg::TraceProbe(targets));
    });
}

pub(crate) fn spawn_trace_install(app: &mut App) {
    if app.trace_active > 0 {
        return;
    }
    app.trace_following = false;
    app.trace_active = app.hosts.len().max(1);
    app.log("installing osdtrace on configured hosts");
    let tx = app.tx.clone();
    let hosts = app.hosts.clone();
    let install = app.trace_install.clone();
    thread::spawn(move || {
        let targets = hosts
            .iter()
            .map(|host| install_trace_host(host, &install))
            .collect();
        let _ = tx.send(WorkerMsg::TraceInstall(targets));
    });
}

pub(crate) fn spawn_trace_run(app: &mut App, latency_ms: u64) {
    if app.trace_active > 0 {
        return;
    }
    if app.hosts.is_empty() {
        app.trace_following = false;
        app.log("trace skipped: no configured hosts");
        return;
    }
    app.trace_events.clear();
    app.trace_series.clear();
    app.trace_following = true;
    app.trace_stop.store(true, Ordering::SeqCst);
    app.trace_stop = Arc::new(AtomicBool::new(false));
    app.trace_active = app.hosts.len();
    let threshold = trace_threshold_label(latency_ms);
    let ttl_secs = app.trace_ttl_secs.max(1);
    let session = trace_session_id();
    app.trace_session = Some(session.clone());
    app.log(format!(
        "deploying temp trace runners on {} hosts: ttl={}s {threshold}",
        app.hosts.len(),
        ttl_secs
    ));
    for host in app.hosts.clone() {
        spawn_trace_runner(
            host,
            app.tx.clone(),
            latency_ms,
            ttl_secs,
            session.clone(),
            app.trace_stop.clone(),
        );
    }
}

pub(crate) fn stop_trace_follow(app: &mut App) {
    app.trace_following = false;
    app.trace_stop.store(true, Ordering::SeqCst);
    cleanup_trace_runners_async(app.hosts.clone(), app.trace_session.clone());
    if app.trace_active > 0 {
        app.log("trace runners stopping and cleaning up");
    } else {
        app.log("trace follow stopped");
        app.trace_session = None;
    }
}

pub(crate) fn toggle_trace(app: &mut App, latency_ms: u64) {
    if app.trace_active > 0 || app.trace_following {
        stop_trace_follow(app);
    } else {
        spawn_trace_run(app, latency_ms);
    }
}

fn trace_session_id() -> String {
    format!("{}-{}", Utc::now().timestamp(), std::process::id())
}

pub(crate) fn replay_move(app: &mut App, delta: isize) {
    let Mode::Replay { index, snapshots } = &mut app.mode else {
        return;
    };
    if snapshots.is_empty() {
        return;
    }
    let max = snapshots.len() - 1;
    let next = if delta < 0 {
        index.saturating_sub(delta.unsigned_abs())
    } else {
        (*index + delta as usize).min(max)
    };
    *index = next;
    app.snapshot = snapshots.get(next).cloned();
    let len = snapshots.len();
    app.log(format!("replay snapshot {}/{}", next + 1, len));
}

fn spawn_trace_runner(
    host: String,
    tx: Sender<WorkerMsg>,
    latency_ms: u64,
    ttl_secs: u64,
    session: String,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let command = trace_runner_install_command(&session, latency_ms, ttl_secs);
        let remote = format!("sh -c {}", shell_quote(&command));
        let child_result = ProcessCommand::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
            ])
            .arg(&host)
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child_result {
            Ok(child) => child,
            Err(err) => {
                let _ = tx.send(WorkerMsg::TraceDone {
                    host,
                    message: format!("failed to start ssh: {err}"),
                });
                return;
            }
        };

        if let Some(mut stdin) = child.stdin.take()
            && let Err(err) = stdin.write_all(trace_runner_script().as_bytes())
        {
            let _ = child.kill();
            let _ = tx.send(WorkerMsg::TraceDone {
                host,
                message: format!("failed to upload runner: {err}"),
            });
            return;
        }

        if let Some(stderr) = child.stderr.take() {
            let err_tx = tx.clone();
            let err_host = host.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        let _ = err_tx.send(WorkerMsg::TraceLine {
                            host: err_host.clone(),
                            line,
                        });
                    }
                }
            });
        }

        let mut event_count = 0usize;
        let (line_tx, line_rx) = mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    let _ = line_tx.send(line);
                }
            });
        }

        let message = loop {
            for _ in 0..256 {
                let Ok(line) = line_rx.try_recv() else {
                    break;
                };
                let trimmed = line.trim();
                if parse_trace_event(&host, trimmed).is_some() {
                    event_count += 1;
                }
                if !trimmed.is_empty() {
                    let _ = tx.send(WorkerMsg::TraceLine {
                        host: host.clone(),
                        line,
                    });
                }
            }

            if stop.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                break format!("stopped; {event_count} events observed");
            }

            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    break format!("runner exited; {event_count} events observed");
                }
                Ok(Some(status)) => break format!("runner failed with {status}"),
                Err(err) => break format!("runner wait failed: {err}"),
                Ok(None) => thread::sleep(Duration::from_millis(100)),
            }
        };
        while let Ok(line) = line_rx.try_recv() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                let _ = tx.send(WorkerMsg::TraceLine {
                    host: host.clone(),
                    line,
                });
            }
        }
        let _ = tx.send(WorkerMsg::TraceDone { host, message });
    });
}

fn record_trace_event(app: &mut App, event: &TraceEvent) {
    let osd = normalize_osd_name(&event.osd);
    if osd == "-" || event.op == "error" {
        return;
    }

    let now_bucket = Utc::now().timestamp() / TRACE_BUCKET_SECS;
    let series = app.trace_series.entry(osd).or_default();
    let needs_new_bucket = series
        .back()
        .map(|bucket| bucket.bucket != now_bucket)
        .unwrap_or(true);
    if needs_new_bucket {
        series.push_back(TraceBucket {
            bucket: now_bucket,
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
