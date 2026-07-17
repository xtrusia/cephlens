use std::{
    fs,
    path::{Path, PathBuf},
    thread,
};

use anyhow::{Context, Error, Result};
use clap::ValueEnum;

use crate::{
    collect::{collect_snapshot, run_bench},
    config::ResolvedConfig,
    report::build_report,
    runner::{trace_runner_install_command, trace_runner_script},
    session::{
        TRACE_KFS_LOG, TRACE_OSD_LOG, TRACE_RADOS_LOG, append_snapshot, append_trace_line,
        create_session_dir, session_snapshot_path,
    },
    ssh::ssh_output,
    trace::{kfstrace_run_command, radostrace_run_command},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum LabTrace {
    None,
    Osd,
    All,
}

pub(crate) struct LabResult {
    pub(crate) session_dir: PathBuf,
    pub(crate) report_path: PathBuf,
    pub(crate) bench_output: String,
    pub(crate) bench_error: Option<Error>,
}

pub(crate) fn run_lab(
    cfg: &ResolvedConfig,
    bench_host: &str,
    seconds: u64,
    trace: LabTrace,
    keep_pool: bool,
) -> Result<LabResult> {
    let seconds = seconds.max(1);
    let session_dir = create_session_dir(cfg.session_keep)?;
    let snapshot_path = session_snapshot_path(&session_dir);
    append_snapshot(&snapshot_path, &collect_snapshot(cfg)?)?;

    let trace_ttl_secs = seconds.saturating_add(2);
    let mut handles = Vec::new();
    if matches!(trace, LabTrace::Osd | LabTrace::All) {
        let session = session_name(&session_dir);
        for host in &cfg.hosts {
            handles.push(spawn_trace_capture(
                session_dir.clone(),
                host.clone(),
                TRACE_OSD_LOG,
                trace_runner_install_command(&session, cfg.trace_latency_ms, trace_ttl_secs),
                Some(trace_runner_script().to_owned()),
                "__CEPHLENS_TRACE_ERROR__",
            ));
        }
    }
    if matches!(trace, LabTrace::All) {
        let latency_us = cfg.trace_latency_ms.saturating_mul(1000);
        for host in &cfg.client_hosts {
            handles.push(spawn_trace_capture(
                session_dir.clone(),
                host.clone(),
                TRACE_KFS_LOG,
                kfstrace_run_command(latency_us, trace_ttl_secs),
                None,
                "__CEPHLENS_KFS_ERROR__",
            ));
            handles.push(spawn_trace_capture(
                session_dir.clone(),
                host.clone(),
                TRACE_RADOS_LOG,
                radostrace_run_command(trace_ttl_secs),
                None,
                "__CEPHLENS_RADOS_ERROR__",
            ));
        }
    }

    thread::sleep(std::time::Duration::from_millis(600));
    let session = session_name(&session_dir);
    let (bench_output, bench_error) =
        match run_bench(bench_host, seconds, Some(&session), keep_pool) {
            Ok(output) => (output, None),
            Err(err) => (format!("bench failed: {err:#}"), Some(err)),
        };
    fs::write(session_dir.join("bench.log"), &bench_output)
        .with_context(|| "failed to write bench.log")?;

    for handle in handles {
        let _ = handle.join();
    }

    if let Ok(snapshot) = collect_snapshot(cfg) {
        append_snapshot(&snapshot_path, &snapshot)?;
    }
    let report_path = session_dir.join("report.md");
    fs::write(&report_path, build_report(&session_dir)?)?;

    Ok(LabResult {
        session_dir,
        report_path,
        bench_output,
        bench_error,
    })
}

fn spawn_trace_capture(
    session_dir: PathBuf,
    host: String,
    file_name: &'static str,
    command: String,
    stdin: Option<String>,
    error_prefix: &'static str,
) -> thread::JoinHandle<()> {
    thread::spawn(
        move || match ssh_output(&host, &command, stdin.as_deref()) {
            Ok(output) => {
                append_output_lines(&session_dir, file_name, &host, &output.stdout);
                append_output_lines(&session_dir, file_name, &host, &output.stderr);
                if !output.success {
                    append_trace_error(
                        &session_dir,
                        file_name,
                        &host,
                        error_prefix,
                        &format!("remote command exited with {}", output.status),
                    );
                }
            }
            Err(err) => append_trace_error(
                &session_dir,
                file_name,
                &host,
                error_prefix,
                &format!("{err:#}"),
            ),
        },
    )
}

fn append_output_lines(session_dir: &Path, file_name: &str, host: &str, output: &str) {
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let _ = append_trace_line(session_dir, file_name, host, line);
    }
}

fn append_trace_error(
    session_dir: &Path,
    file_name: &str,
    host: &str,
    prefix: &str,
    message: &str,
) {
    let _ = append_trace_line(session_dir, file_name, host, &format!("{prefix} {message}"));
}

fn session_name(session_dir: &Path) -> String {
    session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lab")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_uses_leaf_directory() {
        assert_eq!(
            session_name(Path::new(".cephlens/sessions/20260707-120000")),
            "20260707-120000"
        );
    }
}
