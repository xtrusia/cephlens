use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

mod app;
mod collect;
mod config;
mod editor;
mod model;
mod runner;
mod session;
mod ssh;
mod stream;
mod trace;
mod ui;
mod util;

use app::{
    App, EVENT_LOG_DEFAULT_HEIGHT, EVENT_LOG_MAX_HEIGHT, EVENT_LOG_MIN_HEIGHT, Mode, PanelFocus,
    drain_worker_messages, replay_move, request_quit, shutdown_streams, spawn_probe,
    spawn_snapshot, spawn_trace_install, spawn_trace_probe, spawn_trace_run, start_live_streams,
    stop_trace_follow,
};
use collect::{collect_snapshot, run_bench, run_probe};
use config::{
    DEFAULT_TRACE_TTL_SECS, ResolvedConfig, clean_optional, default_hosts, load_config_file,
    parse_hosts, write_default_config,
};
use editor::{
    ConfigDraft, ConfigEditor, handle_config_input, handle_config_key, open_config_editor,
};
use runner::{CleanupResult, report_cleanup_results};
use session::{append_snapshot, create_session_path, load_snapshots};
use trace::{TraceInstallConfig, validate_sha256};

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
        session_records: 0,
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
        session_records: 0,
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

        terminal.draw(|frame| ui::draw(frame, &app))?;

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
        KeyCode::Char('x') => {
            app.trace_events.clear();
            app.trace_series.clear();
            app.log("trace graph cleared");
            Ok(false)
        }
        _ => Ok(false),
    }
}
