//! Workflow model for bongos: the definition format and its parser with line numbers, the layers a
//! definition resolves through, and prompt templates.
//!
//! This crate is plain Rust. It reads definition files and nothing else, and it has no notion of a
//! run: runs, validation and scheduling are built on top of the types here.

pub mod definition;
pub mod layers;
pub mod template;

pub use definition::{Definition, ParseError, Warning, parse};
pub use layers::{Layer, LayerDirs, Resolution, Resolved};
pub use template::{Scope, TemplateError};

#[cfg(test)]
mod definition_tests;
#[cfg(test)]
mod layers_tests;
#[cfg(test)]
mod template_tests;
