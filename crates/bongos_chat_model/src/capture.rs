//! Finding a session id in an agent's own files. The scans read file names, times and the first
//! bytes of a file; they never keep or return anything else, and they never touch a process.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What `capture_session_id` is asked.
///
/// Caller rule (hard): these functions keep no state. The runtime must serialize launches of
/// agents that need a capture per `cwd` (Codex): the next one starts only after the first one's
/// id was captured and added to `claimed`, and `claimed` must be up to date before every ask.
/// Without that, two chats started in one directory can be given each other's session.
///
/// A session file whose head holds `chat id: <id>` (the line `codex-instructions.md` carries)
/// belongs to that chat: it is the match for that chat and never a candidate for another. Whether
/// Codex stores the instructions text in its session files is unverified; when it does not, the
/// serialization above is what keeps captures apart.
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

/// The bongos chat id a session file names in its head (`chat id: <id>`), if any.
fn head_chat_id(head: &str) -> Option<String> {
    let start = head.find(crate::inject::CHAT_ID_LABEL)? + crate::inject::CHAT_ID_LABEL.len();
    let id: String = head[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!id.is_empty()).then_some(id)
}

/// Drops the candidates whose head names a different bongos chat.
pub(crate) fn drop_other_chats(query: &CaptureQuery, candidates: &mut Vec<Candidate>) {
    candidates.retain(|candidate| {
        head_chat_id(&read_head(&candidate.path)).is_none_or(|id| id == query.chat_id)
    });
}

/// The candidate whose head names this chat, if any.
pub(crate) fn pick_by_chat_id(query: &CaptureQuery, candidates: &[Candidate]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| {
            head_chat_id(&read_head(&candidate.path)).as_deref() == Some(query.chat_id)
        })
        .map(|candidate| candidate.id.clone())
}

/// The one candidate to use, or `None` when it cannot be told. Candidates that name another chat
/// are dropped; one that names this chat is the match; otherwise a single candidate is the match
/// and several are not. The caller must make sure the cases that remain cannot be mistaken (see
/// [`CaptureQuery`]).
pub(crate) fn pick_unambiguous(
    query: &CaptureQuery,
    mut candidates: Vec<Candidate>,
) -> Option<String> {
    drop_other_chats(query, &mut candidates);
    if let Some(id) = pick_by_chat_id(query, &candidates) {
        return Some(id);
    }
    match candidates.as_slice() {
        [only] => Some(only.id.clone()),
        _ => None,
    }
}

/// The newest candidate by creation time, after dropping those that name another chat.
pub(crate) fn pick_newest(query: &CaptureQuery, mut candidates: Vec<Candidate>) -> Option<String> {
    drop_other_chats(query, &mut candidates);
    pick_by_chat_id(query, &candidates).or_else(|| {
        candidates
            .iter()
            .max_by_key(|candidate| candidate.created)
            .map(|candidate| candidate.id.clone())
    })
}
