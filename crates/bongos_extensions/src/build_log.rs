//! The build log of an install: `git/logs/<key>.log`, one timestamped line per phase, the commands
//! run and the full error chain. The key is the extension id, or the source key until the id is
//! known. Lines are buffered in memory and [`BuildLog::flush`] rewrites the file (the installer
//! flushes on the background executor after every step), so a crash leaves what happened so far.
//! The log keeps only the last mebibyte.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use bongos_extensions_model::Layout;
use chrono::{DateTime, SecondsFormat, Utc};

/// The most a log keeps; older text is dropped from the front.
pub const MAX_LOG_BYTES: usize = 1024 * 1024;

/// Where the time comes from, so tests can pin it.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// The system clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that always answers the same time.
pub struct FixedClock(pub SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

/// RFC 3339 text in UTC, to the second.
pub fn rfc3339(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// One build log being written.
pub struct BuildLog {
    layout: Layout,
    clock: Arc<dyn Clock>,
    key: String,
    text: String,
}

impl BuildLog {
    /// Starts a new log under `key`, replacing an earlier log of that key.
    pub fn start(layout: Layout, clock: Arc<dyn Clock>, key: String) -> Self {
        Self {
            layout,
            clock,
            key,
            text: String::new(),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn path(&self) -> PathBuf {
        self.layout.log_path(&self.key)
    }

    /// Everything written so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Adds one timestamped line.
    pub fn line(&mut self, message: impl AsRef<str>) {
        let stamp = rfc3339(self.clock.now());
        for (index, line) in message.as_ref().lines().enumerate() {
            if index == 0 {
                self.text.push_str(&format!("{stamp} {line}\n"));
            } else {
                self.text.push_str(&format!("    {line}\n"));
            }
        }
        self.trim();
    }

    /// Adds a command line, indented under the phase.
    pub fn command(&mut self, command: impl AsRef<str>) {
        self.text.push_str(&format!("    $ {}\n", command.as_ref()));
        self.trim();
    }

    /// Adds a failure with its whole cause chain.
    pub fn error(&mut self, error: &anyhow::Error) {
        self.line(format!("error: {error:#}"));
    }

    /// Moves the log to `key` (once the extension id is known), deleting the old file, and writes
    /// it.
    pub fn rekey(&mut self, key: String) {
        if key == self.key {
            return;
        }
        let old = self.path();
        self.key = key;
        std::fs::remove_file(old).ok();
        self.flush();
    }

    fn trim(&mut self) {
        if self.text.len() > MAX_LOG_BYTES {
            let mut cut = self.text.len() - MAX_LOG_BYTES;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
        }
    }

    /// Writes the log file. Does file I/O: call it off the foreground.
    pub fn flush(&self) {
        if self.text.is_empty() {
            return;
        }
        if let Err(err) = write_log(&self.path(), &self.text) {
            log::warn!("could not write the extension build log: {err:#}");
        }
    }
}

fn write_log(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("could not write {}", path.display()))
}
