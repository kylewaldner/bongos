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
    /// Another summary pattern; any change is reported, as a looser regex cannot be told apart.
    ChangedSummary {
        old: String,
        new: String,
    },
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
    /// The accepting check of an `or_accepted` row (named `by`) got looser, or is new in the set
    /// where it was absent before, so the row is accepted more easily.
    AcceptorLoosened {
        by: String,
        kind: Box<LooseningKind>,
    },
    /// The accepting check of an `or_accepted` row is in the new set and was not in the old one.
    AcceptorAdded {
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
            LooseningKind::ChangedSummary { old, new } => {
                write!(f, "summary {old} -> {new}")
            }
            LooseningKind::RemovedResultFile => f.write_str("result file no longer required"),
            LooseningKind::ScopeWidened { old, new } => write!(f, "scope {old} -> {new}"),
            LooseningKind::NewAccepted { by } => write!(f, "now also accepted by {by}"),
            LooseningKind::AcceptorLoosened { by, kind } => {
                let text = Loosening {
                    check: by.clone(),
                    kind: (**kind).clone(),
                }
                .to_string();
                write!(f, "accepting {text}")
            }
            LooseningKind::AcceptorAdded { by } => {
                write!(f, "accepting check {by} is newly in the set")
            }
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
    if let (Some(before), Some(after)) = (&old.summary, &new.summary)
        && before != after
    {
        found.push(LooseningKind::ChangedSummary {
            old: before.clone(),
            new: after.clone(),
        });
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

/// Loosenings of the check that accepts `row` through `or_accepted`, when that check is in the
/// new set. Becoming advisory is not one: an advisory check may still accept.
fn acceptor_loosenings(old: &EffectiveSet, new: &EffectiveSet, row: &Row) -> Vec<LooseningKind> {
    let Some(by) = row.decl.or_accepted.as_deref() else {
        return Vec::new();
    };
    let find = |set: &EffectiveSet| set.judged().iter().find(|r| r.decl.name == by).cloned();
    let Some(after) = find(new) else {
        return Vec::new();
    };
    let Some(before) = find(old) else {
        return vec![LooseningKind::AcceptorAdded { by: by.to_string() }];
    };
    kinds(&before.decl, &after.decl)
        .into_iter()
        .filter(|kind| {
            !matches!(
                kind,
                LooseningKind::MadeAdvisory | LooseningKind::NewAccepted { .. }
            )
        })
        .map(|kind| LooseningKind::AcceptorLoosened {
            by: by.to_string(),
            kind: Box::new(kind),
        })
        .collect()
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
        let mut kinds = kinds;
        if new_rows.iter().any(|other| &other.decl.name == name) {
            kinds.extend(acceptor_loosenings(old, new, row));
        }
        found.extend(kinds.into_iter().map(|kind| Loosening {
            check: name.clone(),
            kind,
        }));
    }
    found
}
