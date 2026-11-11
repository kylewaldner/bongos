//! Workflow model for bongos: the definition format and its parser with line numbers, the layers a
//! definition resolves through, and prompt templates.
//!
//! This crate is plain Rust. It reads definition files and nothing else, and it has no notion of a
//! run: runs and scheduling are built on top of the types here. [`validate()`] checks a parsed
//! definition before anything starts, and [`schema`] types node results.

pub mod definition;
pub mod layers;
pub mod schema;
pub mod template;
pub mod validate;

pub use definition::{Definition, ParseError, Warning, parse};
pub use layers::{Layer, LayerDirs, Resolution, Resolved};
pub use schema::{TypedField, TypedResult, TypedValue, check, json_schema};
pub use template::{Scope, TemplateError};
pub use validate::{Ceilings, Diagnostic, Severity, ValidationContext, has_errors, validate};

#[cfg(test)]
mod definition_tests;
#[cfg(test)]
mod layers_tests;
#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod template_tests;
#[cfg(test)]
mod validate_tests;
