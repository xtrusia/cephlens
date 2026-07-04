use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::{self, BufRead, BufReader, Stdout, Write},
    path::{Path, PathBuf},
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
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};
use serde_json::Value;

mod collect;
mod config;
mod model;
mod runner;
mod session;
mod ssh;
mod stream;
mod trace;
mod util;

use collect::{collect_snapshot, parse_cluster_summary, parse_osds, run_bench, run_probe};
use config::{
    ClusterProfile, ConfigFile, DEFAULT_TRACE_TTL_SECS, ResolvedConfig, clean_optional,
    default_hosts, load_config_file, normalize_hosts, parse_hosts, write_default_config,
};
use model::{NodeSummary, Snapshot};
use runner::{
    CleanupResult, cleanup_trace_runners_async, cleanup_trace_runners_wait, install_trace_host,
    probe_trace_host, report_cleanup_results, trace_runner_install_command, trace_runner_script,
    trace_threshold_label,
};
use session::{append_snapshot, create_session_path, load_snapshots};
use stream::{cluster_stream_command, node_stream_command, parse_node_stream_payload};
use trace::{
    TRACE_BUCKET_COUNT, TRACE_BUCKET_SECS, TraceBucket, TraceEvent, TraceGraphRow,
    TraceInstallConfig, TraceTarget, dominant_component, normalize_osd_name, normalize_pg_name,
    parse_trace_event, trace_graph_rows as build_trace_graph_rows, trace_platform_label,
    validate_sha256,
};
use util::{clamp_bottom_scroll, clamp_top_scroll, shell_quote, short};

const ACCENT: Color = Color::Rgb(93, 228, 199);
const BLUE: Color = Color::Rgb(130, 170, 255);
const OK: Color = Color::Rgb(195, 232, 141);
const WARN: Color = Color::Rgb(255, 203, 107);
const BAD: Color = Color::Rgb(255, 83, 112);
const MUTED: Color = Color::Rgb(91, 99, 112);
const TEXT: Color = Color::Rgb(198, 208, 219);
const EVENT_LOG_MIN_HEIGHT: u16 = 3;
const EVENT_LOG_DEFAULT_HEIGHT: u16 = 6;
const EVENT_LOG_MAX_HEIGHT: u16 = 16;

#[derive(Parser, Debug)]
#[command(name = "cephlens")]
#[command(about = "A small Ceph investigation TUI prototype")]
struct Cli {
    #[arg(long, global = true, default_value = "cephlens.toml")]
    config: PathBuf,

    #[arg(long, global = true)]
    profile: Option<String>,

    #[arg(long, global = true, help = "Override profile hosts, comma-separated")]
    hosts: Option<String>,

    #[arg(long, global = true, help = "Override the Ceph admin host")]
    admin_host: Option<String>,

