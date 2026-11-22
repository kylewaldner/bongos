//! A log file that keeps one older copy: once it would grow past its limit it is renamed to
//! `<name>.1.log` (replacing an older one) and a new one is started.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

pub struct RotatingLogFile {
    path: PathBuf,
    file: File,
    size_bytes: u64,
    max_bytes: u64,
}

impl RotatingLogFile {
    /// Opens the log at `path` for appending, rotating it first when it is already `max_bytes`
    /// or larger.
    pub fn open(path: &Path, max_bytes: u64) -> Result<Self> {
        if std::fs::metadata(path)
            .map(|metadata| metadata.len() >= max_bytes)
            .unwrap_or(false)
        {
            rotate_log_file(path, &rotated_log_path(path))
                .with_context(|| format!("failed to rotate the log {}", path.display()))?;
        }

        let file = open_log_file(path)
            .with_context(|| format!("failed to open the log {}", path.display()))?;
        let size_bytes = file
            .metadata()
            .with_context(|| format!("failed to read the metadata of {}", path.display()))?
            .len();

        Ok(Self {
            path: path.to_path_buf(),
            file,
            size_bytes,
            max_bytes,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        rotate_log_file(&self.path, &rotated_log_path(&self.path))?;
        self.file = open_log_file(&self.path)?;
        self.size_bytes = 0;
        Ok(())
    }
}

impl Write for RotatingLogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.size_bytes.saturating_add(buf.len() as u64) > self.max_bytes {
            self.rotate()?;
        }

        self.file.write_all(buf)?;
        self.size_bytes += buf.len() as u64;

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn open_log_file(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// The older copy of the log at `path`: `server-12.log` becomes `server-12.1.log`.
pub fn rotated_log_path(path: &Path) -> PathBuf {
    path.with_extension("1.log")
}

fn rotate_log_file(path: &Path, rotated_path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(rotated_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    match std::fs::rename(path, rotated_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_BYTES: u64 = 1024;

    #[test]
    fn rotated_log_path_uses_numbered_log_suffix() {
        assert_eq!(
            rotated_log_path(Path::new("server-workspace-12.log")),
            PathBuf::from("server-workspace-12.1.log")
        );
    }

    #[test]
    fn opening_log_rotates_existing_oversized_log() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let log_path = temp_dir.path().join("server-workspace-12.log");
        let rotated_path = temp_dir.path().join("server-workspace-12.1.log");
        let existing_contents = vec![b'x'; MAX_BYTES as usize];
        std::fs::write(&log_path, &existing_contents).expect("write oversized log");

        let _log_file = RotatingLogFile::open(&log_path, MAX_BYTES).expect("open rotating log");

        assert_eq!(
            std::fs::read(&rotated_path).expect("read rotated log"),
            existing_contents
        );
        assert_eq!(
            std::fs::metadata(&log_path)
                .expect("active log metadata")
                .len(),
            0
        );
    }

    #[test]
    fn writing_log_rotates_before_exceeding_size_limit() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let log_path = temp_dir.path().join("server-workspace-12.log");
        let rotated_path = temp_dir.path().join("server-workspace-12.1.log");
        let existing_contents = vec![b'x'; MAX_BYTES as usize - 1];
        let new_contents = b"yz";
        std::fs::write(&log_path, &existing_contents).expect("write existing log contents");
        let mut log_file = RotatingLogFile::open(&log_path, MAX_BYTES).expect("open rotating log");

        log_file
            .write_all(new_contents)
            .expect("write log contents");
        log_file.flush().expect("flush log file");

        assert_eq!(
            std::fs::read(&rotated_path).expect("read rotated log"),
            existing_contents
        );
        assert_eq!(
            std::fs::read(&log_path).expect("read active log"),
            new_contents
        );
    }
}
