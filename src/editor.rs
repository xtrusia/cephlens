use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    app::{App, Mode, request_quit, shutdown_streams, start_live_streams},
    config::{
        ClusterProfile, ConfigFile, ResolvedConfig, load_config_file, normalize_hosts,
        validate_ssh_destination, validate_ssh_destinations,
    },
    trace::{TraceInstallConfig, validate_sha256},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigDraft {
    pub(crate) profile: String,
    pub(crate) admin_host: String,
    pub(crate) hosts: Vec<String>,
    pub(crate) client_hosts: Vec<String>,
    pub(crate) refresh_secs: u64,
    pub(crate) trace_auto_start: bool,
    pub(crate) trace_window_secs: u64,
    pub(crate) trace_latency_ms: u64,
    pub(crate) trace_ttl_secs: u64,
    pub(crate) osdtrace_url: String,
    pub(crate) osdtrace_sha256: String,
    pub(crate) osdtrace_allow_unverified: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigEditor {
    pub(crate) draft: ConfigDraft,
    pub(crate) selected: usize,
    pub(crate) input: Option<EditorInput>,
    pub(crate) dirty: bool,
    pub(crate) message: String,
}

#[derive(Clone, Debug)]
pub(crate) struct EditorInput {
    pub(crate) action: EditorAction,
    pub(crate) label: String,
    pub(crate) buffer: String,
}

#[derive(Clone, Debug)]
pub(crate) enum EditorAction {
    SetAdminHost,
    SetRefreshSecs,
    SetTraceWindowSecs,
    SetTraceLatencyMs,
    SetTraceTtlSecs,
    SetOsdtraceUrl,
    SetOsdtraceSha256,
    AddHost,
    EditHost { index: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigSelection {
    AdminHost,
    RefreshSecs,
    TraceAutoStart,
    TraceWindowSecs,
    TraceLatencyMs,
    TraceTtlSecs,
    OsdtraceUrl,
    OsdtraceSha256,
    OsdtraceAllowUnverified,
    Host(usize),
}

impl ConfigDraft {
    pub(crate) const FIXED_ROWS: usize = 9;

    pub(crate) fn from_resolved(cfg: &ResolvedConfig) -> Self {
        Self {
            profile: cfg.profile.clone(),
            admin_host: cfg.admin_host.clone(),
            hosts: cfg.hosts.clone(),
            client_hosts: cfg.client_hosts.clone(),
            refresh_secs: cfg.refresh_secs.max(1),
            trace_auto_start: cfg.trace_auto_start,
            trace_window_secs: cfg.trace_window_secs.max(1),
            trace_latency_ms: cfg.trace_latency_ms,
            trace_ttl_secs: cfg.trace_ttl_secs.max(1),
            osdtrace_url: cfg.trace_install.url.clone().unwrap_or_default(),
            osdtrace_sha256: cfg.trace_install.sha256.clone().unwrap_or_default(),
            osdtrace_allow_unverified: cfg.trace_install.allow_unverified,
        }
    }

    fn from_app(app: &App) -> Self {
        Self {
            profile: app.profile.clone(),
            admin_host: app.admin_host.clone(),
            hosts: app.hosts.clone(),
            client_hosts: app.client_hosts.clone(),
            refresh_secs: app.refresh.as_secs().max(1),
            trace_auto_start: app.trace_auto_start,
            trace_window_secs: app.trace_window_secs.max(1),
            trace_latency_ms: app.trace_latency_ms,
            trace_ttl_secs: app.trace_ttl_secs.max(1),
            osdtrace_url: app.trace_install.url.clone().unwrap_or_default(),
            osdtrace_sha256: app.trace_install.sha256.clone().unwrap_or_default(),
            osdtrace_allow_unverified: app.trace_install.allow_unverified,
        }
    }
}

impl ConfigEditor {
    pub(crate) fn new(draft: ConfigDraft) -> Self {
        Self {
            draft,
            selected: 0,
            input: None,
            dirty: false,
            message: String::new(),
        }
    }

    fn selection_count(&self) -> usize {
        ConfigDraft::FIXED_ROWS + self.draft.hosts.len()
    }

    fn selection(&self) -> ConfigSelection {
        match self.selected {
            0 => ConfigSelection::AdminHost,
            1 => ConfigSelection::RefreshSecs,
            2 => ConfigSelection::TraceAutoStart,
            3 => ConfigSelection::TraceWindowSecs,
            4 => ConfigSelection::TraceLatencyMs,
            5 => ConfigSelection::TraceTtlSecs,
            6 => ConfigSelection::OsdtraceUrl,
            7 => ConfigSelection::OsdtraceSha256,
            8 => ConfigSelection::OsdtraceAllowUnverified,
            index => ConfigSelection::Host(index.saturating_sub(ConfigDraft::FIXED_ROWS)),
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

pub(crate) fn open_config_editor(app: &mut App) {
    app.config_editor = ConfigEditor::new(ConfigDraft::from_app(app));
    app.config_editor.message = "editing live config; changes apply immediately".to_owned();
    app.mode = Mode::Config;
}

pub(crate) fn handle_config_key(app: &mut App, key: KeyEvent) -> Result<bool> {
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

pub(crate) fn handle_config_input(app: &mut App, key: KeyEvent) {
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
        ConfigSelection::TraceAutoStart => {
            app.config_editor.draft.trace_auto_start = !app.config_editor.draft.trace_auto_start;
            app.config_editor.dirty = true;
            persist_and_apply_config(app);
        }
        ConfigSelection::TraceWindowSecs => {
            app.config_editor.start_input(
                EditorAction::SetTraceWindowSecs,
                "trace window secs".to_owned(),
                app.config_editor.draft.trace_window_secs.to_string(),
            );
        }
        ConfigSelection::TraceLatencyMs => {
            app.config_editor.start_input(
                EditorAction::SetTraceLatencyMs,
                "trace latency ms".to_owned(),
                app.config_editor.draft.trace_latency_ms.to_string(),
            );
        }
        ConfigSelection::TraceTtlSecs => {
            app.config_editor.start_input(
                EditorAction::SetTraceTtlSecs,
                "trace ttl secs".to_owned(),
                app.config_editor.draft.trace_ttl_secs.to_string(),
            );
        }
        ConfigSelection::OsdtraceUrl => {
            app.config_editor.start_input(
                EditorAction::SetOsdtraceUrl,
                "osdtrace url".to_owned(),
                app.config_editor.draft.osdtrace_url.clone(),
            );
        }
        ConfigSelection::OsdtraceSha256 => {
            app.config_editor.start_input(
                EditorAction::SetOsdtraceSha256,
                "osdtrace sha256".to_owned(),
                app.config_editor.draft.osdtrace_sha256.clone(),
            );
        }
        ConfigSelection::OsdtraceAllowUnverified => {
            app.config_editor.draft.osdtrace_allow_unverified =
                !app.config_editor.draft.osdtrace_allow_unverified;
            app.config_editor.dirty = true;
            persist_and_apply_config(app);
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
            validate_ssh_destination("admin host", value)?;
            editor.draft.admin_host = value.to_owned();
        }
        EditorAction::SetRefreshSecs => {
            let refresh_secs = value
                .parse::<u64>()
                .with_context(|| format!("invalid refresh interval '{value}'"))?
                .max(1);
            editor.draft.refresh_secs = refresh_secs;
        }
        EditorAction::SetTraceWindowSecs => {
            let trace_window_secs = value
                .parse::<u64>()
                .with_context(|| format!("invalid trace window '{value}'"))?
                .max(1);
            editor.draft.trace_window_secs = trace_window_secs;
        }
        EditorAction::SetTraceLatencyMs => {
            let trace_latency_ms = value
                .parse::<u64>()
                .with_context(|| format!("invalid trace latency '{value}'"))?;
            editor.draft.trace_latency_ms = trace_latency_ms;
        }
        EditorAction::SetTraceTtlSecs => {
            let trace_ttl_secs = value
                .parse::<u64>()
                .with_context(|| format!("invalid trace ttl '{value}'"))?
                .max(1);
            editor.draft.trace_ttl_secs = trace_ttl_secs;
        }
        EditorAction::SetOsdtraceUrl => {
            editor.draft.osdtrace_url = value.to_owned();
        }
        EditorAction::SetOsdtraceSha256 => {
            if !value.is_empty() {
                validate_sha256(value)?;
            }
            editor.draft.osdtrace_sha256 = value.to_owned();
        }
        EditorAction::AddHost => {
            if value.is_empty() {
                return Err(anyhow!("host is empty"));
            }
            validate_ssh_destination("host", value)?;
            if editor.draft.hosts.iter().any(|host| host == value) {
                return Err(anyhow!("host '{value}' already exists"));
            }
            editor.draft.hosts.push(value.to_owned());
            editor.selected = ConfigDraft::FIXED_ROWS + editor.draft.hosts.len() - 1;
        }
        EditorAction::EditHost { index } => {
            if value.is_empty() {
                return Err(anyhow!("host is empty"));
            }
            validate_ssh_destination("host", value)?;
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
    let streams_changed = current.admin_host != draft.admin_host
        || current.hosts != draft.hosts
        || current.refresh_secs != draft.refresh_secs;
    if current != draft {
        if streams_changed {
            let _ = shutdown_streams(app, false);
        }
        app.profile = draft.profile.clone();
        app.admin_host = draft.admin_host.clone();
        app.hosts = draft.hosts.clone();
        app.client_hosts = draft.client_hosts.clone();
        app.refresh = Duration::from_secs(draft.refresh_secs.max(1));
        app.trace_auto_start = draft.trace_auto_start;
        app.trace_window_secs = draft.trace_window_secs.max(1);
        app.trace_latency_ms = draft.trace_latency_ms;
        app.trace_ttl_secs = draft.trace_ttl_secs.max(1);
        app.trace_install = TraceInstallConfig {
            url: draft_optional(&draft.osdtrace_url),
            sha256: draft_optional(&draft.osdtrace_sha256),
            allow_unverified: draft.osdtrace_allow_unverified,
        };
        if streams_changed {
            app.stream_stop = Arc::new(AtomicBool::new(false));
            app.trace_stop = Arc::new(AtomicBool::new(false));
            app.trace_following = false;
            app.trace_active = 0;
            app.trace_session = None;
            app.stream_statuses.clear();
            app.node_summaries.clear();
            app.snapshot = None;
            start_live_streams(app);
        }
    }

    app.config_editor.draft = ConfigDraft::from_app(app);
    app.config_editor.dirty = false;
    app.config_editor.clamp_selection();
    app.config_editor.message = if streams_changed {
        format!("saved {} and restarted ssh streams", path.display())
    } else {
        format!("saved {}", path.display())
    };
    app.log(format!("config saved to {}", path.display()));
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
    config.profiles.insert(
        draft.profile.clone(),
        ClusterProfile {
            admin_host: draft.admin_host.clone(),
            hosts: draft.hosts.clone(),
            client_hosts: if draft.client_hosts.is_empty() {
                None
            } else {
                Some(draft.client_hosts.clone())
            },
            refresh_secs: Some(draft.refresh_secs.max(1)),
            trace_auto_start: Some(draft.trace_auto_start),
            trace_window_secs: Some(draft.trace_window_secs.max(1)),
            trace_latency_ms: Some(draft.trace_latency_ms),
            trace_ttl_secs: Some(draft.trace_ttl_secs.max(1)),
            osdtrace_url: draft_optional(&draft.osdtrace_url),
            osdtrace_sha256: draft_optional(&draft.osdtrace_sha256),
            osdtrace_allow_unverified: Some(draft.osdtrace_allow_unverified),
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
    validate_ssh_destination("admin host", &draft.admin_host)?;
    validate_ssh_destinations("host", &draft.hosts)?;
    validate_ssh_destinations("client host", &draft.client_hosts)?;
    if draft.refresh_secs == 0 {
        return Err(anyhow!("refresh interval must be at least 1 second"));
    }
    if draft.trace_window_secs == 0 {
        return Err(anyhow!("trace window must be at least 1 second"));
    }
    if draft.trace_ttl_secs == 0 {
        return Err(anyhow!("trace runner ttl must be at least 1 second"));
    }
    if let Some(sha256) = draft_optional(&draft.osdtrace_sha256) {
        validate_sha256(&sha256)?;
    }
    Ok(())
}

fn draft_optional(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> ConfigDraft {
        ConfigDraft {
            profile: "test".to_owned(),
            admin_host: "a".to_owned(),
            hosts: vec!["a".to_owned(), "b".to_owned()],
            client_hosts: Vec::new(),
            refresh_secs: 1,
            trace_auto_start: false,
            trace_window_secs: 10,
            trace_latency_ms: 1,
            trace_ttl_secs: 1800,
            osdtrace_url: String::new(),
            osdtrace_sha256: String::new(),
            osdtrace_allow_unverified: false,
        }
    }

    fn editor() -> ConfigEditor {
        ConfigEditor::new(draft())
    }

    fn input(action: EditorAction, buffer: &str) -> EditorInput {
        EditorInput {
            action,
            label: String::new(),
            buffer: buffer.to_owned(),
        }
    }

    #[test]
    fn add_host_rejects_duplicates_and_appends() {
        let mut editor = editor();
        assert!(apply_editor_input(&mut editor, &input(EditorAction::AddHost, "b")).is_err());
        apply_editor_input(&mut editor, &input(EditorAction::AddHost, " c ")).unwrap();
        assert_eq!(editor.draft.hosts, ["a", "b", "c"]);
        assert!(editor.dirty);
        assert_eq!(editor.selection(), ConfigSelection::Host(2));
    }

    #[test]
    fn edit_host_renames_admin_host_alias() {
        let mut editor = editor();
        apply_editor_input(
            &mut editor,
            &input(EditorAction::EditHost { index: 0 }, "a2"),
        )
        .unwrap();
        assert_eq!(editor.draft.admin_host, "a2");
        assert_eq!(editor.draft.hosts, ["a2", "b"]);
    }

    #[test]
    fn numeric_fields_parse_and_clamp() {
        let mut editor = editor();
        apply_editor_input(&mut editor, &input(EditorAction::SetRefreshSecs, "0")).unwrap();
        assert_eq!(editor.draft.refresh_secs, 1);
        assert!(
            apply_editor_input(&mut editor, &input(EditorAction::SetRefreshSecs, "abc")).is_err()
        );
        apply_editor_input(&mut editor, &input(EditorAction::SetTraceLatencyMs, "0")).unwrap();
        assert_eq!(editor.draft.trace_latency_ms, 0);
    }

    #[test]
    fn sha256_input_is_validated() {
        let mut editor = editor();
        assert!(
            apply_editor_input(&mut editor, &input(EditorAction::SetOsdtraceSha256, "1234"))
                .is_err()
        );
        apply_editor_input(&mut editor, &input(EditorAction::SetOsdtraceSha256, "")).unwrap();
        assert_eq!(editor.draft.osdtrace_sha256, "");
    }

    #[test]
    fn validate_rejects_empty_hosts_and_zero_intervals() {
        assert!(validate_config_draft(&draft()).is_ok());
        let mut bad = draft();
        bad.hosts.clear();
        assert!(validate_config_draft(&bad).is_err());
        let mut bad = draft();
        bad.refresh_secs = 0;
        assert!(validate_config_draft(&bad).is_err());
    }

    #[test]
    fn validate_rejects_ssh_option_like_hosts() {
        let mut editor = editor();
        assert!(
            apply_editor_input(
                &mut editor,
                &input(EditorAction::AddHost, "-oProxyCommand=sh")
            )
            .is_err()
        );

        let mut bad = draft();
        bad.admin_host = "-oProxyCommand=sh".to_owned();
        assert!(validate_config_draft(&bad).is_err());

        let mut bad = draft();
        bad.hosts = vec!["a".to_owned(), "bad host".to_owned()];
        assert!(validate_config_draft(&bad).is_err());
    }

    #[test]
    fn selection_maps_fixed_rows_then_hosts() {
        let mut editor = editor();
        editor.selected = 0;
        assert_eq!(editor.selection(), ConfigSelection::AdminHost);
        editor.selected = 8;
        assert_eq!(editor.selection(), ConfigSelection::OsdtraceAllowUnverified);
        editor.selected = ConfigDraft::FIXED_ROWS + 1;
        assert_eq!(editor.selection(), ConfigSelection::Host(1));
    }
}
