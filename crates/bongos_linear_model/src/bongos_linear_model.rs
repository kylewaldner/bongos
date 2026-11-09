//! Linear data model for bongos: identifiers and URLs, the `LinearApi` trait, an in-memory fake and
//! the GraphQL request text with its response parsing.
//!
//! The sync half reads and writes the `linear` and `linear_state` of tasks through
//! `bongos_work_model`: link and unlink, the refresh diff, suggestions, import, subtickets, the
//! status rules and the branch and PR conventions.
//!
//! This crate is plain Rust and does no I/O. It has no notion of a token: credentials belong to
//! whatever sends the requests, never to the model.

pub mod api;
pub mod conventions;
pub mod graphql;
pub mod ident;
pub mod import;
pub mod status;
pub mod subtickets;
pub mod sync;

#[cfg(any(test, feature = "test-support"))]
pub mod fake;

pub use api::{
    ApiError, Issue, IssueRef, IssueSummary, LinearApi, NewIssue, StateKind, WorkflowState,
};
pub use ident::{Identifier, IssueUrl};

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod conventions_tests;
#[cfg(test)]
mod fake_tests;
#[cfg(test)]
mod graphql_tests;
#[cfg(test)]
mod ident_tests;
#[cfg(test)]
mod import_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod subtickets_tests;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod test_support;
