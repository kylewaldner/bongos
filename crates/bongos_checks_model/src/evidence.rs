//! Evidence: the record of one check run, whether it is still fresh, and whether the records
//! satisfy the required checks of a task. Pure; the records come from the evidence store.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::decl::{self, CheckDecl, Digest};
use crate::effective::Row;
use crate::verdict::{Exit, Verdict};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeKind {
    Tip,
    Base,
}

/// What part of the tree a run covered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// The whole check ran.
    All,
    /// The scope script named items.
    Items { digest: String, count: u32 },
    /// The scope script answered `none`.
    None { reason: String },
    /// The check declares no scope.
    Unscoped,
}

/// The parameters of a parameterized run (items, repeat count, tree).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub repeat_index: Option<u32>,
    #[serde(default)]
    pub tree: Option<TreeKind>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variant {
    pub scope: Scope,
    /// The number of parts the whole check has.
    pub parts: u32,
    /// `Some` for a parameterized run, which never satisfies anything.
    #[serde(default)]
    pub params: Option<Params>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartedBy {
    Person,
    Run { run_id: String, node_key: String },
    Landing { tree: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartRecord {
    pub n: u32,
    pub verdict: Verdict,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub exit: Option<Exit>,
    pub started: String,
    #[serde(default)]
    pub ended: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub pid_start_time: Option<u64>,
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Output {
    Path(String),
    /// Deleted after retention; the record stays.
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    /// A UUID v7, so ids order by time.
    pub id: String,
    pub task_id: String,
    pub check: String,
    /// The check's digest when it ran.
    pub digest: String,
    pub tree_key: String,
    pub tree: TreeKind,
    /// The combined digest of the key input files.
    pub key_inputs: String,
    pub variant: Variant,
    pub pin_commit: String,
    pub master_path: String,
    /// The command as run.
    pub command: Vec<String>,
    pub started: String,
    /// Empty while the run is unfinished; an unfinished record never counts.
    #[serde(default)]
    pub ended: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    pub verdict: Verdict,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub exit: Option<Exit>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub failing: Vec<String>,
    #[serde(default)]
    pub scope_reason: Option<String>,
    pub started_by: StartedBy,
    #[serde(default)]
    pub parts: Vec<PartRecord>,
    pub output: Output,
    /// Bytes cut from the middle of the output.
    #[serde(default)]
    pub cut: u64,
}

impl EvidenceRecord {
    pub fn is_finished(&self) -> bool {
        self.ended.is_some()
    }
}

/// What a check is now: the tree it would run on, the key inputs and its digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckState {
    pub tree_key: String,
    pub key_inputs: String,
    pub digest: String,
}

impl CheckState {
    pub fn new(tree_key: &str, key_inputs: &str, digest: &Digest) -> Self {
        Self {
            tree_key: tree_key.to_string(),
            key_inputs: key_inputs.to_string(),
            digest: digest.as_str().to_string(),
        }
    }
}

/// The current state of every check of a task, by check name. A check with no entry has no
/// current state, so nothing about it is fresh.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Current {
    pub checks: BTreeMap<String, CheckState>,
}

/// Why a record is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    EarlierTree,
    CheckChanged,
    KeyInputChanged,
}

impl Stale {
    pub fn text(self) -> &'static str {
        match self {
            Self::EarlierTree => "ran on an earlier tree",
            Self::CheckChanged => "the check changed",
            Self::KeyInputChanged => "a key input changed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale(Stale),
}

/// Whether `record` still stands against `state`: the same tree key, key inputs and digest.
/// With no state (`None`) nothing is fresh.
pub fn freshness(record: &EvidenceRecord, state: Option<&CheckState>) -> Freshness {
    let Some(state) = state else {
        return Freshness::Stale(Stale::EarlierTree);
    };
    if record.tree_key != state.tree_key {
        Freshness::Stale(Stale::EarlierTree)
    } else if record.digest != state.digest {
        Freshness::Stale(Stale::CheckChanged)
    } else if record.key_inputs != state.key_inputs {
        Freshness::Stale(Stale::KeyInputChanged)
    } else {
        Freshness::Fresh
    }
}

/// Whether a record's variant is wide enough for the declared scope: `scope = "all"`
/// needs `All`; a scope script takes `All`, `Items` or `None`; an unscoped check takes
/// `Unscoped`. An invalid declaration is satisfied by nothing.
fn scope_wide_enough(decl: &CheckDecl, scope: &Scope) -> bool {
    match (&decl.scope, scope) {
        (None, Scope::Unscoped) => true,
        (Some(decl::Scope::Word(word)), Scope::All) => word == "all",
        (Some(decl::Scope::Script { .. }), Scope::All) => true,
        (Some(decl::Scope::Script { .. }), Scope::Items { count, .. }) => *count > 0,
        (Some(decl::Scope::Script { .. }), Scope::None { reason }) => !reason.trim().is_empty(),
        _ => false,
    }
}

/// A finished run of the whole check, not parameterized, whose every part passed.
fn whole_pass(decl: &CheckDecl, record: &EvidenceRecord) -> bool {
    if !record.is_finished()
        || record.verdict != Verdict::Pass
        || record.variant.params.is_some()
        || !scope_wide_enough(decl, &record.variant.scope)
    {
        return false;
    }
    let total = record.variant.parts;
    if total == 0 {
        return false;
    }
    if total == 1 && record.parts.is_empty() {
        return true;
    }
    let mut seen: Vec<u32> = record.parts.iter().map(|part| part.n).collect();
    seen.sort_unstable();
    let every_part_once = seen.len() as u64 == u64::from(total)
        && seen.iter().zip(1u32..).all(|(n, expected)| *n == expected);
    every_part_once
        && record
            .parts
            .iter()
            .all(|part| part.verdict == Verdict::Pass && part.ended.is_some())
}

/// The fresh pass of `decl`'s check that satisfies it, on `required_tree` when given. The
/// newest (by id, which orders by time) fresh finished whole-check record decides, so a later fail or no verdict on the same
/// tree means no pass (fail closed). A newer narrow or parameterized pass is no verdict either
/// way and is skipped.
fn passing<'a>(
    decl: &CheckDecl,
    records: &'a [EvidenceRecord],
    current: &Current,
    required_tree: Option<&str>,
) -> Option<&'a EvidenceRecord> {
    let state = current.checks.get(&decl.name);
    records
        .iter()
        .filter(|record| {
            record.check == decl.name
                && record.is_finished()
                && record.variant.params.is_none()
                && freshness(record, state) == Freshness::Fresh
                && required_tree.is_none_or(|tree| record.tree_key == tree)
                && (record.verdict != Verdict::Pass || whole_pass(decl, record))
        })
        .max_by(|a, b| a.id.cmp(&b.id))
        .filter(|record| record.verdict == Verdict::Pass)
}

