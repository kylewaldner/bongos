//! The file model of bongos chat sessions: the record of one chat (`chats/<chat id>.toml` in a task
//! or project directory), reading and writing it without losing keys this version does not know,
//! the open and archived lists with their order, fork lookup, chat names, and the digest of the
//! artifact list a chat was given. It has no UI dependency and never touches the real home
//! directory: every path function takes the directory it works under.

mod digest;
mod error;
mod list;
mod naming;
mod record;

pub use digest::{DIGEST_PREFIX, context_digest};
pub use error::{Error, ErrorKind, Result};
pub use list::{ChatEntry, ChatLists, chat_lists, forks_of, read_chats_dir, read_scope_chats};
pub use naming::{MAX_NAME_CHARS, default_name, from_prompt, from_title};
pub use record::{
    Agent, ChatRecord, Extra, Fork, Layout, NamedBy, SCHEMA, Timestamp, chats_dir, create_record,
    is_record_file_name, parse_record, read_record, record_path, serialize_record, timestamp_utc,
    update_record, write_record,
};

#[cfg(test)]
mod digest_tests;

#[cfg(test)]
mod list_tests;

#[cfg(test)]
mod naming_tests;

#[cfg(test)]
mod record_tests;
