use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::trace::TraceInstallConfig;

pub(crate) const DEFAULT_TRACE_TTL_SECS: u64 = 30 * 60;

pub(crate) const DEFAULT_CONFIG: &str = r#"# cephlens cluster profiles
default_profile = "example"

[profiles.example]
admin_host = "ceph-admin"
hosts = ["ceph-admin", "ceph-node-1", "ceph-node-2", "ceph-node-3"]
refresh_secs = 1
trace_auto_start = false
trace_window_secs = 10
trace_latency_ms = 1
trace_ttl_secs = 1800
# Optional client-side tracing targets for kfstrace and radostrace.
# client_hosts = ["ceph-client-1"]
# osdtrace_url = "https://example.invalid/artifacts/osdtrace"
# osdtrace_sha256 = "64 lowercase hex chars"
# osdtrace_allow_unverified = false
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConfigFile {
    pub(crate) default_profile: Option<String>,
    pub(crate) profiles: BTreeMap<String, ClusterProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClusterProfile {
    pub(crate) admin_host: String,
    pub(crate) hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_hosts: Option<Vec<String>>,
    pub(crate) refresh_secs: Option<u64>,
    pub(crate) trace_auto_start: Option<bool>,
    pub(crate) trace_window_secs: Option<u64>,
    pub(crate) trace_latency_ms: Option<u64>,
    pub(crate) trace_ttl_secs: Option<u64>,
    pub(crate) osdtrace_url: Option<String>,
    pub(crate) osdtrace_sha256: Option<String>,
    pub(crate) osdtrace_allow_unverified: Option<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedConfig {
    pub(crate) profile: String,
    pub(crate) admin_host: String,
    pub(crate) hosts: Vec<String>,
    pub(crate) client_hosts: Vec<String>,
    pub(crate) refresh_secs: u64,
    pub(crate) trace_auto_start: bool,
    pub(crate) trace_window_secs: u64,
    pub(crate) trace_latency_ms: u64,
    pub(crate) trace_ttl_secs: u64,
    pub(crate) trace_install: TraceInstallConfig,
}

pub(crate) fn load_config_file(path: &Path) -> Result<Option<ConfigFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let config = toml::from_str(&raw)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    Ok(Some(config))
}

pub(crate) fn write_default_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        return Err(anyhow!(
            "{} already exists; pass --force to overwrite",
            path.display()
        ));
    }
    fs::write(path, DEFAULT_CONFIG)?;
    println!("wrote {}", path.display());
    Ok(())
}

pub(crate) fn default_hosts() -> Vec<String> {
    parse_hosts("ceph-admin,ceph-node-1,ceph-node-2,ceph-node-3")
}

pub(crate) fn parse_hosts(hosts: &str) -> Vec<String> {
    normalize_hosts(hosts.split(','))
}

pub(crate) fn normalize_hosts<'a>(hosts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut clean = Vec::new();
    for host in hosts {
        let host = host.trim();
        if !host.is_empty() && !clean.iter().any(|existing| existing == host) {
            clean.push(host.to_owned());
        }
    }
    clean
}

pub(crate) fn validate_ssh_destination(label: &str, destination: &str) -> Result<()> {
    if destination.is_empty() {
        return Err(anyhow!("{label} is empty"));
    }
    if destination.starts_with('-') {
        return Err(anyhow!("{label} must not start with '-'"));
    }
    if destination
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(anyhow!(
            "{label} must not contain whitespace or control characters"
        ));
    }
    Ok(())
}

pub(crate) fn validate_ssh_destinations(label: &str, destinations: &[String]) -> Result<()> {
    for destination in destinations {
        validate_ssh_destination(label, destination)?;
    }
    Ok(())
}

pub(crate) fn clean_optional(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_destination_validation_rejects_options_and_shell_fragments() {
        assert!(validate_ssh_destination("host", "ceph-admin").is_ok());
        assert!(validate_ssh_destination("host", "user@ceph-node-1").is_ok());
        assert!(validate_ssh_destination("host", "-oProxyCommand=sh").is_err());
        assert!(validate_ssh_destination("host", "ceph admin").is_err());
        assert!(validate_ssh_destination("host", "ceph\nadmin").is_err());
    }
}
