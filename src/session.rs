use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use chrono::Local;

use crate::model::Snapshot;

pub(crate) fn append_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, snapshot)?;
    writeln!(file)?;
    Ok(())
}

pub(crate) fn load_snapshots(path: &Path) -> Result<Vec<Snapshot>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
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

pub(crate) fn create_session_path() -> Result<PathBuf> {
    let dir = PathBuf::from(".cephlens").join("sessions");
    fs::create_dir_all(&dir)?;
    let name = format!("{}.jsonl", Local::now().format("%Y%m%d-%H%M%S"));
    Ok(dir.join(name))
}
