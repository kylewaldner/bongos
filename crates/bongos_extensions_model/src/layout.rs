//! Paths of the extensions directory.

use std::path::PathBuf;

use crate::SourceUrl;

const RESERVED_IDS: [&str; 3] = ["staging", "logs", "locks"];

/// Why an extension id was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("the extension id is empty")]
    Empty,
    #[error("`{0}` is reserved and cannot be an extension id")]
    Reserved(String),
    #[error("`{0}` is not a valid extension id; use ASCII letters, digits, `-` and `_`")]
    Invalid(String),
}

/// Names under `git/` that hold bookkeeping instead of an extension.
pub fn is_reserved_id(id: &str) -> bool {
    RESERVED_IDS.contains(&id)
}

/// Checks that `id` is safe to use as a directory name under the extensions directory.
pub fn validate_id(id: &str) -> Result<(), IdError> {
    if id.is_empty() {
        return Err(IdError::Empty);
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(IdError::Invalid(id.to_string()));
    }
    if is_reserved_id(id) {
        return Err(IdError::Reserved(id.to_string()));
    }
    Ok(())
}

/// The on-disk layout below the extensions directory.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub fn installed_dir(&self) -> PathBuf {
        self.root.join("installed")
    }

    pub fn git_dir(&self) -> PathBuf {
        self.root.join("git")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.git_dir().join("staging")
    }

    pub fn staging_for(&self, nonce: &str) -> PathBuf {
        self.staging_dir().join(nonce)
    }

    pub fn extension_dir(&self, id: &str) -> PathBuf {
        self.git_dir().join(id)
    }

    pub fn checkout_dir(&self, id: &str, commit: &str) -> PathBuf {
        self.extension_dir(id).join(commit)
    }

    pub fn rebuild_checkout_dir(&self, id: &str, commit: &str, nonce: &str) -> PathBuf {
        self.extension_dir(id)
            .join(format!("{commit}.rebuild-{nonce}"))
    }

    pub fn record_path(&self, id: &str) -> PathBuf {
        self.extension_dir(id).join("record.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.git_dir().join("logs")
    }

    pub fn log_path(&self, key: &str) -> PathBuf {
        self.logs_dir().join(format!("{key}.log"))
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.git_dir().join("locks")
    }

    pub fn lock_path(&self, key: &str) -> PathBuf {
        self.locks_dir().join(format!("{key}.lock"))
    }

    pub fn symlink_path(&self, id: &str) -> PathBuf {
        self.installed_dir().join(id)
    }

    /// A short stable key for a source: `source-` and 8 hex digits of FNV-1a over the
    /// normalized URL.
    pub fn source_key(&self, url: &SourceUrl) -> String {
        let mut hash: u32 = 0x811c_9dc5;
        for byte in url.as_str().bytes() {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        format!("source-{hash:08x}")
    }
}