    #[arg(long, global = true)]
    refresh_secs: Option<u64>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Tui,
    InitConfig {
        #[arg(long)]
        force: bool,
    },
    Snapshot,
    Probe,
    Record {
        #[arg(long, default_value_t = 3)]
        count: u64,
        #[arg(long, default_value_t = 2)]
        interval_secs: u64,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Bench {
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    Replay {
        file: PathBuf,
    },
}

#[derive(Clone, Debug)]
enum WorkerMsg {
    Snapshot(Box<Result<Snapshot, String>>),
    Probe(String),
    Stream(StreamMsg),
    TraceProbe(Vec<TraceTarget>),
    TraceInstall(Vec<TraceTarget>),
    TraceLine { host: String, line: String },
    TraceDone { host: String, message: String },
}

#[derive(Clone, Debug)]
enum StreamMsg {
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
struct StreamStatus {
    state: StreamState,
    last_seen: Option<DateTime<Utc>>,
    detail: String,
    reconnects: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum StreamState {
    Connecting,
    Live,
    Reconnecting,
    Error,
}

#[derive(Clone, Debug)]
enum Mode {
    Live,
    Config,
    Trace,
    Replay {
        index: usize,
        snapshots: Vec<Snapshot>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfigDraft {
    profile: String,
    admin_host: String,
    hosts: Vec<String>,
    refresh_secs: u64,
}

#[derive(Clone, Debug)]
struct ConfigEditor {
    draft: ConfigDraft,
    selected: usize,
    input: Option<EditorInput>,
    dirty: bool,
    message: String,
}

#[derive(Clone, Debug)]
struct EditorInput {
    action: EditorAction,
    label: String,
    buffer: String,
}

#[derive(Clone, Debug)]
enum EditorAction {
    SetAdminHost,
    SetRefreshSecs,
    AddHost,
    EditHost { index: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigSelection {
    AdminHost,
    RefreshSecs,
    Host(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelFocus {
    Nodes,
    Osds,
    Trace,
    Logs,
    Targets,
}

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

struct App {
    profile: String,
    hosts: Vec<String>,
    admin_host: String,
    config_path: Option<PathBuf>,
    config_editor: ConfigEditor,
    refresh: Duration,
    mode: Mode,
    snapshot: Option<Snapshot>,
    collecting: bool,
    confirm_quit: bool,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    logs: Vec<String>,
    event_log_height: u16,
    focused_panel: PanelFocus,
    nodes_scroll: usize,
    osds_scroll: usize,
    trace_scroll: usize,
    logs_scroll: usize,
    targets_scroll: usize,
    node_summaries: HashMap<String, NodeSummary>,
    stream_statuses: HashMap<String, StreamStatus>,
    trace_targets: Vec<TraceTarget>,
    trace_events: Vec<TraceEvent>,
    trace_series: HashMap<String, VecDeque<TraceBucket>>,
    trace_active: usize,
    trace_following: bool,
    trace_session: Option<String>,
    trace_auto_start: bool,
    trace_window_secs: u64,
    trace_latency_ms: u64,
    trace_ttl_secs: u64,
    trace_install: TraceInstallConfig,
    trace_stop: Arc<AtomicBool>,
    stream_stop: Arc<AtomicBool>,
    session_path: Option<PathBuf>,
    last_refresh: Instant,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Commands::InitConfig { force }) = &cli.command {
        return write_default_config(&cli.config, *force);
    }
    let cfg = resolve_config(&cli)?;

    let config_path = cli.config.clone();
    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Tui => run_live_tui(config_path, cfg),
        Commands::InitConfig { .. } => unreachable!("handled before config resolution"),
        Commands::Snapshot => {
            let snapshot = collect_snapshot(&cfg)?;
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
            Ok(())
        }
        Commands::Probe => {
            println!("{}", run_probe(&cfg.hosts));
            Ok(())
        }
        Commands::Record {
            count,
            interval_secs,
            out,
        } => {
            let path = out.unwrap_or(create_session_path()?);
            for i in 0..count {
                let snapshot = collect_snapshot(&cfg)?;
                append_snapshot(&path, &snapshot)?;
                println!(
                    "recorded {}/{} {} {}",
                    i + 1,
                    count,
                    snapshot.cluster.health,
                    path.display()
                );
                if i + 1 < count {
                    thread::sleep(Duration::from_secs(interval_secs));
                }
            }
            Ok(())
        }
        Commands::Bench { host, seconds } => {
            let output = run_bench(&host, seconds)?;
            println!("{output}");
            Ok(())
        }
        Commands::Replay { file } => run_replay_tui(file),
    }
}

fn resolve_config(cli: &Cli) -> Result<ResolvedConfig> {
    let file = load_config_file(&cli.config)?;
    let profile_name = cli
        .profile
        .clone()
        .or_else(|| file.as_ref().and_then(|cfg| cfg.default_profile.clone()))
        .or_else(|| {
            file.as_ref()
                .and_then(|cfg| cfg.profiles.keys().next().cloned())
        })
        .unwrap_or_else(|| "example".to_owned());

    let profile = file
        .as_ref()
        .and_then(|cfg| cfg.profiles.get(&profile_name));
    if cli.profile.is_some() && profile.is_none() {
        return Err(anyhow!(
            "profile '{}' was not found in {}",
            profile_name,
            cli.config.display()
        ));
    }

    let mut hosts = profile
        .map(|profile| profile.hosts.clone())
        .unwrap_or_else(default_hosts);
    if let Some(override_hosts) = &cli.hosts {
        hosts = parse_hosts(override_hosts);
    }
    if hosts.is_empty() {
        return Err(anyhow!("host list is empty"));
    }

    let admin_host = cli
        .admin_host
        .clone()
        .or_else(|| profile.map(|profile| profile.admin_host.clone()))
        .unwrap_or_else(|| hosts[0].clone());

    let refresh_secs = cli
        .refresh_secs
        .or_else(|| profile.and_then(|profile| profile.refresh_secs))
        .unwrap_or(1)
        .max(1);
    let trace_auto_start = profile
        .and_then(|profile| profile.trace_auto_start)
        .unwrap_or(false);
    let trace_window_secs = profile
        .and_then(|profile| profile.trace_window_secs)
        .unwrap_or(10)
        .max(1);
    let trace_latency_ms = profile
        .and_then(|profile| profile.trace_latency_ms)
        .unwrap_or(1);
    let trace_ttl_secs = profile
        .and_then(|profile| profile.trace_ttl_secs)
        .unwrap_or(DEFAULT_TRACE_TTL_SECS)
        .max(1);
    let osdtrace_url = profile.and_then(|profile| clean_optional(&profile.osdtrace_url));
    let osdtrace_sha256 = profile.and_then(|profile| clean_optional(&profile.osdtrace_sha256));
    if let Some(sha256) = &osdtrace_sha256 {
        validate_sha256(sha256)?;
    }
    let trace_install = TraceInstallConfig {
        url: osdtrace_url,
        sha256: osdtrace_sha256,
        allow_unverified: profile
            .and_then(|profile| profile.osdtrace_allow_unverified)
            .unwrap_or(false),
    };

    Ok(ResolvedConfig {
        profile: profile_name,
        admin_host,
        hosts,
        refresh_secs,
        trace_auto_start,
        trace_window_secs,
        trace_latency_ms,
        trace_ttl_secs,
        trace_install,
    })
}

fn save_profile_config(path: &Path, draft: &ConfigDraft) -> Result<()> {
    validate_config_draft(draft)?;
    let mut config = load_config_file(path)?.unwrap_or_else(|| ConfigFile {
        default_profile: Some(draft.profile.clone()),
        profiles: BTreeMap::new(),
    });
    if config.default_profile.is_none() {
        config.default_profile = Some(draft.profile.clone());
    }
    let existing_profile = config.profiles.get(&draft.profile).cloned();
    config.profiles.insert(
        draft.profile.clone(),
        ClusterProfile {
            admin_host: draft.admin_host.clone(),
            hosts: draft.hosts.clone(),
            refresh_secs: Some(draft.refresh_secs.max(1)),
            trace_auto_start: existing_profile
                .as_ref()
                .and_then(|profile| profile.trace_auto_start),
            trace_window_secs: existing_profile
                .as_ref()
                .and_then(|profile| profile.trace_window_secs),
            trace_latency_ms: existing_profile
                .as_ref()
                .and_then(|profile| profile.trace_latency_ms),
            trace_ttl_secs: existing_profile
                .as_ref()
                .and_then(|profile| profile.trace_ttl_secs),
            osdtrace_url: existing_profile
                .as_ref()
                .and_then(|profile| profile.osdtrace_url.clone()),
            osdtrace_sha256: existing_profile
                .as_ref()
                .and_then(|profile| profile.osdtrace_sha256.clone()),
            osdtrace_allow_unverified: existing_profile
                .as_ref()
                .and_then(|profile| profile.osdtrace_allow_unverified),
        },
    );
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(&config)?;
    fs::write(path, raw).with_context(|| format!("failed to write config {}", path.display()))?;
    Ok(())
}

fn validate_config_draft(draft: &ConfigDraft) -> Result<()> {
    if draft.profile.trim().is_empty() {
        return Err(anyhow!("profile is empty"));
    }
    if draft.admin_host.trim().is_empty() {
        return Err(anyhow!("admin host is empty"));
    }
    if draft.hosts.is_empty() {
        return Err(anyhow!("host list is empty"));
    }
    if draft.refresh_secs == 0 {
        return Err(anyhow!("refresh interval must be at least 1 second"));
    }
    Ok(())
}

fn run_live_tui(config_path: PathBuf, cfg: ResolvedConfig) -> Result<()> {
    let session_path = create_session_path()?;
    let (tx, rx) = mpsc::channel();
    let config_editor = ConfigEditor::new(ConfigDraft::from_resolved(&cfg));
    let mut app = App {
        profile: cfg.profile,
        hosts: cfg.hosts,
        admin_host: cfg.admin_host,
        config_path: Some(config_path),
        config_editor,
        refresh: Duration::from_secs(cfg.refresh_secs),
        mode: Mode::Live,
        snapshot: None,
        collecting: false,
        confirm_quit: false,
        tx,
        rx,
        logs: Vec::new(),
        event_log_height: EVENT_LOG_DEFAULT_HEIGHT,
        focused_panel: PanelFocus::Osds,
        nodes_scroll: 0,
        osds_scroll: 0,
        trace_scroll: 0,
        logs_scroll: 0,
        targets_scroll: 0,
        node_summaries: HashMap::new(),
        stream_statuses: HashMap::new(),
        trace_targets: Vec::new(),
        trace_events: Vec::new(),
        trace_series: HashMap::new(),
        trace_active: 0,
        trace_following: false,
        trace_session: None,
        trace_auto_start: cfg.trace_auto_start,
        trace_window_secs: cfg.trace_window_secs,
        trace_latency_ms: cfg.trace_latency_ms,
        trace_ttl_secs: cfg.trace_ttl_secs,
        trace_install: cfg.trace_install,
        trace_stop: Arc::new(AtomicBool::new(false)),
        stream_stop: Arc::new(AtomicBool::new(false)),
        session_path: Some(session_path),
        last_refresh: Instant::now() - Duration::from_secs(cfg.refresh_secs),
    };
    app.log("cephlens live session started");
    start_live_streams(&mut app);
    if app.trace_auto_start {
        let latency_ms = app.trace_latency_ms;
        spawn_trace_run(&mut app, latency_ms);
    }
    with_terminal(|terminal| run_app(terminal, app))
}

fn run_replay_tui(file: PathBuf) -> Result<()> {
    let snapshots = load_snapshots(&file)?;
    let snapshot = snapshots.last().cloned();
    let index = snapshots.len().saturating_sub(1);
    let (tx, rx) = mpsc::channel();
    let fallback = ResolvedConfig {
        profile: snapshot
            .as_ref()
            .map(|s| s.profile.clone())
            .unwrap_or_else(|| "replay".to_owned()),
        admin_host: snapshot
            .as_ref()
            .map(|s| s.admin_host.clone())
            .unwrap_or_default(),
        hosts: snapshot
            .as_ref()
            .map(|s| s.hosts.clone())
            .unwrap_or_default(),
        refresh_secs: 1,
        trace_auto_start: false,
        trace_window_secs: 10,
        trace_latency_ms: 1,
        trace_ttl_secs: DEFAULT_TRACE_TTL_SECS,
        trace_install: TraceInstallConfig::default(),
    };
    let mut app = App {
        profile: fallback.profile.clone(),
        hosts: fallback.hosts.clone(),
        admin_host: fallback.admin_host.clone(),
        config_path: None,
        config_editor: ConfigEditor::new(ConfigDraft::from_resolved(&fallback)),
        refresh: Duration::from_secs(0),
        mode: Mode::Replay { index, snapshots },
        snapshot,
        collecting: false,
        confirm_quit: false,
        tx,
        rx,
        logs: vec![format!("replay loaded from {}", file.display())],
        event_log_height: EVENT_LOG_DEFAULT_HEIGHT,
        focused_panel: PanelFocus::Osds,
        nodes_scroll: 0,
        osds_scroll: 0,
        trace_scroll: 0,
        logs_scroll: 0,
        targets_scroll: 0,
        node_summaries: HashMap::new(),
        stream_statuses: HashMap::new(),
        trace_targets: Vec::new(),
        trace_events: Vec::new(),
        trace_series: HashMap::new(),
        trace_active: 0,
        trace_following: false,
        trace_session: None,
        trace_auto_start: false,
        trace_window_secs: 10,
        trace_latency_ms: 1,
        trace_ttl_secs: DEFAULT_TRACE_TTL_SECS,
        trace_install: TraceInstallConfig::default(),
        trace_stop: Arc::new(AtomicBool::new(false)),
        stream_stop: Arc::new(AtomicBool::new(false)),
        session_path: None,
        last_refresh: Instant::now(),
    };
    if let Mode::Replay { snapshots, .. } = &app.mode {
        app.log(format!("{} snapshots available", snapshots.len()));
    }
    with_terminal(|terminal| run_app(terminal, app))
}

fn with_terminal<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Terminal<CrosstermBackend<Stdout>>) -> Result<Vec<CleanupResult>>,
{
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = f(&mut terminal);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    if let Ok(results) = &result {
        report_cleanup_results(results);
    }
    result.map(|_| ())
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
) -> Result<Vec<CleanupResult>> {
    loop {
        drain_worker_messages(&mut app);

        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(Duration::from_millis(150))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(shutdown_streams(&app, true));
                    }

                    if handle_key(&mut app, key)? {
                        return Ok(shutdown_streams(&app, true));
                    }
                }
                Event::Resize(_, _) => {
                    terminal.clear()?;
                }
                _ => {}
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    if app.confirm_quit {
        return Ok(handle_quit_confirm(app, key));
    }

    if matches!(app.mode, Mode::Config) && app.config_editor.input.is_some() {
        handle_config_input(app, key);
        return Ok(false);
    }

    if handle_global_key(app, key) {
        return Ok(false);
    }

    if matches!(app.mode, Mode::Live | Mode::Trace) && handle_panel_key(app, key) {
        return Ok(false);
    }

    match app.mode.clone() {
        Mode::Live => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                request_quit(app);
                Ok(false)
            }
            KeyCode::Char('r') => {
                spawn_snapshot(app);
                Ok(false)
            }
            KeyCode::Char('p') => {
                spawn_probe(app);
                Ok(false)
            }
            KeyCode::Char('c') => {
                open_config_editor(app);
                Ok(false)
            }
            KeyCode::Char('t') => {
                let latency_ms = app.trace_latency_ms.max(1);
                spawn_trace_run(app, latency_ms);
                Ok(false)
            }
            KeyCode::Char('0') => {
                spawn_trace_run(app, 0);
                Ok(false)
            }
            KeyCode::Char('s') => {
                stop_trace_follow(app);
                Ok(false)
            }
            KeyCode::Char('z') => {
                stop_trace_follow(app);
                Ok(false)
            }
            KeyCode::Char('i') => {
                spawn_trace_install(app);
                Ok(false)
            }
            KeyCode::Char('x') => {
                app.trace_events.clear();
                app.trace_series.clear();
                app.log("trace graph cleared");
                Ok(false)
            }
            KeyCode::Char('v') => {
                app.mode = Mode::Trace;
                if app.trace_targets.is_empty() {
                    spawn_trace_probe(app);
                }
                Ok(false)
            }
            _ => Ok(false),
        },
        Mode::Config => handle_config_key(app, key),
        Mode::Trace => handle_trace_key(app, key),
        Mode::Replay { .. } => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                request_quit(app);
                Ok(false)
            }
            KeyCode::Left => {
                replay_move(app, -1);
                Ok(false)
            }
            KeyCode::Right => {
                replay_move(app, 1);
                Ok(false)
            }
            _ => Ok(false),
        },
    }
}

fn handle_global_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(']') | KeyCode::Char('+') | KeyCode::Char('=') => {
            adjust_event_log_height(app, 1);
            true
        }
        KeyCode::Char('[') | KeyCode::Char('-') => {
            adjust_event_log_height(app, -1);
            true
        }
        _ => false,
    }
}

