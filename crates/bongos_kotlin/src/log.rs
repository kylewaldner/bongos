//! `kotlin.log`: one timestamped line per Kotlin event (install, start, stop, switch, crash) and the
//! Kotlin server's stderr. It outlives switches, which the per-server LSP log does not. Rotation to
//! `kotlin.log.old` comes with the status item.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
};

const FILE_NAME: &str = "kotlin.log";

#[derive(Clone, Debug)]
pub struct KotlinLog {
    path: PathBuf,
}

impl KotlinLog {
    /// The log in `logs_dir`. Nothing is created until the first line.
    pub fn new(logs_dir: &Path) -> Self {
        Self {
            path: logs_dir.join(FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends stderr text from the server, each line prefixed with `stderr`.
    pub fn stderr(&self, text: &str) {
        let lines: Vec<String> = text.lines().map(|line| format!("stderr {line}")).collect();
        self.line(&lines.join("\n"));
    }

    /// Appends `text` with a timestamp, one log line per line of text. A write error is logged
    /// and dropped: the Kotlin log never stops the server.
    pub fn line(&self, text: &str) {
        if let Err(error) = self.append(text) {
            log::warn!("cannot write {}: {error}", self.path.display());
        }
    }

    fn append(&self, text: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut lines = String::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            lines.push_str(&stamp);
            lines.push(' ');
            lines.push_str(line);
            lines.push('\n');
        }
        if lines.is_empty() {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(lines.as_bytes())
    }
}