/// The records of one task that count for its checks: its own, not those a landing started.
pub fn records_of_task(records: &[EvidenceRecord], task_id: &str) -> Vec<EvidenceRecord> {
    records
        .iter()
        .filter(|record| {
            record.task_id == task_id && !matches!(record.started_by, StartedBy::Landing { .. })
        })
        .cloned()
        .collect()
}

/// What accepted a row without its own pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// The accepting check, the row's `or_accepted`.
    pub by: String,
    pub record_id: String,
}

/// `or_accepted = "B"`: a fresh pass of B on the same tree as the row's own current tree,
/// under B's own scope and parts rules. B must be a check of `set`; its pass on another
/// tree, a stale or parameterized one, or a pass of a check not in the set accepts nothing.
pub fn or_accepted(
    row: &Row,
    records: &[EvidenceRecord],
    current: &Current,
    set: &[Row],
) -> Option<Accepted> {
    let by = row.decl.or_accepted.as_deref()?;
    if by == row.decl.name {
        return None;
    }
    let accepting = set.iter().find(|other| other.decl.name == by)?;
    let tree = &current.checks.get(&row.decl.name)?.tree_key;
    let record = passing(&accepting.decl, records, current, Some(tree))?;
    Some(Accepted {
        by: by.to_string(),
        record_id: record.id.clone(),
    })
}