fn handle_panel_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Tab => {
            focus_next_panel(app, 1);
            true
        }
        KeyCode::BackTab => {
            focus_next_panel(app, -1);
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            scroll_focused_panel(app, 1);
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            scroll_focused_panel(app, -1);
            true
        }
        KeyCode::PageDown => {
            scroll_focused_panel(app, 8);
            true
        }
        KeyCode::PageUp => {
            scroll_focused_panel(app, -8);
            true
        }
        KeyCode::Home => {
            if app.focused_panel == PanelFocus::Logs {
                set_focused_panel_scroll(app, usize::MAX);
            } else {
                set_focused_panel_scroll(app, 0);
            }
            true
        }
        KeyCode::End => {
            if app.focused_panel == PanelFocus::Logs {
                set_focused_panel_scroll(app, 0);
            } else {
                set_focused_panel_scroll(app, usize::MAX);
            }
            true
        }
        _ => false,
    }
}

fn focus_next_panel(app: &mut App, delta: i32) {
    let panels = focusable_panels(app);
    let current = panels
        .iter()
        .position(|panel| *panel == app.focused_panel)
        .unwrap_or(0) as i32;
    let len = panels.len() as i32;
    let next = (current + delta).rem_euclid(len) as usize;
    app.focused_panel = panels[next];
}

