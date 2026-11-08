//! Finding a session id in an agent's own files. The scans read file names, times and the first
//! bytes of a file; they never keep or return anything else, and they never touch a process.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What `capture_session_id` is asked.
#[derive(Clone, Copy, Debug)]
pub struct CaptureQuery<'a> {
    /// The bongos chat id. A Codex instructions file contains it, which tells two chats apart.
    pub chat_id: &'a str,
    pub cwd: &'a Path,
    /// The launch time: only files created at or after it count.
    pub since: SystemTime,
    /// Session ids that other chat records already hold.
    pub claimed: &'a BTreeSet<String>,
    /// The home directory the agents keep their files under.
    pub home: &'a Path,
}

pub(crate) struct Candidate {
    pub id: String,
    pub path: PathBuf,
    pub created: SystemTime,
}

/// How much of a file is read when looking for a chat id or a first-line record.
const HEAD_BYTES: u64 = 64 * 1024;

pub(crate) fn read_head(path: &Path) -> String {
    let mut bytes = Vec::new();
    if let Ok(file) = File::open(path) {
        let _ = file.take(HEAD_BYTES).read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn created_at(path: &Path) -> Option<SystemTime> {
    let meta = fs::metadata(path).ok()?;
    meta.created().or_else(|_| meta.modified()).ok()
}

/// A UUID in its 36 character form.
pub(crate) fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.char_indices().all(|(index, c)| match index {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The candidate whose file holds the chat id, if any.
pub(crate) fn pick_by_chat_id(query: &CaptureQuery, candidates: &[Candidate]) -> Option<String> {
    if query.chat_id.is_empty() {
        return None;
    }
    candidates
        .iter()
        .find(|candidate| read_head(&candidate.path).contains(query.chat_id))
        .map(|candidate| candidate.id.clone())
}

/// The one candidate to use, or `None` when it cannot be told. A file that holds the chat id is the
/// match. Otherwise a single candidate is the match, and several are not: the second of two chats
/// started together waits until the first has been claimed.
pub(crate) fn pick_unambiguous(query: &CaptureQuery, candidates: &[Candidate]) -> Option<String> {
    if let Some(id) = pick_by_chat_id(query, candidates) {
        return Some(id);
    }
    match candidates {
        [only] => Some(only.id.clone()),
        _ => None,
    }
}

/// The newest candidate by creation time.
pub(crate) fn pick_newest(candidates: &[Candidate]) -> Option<String> {
    candidates
        .iter()
        .max_by_key(|candidate| candidate.created)
        .map(|candidate| candidate.id.clone())
}
