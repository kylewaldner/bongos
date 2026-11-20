//! The chats of a scope: reading every record in a `chats` directory, the open and archived
//! lists with their order, and fork lookup by `parent`. An invalid file is an entry of its own
//! with its error, so it is listed by file name and never rewritten.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::record::{
    ChatRecord, Timestamp, chats_dir, is_record_file_name, read_record, timestamp_utc,
};

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
/// valid ones, by file name, in the open list where it can be seen. A live chat sorts by its entry
/// in `activity` (its in-memory last activity, newer than its record's), a closed one by its record.
pub fn chat_lists<'a>(
    entries: &'a [ChatEntry],
    activity: &HashMap<String, Timestamp>,
) -> ChatLists<'a> {
    let (mut archived, mut open): (Vec<&ChatEntry>, Vec<&ChatEntry>) = entries
        .iter()
        .partition(|entry| entry.record().is_some_and(|record| record.archived));
    open.sort_by(|a, b| display_order(a, b, activity));
    archived.sort_by(|a, b| display_order(a, b, activity));
    ChatLists { open, archived }
}

/// The chats forked from `parent_id`, in display order.
pub fn forks_of<'a>(
    entries: &'a [ChatEntry],
    parent_id: &str,
    activity: &HashMap<String, Timestamp>,
) -> Vec<&'a ChatEntry> {
    let mut forks: Vec<&ChatEntry> = entries
        .iter()
        .filter(|entry| {
            entry
                .record()
                .is_some_and(|record| record.parent.as_deref() == Some(parent_id))
        })
        .collect();
    forks.sort_by(|a, b| display_order(a, b, activity));
    forks
}

/// Newest last activity first, ties by id; `activity` overrides the record's `last_active` for
/// live chats. A file that could not be read has no time: it comes after the readable ones, by
/// file name.
pub fn display_order(
    a: &ChatEntry,
    b: &ChatEntry,
    activity: &HashMap<String, Timestamp>,
) -> Ordering {
    let time = |record: &ChatRecord| {
        timestamp_utc(activity.get(&record.id).unwrap_or(&record.last_active))
    };
    match (a.record(), b.record()) {
        (Some(a), Some(b)) => time(b).cmp(&time(a)).then_with(|| a.id.cmp(&b.id)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.file_name().cmp(&b.file_name()),
    }
}
