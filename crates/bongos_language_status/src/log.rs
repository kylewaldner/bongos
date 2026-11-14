//! `languages.log`: one timestamped line per language server event the status line reports
//! (download start and progress, success, failure, start failure with its stderr, notices,
//! retries). It rotates to `languages.log.old` at 5 MB. Timestamps come from an injected clock.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, SecondsFormat, Utc};

const FILE_NAME: &str = "languages.log";
const OLD_FILE_NAME: &str = "languages.log.old";

/// The size at which `languages.log` moves to `languages.log.old`.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Where timestamps come from. Production passes [`system_clock`], tests a fixed time.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

#[derive(Clone)]
pub struct LanguagesLog {
    path: PathBuf,
    max_bytes: u64,
    clock: Clock,
}

impl LanguagesLog {
    /// The log in `logs_dir`. Nothing is created until the first line.
    pub fn new(logs_dir: &Path, clock: Clock) -> Self {
        Self {
            path: logs_dir.join(FILE_NAME),
            max_bytes: MAX_LOG_BYTES,
            clock,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the file if it does not exist, so it can be opened.
    pub fn ensure_exists(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map(drop)
    }

    /// Appends `text` with a timestamp, one log line per line of text. A write error is logged
    /// and dropped: the log never stops a server.
    pub fn line(&self, text: &str) {
        if let Err(error) = self.append(text) {
            log::warn!("cannot write {}: {error}", self.path.display());
        }
    }

    fn append(&self, text: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stamp = (self.clock)().to_rfc3339_opts(SecondsFormat::Millis, true);
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
        self.rotate_if_full();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(lines.as_bytes())
    }

    /// Moves a full log to `languages.log.old`, replacing the previous one.
    fn rotate_if_full(&self) {
        let full = fs::metadata(&self.path).is_ok_and(|meta| meta.len() >= self.max_bytes);
        if full && let Err(error) = fs::rename(&self.path, self.path.with_file_name(OLD_FILE_NAME))
        {
            log::warn!("cannot rotate {}: {error}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn fixed_clock() -> Clock {
        Arc::new(|| Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap())
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn test_lines_carry_the_injected_timestamp() {
        let temp = tempfile::tempdir().unwrap();
        let log = LanguagesLog::new(temp.path(), fixed_clock());
        log.line("Rust: download started\nsecond");
        assert_eq!(
            read(log.path()),
            "2026-01-02T03:04:05.000Z Rust: download started\n2026-01-02T03:04:05.000Z second\n"
        );
    }

    #[test]
    fn test_a_log_at_five_megabytes_rotates_to_old() {
        assert_eq!(MAX_LOG_BYTES, 5 * 1024 * 1024);
        let temp = tempfile::tempdir().unwrap();
        let log = LanguagesLog::new(temp.path(), fixed_clock());
        fs::write(log.path(), vec![b'x'; MAX_LOG_BYTES as usize - 1]).unwrap();
        log.line("first");
        assert!(!temp.path().join("languages.log.old").exists());
        log.line("second");
        let old = temp.path().join("languages.log.old");
        assert!(fs::metadata(&old).unwrap().len() >= MAX_LOG_BYTES);
        assert!(read(&old).ends_with("first\n"));
        let current = read(log.path());
        assert!(current.ends_with(" second\n"));
        assert_eq!(current.lines().count(), 1);
    }

    #[test]
    fn test_rotation_replaces_the_previous_old_log() {
        let temp = tempfile::tempdir().unwrap();
        let log = LanguagesLog::new(temp.path(), fixed_clock()).with_max_bytes(40);
        for n in 0..6 {
            log.line(&format!("line number {n} with some padding"));
        }
        let old = read(&temp.path().join("languages.log.old"));
        assert!(old.contains("line number 4"), "{old}");
        assert!(!old.contains("line number 0"), "{old}");
        assert!(read(log.path()).contains("line number 5"));
    }

    #[test]
    fn test_ensure_exists_creates_an_empty_file_once() {
        let temp = tempfile::tempdir().unwrap();
        let log = LanguagesLog::new(&temp.path().join("logs"), fixed_clock());
        log.ensure_exists().unwrap();
        log.line("kept");
        log.ensure_exists().unwrap();
        assert!(read(log.path()).ends_with(" kept\n"));
    }
}
