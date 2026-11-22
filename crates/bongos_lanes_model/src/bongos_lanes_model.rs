//! Lane model for bongos: the lane table (`lanes.toml`) parsed with line numbers and validated,
//! and the expansion of binding tokens.
//!
//! This crate is plain Rust: no gpui, no store, no file access.

pub mod binding;
pub mod problem;
pub mod table;

pub use binding::{TokenError, TokenValues, check_tokens, expand_tokens};
pub use bongos_workflow_model::EnvEdit;
pub use problem::Problem;
pub use table::{
    Binding, Diagnostic, DispatchRule, Exemption, LaneKind, LaneTable, ParseError, Pin, TreeSpec,
    ValidationContext, names_lane_instance, parse, prompt_lane_problems, validate_lane_table,
};

#[cfg(test)]
mod binding_tests;
#[cfg(test)]
mod problem_tests;
#[cfg(test)]
mod table_tests;
