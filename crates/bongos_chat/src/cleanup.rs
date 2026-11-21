//! The never-prompted cleanup (design D17): the only time bongos deletes a chat record by itself.
//!
//! When a chat is closed or its CLI exits, its record is deleted only when bongos is sure nothing
//! was said in it:
//! - the record says `prompted = false` (read fresh, after every write queued before), and
//! - the runtime saw no keyboard input and no send while it ran, and
//! - the CLI stored no conversation for it: for Claude Code, no transcript file for its session id
//!   under `~/.claude/projects/`; for Codex, no session id was captured and none can be found now.
//!
//! When any of these is unknown (an unknown agent, a Claude chat without a session id, a CLI
//! storage directory that is not there or cannot be read, a CLI whose storage the environment
//! moved with `CLAUDE_CONFIG_DIR` or `CODEX_HOME`) the record is kept: when unsure, keep.
//!
//! A new Claude chat whose channel went live in this run answers the last question from its
//! reports instead (design D11): no `prompt_submitted` and no `turn_done` report means the CLI stored
//! no conversation, and no file is read. Any other chat (resumed, forked, or one whose channel
//! never went live) keeps the file checks.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use bongos_chat_model::{Agent, ChatRecord, project_slug};

/// The variable that moves Claude Code's storage away from `~/.claude`.
const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
/// The variable that moves Codex's storage away from `~/.codex`.
const CODEX_HOME: &str = "CODEX_HOME";

/// What the cleanup does with a chat that ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanup {
    /// Nothing was said: the record goes.
    Delete,
    /// The record stays, for this reason.
    Keep(KeepReason),
}

/// Why the cleanup kept a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepReason {
    /// The record says the chat was prompted.
    Prompted,
    /// The runtime saw keyboard input or a send.
    InputSeen,
    /// The record names an agent this version does not know.
    UnknownAgent,
    /// A Claude chat with no session id: its transcript cannot be looked up.
    SessionUnknown,
    /// The CLI stored a conversation (a Claude transcript, a Codex session).
    ConversationStored,
    /// The CLI's storage directory is not there or could not be read.
    CannotCheck,
    /// The chat's channel reported a prompt or a finished turn.
    PromptReported,
}

/// The verdict on `record`, which ended. `saw_input` is whether the runtime saw keyboard input or
/// a send; `live_session` the session id the runtime knew; `home` where the CLIs keep their files;
/// `env` the environment the CLIs run in (a CLI whose storage it moves cannot be checked);
/// `find_codex_session` looks once more for a Codex session id in Codex's own files.
pub fn cleanup_verdict(
    record: &ChatRecord,
    saw_input: bool,
    live_session: Option<&str>,
    home: &Path,
    env: &BTreeMap<String, String>,
    find_codex_session: impl FnOnce() -> Option<String>,
) -> Cleanup {
    cleanup_verdict_with_reports(
        record,
        saw_input,
        None,
        live_session,
        home,
        env,
        find_codex_session,
    )
}

/// [`cleanup_verdict`] for a chat whose channel may answer: `prompt_reported` is `Some` only for a
/// new Claude chat whose channel went live in this run, and says whether a `prompt_submitted` or
/// `turn_done` report was accepted. Then no file is read.
pub fn cleanup_verdict_with_reports(
    record: &ChatRecord,
    saw_input: bool,
    prompt_reported: Option<bool>,
    live_session: Option<&str>,
    home: &Path,
    env: &BTreeMap<String, String>,
    find_codex_session: impl FnOnce() -> Option<String>,
) -> Cleanup {
    if record.prompted {
        return Cleanup::Keep(KeepReason::Prompted);
    }
    if saw_input {
        return Cleanup::Keep(KeepReason::InputSeen);
    }
    let session = live_session.or(record.session_id.as_deref());
    match record.known_agent() {
        None => Cleanup::Keep(KeepReason::UnknownAgent),
        Some(Agent::Claude) => {
            match prompt_reported {
                Some(true) => return Cleanup::Keep(KeepReason::PromptReported),
                Some(false) => return Cleanup::Delete,
                None => {}
            }
            let Some(session) = session else {
                return Cleanup::Keep(KeepReason::SessionUnknown);
            };
            if env.contains_key(CLAUDE_CONFIG_DIR) {
                return Cleanup::Keep(KeepReason::CannotCheck);
            }
            match claude_transcript_exists(home, &record.cwd, session) {
                Some(false) => Cleanup::Delete,
                Some(true) => Cleanup::Keep(KeepReason::ConversationStored),
                None => Cleanup::Keep(KeepReason::CannotCheck),
            }
        }
        Some(Agent::Codex) => {
            if session.is_some() {
                return Cleanup::Keep(KeepReason::ConversationStored);
            }
            if env.contains_key(CODEX_HOME) {
                return Cleanup::Keep(KeepReason::CannotCheck);
            }
            note_storage_read(home);
            if !home.join(".codex").join("sessions").is_dir() {
                return Cleanup::Keep(KeepReason::CannotCheck);
            }
            match find_codex_session() {
                Some(_) => Cleanup::Keep(KeepReason::ConversationStored),
                None => Cleanup::Delete,
            }
        }
    }
}

/// The homes whose CLI storage a lookup read, in order, for the tests.
#[cfg(test)]
static STORAGE_READS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Notes a look into the CLI storage under `home` (tests only; nothing in release builds).
fn note_storage_read(home: &Path) {
    #[cfg(test)]
    STORAGE_READS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(home.to_path_buf());
    #[cfg(not(test))]
    let _ = home;
}

/// How many times a lookup read the CLI storage under `home` so far.
#[cfg(test)]
pub(crate) fn storage_reads(home: &Path) -> usize {
    STORAGE_READS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|read| read.as_path() == home)
        .count()
}

/// Whether Claude Code keeps a transcript of `session` run in `cwd`: `None` when its projects
/// directory is not there or a lookup failed.
fn claude_transcript_exists(home: &Path, cwd: &Path, session: &str) -> Option<bool> {
    claude_transcript(home, cwd, session).map(|found| found.is_some())
}

/// The transcript file of the Claude Code `session` run in `cwd`: `Some(None)` when there is none,
/// `None` when its projects directory is not there or a lookup failed. The directory is looked up
/// for `cwd` as written and as resolved, since either can name it.
pub(crate) fn claude_transcript(home: &Path, cwd: &Path, session: &str) -> Option<Option<PathBuf>> {
    // The id is one file name; anything else is not looked up.
    if session.is_empty() || session.starts_with('.') || session.contains(['/', '\\', '\0']) {
        return None;
    }
    note_storage_read(home);
    let projects = home.join(".claude").join("projects");
    if !projects.is_dir() {
        return None;
    }
    let mut cwds: Vec<PathBuf> = vec![cwd.to_path_buf()];
    if let Ok(resolved) = std::fs::canonicalize(cwd)
        && resolved != cwd
    {
        cwds.push(resolved);
    }
    for cwd in cwds {
        let transcript = projects
            .join(project_slug(&cwd))
            .join(format!("{session}.jsonl"));
        match std::fs::symlink_metadata(&transcript) {
            Ok(_) => return Some(Some(transcript)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(None)
}
