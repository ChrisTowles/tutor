//! Append-only JSONL, one record per line, one file per UTC day.

use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};

struct OpenDay {
    date: String,
    writer: BufWriter<File>,
}

/// Writes `events-<date>.jsonl` in `dir`, keeping the newest `retain_days` files.
///
/// Every record is flushed: the log is read after something went wrong, often
/// a crash or a killed process, so buffered records would be the lost ones.
pub struct EventLog {
    dir: PathBuf,
    open: Option<OpenDay>,
    retain_days: usize,
}

impl EventLog {
    pub fn new(dir: impl Into<PathBuf>, retain_days: usize) -> Self {
        Self { dir: dir.into(), open: None, retain_days: retain_days.max(1) }
    }

    /// False when the record couldn't be written. Never an error: logging must
    /// not be able to break what it observes.
    pub fn append(&mut self, record: &serde_json::Value, now: DateTime<Utc>) -> bool {
        let date = now.format("%Y-%m-%d").to_string();
        let Ok(line) = serde_json::to_string(record) else { return false };
        if self.open.as_ref().is_none_or(|o| o.date != date) {
            if !self.rotate_to(&date) {
                return false;
            }
            let _ = prune(&self.dir, self.retain_days);
        }
        let Some(open) = self.open.as_mut() else { return false };
        writeln!(open.writer, "{line}").is_ok() && open.writer.flush().is_ok()
    }

    fn rotate_to(&mut self, date: &str) -> bool {
        self.open = None;
        if fs::create_dir_all(&self.dir).is_err() {
            return false;
        }
        match OpenOptions::new().create(true).append(true).open(self.dir.join(format!("events-{date}.jsonl"))) {
            Ok(file) => {
                self.open = Some(OpenDay { date: date.to_string(), writer: BufWriter::new(file) });
                true
            }
            Err(_) => false,
        }
    }
}

/// Delete all but the newest `retain_days` event logs; nothing else in `dir` is touched.
fn prune(dir: &Path, retain_days: usize) -> std::io::Result<()> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl")))
        .collect();
    files.sort();
    for path in &files[..files.len().saturating_sub(retain_days)] {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("{day}T12:00:00Z")).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn rotates_daily_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("settings.toml"), "").unwrap();
        let mut log = EventLog::new(dir.path(), 2);
        for day in ["2026-10-01", "2026-10-02", "2026-10-03"] {
            assert!(log.append(&serde_json::json!({ "day": day }), at(day)));
        }
        let mut names: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["events-2026-10-02.jsonl", "events-2026-10-03.jsonl", "settings.toml"]);
    }
}
