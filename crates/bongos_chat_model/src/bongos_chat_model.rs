//! The file model of bongos chat sessions: the record of one chat (`chats/<chat id>.toml` in a task
//! or project directory), reading and writing it without losing keys this version does not know,
//! the open and archived lists with their order, fork lookup, chat names, and the digest of the
//! artifact list a chat was given, the agent adapters (capabilities, launch lines and fallbacks, login
//! probe, session id capture), the pure renderer of what a launch injects (`context.md`, the
//! system prompt, the hook settings and the environment), and the session types (directories of plain files, with their
//! shipped defaults, seeding and resolution). It has no UI dependency and never touches the real home
//! directory: every path function takes the directory it works under.

mod adapter;
mod capture;
mod claude;
mod codex;
mod digest;
mod error;
mod facts;
mod inject;
mod list;
mod naming;
mod record;
mod shipped;
mod types;

pub use adapter::{
    AgentAdapter, Capabilities, CaptureQuery, ENV_NAMES, Fallback, GeneratedFiles, HiddenFlag,
    LaunchKind, LaunchPlan, LaunchSpec, Login, Shape, adapter_for, version_at_least,
};
pub use claude::{BELL_KEY, BELL_VALUE, ClaudeAdapter, project_slug};
pub use codex::{CODEX_TABLE, CodexAdapter, CodexTable};
pub use digest::{DIGEST_PREFIX, context_digest};
pub use error::{Error, ErrorKind, Result};
pub use facts::TaskFacts;
pub use inject::{
    CHAT_ID_LABEL, HandoffParent, RenderInput, handoff_note, render_codex_instructions,
    render_context, render_env, render_settings, render_system, shell_quote,
};
pub use list::{ChatEntry, ChatLists, chat_lists, forks_of, read_chats_dir, read_scope_chats};
pub use naming::{MAX_NAME_CHARS, default_name, from_prompt, from_title};
pub use record::{
    Agent, ChatRecord, Extra, Fork, Layout, NamedBy, SCHEMA, Timestamp, chats_dir, create_record,
    is_record_file_name, parse_record, read_record, record_path, serialize_record, timestamp_utc,
    update_record, write_record,
};
pub use shipped::{SHIPPED_TYPES, ShippedType};
pub use types::{
    AgentOverrides, ResolvedSkill, ResolvedType, SKILLS_DIR, TypeConfig, parse_type_config,
    resolve, seed,
};

#[cfg(test)]
mod adapter_tests;

#[cfg(test)]
mod claude_tests;

#[cfg(test)]
mod codex_tests;

#[cfg(test)]
mod digest_tests;

#[cfg(test)]
mod inject_tests;

#[cfg(test)]
mod list_tests;

#[cfg(test)]
mod naming_tests;

#[cfg(test)]
mod record_tests;

#[cfg(test)]
mod types_tests;
