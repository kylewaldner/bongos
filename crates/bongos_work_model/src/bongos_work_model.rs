//! The file model of bongos projects and tasks: the types in `task.toml`, `project.toml`,
//! `artifacts.toml` and `<name>.link.toml`, their parsing and serialization, slugs, the on-disk
//! layout, and atomic file writes. It has no UI dependency and never touches the real home
//! directory: every path function takes the root it works under.

mod error;
pub mod files;
pub mod io;
pub mod layout;
pub mod slug;
mod types;

pub use error::{Error, ErrorKind, Result};
pub use io::{
    SchemaFile, parse_artifacts, parse_link, parse_project, parse_task, read_artifacts, read_link,
    read_project, read_task, serialize_artifacts, serialize_link, serialize_project,
    serialize_task,
};
pub use types::*;

#[cfg(test)]
mod tests;
