//! The definition model of a project repository: how a `task.toml` divides into the part that
//! travels in the repository (the definition) and the part that stays on one machine (the
//! state), and the layout of the repository and of the project folder around it. Pure data and
//! functions; no UI. Only the loader of definitions reads files.

mod change_record;
pub mod checks;
mod definitions;
pub mod layout;
mod split;
mod summary;

pub use bongos_work_model::{Author, AuthorKind, PersonAtKeyboard};
pub use change_record::{
    ChangeMeta, CompatibilityMark, LandingFacts, TRAILER_PREFIX, author_kind_word, commit_message,
    parse_trailers, strip_trailers,
};
pub use definitions::{
    DefinitionNode, DefinitionTree, Problem, RelPath, StateLookup, work_tree_from,
};
pub use split::{
    RESERVED_STATE_KEYS, TaskDefinition, TaskStateRecord, TodoDefinition, TodoStateRecord, join,
    reserved_keys_in, split,
};
pub use summary::{ChangeSummary, EdgeChange, EdgeKind, FileChange, summarize};

#[cfg(test)]
mod change_record_tests;
#[cfg(test)]
mod checks_tests;
#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod split_tests;
#[cfg(test)]
mod summary_tests;
#[cfg(test)]
mod test_trees;
