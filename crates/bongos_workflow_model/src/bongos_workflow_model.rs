//! Workflow model for bongos: the definition format and its parser with line numbers, the layers a
//! definition resolves through, and prompt templates.
//!
//! This crate is plain Rust. [`validate()`] checks a parsed definition before anything starts, and
//! [`schema`] types node results. [`record`] is the run record (`run.toml` and the checkpoint log,
//! replayed into the run state), [`failure`] the closed list of failure reasons, [`inputs`] resolves
//! a definition's inputs against a task, and [`state_doc`] builds what a program node reads.

pub mod definition;
pub mod failure;
pub mod inputs;
pub mod layers;
pub mod record;
pub mod schema;
pub mod state_doc;
pub mod template;
pub mod validate;

pub use definition::{Definition, ParseError, Warning, parse};
pub use failure::{Failure, FailureClass, FailureReason};
pub use inputs::{InputValue, resolve};
pub use layers::{Layer, LayerDirs, Resolution, Resolved};
pub use record::{Event, NodeKey, RunState, RunToml, append, parse_log, replay};
pub use schema::{TypedField, TypedResult, TypedValue, check, json_schema};
pub use state_doc::StateDoc;
pub use template::{Scope, TemplateError};
pub use validate::{Ceilings, Diagnostic, Severity, ValidationContext, has_errors, validate};

#[cfg(test)]
mod definition_tests;
#[cfg(test)]
mod failure_tests;
#[cfg(test)]
mod inputs_tests;
#[cfg(test)]
mod layers_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod state_doc_tests;
#[cfg(test)]
mod template_tests;
#[cfg(test)]
mod validate_tests;
