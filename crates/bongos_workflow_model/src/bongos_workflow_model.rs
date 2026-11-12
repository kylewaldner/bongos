//! Workflow model for bongos: the definition format and its parser with line numbers, the layers a
//! definition resolves through, and prompt templates.
//!
//! This crate is plain Rust. [`validate()`] checks a parsed definition before anything starts, and
//! [`schema`] types node results. [`record`] is the run record (`run.toml` and the checkpoint log,
//! replayed into the run state), [`failure`] the closed list of failure reasons, [`inputs`] resolves
//! a definition's inputs against a task, and [`state_doc`] builds what a program node reads.
//! [`scheduler`] decides what a run does next from its definition and replayed state; `fake` (feature
//! `test-support`) drives it with scripted results.

pub mod definition;
pub mod failure;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod inputs;
pub mod layers;
pub mod record;
pub mod scheduler;
pub mod schema;
pub mod state_doc;
pub mod template;
pub mod validate;

pub use definition::{Definition, ParseError, Warning, parse};
pub use failure::{Failure, FailureClass, FailureReason};
pub use inputs::{InputValue, resolve};
pub use layers::{Layer, LayerDirs, Resolution, Resolved};
pub use record::{Event, NodeKey, ParsedLog, RunState, RunToml, append, parse_log, repair, replay};
pub use scheduler::{
    Command, FreshRetryRefusal, Limits, Plan, check_fresh_retry, plan, resume_set, retry_set, step,
};
pub use schema::{TypedField, TypedResult, TypedValue, check, json_schema, json_schema_strict};
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
mod scheduler_tests;
#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod state_doc_tests;
#[cfg(test)]
mod template_tests;
#[cfg(test)]
mod validate_tests;
