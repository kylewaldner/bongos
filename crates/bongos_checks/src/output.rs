//! The output file of one check part. Bytes are written as they arrive, so the file can be read
//! while the part runs. Past the size limit the first half stays in the file and later bytes go
//! to a ring of two tail files of a quarter each; at the end the file is the head, a marker
//! naming the bytes cut, and the tail. Lines are matched against the check's summary regex as
//! they pass. Memory holds one read buffer and at most one partial line.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use regex::Regex;

/// The longest partial line kept for the summary match; a longer line is matched on its start.
const LINE_CAP: usize = 64 * 1024;

/// What the output of a finished part came to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSummary {
    /// Every byte the process wrote.
    pub total_bytes: u64,
    /// Bytes cut from the middle.
    pub cut_bytes: u64,
    /// A line matched the summary regex.
    pub summary_seen: bool,
    /// The first error writing the file, when there was one; the output after it is lost.
    pub write_error: Option<String>,
}

/// The marker between head and tail of a cut output.
pub fn cut_marker(cut: u64) -> String {
    format!("\n[... {cut} bytes cut by bongos ...]\n")
}

pub(crate) struct OutputSink {
    log: Option<File>,
    log_path: PathBuf,
    head_limit: u64,
    tail_limit: u64,
    head_written: u64,
    tails: [PathBuf; 2],
    /// The tail file being written, and its length.
    current: Option<(usize, File, u64)>,
    /// Whether the tail ring has wrapped, so the other file holds older bytes.
    wrapped: bool,
    total: u64,
    summary: Option<Regex>,
    summary_seen: bool,
    line: Vec<u8>,
    write_error: Option<String>,
    finished: Option<OutputSummary>,
}

impl OutputSink {
    /// A sink writing to `log_path` (created, truncated). `limit`: the most bytes the finished
    /// file keeps besides the marker; `None` keeps everything.
    pub(crate) fn create(
        log_path: &Path,
        limit: Option<u64>,
        summary: Option<Regex>,
    ) -> std::io::Result<Self> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = File::create(log_path)?;
        let (head_limit, tail_limit) = match limit {
            Some(limit) => {
                let limit = limit.max(4);
                (limit / 2, (limit / 4).max(1))
            }
            None => (u64::MAX, 1),
        };
        let tail = |n: usize| {
            let mut name = log_path.as_os_str().to_owned();
            name.push(format!(".tail{n}"));
            PathBuf::from(name)
        };
        Ok(Self {
            log: Some(log),
            log_path: log_path.to_path_buf(),
            head_limit,
            tail_limit,
            head_written: 0,
            tails: [tail(0), tail(1)],
            current: None,
            wrapped: false,
            total: 0,
            summary,
            summary_seen: false,
            line: Vec::new(),
            write_error: None,
            finished: None,
        })
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished.is_some()
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) {
        if self.finished.is_some() {
            return;
        }
        self.total += bytes.len() as u64;
        self.match_lines(bytes);
        if self.write_error.is_none()
            && let Err(error) = self.store(bytes)
        {
            log::warn!(
                "checks: writing the output {}: {error}",
                self.log_path.display()
            );
            self.write_error = Some(error.to_string());
        }
    }

    fn store(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        if self.head_written < self.head_limit {
            let room = self.head_limit - self.head_written;
            let take = (bytes.len() as u64).min(room) as usize;
            if let Some(log) = &mut self.log {
                log.write_all(&bytes[..take])?;
            }
            self.head_written += take as u64;
            bytes = &bytes[take..];
        }
        while !bytes.is_empty() {
            let full = self
                .current
                .as_ref()
                .is_none_or(|(_, _, len)| *len >= self.tail_limit);
            if full {
                let next = match &self.current {
                    None => 0,
                    Some((index, _, _)) => {
                        self.wrapped = true;
                        1 - index
                    }
                };
                let file = File::create(&self.tails[next])?;
                self.current = Some((next, file, 0));
            }
            let Some((_, file, len)) = &mut self.current else {
                break;
            };
            let take = (bytes.len() as u64).min(self.tail_limit - *len) as usize;
            file.write_all(&bytes[..take])?;
            *len += take as u64;
            bytes = &bytes[take..];
        }
        Ok(())
    }

    fn match_lines(&mut self, bytes: &[u8]) {
        let Some(summary) = &self.summary else {
            return;
        };
        if self.summary_seen {
            return;
        }
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            let room = LINE_CAP.saturating_sub(self.line.len());
            self.line.extend_from_slice(&rest[..end.min(room)]);
            if line_matches(summary, &self.line) {
                self.summary_seen = true;
                self.line.clear();
                return;
            }
            self.line.clear();
            rest = &rest[end + 1..];
        }
        let room = LINE_CAP.saturating_sub(self.line.len());
        self.line.extend_from_slice(&rest[..rest.len().min(room)]);
    }

    /// Ends the output: matches the last partial line and joins head, marker and tail into the
    /// file. Later writes are dropped. Idempotent.
    pub(crate) fn finish(&mut self) -> OutputSummary {
        if let Some(done) = &self.finished {
            return done.clone();
        }
        if let Some(summary) = &self.summary
            && !self.summary_seen
            && line_matches(summary, &self.line)
        {
            self.summary_seen = true;
        }
        self.line = Vec::new();
        let cut = match self.join_tail() {
            Ok(cut) => cut,
            Err(error) => {
                log::warn!(
                    "checks: finishing the output {}: {error}",
                    self.log_path.display()
                );
                self.write_error.get_or_insert(error.to_string());
                0
            }
        };
        for tail in &self.tails {
            std::fs::remove_file(tail).ok();
        }
        self.log = None;
        let done = OutputSummary {
            total_bytes: self.total,
            cut_bytes: cut,
            summary_seen: self.summary_seen,
            write_error: self.write_error.clone(),
        };
        self.finished = Some(done.clone());
        done
    }

    /// Appends the kept tail to the file, with the marker when bytes were cut. Returns the
    /// bytes cut.
    fn join_tail(&mut self) -> std::io::Result<u64> {
        let Some((current, current_file, current_len)) = self.current.take() else {
            return Ok(0);
        };
        drop(current_file);
        let older = 1 - current;
        let older_len = if self.wrapped { self.tail_limit } else { 0 };
        let kept = older_len + current_len;
        let cut = self
            .total
            .saturating_sub(self.head_written)
            .saturating_sub(kept);
        let Some(log) = &mut self.log else {
            return Ok(cut);
        };
        if self.write_error.is_some() {
            return Ok(cut);
        }
        if cut > 0 {
            log.write_all(cut_marker(cut).as_bytes())?;
        }
        let mut buffer = vec![0u8; 64 * 1024];
        let mut order = Vec::new();
        if self.wrapped {
            order.push(older);
        }
        order.push(current);
        for index in order {
            let path = &self.tails[index];
            let mut file = File::open(path)?;
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                log.write_all(&buffer[..n])?;
            }
            drop(file);
            // Each tail goes as soon as it is copied, so the folder never holds much more than
            // the limit.
            std::fs::remove_file(path).ok();
        }
        log.flush()?;
        Ok(cut)
    }
}

impl Drop for OutputSink {
    fn drop(&mut self) {
        for tail in &self.tails {
            std::fs::remove_file(tail).ok();
        }
    }
}

fn line_matches(summary: &Regex, line: &[u8]) -> bool {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    summary.is_match(&String::from_utf8_lossy(line))
}
