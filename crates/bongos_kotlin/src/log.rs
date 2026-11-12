//! `kotlin.log`: one timestamped line per Kotlin event (install, start, stop, switch, crash, manifest
//! check) and the Kotlin server's stderr. It outlives switches, which the per-server LSP log does not. The file
//! rotates to `kotlin.log.old` at 5 MB.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::manifest_cache::FetchOutcome;

const FILE_NAME: &str = "kotlin.log";
const OLD_FILE_NAME: &str = "kotlin.log.old";

/// The size at which `kotlin.log` moves to `kotlin.log.old`.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct KotlinLog {
    path: PathBuf,
    max_bytes: u64,
}

impl KotlinLog {
    /// The log in `logs_dir`. Nothing is created until the first line.
    pub fn new(logs_dir: &Path) -> Self {
        Self {
            path: logs_dir.join(FILE_NAME),
            max_bytes: MAX_LOG_BYTES,
        }
    }

    /// The log path in `logs_dir`, for opening it before the first line is written.
    pub fn path_in(logs_dir: &Path) -> PathBuf {
        logs_dir.join(FILE_NAME)
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

    /// `manifest: off (kotlin.manifest_url is empty)`: bongos does not check for servers.
    pub fn manifest_off(&self) {
        self.manifest_line("manifest: off (kotlin.manifest_url is empty)");
    }

    /// `manifest: off (kotlin.manifest_url is refused)`: the setting names a URL bongos may not
    /// fetch; the app log has the reason.
    pub fn manifest_refused(&self) {
        self.manifest_line("manifest: off (kotlin.manifest_url is refused)");
    }

    /// The outcome of one manifest check, with the manifest's host: `manifest: fetched serial N`
    /// or `manifest: rejected: <reason>`.
    pub fn manifest_outcome(&self, host: &str, outcome: &FetchOutcome) {
        self.manifest_line(&format!("{} (host {host})", outcome.describe()));
    }

    /// A manifest check that could not read or write its state; nothing was fetched or stored.
    pub fn manifest_skipped(&self, host: &str, reason: &str) {
        self.manifest_line(&format!("manifest: skipped: {reason} (host {host})"));
    }

    /// One manifest line, folded onto a single line, in this log and in the app log.
    fn manifest_line(&self, text: &str) {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        log::info!("{text}");
        self.line(&text);
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
        self.rotate_if_full();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(lines.as_bytes())
    }

    /// Moves a full log to `kotlin.log.old`, replacing the previous one.
    fn rotate_if_full(&self) {
        let full = fs::metadata(&self.path).is_ok_and(|meta| meta.len() >= self.max_bytes);
        if full && let Err(error) = fs::rename(&self.path, self.path.with_file_name(OLD_FILE_NAME))
        {
            log::warn!("cannot rotate {}: {error}", self.path.display());
        }
    }
}

/// Mirrors one server run's stderr into `kotlin.log` exactly once.
///
/// Zed hands the manager the same stderr twice: the lines read while the server started arrive as
/// one `LanguageServerStartupStderr` text, and `on_io` delivers every line live from the moment the
/// manager subscribes, which is just before that text. A line read in between is in both. So live
/// lines are held until the startup text is seen (or until [`StderrMirror::release`] says none is
/// coming), and the part of the held lines the startup text already contains is dropped.
#[derive(Clone, Debug)]
pub struct StderrMirror {
    log: KotlinLog,
    /// `Some` while live lines are held back.
    held: Arc<Mutex<Option<Vec<String>>>>,
}

impl StderrMirror {
    pub fn new(log: KotlinLog) -> Self {
        Self {
            log,
            held: Arc::new(Mutex::new(Some(Vec::new()))),
        }
    }

    /// A line read after the manager subscribed.
    pub fn live(&self, text: &str) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match held.as_mut() {
            Some(lines) => lines.extend(text.lines().map(str::to_string)),
            None => self.log.stderr(text),
        }
    }

