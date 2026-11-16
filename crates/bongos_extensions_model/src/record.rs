//! The install record kept beside each git-installed extension.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const TEMP_PREFIX: &str = "record.json.tmp-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordState {
    Installed,
    Removing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstallRecord {
    pub version: u32,
    pub id: String,
    /// Normalized source URL.
    pub source_url: String,
    pub path: Option<String>,
    pub requested_ref: Option<String>,
    /// Full commit hash.
    pub commit: String,
    pub listed: bool,
    pub trusted_commit: Option<String>,
    pub state: RecordState,
    /// RFC 3339 text supplied by the caller.
    pub installed_at: String,
    /// Fields written by newer versions, kept on rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReadOutcome {
    Missing,
    Ok(InstallRecord),
    /// The file could not be used and was renamed to `record.json.bad`.
    Unreadable,
}

/// Writes the record through a temp file in the same directory, then renames it into place.
pub fn write_atomic(path: &Path, record: &InstallRecord, nonce: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!("{TEMP_PREFIX}{nonce}"));
    let bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    let result = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        fs::remove_file(&temp).ok();
        return result;
    }
    if let Ok(dir_file) = File::open(dir) {
        dir_file.sync_all().ok();
    }
    Ok(())
}

/// Reads the record. A file that cannot be parsed, or has a newer version, is moved aside to
/// `record.json.bad`. Other I/O errors are returned and leave the file in place. Temp files are
/// never read.
pub fn read(path: &Path) -> std::io::Result<ReadOutcome> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReadOutcome::Missing);
        }
        Err(error) => return Err(error),
    };
    match serde_json::from_slice::<InstallRecord>(&bytes) {
        Ok(record) if record.version <= 1 => Ok(ReadOutcome::Ok(record)),
        _ => Ok(move_aside(path)),
    }
}

fn move_aside(path: &Path) -> ReadOutcome {
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    let bad = PathBuf::from(bad);
    fs::remove_file(&bad).ok();
    fs::rename(path, &bad).ok();
    ReadOutcome::Unreadable
}

/// Deletes leftover `record.json.tmp-*` files in `dir`.
pub fn remove_torn_temp_files(dir: &Path) -> std::io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}
