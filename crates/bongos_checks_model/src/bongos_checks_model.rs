//! The model of task checks: the declaration of a check and its validation and digest
//! ([`decl`]), and the effective set of checks of a task, with where each row comes from
//! ([`effective`]). Pure data and functions; no UI and no file access.

pub mod decl;
pub mod effective;

pub use decl::{
    CheckDecl, ChecksTable, Digest, DigestError, Finding, Inherit, NeedEntry, Parts, Scope,
    Severity, digest, parse, parse_file, validate,
};
pub use effective::{
    Applicability, Declared, Dropped, EffectiveSet, NamedSet, NamedSets, Row, Source, effective_set,
};

#[cfg(test)]
mod decl_tests;
#[cfg(test)]
mod effective_tests;
