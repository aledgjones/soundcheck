use super::event::Event;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub at: u64,
    #[serde(flatten)]
    pub event: Event,
}

/// `journal.jsonl`: the only record of a run. An entry is on disk before its command is acknowledged.
pub struct Journal {
    path: PathBuf,
    file: File,
    next_seq: u64,
}

impl Journal {
    /// Opens (or creates) a journal and returns the entries already in it, for replay.
    pub fn open(path: &Path) -> Result<(Journal, Vec<Entry>)> {
        let mut entries = Vec::new();
        if path.exists() {
            let reader = BufReader::new(File::open(path)?);
            for (n, line) in reader.lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let entry: Entry = serde_json::from_str(&line)
                    .with_context(|| format!("{}:{}: unreadable entry", path.display(), n + 1))?;
                entries.push(entry);
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("can't open {}", path.display()))?;
        let next_seq = entries.last().map_or(1, |e| e.seq + 1);
        Ok((
            Journal {
                path: path.to_path_buf(),
                file,
                next_seq,
            },
            entries,
        ))
    }

    pub fn append(&mut self, at: u64, event: Event) -> Result<Entry> {
        let entry = Entry {
            seq: self.next_seq,
            at,
            event,
        };
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .with_context(|| format!("can't write {}", self.path.display()))?;
        self.next_seq += 1;
        Ok(entry)
    }
}
