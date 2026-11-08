//! The file model of bongos projects and tasks: the types in `task.toml`, `project.toml`,
//! `artifacts.toml` and `<name>.link.toml`, their parsing and serialization, slugs, the on-disk
//! layout, and atomic file writes; loading the tree, the derived state, and the rules and commands
//! that change it. It has no UI dependency and never touches the real home directory: every path
//! function takes the root it works under.

pub mod activity;
pub mod commands;
pub mod derive;
mod error;
pub mod files;
pub mod graph;
pub mod io;
pub mod journal;
pub mod layout;
pub mod rollup;
pub mod rules;
pub mod slug;
pub mod store;
mod types;

pub use error::{Error, ErrorKind, Result};
pub use io::{
    SchemaFile, parse_artifacts, parse_history, parse_link, parse_project, parse_task,
    read_artifacts, read_history, read_link, read_project, read_task, serialize_artifacts,
    serialize_history, serialize_link, serialize_project, serialize_task,
};
pub use types::*;

#[cfg(test)]
mod activity_tests;

#[cfg(test)]
mod commands_tests;

#[cfg(test)]
mod derive_tests;

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod graph_tests;

#[cfg(test)]
mod rollup_tests;

#[cfg(test)]
mod rules_tests;

#[cfg(test)]
mod store_tests;

#[cfg(test)]
mod tests;
