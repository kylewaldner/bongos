//! The proposal format of the bongos task builder: the grammar of a `<n>.proposal.txt` file as a
//! syntax tree, a parser that never fails and reports every syntax problem with its line, a
//! printer that writes a task subtree back as a proposal, validation, the diff of a proposal
//! against the tree with its preview text and fingerprint, the one change that applies or rejects
//! it, and the format help a chat is given. It has no UI dependency and does not touch the file
//! system: feature 03's writer writes the change.

pub mod apply;
pub mod diff;
pub mod grammar;
pub mod help;
pub mod parse;
pub mod preview;
pub mod print;
pub mod validate;

pub use diff::{
    Choices, Diff, Keeper, KeeperSource, NewTask, Op, SplitWork, TaskRef, TodoOp, diff, fingerprint,
};
pub use grammar::{
    By, Edge, FORMAT_VERSION, Header, Located, Node, Parsed, Problem, Severity, TaskAttributes,
    Todo, TodoMark,
};
pub use parse::parse;
pub use preview::{applied_summary, preview};
pub use print::{LiveSet, LiveSource, print};
pub use validate::{Checks, ProposalFile, validate};

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod diff_tests;
#[cfg(test)]
mod grammar_tests;
#[cfg(test)]
mod parse_tests;
#[cfg(test)]
mod preview_tests;
#[cfg(test)]
mod print_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod validate_tests;
