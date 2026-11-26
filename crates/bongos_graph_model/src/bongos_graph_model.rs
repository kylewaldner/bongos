//! Graph edits: the operations a workflow node returns in a `graph_edits` output field
//! ([`ops`]), the scope a workflow declares for them ([`scope`]), and their pure application to a
//! project's parsed definitions ([`apply`]), which yields the definition changes to land or the
//! reason the batch is refused. Nothing here touches git, the state store, a process or the clock.

pub mod apply;
pub mod ops;
pub mod scope;

pub use apply::{Applied, ApplyCtx, EditRefusal, StartedFacts, apply};
pub use ops::{BatchError, EdgeKind, GraphEditBatch, GraphOp, NewTodo, TaskRef, validate_batch};

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod test_support;