/// Where one row stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowStatus {
    /// A fresh pass of the check itself, or of its accepting check.
    Satisfied {
        record_id: String,
        accepted_by: Option<String>,
    },
    /// The latest fresh record is a fail.
    Failing,
    /// The latest fresh record is a no verdict.
    NoVerdict,
    /// Fresh passes exist but none is wide enough (narrow scope, parts missing).
    Narrow,
    /// Only records of an earlier tree, digest or key input exist.
    Stale(Stale),
    /// No finished record.
    Missing,
}

/// The status of one row against `records` (any records; only those of `task_id` count, and
/// other checks' are ignored), with `set` the rows of the task so `or_accepted` can find its
/// check.
pub fn row_status(
    row: &Row,
    records: &[EvidenceRecord],
    current: &Current,
    set: &[Row],
    task_id: &str,
) -> RowStatus {
    let records = records_of_task(records, task_id);
    let records = records.as_slice();
    if let Some(record) = passing(&row.decl, records, current, None) {
        return RowStatus::Satisfied {
            record_id: record.id.clone(),
            accepted_by: None,
        };
    }
    if let Some(accepted) = or_accepted(row, records, current, set) {
        return RowStatus::Satisfied {
            record_id: accepted.record_id,
            accepted_by: Some(accepted.by),
        };
    }
    let state = current.checks.get(&row.decl.name);
    // Parameterized runs never count, neither for nor against.
    let own: Vec<&EvidenceRecord> = records
        .iter()
        .filter(|record| {
            record.check == row.decl.name && record.is_finished() && record.variant.params.is_none()
        })
        .collect();
    let fresh: Vec<&&EvidenceRecord> = own
        .iter()
        .filter(|record| freshness(record, state) == Freshness::Fresh)
        .collect();
    match fresh.iter().max_by(|a, b| a.id.cmp(&b.id)) {
        Some(record) => match record.verdict {
            Verdict::Fail => RowStatus::Failing,
            Verdict::NoVerdict(_) => RowStatus::NoVerdict,
            Verdict::Pass => RowStatus::Narrow,
        },
        None => match own.iter().max_by(|a, b| a.id.cmp(&b.id)) {
            Some(record) => match freshness(record, state) {
                Freshness::Stale(why) => RowStatus::Stale(why),
                Freshness::Fresh => RowStatus::Missing,
            },
            None => RowStatus::Missing,
        },
    }
}

/// The required checks of a task that are not satisfied, by name, in the order of `rows`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Satisfaction {
    /// No finished record, or only a narrower pass than the declared scope needs.
    pub missing: Vec<String>,
    pub failing: Vec<String>,
    pub no_verdict: Vec<String>,
    pub stale: Vec<String>,
}

impl Satisfaction {
    /// Every required row is satisfied. A task with no required rows is vacuously satisfied;
    /// the caller decides what a task with no checks shows.
    pub fn is_satisfied(&self) -> bool {
        self.missing.is_empty()
            && self.failing.is_empty()
            && self.no_verdict.is_empty()
            && self.stale.is_empty()
    }

    pub fn unsatisfied_count(&self) -> usize {
        self.missing.len() + self.failing.len() + self.no_verdict.len() + self.stale.len()
    }
}

/// Whether the required rows of `rows` (the rows judged on the task) are satisfied on the
/// current tree. Advisory rows are never counted (they may still accept a required one).
pub fn satisfied(
    rows: &[Row],
    records: &[EvidenceRecord],
    current: &Current,
    task_id: &str,
) -> Satisfaction {
    let mut result = Satisfaction::default();
    for row in rows.iter().filter(|row| row.decl.is_required()) {
        let name = row.decl.name.clone();
        match row_status(row, records, current, rows, task_id) {
            RowStatus::Satisfied { .. } => {}
            RowStatus::Failing => result.failing.push(name),
            RowStatus::NoVerdict => result.no_verdict.push(name),
            RowStatus::Stale(_) => result.stale.push(name),
            RowStatus::Narrow | RowStatus::Missing => result.missing.push(name),
        }
    }
    result
}
