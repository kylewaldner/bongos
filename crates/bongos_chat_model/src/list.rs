//! The chats of a scope: reading every record in a `chats` directory, the open and archived
//! lists with their order, and fork lookup by `parent`. An invalid file is an entry of its own
//! with its error, so it is listed by file name and never rewritten.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::record::{ChatRecord, chats_dir, is_record_file_name, read_record, timestamp_utc};

/// One file of a `chats` directory: its record, or the reason it could not be read.
#[derive(Debug)]
pub struct ChatEntry {
    pub path: PathBuf,
    pub chat: Result<ChatRecord, Error>,
}

impl ChatEntry {
    pub fn record(&self) -> Option<&ChatRecord> {
        self.chat.as_ref().ok()
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The record's id, or for an invalid file its name without `.toml`.
    pub fn id(&self) -> String {
        match &self.chat {
            Ok(record) => record.id.clone(),
            Err(_) => self
                .path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

/// Reads every record in `chats_dir`, in file name order. Temporary files and anything that is
/// not `*.toml` are skipped. A missing directory has no chats.
pub fn read_chats_dir(chats_dir: &Path) -> Result<Vec<ChatEntry>> {
    let names = match std::fs::read_dir(chats_dir) {
        Ok(names) => names,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io(chats_dir, error)),
    };
    let mut paths = Vec::new();
    for entry in names {
        let entry = entry.map_err(|error| Error::io(chats_dir, error))?;
        let name = entry.file_name();
        if is_record_file_name(&name.to_string_lossy()) && entry.path().is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| ChatEntry {
            chat: read_record(&path),
            path,
        })
        .collect())
}

/// [`read_chats_dir`] for `<scope dir>/chats`.
pub fn read_scope_chats(scope_dir: &Path) -> Result<Vec<ChatEntry>> {
    read_chats_dir(&chats_dir(scope_dir))
}

/// The open and archived chats, each in display order.
#[derive(Debug)]
pub struct ChatLists<'a> {
    pub open: Vec<&'a ChatEntry>,
    pub archived: Vec<&'a ChatEntry>,
}

/// Splits `entries` into the open and archived lists. Both are sorted by `last_active`, most
/// recent first, ties by id; an invalid file has no record to sort by, so it comes after the
/// valid ones, by file name, in the open list where it can be seen.
pub fn chat_lists(entries: &[ChatEntry]) -> ChatLists<'_> {
    let (mut archived, mut open): (Vec<&ChatEntry>, Vec<&ChatEntry>) = entries
        .iter()
        .partition(|entry| entry.record().is_some_and(|record| record.archived));
    open.sort_by(|a, b| display_order(a, b));
    archived.sort_by(|a, b| display_order(a, b));
    ChatLists { open, archived }
}

/// The chats forked from `parent_id`, in display order.
pub fn forks_of<'a>(entries: &'a [ChatEntry], parent_id: &str) -> Vec<&'a ChatEntry> {
    let mut forks: Vec<&ChatEntry> = entries
        .iter()
        .filter(|entry| {
            entry
                .record()
                .is_some_and(|record| record.parent.as_deref() == Some(parent_id))
        })
        .collect();
    forks.sort_by(|a, b| display_order(a, b));
    forks
}

fn display_order(a: &ChatEntry, b: &ChatEntry) -> Ordering {
    match (a.record(), b.record()) {
        (Some(a), Some(b)) => timestamp_utc(&b.last_active)
            .cmp(&timestamp_utc(&a.last_active))
            .then_with(|| a.id.cmp(&b.id)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.file_name().cmp(&b.file_name()),
    }
}
