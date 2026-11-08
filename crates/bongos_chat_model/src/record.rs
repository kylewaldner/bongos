//! The record of one chat: `<scope dir>/chats/<chat id>.toml`. The scope (task or project) and the
//! task id are not stored; they come from where the file is. Keys this version does not know are
//! kept on read and written back.

use std::path::{Path, PathBuf};

use bongos_work_model::files::write_atomic;
use bongos_work_model::layout;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};

pub use bongos_work_model::{Extra, SCHEMA, Timestamp};

/// The command line agents bongos knows how to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    #[default]
    Dual,
    ChatOnly,
}

/// Who chose the chat's name. A name set by `Kyle` is never overwritten.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamedBy {
    #[default]
    Auto,
    Kyle,
    None,
}

/// How a forked chat got its conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fork {
    Native,
    Handoff,
}

fn schema_one() -> u32 {
    SCHEMA
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatRecord {
    #[serde(default = "schema_one")]
    pub schema: u32,
    /// The bongos chat id, a UUID v7.
    pub id: String,
    /// The agent's name as written. Use [`ChatRecord::known_agent`] to get an [`Agent`]; a record
    /// with another name is still listed, with an error.
    pub agent: String,
    /// The CLI's own session id; absent until known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The name of a directory under `chat-types/`.
    #[serde(rename = "type")]
    pub chat_type: String,
    pub name: String,
    #[serde(default)]
    pub named_by: NamedBy,
    pub cwd: PathBuf,
    #[serde(default)]
    pub layout: Layout,
    /// Present only when it differs from the `prompts.txt` of the scope directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The id of the chat this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<Fork>,
    pub created: Timestamp,
    pub last_active: Timestamp,
    #[serde(default)]
    pub prompted: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<Timestamp>,
    /// The digest of the artifact list given at the last launch (see [`crate::context_digest`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_given: Option<Timestamp>,
    /// Keys this version does not know, kept so a later feature's fields survive our writes.
    #[serde(flatten)]
    pub extra: Extra,
}

impl ChatRecord {
    /// A new, unprompted record named "<type> <agent> <date>" with a fresh id.
    pub fn new(agent: Agent, chat_type: &str, cwd: PathBuf) -> Self {
        let now = bongos_work_model::now();
        Self {
            schema: SCHEMA,
            id: bongos_work_model::new_id(),
            agent: agent.as_str().to_string(),
            session_id: None,
            chat_type: chat_type.to_string(),
            name: crate::naming::default_name(chat_type, agent, chrono::Utc::now().date_naive()),
            named_by: NamedBy::None,
            cwd,
            layout: Layout::Dual,
            prompts_file: None,
            model: None,
            parent: None,
            fork: None,
            created: now,
            last_active: now,
            prompted: false,
            archived: false,
            archived_at: None,
            context_digest: None,
            context_given: None,
            extra: Extra::new(),
        }
    }

    pub fn known_agent(&self) -> Option<Agent> {
        Agent::from_name(&self.agent)
    }

    /// Sets an automatic name unless Kyle named the chat. Returns whether the name changed.
    pub fn apply_auto_name(&mut self, name: String) -> bool {
        if self.named_by == NamedBy::Kyle {
            return false;
        }
        self.name = name;
        self.named_by = NamedBy::Auto;
        true
    }
}

/// `<scope dir>/chats`.
pub fn chats_dir(scope_dir: &Path) -> PathBuf {
    layout::chats_dir(scope_dir)
}

/// `<scope dir>/chats/<chat id>.toml`.
pub fn record_path(scope_dir: &Path, chat_id: &str) -> PathBuf {
    chats_dir(scope_dir).join(format!("{chat_id}.toml"))
}

/// Whether `name`, an entry of a `chats` directory, is a chat record: `<something>.toml` that is
/// not a temporary file of an atomic write.
pub fn is_record_file_name(name: &str) -> bool {
    name.strip_suffix(".toml")
        .is_some_and(|stem| !stem.is_empty() && !layout::is_temp_file(name))
}

#[derive(Deserialize)]
struct SchemaProbe {
    schema: Option<toml::Value>,
}

/// Parses a TOML file that must carry `schema = 1`, naming `path` in any error.
pub(crate) fn parse<T: serde::de::DeserializeOwned>(text: &str, path: Option<&Path>) -> Result<T> {
    let probe: SchemaProbe =
        toml::from_str(text).map_err(|error| Error::syntax(text, &error).with_path(path))?;
    match probe.schema {
        Some(toml::Value::Integer(version)) if version == i64::from(SCHEMA) => {}
        found => {
            let found = found.map(|value| value.to_string());
            return Err(Error::new(ErrorKind::UnsupportedSchema { found }).with_path(path));
        }
    }
    toml::from_str(text).map_err(|error| Error::syntax(text, &error).with_path(path))
}

pub fn parse_record(text: &str) -> Result<ChatRecord> {
    parse(text, None)
}

/// Reads a record. A file that does not parse is an error naming the file, line and column, and
/// is left as it is.
pub fn read_record(path: &Path) -> Result<ChatRecord> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    parse(&text, Some(path))
}

pub fn serialize_record(record: &ChatRecord) -> Result<String> {
    if record.schema != SCHEMA {
        return Err(Error::new(ErrorKind::UnsupportedSchema {
            found: Some(record.schema.to_string()),
        }));
    }
    toml::to_string(record).map_err(|error| Error::new(ErrorKind::Serialize(error.to_string())))
}

/// Replaces the file at `path` with `record`, atomically. The directory must exist.
pub fn write_record(path: &Path, record: &ChatRecord) -> Result<()> {
    let text = serialize_record(record).map_err(|error| error.with_path(Some(path)))?;
    write_atomic(path, text.as_bytes()).map_err(|error| Error::io(path, error))
}

/// Writes a new record into `<scope dir>/chats/`, creating the directory, and returns its path.
/// Refuses to replace an existing file.
pub fn create_record(scope_dir: &Path, record: &ChatRecord) -> Result<PathBuf> {
    let path = record_path(scope_dir, &record.id);
    if path.exists() {
        let error = std::io::Error::new(std::io::ErrorKind::AlreadyExists, "chat already exists");
        return Err(Error::io(&path, error));
    }
    std::fs::create_dir_all(chats_dir(scope_dir)).map_err(|error| Error::io(&path, error))?;
    write_record(&path, record)?;
    Ok(path)
}

/// Applies `change` to a fresh read of the record at `path` and writes the result back. A missing
/// file is an error, not a new record; a file that does not parse is an error and is not touched.
pub fn update_record(
    path: &Path,
    change: impl FnOnce(&mut ChatRecord) -> Result<()>,
) -> Result<ChatRecord> {
    let mut record = read_record(path)?;
    change(&mut record)?;
    write_record(path, &record)?;
    Ok(record)
}

/// A timestamp as a UTC instant, for ordering. A datetime without an offset counts as UTC; a
/// date or time alone has no instant.
pub fn timestamp_utc(timestamp: &Timestamp) -> Option<DateTime<Utc>> {
    let text = timestamp.to_string();
    if let Ok(with_offset) = DateTime::parse_from_rfc3339(&text) {
        return Some(with_offset.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(&text, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|local| local.and_utc())
}
