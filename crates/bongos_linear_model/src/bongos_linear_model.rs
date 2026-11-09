//! Linear data model for bongos: identifiers and URLs, the `LinearApi` trait, an in-memory fake and
//! the GraphQL request text with its response parsing.
//!
//! This crate is plain Rust and does no I/O. It has no notion of a token: credentials belong to
//! whatever sends the requests, never to the model.

pub mod api;
pub mod graphql;
pub mod ident;

#[cfg(any(test, feature = "test-support"))]
pub mod fake;

pub use api::{
    ApiError, Issue, IssueRef, IssueSummary, LinearApi, NewIssue, StateKind, WorkflowState,
};
pub use ident::{Identifier, IssueUrl};

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod fake_tests;
#[cfg(test)]
mod graphql_tests;
#[cfg(test)]
mod ident_tests;
