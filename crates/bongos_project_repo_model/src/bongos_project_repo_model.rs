//! The definition model of a project repository: how a `task.toml` divides into the part that
//! travels in the repository (the definition) and the part that stays on one machine (the
//! state), and the layout of the repository and of the project folder around it. Pure data and
//! functions; no UI. Only the loader of definitions reads files.

mod definitions;
pub mod layout;
mod split;

pub use bongos_work_model::{Author, AuthorKind, PersonAtKeyboard};
pub use definitions::{
    DefinitionNode, DefinitionTree, Problem, RelPath, StateLookup, work_tree_from,
};
pub use split::{
    RESERVED_STATE_KEYS, TaskDefinition, TaskStateRecord, TodoDefinition, TodoStateRecord, join,
    reserved_keys_in, split,
};

#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod split_tests;
