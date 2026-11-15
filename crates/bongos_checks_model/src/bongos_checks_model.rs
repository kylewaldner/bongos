//! The model of task checks: the declaration of a check and its validation and digest
//! ([`decl`]), and the effective set of checks of a task, with where each row comes from
//! ([`effective`]). Pure data and functions; no UI and no file access.

pub mod decl;
pub mod effective;
pub mod evidence;
pub mod section;
pub mod verdict;

pub use decl::{
    CheckDecl, ChecksTable, Digest, DigestError, Finding, Inherit, NeedEntry, Parts, Scope,
    Severity, digest, parse, parse_file, validate,
};
pub use effective::{
    Applicability, Declared, Dropped, EffectiveSet, Loosening, LooseningKind, NamedSet, NamedSets,
    Row, Source, effective_set, loosening,
};
pub use evidence::{
    Accepted, CheckState, Current, EvidenceRecord, Freshness, RowStatus, Satisfaction, Stale,
    freshness, or_accepted, row_status, satisfied,
};
pub use section::{SectionRow, TaskSection, Tone, section};
pub use verdict::{
    Exit, NoVerdictReason, ResultFile, ScopeAnswer, Verdict, combine, judge, parse_plan_answer,
    parse_result_file, parse_scope_answer,
};

#[cfg(test)]
mod decl_tests;
#[cfg(test)]
mod effective_tests;
#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod loosening_tests;
#[cfg(test)]
mod section_tests;
#[cfg(test)]
mod verdict_tests;