fn focusable_panels(app: &App) -> &'static [PanelFocus] {
    if matches!(app.mode, Mode::Trace) {
        &[
            PanelFocus::Targets,
            PanelFocus::Trace,
            PanelFocus::Logs,
            PanelFocus::Osds,
            PanelFocus::Nodes,
        ]
    } else {
        &[
            PanelFocus::Osds,
            PanelFocus::Trace,
            PanelFocus::Logs,
            PanelFocus::Nodes,
        ]
    }
}

fn scroll_focused_panel(app: &mut App, delta: isize) {
    let delta = if app.focused_panel == PanelFocus::Logs {
        -delta
    } else {
        delta
    };
    let scroll = focused_scroll_mut(app);
    *scroll = scroll_with_delta(*scroll, delta);
}

fn set_focused_panel_scroll(app: &mut App, value: usize) {
    *focused_scroll_mut(app) = value;
}

fn focused_scroll_mut(app: &mut App) -> &mut usize {
    match app.focused_panel {
        PanelFocus::Nodes => &mut app.nodes_scroll,
        PanelFocus::Osds => &mut app.osds_scroll,
        PanelFocus::Trace => &mut app.trace_scroll,
        PanelFocus::Logs => &mut app.logs_scroll,
        PanelFocus::Targets => &mut app.targets_scroll,
    }
}

