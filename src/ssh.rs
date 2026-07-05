use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, anyhow};

use crate::util::shell_quote;

pub(crate) fn ssh_capture(host: &str, command: &str) -> Result<String> {
    let remote = format!("sh -c {}", shell_quote(command));
    let output = ProcessCommand::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
        .arg("--")
        .arg(host)
        .arg(remote)
        .output()
        .with_context(|| format!("failed to start ssh for {host}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow!(
            "ssh {host} failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            stdout.trim(),
            stderr.trim()
        ))
    }
}
