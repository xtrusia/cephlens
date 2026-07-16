use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use chrono::{Local, SecondsFormat, Utc};

use crate::model::Snapshot;

pub(crate) const SNAPSHOTS_FILE: &str = "snapshots.jsonl";
pub(crate) const TRACE_OSD_LOG: &str = "trace-osd.log";
pub(crate) const TRACE_KFS_LOG: &str = "trace-kfs.log";
pub(crate) const TRACE_RADOS_LOG: &str = "trace-rados.log";
pub(crate) const DEFAULT_SESSION_KEEP: usize = 20;

pub(crate) fn append_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, snapshot)?;
    writeln!(file)?;
    Ok(())
}

pub(crate) fn append_trace_line(
    session_dir: &Path,
    file_name: &str,
    host: &str,
    line: &str,
) -> Result<()> {
    let path = session_dir.join(file_name);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let stamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    writeln!(file, "{stamp}\t{host}\t{line}")?;
    Ok(())
}

pub(crate) fn load_snapshots(path: &Path) -> Result<Vec<Snapshot>> {
    let path = snapshot_input_path(path);
    let file = File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut snapshots = Vec::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let snapshot: Snapshot = serde_json::from_str(&line)
            .with_context(|| format!("invalid jsonl at {}:{}", path.display(), line_no + 1))?;
        snapshots.push(snapshot);
    }
    if snapshots.is_empty() {
        return Err(anyhow!("no snapshots in {}", path.display()));
    }
    Ok(snapshots)
}

pub(crate) fn session_snapshot_path(session_dir: &Path) -> PathBuf {
    session_dir.join(SNAPSHOTS_FILE)
}

pub(crate) fn create_session_dir(session_keep: usize) -> Result<PathBuf> {
    let root = PathBuf::from(".cephlens").join("sessions");
    fs::create_dir_all(&root)?;
    prune_old_sessions(&root, session_keep);
    let name = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let dir = root.join(name);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub(crate) fn create_session_path(session_keep: usize) -> Result<PathBuf> {
    Ok(session_snapshot_path(&create_session_dir(session_keep)?))
}

fn snapshot_input_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        session_snapshot_path(path)
    } else {
        path.to_path_buf()
    }
}

fn prune_old_sessions(dir: &Path, session_keep: usize) {
    let session_keep = session_keep.max(1);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut sessions = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") || is_session_dir(path))
        .collect::<Vec<_>>();
    if sessions.len() < session_keep {
        return;
    }
    // File names are timestamps, so lexical order is chronological. Keep room
    // for the session about to be created.
    sessions.sort();
    let excess = sessions.len() + 1 - session_keep;
    for path in sessions.into_iter().take(excess) {
        if path.is_dir() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

fn is_session_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_session_name)
}

fn is_session_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 15
        && bytes[0..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClusterSummary, Snapshot};

    fn temp_session_dir() -> PathBuf {
        let id = Utc::now()
            .timestamp_nanos_opt()
            .expect("test timestamp should fit");
        std::env::temp_dir().join(format!("cephlens-session-test-{id}"))
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            captured_at: Utc::now(),
            profile: "test".to_owned(),
            admin_host: "admin".to_owned(),
            hosts: vec!["node-a".to_owned()],
            cluster: ClusterSummary::default(),
            nodes: Vec::new(),
            osds: Vec::new(),
        }
    }

    #[test]
    fn append_trace_line_writes_plain_text_record() {
        let dir = temp_session_dir();
        fs::create_dir_all(&dir).unwrap();

        append_trace_line(&dir, TRACE_OSD_LOG, "node-a", "op_w osd 1 pg 1.2").unwrap();

        let raw = fs::read_to_string(dir.join(TRACE_OSD_LOG)).unwrap();
        assert!(raw.contains("\tnode-a\top_w osd 1 pg 1.2\n"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_snapshots_accepts_session_directory() {
        let dir = temp_session_dir();
        fs::create_dir_all(&dir).unwrap();

        append_snapshot(&session_snapshot_path(&dir), &snapshot()).unwrap();

        let snapshots = load_snapshots(&dir).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].profile, "test");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pruning_honors_configured_session_limit() {
        let root = temp_session_dir();
        fs::create_dir_all(root.join("20260707-120000")).unwrap();
        fs::create_dir_all(root.join("20260707-120001")).unwrap();

        prune_old_sessions(&root, 2);

        assert!(!root.join("20260707-120000").exists());
        assert!(root.join("20260707-120001").exists());
        let _ = fs::remove_dir_all(root);
    }
}