fn scroll_with_delta(current: usize, delta: isize) -> usize {
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize)
    }
}

fn adjust_event_log_height(app: &mut App, delta: i16) {
    let before = app.event_log_height;
    let next = (app.event_log_height as i16 + delta)
        .clamp(EVENT_LOG_MIN_HEIGHT as i16, EVENT_LOG_MAX_HEIGHT as i16) as u16;
    app.event_log_height = next;
    if next != before {
        app.log(format!("event log height: {next} rows"));
    }
}

fn request_quit(app: &mut App) {
    app.confirm_quit = true;
}

fn handle_quit_confirm(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => true,
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
            app.confirm_quit = false;
            false
        }
        _ => false,
    }
}

fn handle_trace_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('q') => {
            request_quit(app);
            Ok(false)
        }
        KeyCode::Esc | KeyCode::Char('c') => {
            app.mode = Mode::Live;
            Ok(false)
        }
        KeyCode::Char('p') => {
            spawn_trace_probe(app);
            Ok(false)
        }
        KeyCode::Char('i') => {
            spawn_trace_install(app);
            Ok(false)
        }
        KeyCode::Char('r') => {
            let latency_ms = app.trace_latency_ms.max(1);
            spawn_trace_run(app, latency_ms);
            Ok(false)
        }
        KeyCode::Char('0') => {
            spawn_trace_run(app, 0);
            Ok(false)
        }
        KeyCode::Char('s') => {
            stop_trace_follow(app);
            Ok(false)
        }
        KeyCode::Char('z') => {
            stop_trace_follow(app);
            Ok(false)
        }
        KeyCode::Char('x') => {
            app.trace_events.clear();
            app.trace_series.clear();
            app.log("trace graph cleared");
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn start_live_streams(app: &mut App) {
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

fn shutdown_streams(app: &App, wait_for_cleanup: bool) -> Vec<CleanupResult> {
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
    if live_streams_active(app) {
        thread::sleep(Duration::from_millis(1200));
    }
    cleanup
}

fn live_streams_active(app: &App) -> bool {
    matches!(app.mode, Mode::Live | Mode::Config | Mode::Trace)
}

fn open_config_editor(app: &mut App) {
    app.config_editor = ConfigEditor::new(ConfigDraft::from_app(app));
    app.config_editor.message = "editing live config; changes apply immediately".to_owned();
    app.mode = Mode::Config;
}

fn handle_config_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('q') => {
            request_quit(app);
            Ok(false)
        }
        KeyCode::Esc | KeyCode::Char('c') => {
            app.config_editor.input = None;
            app.config_editor.message.clear();
            app.mode = Mode::Live;
            Ok(false)
        }
        KeyCode::Up => {
            app.config_editor.select_prev();
            Ok(false)
        }
        KeyCode::Down => {
            app.config_editor.select_next();
            Ok(false)
        }
        KeyCode::Char('a') => {
            app.config_editor.start_input(
                EditorAction::AddHost,
                "add host".to_owned(),
                String::new(),
            );
            Ok(false)
        }
        KeyCode::Char('e') | KeyCode::Enter => {
            start_edit_selected_config(app);
            Ok(false)
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            delete_selected_config_host(app);
            Ok(false)
        }
        KeyCode::Char('s') => {
            persist_and_apply_config(app);
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn handle_config_input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.config_editor.input = None;
            app.config_editor.message = "input cancelled".to_owned();
        }
        KeyCode::Enter => finish_config_input(app),
        KeyCode::Backspace => {
            if let Some(input) = app.config_editor.input.as_mut() {
                input.buffer.pop();
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(input) = app.config_editor.input.as_mut() {
                input.buffer.clear();
            }
        }
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            if let Some(input) = app.config_editor.input.as_mut() {
                input.buffer.push(ch);
            }
        }
        _ => {}
    }
}

