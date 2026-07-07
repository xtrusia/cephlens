use crate::{
    config::ResolvedConfig, runner::probe_trace_host, ssh::ssh_output, trace::tracer_probe_command,
    util::short,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CheckLevel {
    Ok,
    Warn,
    Bad,
}

struct DoctorCheck {
    level: CheckLevel,
    scope: String,
    detail: String,
}

pub(crate) fn run_doctor(cfg: &ResolvedConfig) -> String {
    let mut checks = Vec::new();
    checks.push(DoctorCheck {
        level: CheckLevel::Ok,
        scope: "profile".to_owned(),
        detail: format!("{} admin={}", cfg.profile, cfg.admin_host),
    });

    push_remote_check(
        &mut checks,
        "admin ssh",
        &cfg.admin_host,
        "hostname >/dev/null",
        "reachable",
    );
    push_remote_check(
        &mut checks,
        "admin sudo",
        &cfg.admin_host,
        "sudo -n true",
        "passwordless sudo",
    );
    push_remote_check(
        &mut checks,
        "admin ceph",
        &cfg.admin_host,
        "sudo -n ceph -s --format json >/dev/null",
        "ceph status",
    );
    push_remote_check(
        &mut checks,
        "admin rados",
        &cfg.admin_host,
        "command -v rados >/dev/null",
        "rados cli",
    );

    for host in &cfg.hosts {
        push_remote_check(
            &mut checks,
            &format!("{host} ssh"),
            host,
            "hostname >/dev/null",
            "reachable",
        );
        push_remote_check(
            &mut checks,
            &format!("{host} sudo"),
            host,
            "sudo -n true",
            "passwordless sudo",
        );
        push_osdtrace_check(&mut checks, host);
    }

    if cfg.client_hosts.is_empty() {
        checks.push(DoctorCheck {
            level: CheckLevel::Warn,
            scope: "client_hosts".to_owned(),
            detail: "not configured; kfstrace and radostrace are disabled".to_owned(),
        });
    } else {
        for host in &cfg.client_hosts {
            push_remote_check(
                &mut checks,
                &format!("{host} ssh"),
                host,
                "hostname >/dev/null",
                "reachable",
            );
            push_remote_check(
                &mut checks,
                &format!("{host} sudo"),
                host,
                "sudo -n true",
                "passwordless sudo",
            );
            push_client_tracer_check(&mut checks, host, "kfstrace");
            push_client_tracer_check(&mut checks, host, "radostrace");
        }
    }

    render_doctor_report(&checks)
}

fn push_remote_check(
    checks: &mut Vec<DoctorCheck>,
    scope: &str,
    host: &str,
    command: &str,
    ok_detail: &str,
) {
    match ssh_output(host, command, None) {
        Ok(output) if output.success => checks.push(DoctorCheck {
            level: CheckLevel::Ok,
            scope: scope.to_owned(),
            detail: ok_detail.to_owned(),
        }),
        Ok(output) => checks.push(DoctorCheck {
            level: CheckLevel::Bad,
            scope: scope.to_owned(),
            detail: output_detail(&output.stdout, &output.stderr, &output.status),
        }),
        Err(err) => checks.push(DoctorCheck {
            level: CheckLevel::Bad,
            scope: scope.to_owned(),
            detail: short(&format!("{err:#}"), 160),
        }),
    }
}

fn push_osdtrace_check(checks: &mut Vec<DoctorCheck>, host: &str) {
    let target = probe_trace_host(host);
    let scope = format!("{host} osdtrace");
    if let Some(error) = target.error {
        checks.push(DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: short(&error, 160),
        });
    } else if target.installed {
        checks.push(DoctorCheck {
            level: CheckLevel::Ok,
            scope,
            detail: format!(
                "{} osds={} traceable={}",
                empty_as_dash(&target.binary),
                empty_as_dash(&target.osds),
                empty_as_dash(&target.traceable)
            ),
        });
    } else {
        checks.push(DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: "missing".to_owned(),
        });
    }
}

fn push_client_tracer_check(checks: &mut Vec<DoctorCheck>, host: &str, tool: &str) {
    let command = tracer_probe_command(tool);
    let scope = format!("{host} {tool}");
    match ssh_output(host, &command, None) {
        Ok(output) if output.stdout.contains("__CEPHLENS_STATUS__ missing") => {
            checks.push(DoctorCheck {
                level: CheckLevel::Bad,
                scope,
                detail: "missing".to_owned(),
            });
        }
        Ok(output) if output.stdout.contains("__CEPHLENS_ERROR__") => {
            checks.push(DoctorCheck {
                level: CheckLevel::Bad,
                scope,
                detail: first_marker_detail(&output.stdout),
            });
        }
        Ok(output) if output.success => checks.push(DoctorCheck {
            level: CheckLevel::Ok,
            scope,
            detail: first_bin_detail(&output.stdout),
        }),
        Ok(output) => checks.push(DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: output_detail(&output.stdout, &output.stderr, &output.status),
        }),
        Err(err) => checks.push(DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: short(&format!("{err:#}"), 160),
        }),
    }
}

fn render_doctor_report(checks: &[DoctorCheck]) -> String {
    let mut out = String::from("cephlens doctor\n");
    let worst = checks
        .iter()
        .map(|check| check.level)
        .max()
        .unwrap_or(CheckLevel::Ok);
    out.push_str(&format!("status: {}\n\n", level_label(worst)));
    for check in checks {
        out.push_str(&format!(
            "[{}] {:<24} {}\n",
            level_label(check.level),
            check.scope,
            check.detail
        ));
    }
    out
}

fn output_detail(stdout: &str, stderr: &str, status: &str) -> String {
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        status
    };
    short(detail, 160)
}

fn first_marker_detail(output: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("__CEPHLENS_ERROR__"))
        .map(|line| line.trim().to_owned())
        .unwrap_or_else(|| "check failed".to_owned())
}

fn first_bin_detail(output: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("__CEPHLENS_BIN__="))
        .map(|line| line.trim().to_owned())
        .unwrap_or_else(|| "installed".to_owned())
}

fn empty_as_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

fn level_label(level: CheckLevel) -> &'static str {
    match level {
        CheckLevel::Ok => "ok",
        CheckLevel::Warn => "warn",
        CheckLevel::Bad => "bad",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_worst_status() {
        let report = render_doctor_report(&[
            DoctorCheck {
                level: CheckLevel::Ok,
                scope: "a".to_owned(),
                detail: "ready".to_owned(),
            },
            DoctorCheck {
                level: CheckLevel::Warn,
                scope: "b".to_owned(),
                detail: "missing optional target".to_owned(),
            },
        ]);

        assert!(report.contains("status: warn"));
        assert!(report.contains("[ok]"));
        assert!(report.contains("[warn]"));
    }
}