    /// The text captured while the server started.
    pub fn startup(&self, text: &str) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.log.stderr(text);
        let Some(lines) = held.take() else {
            return;
        };
        let startup: Vec<&str> = text.lines().collect();
        // The held lines that were also captured are the last lines of the startup text.
        let overlap = (0..=startup.len().min(lines.len()))
            .rev()
            .find(|&count| {
                startup[startup.len() - count..]
                    .iter()
                    .zip(&lines[..count])
                    .all(|(captured, held)| captured == held)
            })
            .unwrap_or(0);
        if overlap < lines.len() {
            self.log.stderr(&lines[overlap..].join("\n"));
        }
    }

    /// No startup text is coming: log what was held and pass later lines straight through.
    pub fn release(&self) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(lines) = held.take()
            && !lines.is_empty()
        {
            self.log.stderr(&lines.join("\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn test_a_log_at_five_megabytes_rotates_to_old() {
        assert_eq!(MAX_LOG_BYTES, 5 * 1024 * 1024);
        let temp = tempfile::tempdir().unwrap();
        let log = KotlinLog::new(temp.path());
        // Just under the limit: nothing rotates.
        fs::write(log.path(), vec![b'x'; MAX_LOG_BYTES as usize - 1]).unwrap();
        log.line("first");
        assert!(!temp.path().join("kotlin.log.old").exists());
        // Now at the limit: the next line starts a new file.
        assert!(fs::metadata(log.path()).unwrap().len() >= MAX_LOG_BYTES);
        log.line("second");
        let old = temp.path().join("kotlin.log.old");
        assert!(fs::metadata(&old).unwrap().len() >= MAX_LOG_BYTES);
        assert!(read(&old).ends_with("first\n"));
        let current = read(log.path());
        assert!(current.ends_with(" second\n"));
        assert_eq!(current.lines().count(), 1);
    }

    #[test]
    fn test_rotation_replaces_the_previous_old_log() {
        let temp = tempfile::tempdir().unwrap();
        let mut log = KotlinLog::new(temp.path());
        log.max_bytes = 40;
        for n in 0..6 {
            log.line(&format!("line number {n} with some padding"));
        }
        let old = read(&temp.path().join("kotlin.log.old"));
        assert!(old.contains("line number 4"), "{old}");
        assert!(!old.contains("line number 0"), "{old}");
        assert!(read(log.path()).contains("line number 5"));
    }

    #[test]
    fn test_ensure_exists_creates_an_empty_file_once() {
        let temp = tempfile::tempdir().unwrap();
        let log = KotlinLog::new(&temp.path().join("logs"));
        log.ensure_exists().unwrap();
        log.line("kept");
        log.ensure_exists().unwrap();
        assert!(read(log.path()).ends_with(" kept\n"));
    }

    fn stderr_texts(log: &KotlinLog) -> Vec<String> {
        read(log.path())
            .lines()
            .filter_map(|line| line.split_once(" stderr ").map(|(_, text)| text.into()))
            .collect()
    }

    #[test]
    fn test_the_mirror_drops_only_the_lines_the_startup_text_has() {
        let temp = tempfile::tempdir().unwrap();
        let log = KotlinLog::new(temp.path());
        let mirror = StderrMirror::new(log.clone());
        mirror.live("b\n");
        mirror.live("c\n");
        mirror.live("d\n");
        // The capture was taken after `c`, so `d` is new.
        mirror.startup("a\nb\nc\n");
        mirror.live("e\n");
        assert_eq!(stderr_texts(&log), ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn test_the_mirror_releases_held_lines_when_no_startup_text_comes() {
        let temp = tempfile::tempdir().unwrap();
        let log = KotlinLog::new(temp.path());
        let mirror = StderrMirror::new(log.clone());
        mirror.live("x\n");
        assert!(stderr_texts(&log).is_empty());
        mirror.release();
        mirror.live("y\n");
        mirror.release();
        assert_eq!(stderr_texts(&log), ["x", "y"]);
    }
}