fn start_edit_selected_config(app: &mut App) {
    match app.config_editor.selection() {
        ConfigSelection::AdminHost => {
            let value = app.config_editor.draft.admin_host.clone();
            app.config_editor.start_input(
                EditorAction::SetAdminHost,
                "admin host".to_owned(),
                value,
            );
        }
        ConfigSelection::RefreshSecs => {
            app.config_editor.start_input(
                EditorAction::SetRefreshSecs,
                "refresh secs".to_owned(),
                app.config_editor.draft.refresh_secs.to_string(),
            );
        }
        ConfigSelection::Host(index) => {
            let Some(value) = app.config_editor.draft.hosts.get(index).cloned() else {
                app.config_editor.message = "no host selected".to_owned();
                return;
            };
            app.config_editor.start_input(
                EditorAction::EditHost { index },
                format!("host {}", index + 1),
                value,
            );
        }
    }
}

fn finish_config_input(app: &mut App) {
    let Some(input) = app.config_editor.input.take() else {
        return;
    };
    match apply_editor_input(&mut app.config_editor, &input) {
        Ok(()) => persist_and_apply_config(app),
        Err(err) => {
            app.config_editor.message = err.to_string();
            app.config_editor.input = Some(input);
        }
    }
}

fn apply_editor_input(editor: &mut ConfigEditor, input: &EditorInput) -> Result<()> {
    let value = input.buffer.trim();
    match input.action {
        EditorAction::SetAdminHost => {
            if value.is_empty() {
                return Err(anyhow!("admin host is empty"));
            }
            editor.draft.admin_host = value.to_owned();
        }
        EditorAction::SetRefreshSecs => {
            let refresh_secs = value
                .parse::<u64>()
                .with_context(|| format!("invalid refresh interval '{value}'"))?
                .max(1);
            editor.draft.refresh_secs = refresh_secs;
        }
        EditorAction::AddHost => {
            if value.is_empty() {
                return Err(anyhow!("host is empty"));
            }
            if editor.draft.hosts.iter().any(|host| host == value) {
                return Err(anyhow!("host '{value}' already exists"));
            }
            editor.draft.hosts.push(value.to_owned());
            editor.selected = 1 + editor.draft.hosts.len();
        }
        EditorAction::EditHost { index } => {
            if value.is_empty() {
                return Err(anyhow!("host is empty"));
            }
            if editor
                .draft
                .hosts
                .iter()
                .enumerate()
                .any(|(i, host)| i != index && host == value)
            {
                return Err(anyhow!("host '{value}' already exists"));
            }
            let Some(host) = editor.draft.hosts.get_mut(index) else {
                return Err(anyhow!("host no longer exists"));
            };
            if editor.draft.admin_host == *host {
                editor.draft.admin_host = value.to_owned();
            }
            *host = value.to_owned();
        }
    }
    editor.draft.hosts = normalize_hosts(editor.draft.hosts.iter().map(String::as_str));
    editor.dirty = true;
    editor.clamp_selection();
    Ok(())
}

