//! The definition model of a project repository: how a `task.toml` divides into the part that
//! travels in the repository (the definition) and the part that stays on one machine (the
//! state), and the layout of the repository and of the project folder around it. Pure data and
//! functions; no UI, no file access.

pub mod layout;
mod split;

pub use bongos_work_model::{Author, AuthorKind, PersonAtKeyboard};
pub use split::{
    RESERVED_STATE_KEYS, TaskDefinition, TaskStateRecord, TodoDefinition, TodoStateRecord, join,
    reserved_keys_in, split,
};

#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod split_tests;
