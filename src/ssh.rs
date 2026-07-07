use std::{io::Write, process::Command as ProcessCommand, thread};

use anyhow::{Context, Result, anyhow};

use crate::util::shell_quote;

pub(crate) struct SshCommandOutput {
    pub(crate) success: bool,
    pub(crate) status: String,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) fn ssh_capture(host: &str, command: &str) -> Result<String> {
    let output = ssh_output(host, command, None)?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(anyhow!(
            "ssh {host} failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            output.stdout.trim(),
            output.stderr.trim()
        ))
    }
}

pub(crate) fn ssh_output(
    host: &str,
    command: &str,
    stdin: Option<&str>,
) -> Result<SshCommandOutput> {
    let remote = format!("sh -c {}", shell_quote(command));
    let mut child = ProcessCommand::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
        .arg("--")
        .arg(host)
        .arg(remote)
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start ssh for {host}"))?;

    let stdin_writer = if let Some(stdin) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        let input = stdin.as_bytes().to_vec();
        Some(thread::spawn(move || child_stdin.write_all(&input)))
    } else {
        None
    };

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for ssh {host}"))?;
    if let Some(writer) = stdin_writer {
        match writer.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Err(err).with_context(|| format!("failed to write ssh stdin for {host}"));
            }
            Err(_) => return Err(anyhow!("ssh stdin writer panicked for {host}")),
        }
    }
    Ok(SshCommandOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}