fn delete_selected_config_host(app: &mut App) {
    let ConfigSelection::Host(index) = app.config_editor.selection() else {
        app.config_editor.message = "select a host row to delete".to_owned();
        return;
    };
    if app.config_editor.draft.hosts.len() <= 1 {
        app.config_editor.message = "at least one host is required".to_owned();
        return;
    }
    if index >= app.config_editor.draft.hosts.len() {
        app.config_editor.clamp_selection();
        return;
    }
    let removed = app.config_editor.draft.hosts.remove(index);
    if app.config_editor.draft.admin_host == removed {
        app.config_editor.draft.admin_host = app
            .config_editor
            .draft
            .hosts
            .first()
            .cloned()
            .unwrap_or_default();
    }
    app.config_editor.dirty = true;
    app.config_editor.clamp_selection();
    app.config_editor.message = format!("removed {removed}");
    persist_and_apply_config(app);
}

fn persist_and_apply_config(app: &mut App) {
    let Some(path) = app.config_path.clone() else {
        app.config_editor.message = "replay sessions cannot be saved as config".to_owned();
        return;
    };
    let draft = app.config_editor.draft.clone();
    if let Err(err) = save_profile_config(&path, &draft) {
        app.config_editor.message = format!("{err:#}");
        return;
    }

    let current = ConfigDraft::from_app(app);
    let changed = current != draft;
    if changed {
        let _ = shutdown_streams(app, false);
        app.profile = draft.profile.clone();
        app.admin_host = draft.admin_host.clone();
        app.hosts = draft.hosts.clone();
        app.refresh = Duration::from_secs(draft.refresh_secs.max(1));
        app.stream_stop = Arc::new(AtomicBool::new(false));
        app.trace_stop = Arc::new(AtomicBool::new(false));
        app.trace_following = false;
        app.trace_active = 0;
        app.trace_session = None;
        app.stream_statuses.clear();
        app.node_summaries.clear();
        app.snapshot = None;
        app.collecting = false;
        app.last_refresh = Instant::now() - app.refresh;
        start_live_streams(app);
    }

    app.config_editor.draft = ConfigDraft::from_app(app);
    app.config_editor.dirty = false;
    app.config_editor.clamp_selection();
    app.config_editor.message = format!("saved {} and refreshed ssh streams", path.display());
    app.log(format!("config saved to {}", path.display()));
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

fn drain_worker_messages(app: &mut App) {
    while let Ok(msg) = app.rx.try_recv() {
        match msg {
            WorkerMsg::Snapshot(result) => {
                app.collecting = false;
                app.last_refresh = Instant::now();
                match *result {
                    Ok(snapshot) => {
                        if let Some(path) = &app.session_path {
                            match append_snapshot(path, &snapshot) {
                                Ok(()) => {
                                    app.log(format!("snapshot recorded to {}", path.display()))
                                }
                                Err(err) => app.log(format!("record failed: {err:#}")),
                            }
                        }
                        app.log(format!(
                            "snapshot ok: {} {} osds {}/{} up/in",
                            snapshot.cluster.health,
                            snapshot.cluster.pg_states,
                            snapshot.cluster.osds_up,
                            snapshot.cluster.osds_in
                        ));
                        app.snapshot = Some(snapshot);
                    }
                    Err(err) => app.log(format!("snapshot failed: {err}")),
                }
            }
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
        if let Some(path) = &app.session_path
            && let Err(err) = append_snapshot(path, &snapshot)
        {
            app.log(format!("record failed: {err:#}"));
        }
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

fn spawn_snapshot(app: &mut App) {
    if app.collecting {
        return;
    }
    app.collecting = true;
    app.log("snapshot requested");
    let tx = app.tx.clone();
    let cfg = ResolvedConfig {
        profile: app.profile.clone(),
        admin_host: app.admin_host.clone(),
        hosts: app.hosts.clone(),
        refresh_secs: app.refresh.as_secs().max(1),
        trace_auto_start: app.trace_auto_start,
        trace_window_secs: app.trace_window_secs,
        trace_latency_ms: app.trace_latency_ms,
        trace_ttl_secs: app.trace_ttl_secs,
        trace_install: app.trace_install.clone(),
    };
    thread::spawn(move || {
        let result = collect_snapshot(&cfg).map_err(|err| format!("{err:#}"));
        let _ = tx.send(WorkerMsg::Snapshot(Box::new(result)));
    });
}

fn spawn_probe(app: &mut App) {
    app.log("probe readiness check requested");
    let tx = app.tx.clone();
    let hosts = app.hosts.clone();
    thread::spawn(move || {
        let _ = tx.send(WorkerMsg::Probe(run_probe(&hosts)));
    });
}

fn spawn_trace_probe(app: &mut App) {
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

fn spawn_trace_install(app: &mut App) {
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

fn spawn_trace_run(app: &mut App, latency_ms: u64) {
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
    app.trace_latency_ms = latency_ms;
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

fn stop_trace_follow(app: &mut App) {
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

fn trace_session_id() -> String {
    format!("{}-{}", Utc::now().timestamp(), std::process::id())
}

fn replay_move(app: &mut App, delta: isize) {
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

fn draw(frame: &mut Frame<'_>, app: &App) {
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

impl ConfigDraft {
    fn from_resolved(cfg: &ResolvedConfig) -> Self {
        Self {
            profile: cfg.profile.clone(),
            admin_host: cfg.admin_host.clone(),
            hosts: cfg.hosts.clone(),
            refresh_secs: cfg.refresh_secs.max(1),
        }
    }

    fn from_app(app: &App) -> Self {
        Self {
            profile: app.profile.clone(),
            admin_host: app.admin_host.clone(),
            hosts: app.hosts.clone(),
            refresh_secs: app.refresh.as_secs().max(1),
        }
    }
}

impl ConfigEditor {
    fn new(draft: ConfigDraft) -> Self {
        Self {
            draft,
            selected: 0,
            input: None,
            dirty: false,
            message: String::new(),
        }
    }

    fn selection_count(&self) -> usize {
        2 + self.draft.hosts.len()
    }

    fn selection(&self) -> ConfigSelection {
        match self.selected {
            0 => ConfigSelection::AdminHost,
            1 => ConfigSelection::RefreshSecs,
            index => ConfigSelection::Host(index.saturating_sub(2)),
        }
    }

    fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.selection_count().saturating_sub(1));
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.selection_count().saturating_sub(1));
    }

    fn start_input(&mut self, action: EditorAction, label: String, buffer: String) {
        self.input = Some(EditorInput {
            action,
            label,
            buffer,
        });
        self.message.clear();
    }
}

impl App {
    fn log(&mut self, message: impl Into<String>) {
        let stamp = Local::now().format("%H:%M:%S");
        self.logs
            .push(format!("[{stamp}] {}", message.into().replace('\n', " ")));
        if self.logs.len() > 400 {
            self.logs.drain(0..100);
        }
    }
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
