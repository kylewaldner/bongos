//! The proposal format of the bongos task builder: the grammar of a `<n>.proposal.txt` file as a
//! syntax tree, a parser that never fails and reports every syntax problem with its line, a
//! printer that writes a task subtree back as a proposal, and the format help a chat is given. It
//! has no UI dependency and does not touch the file system.

pub mod grammar;
pub mod help;
pub mod parse;
pub mod print;
pub mod validate;

pub use grammar::{
    By, Edge, FORMAT_VERSION, Header, Located, Node, Parsed, Problem, Severity, TaskAttributes,
    Todo, TodoMark,
};
pub use parse::parse;
pub use print::{LiveSet, print};
pub use validate::{Checks, ProposalFile, validate};

#[cfg(test)]
mod grammar_tests;
#[cfg(test)]
mod parse_tests;
#[cfg(test)]
mod print_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod validate_tests;
