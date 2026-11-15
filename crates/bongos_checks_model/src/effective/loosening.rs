//! Loosening of a required check: the comparison of a task's effective set in master and in a
//! change, for the approval of a change to the definitions of a started task.

use std::fmt;

use crate::decl::{CheckDecl, Scope};
use crate::effective::{EffectiveSet, Row};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LooseningKind {
    /// Not in the new set (the task no longer has checks that apply, or the row is gone).
    Removed,
    /// Removed by a `drop` of the new set.
    Dropped,
    MadeAdvisory,
    /// A pass code the old check did not have.
    WiderPass {
        old: Vec<i32>,
        new: Vec<i32>,
    },
    /// A fail code the old check had is gone.
    NarrowerFail {
        old: Vec<i32>,
        new: Vec<i32>,
    },
    /// Another `run` or `command`.
    ChangedRun {
        old: String,
        new: String,
    },
    RemovedSummary,
    RemovedResultFile,
    /// A scope script where there was none or another one (all, or unscoped, before).
    ScopeWidened {
        old: String,
        new: String,
    },
    /// An `or_accepted` the old check did not have.
    NewAccepted {
        by: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loosening {
    pub check: String,
    pub kind: LooseningKind,
}

fn codes(codes: &[i32]) -> String {
    codes
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for Loosening {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "check {}: ", self.check)?;
        match &self.kind {
            LooseningKind::Removed => f.write_str("removed"),
            LooseningKind::Dropped => f.write_str("dropped"),
            LooseningKind::MadeAdvisory => f.write_str("made advisory"),
            LooseningKind::WiderPass { old, new } => {
                write!(f, "pass codes {} -> {}", codes(old), codes(new))
            }
            LooseningKind::NarrowerFail { old, new } => {
                write!(f, "fail codes {} -> {}", codes(old), codes(new))
            }
            LooseningKind::ChangedRun { old, new } => write!(f, "runs {old} -> {new}"),
            LooseningKind::RemovedSummary => f.write_str("summary removed"),
            LooseningKind::RemovedResultFile => f.write_str("result file no longer required"),
            LooseningKind::ScopeWidened { old, new } => write!(f, "scope {old} -> {new}"),
            LooseningKind::NewAccepted { by } => write!(f, "now also accepted by {by}"),
        }
    }
}

fn what_runs(decl: &CheckDecl) -> String {
    match (&decl.run, &decl.command) {
        (Some(run), None) => run.clone(),
        (None, Some(command)) => command.join(" "),
        (Some(run), Some(command)) => format!("{run} + {}", command.join(" ")),
        (None, None) => "nothing".to_string(),
    }
}

fn scope_words(decl: &CheckDecl) -> String {
    match &decl.scope {
        None => "unscoped".to_string(),
        Some(Scope::Word(word)) => word.clone(),
        Some(Scope::Script { script }) => format!("script {script}"),
        Some(Scope::Invalid(_)) => "invalid".to_string(),
    }
}

fn kinds(old: &CheckDecl, new: &CheckDecl) -> Vec<LooseningKind> {
    let mut found = Vec::new();
    if old.is_required() && !new.is_required() {
        found.push(LooseningKind::MadeAdvisory);
    }
    let (old_pass, new_pass) = (old.pass_codes(), new.pass_codes());
    if new_pass.iter().any(|code| !old_pass.contains(code)) {
        found.push(LooseningKind::WiderPass {
            old: old_pass,
            new: new_pass,
        });
    }
    let (old_fail, new_fail) = (old.fail_codes(), new.fail_codes());
    if old_fail.iter().any(|code| !new_fail.contains(code)) {
        found.push(LooseningKind::NarrowerFail {
            old: old_fail,
            new: new_fail,
        });
    }
    if old.run != new.run || old.command != new.command {
        found.push(LooseningKind::ChangedRun {
            old: what_runs(old),
            new: what_runs(new),
        });
    }
    if old.summary.is_some() && new.summary.is_none() {
        found.push(LooseningKind::RemovedSummary);
    }
    if old.writes_result_file() && !new.writes_result_file() {
        found.push(LooseningKind::RemovedResultFile);
    }
    if let Some(Scope::Script { script }) = &new.scope {
        let same = matches!(&old.scope, Some(Scope::Script { script: before }) if before == script);
        if !same {
            found.push(LooseningKind::ScopeWidened {
                old: scope_words(old),
                new: scope_words(new),
            });
        }
    }
    if let Some(by) = &new.or_accepted
        && old.or_accepted.as_ref() != Some(by)
    {
        found.push(LooseningKind::NewAccepted { by: by.clone() });
    }
    found
}

/// What `new` loosens of the required checks `old` judges on the task (the ones in master, for
/// a task already started). Only required rows of `old` are compared. Rows are paired by name;
/// a row that is gone is `Removed`, or `Dropped` when the new set drops it by name.
pub fn loosening(old: &EffectiveSet, new: &EffectiveSet) -> Vec<Loosening> {
    let new_rows: &[Row] = new.judged();
    let mut found = Vec::new();
    for row in old.required() {
        let name = &row.decl.name;
        let kinds = match new_rows.iter().find(|other| &other.decl.name == name) {
            Some(other) => kinds(&row.decl, &other.decl),
            None if new
                .dropped
                .iter()
                .any(|dropped| &dropped.row.decl.name == name) =>
            {
                vec![LooseningKind::Dropped]
            }
            None => vec![LooseningKind::Removed],
        };
        found.extend(kinds.into_iter().map(|kind| Loosening {
            check: name.clone(),
            kind,
        }));
    }
    found
}
