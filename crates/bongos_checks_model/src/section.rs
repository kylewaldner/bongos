//! The text of the Checks section of a task's detail view: the header and one row per check.
//! Pure; the view, `task: dump details` and the tests all read these lines.

use std::collections::BTreeMap;

use crate::effective::{Applicability, EffectiveSet, Row, Source};
use crate::evidence::{
    Current, EvidenceRecord, RowStatus, Satisfaction, Scope, freshness, records_of_task,
    row_status, satisfied,
};
use crate::verdict::Verdict;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Success,
    Error,
    /// A no verdict, in its own color.
    Warning,
    /// Stale and not run.
    Muted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionRow {
    pub id: String,
    pub text: String,
    pub tone: Tone,
    /// Further lines: the last real result, the failing items, the output.
    pub detail: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSection {
    pub title: String,
    pub header: String,
    pub rows: Vec<SectionRow>,
}

/// What a running check shows live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Live {
    Waiting { pool: String, place: u32, secs: u64 },
    Running { part: u32, of: u32, kept: u32 },
}

impl Live {
    /// What a running check is doing, as one line: "waiting: test-slots, place 2, 4m 0s" or
    /// "running part 5 of 8, 4 kept".
    pub fn text(&self) -> String {
        match self {
            Self::Waiting { pool, place, secs } => format!(
                "waiting: {pool}, place {place}, {}",
                duration_text(secs * 1000)
            ),
            Self::Running { part, of, kept } => format!("running part {part} of {of}, {kept} kept"),
        }
    }
}

/// Whether the task has a tree to check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeState {
    Available,
    Unavailable,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn header(
    set: &EffectiveSet,
    tree: &TreeState,
    sat: &Satisfaction,
    live: &BTreeMap<String, Live>,
) -> String {
    match set.applicability {
        Applicability::NoChecksApply => return "no checks apply".to_string(),
        Applicability::NoChecks | Applicability::UnknownTask => return "no checks".to_string(),
        Applicability::Checks => {}
    }
    if set.judged().is_empty() {
        return "no checks".to_string();
    }
    if *tree == TreeState::Unavailable {
        return "not done: no tree to check".to_string();
    }
    if sat.is_satisfied() {
        return if set.required().next().is_none() {
            "no required checks".to_string()
        } else {
            "all required checks pass".to_string()
        };
    }
    if live.values().any(|l| matches!(l, Live::Waiting { .. })) {
        return "checks waiting".to_string();
    }
    let mut parts = Vec::new();
    if !sat.failing.is_empty() {
        parts.push(plural(sat.failing.len(), "check failing", "checks failing"));
    }
    if !sat.no_verdict.is_empty() {
        parts.push(plural(
            sat.no_verdict.len(),
            "check with no verdict",
            "checks with no verdict",
        ));
    }
    if !sat.stale.is_empty() {
        parts.push(plural(sat.stale.len(), "check stale", "checks stale"));
    }
    if !sat.missing.is_empty() {
        parts.push(plural(sat.missing.len(), "check missing", "checks missing"));
    }
    format!("not done: {}", parts.join(", "))
}

fn source_text(source: &Source) -> String {
    match source {
        Source::Own => "own".to_string(),
        Source::Set { set } => format!("from set {set}"),
        Source::Inherited { from } => format!("inherited from {from}"),
        Source::Closing { group } => format!("closing check of {group}"),
        Source::Overrides { from } => {
            let inner = match &**from {
                Source::Own => "own entry".to_string(),
                Source::Set { set } => format!("set {set}"),
                Source::Inherited { from } => from.clone(),
                Source::Closing { group } => format!("closing check of {group}"),
                Source::Overrides { .. } => "an earlier entry".to_string(),
            };
            format!("overrides {inner}")
        }
    }
}

fn duration_text(ms: u64) -> String {
    let secs = ms / 1000;
    match (secs / 60, secs % 60) {
        (0, 0) => format!("{ms}ms"),
        (0, s) => format!("{s}s"),
        (m, s) => format!("{m}m {s}s"),
    }
}

fn scope_text(record: &EvidenceRecord) -> String {
    let base = match &record.variant.scope {
        Scope::All => "all".to_string(),
        Scope::Items { count, .. } => plural(*count as usize, "item", "items"),
        Scope::None { reason } => format!("nothing in scope: {reason}"),
        Scope::Unscoped => "unscoped".to_string(),
    };
    match &record.scope_reason {
        Some(reason) if !matches!(record.variant.scope, Scope::None { .. }) => {
            format!("{base} ({reason})")
        }
        _ => base,
    }
}

fn record_result(record: &EvidenceRecord) -> String {
    match &record.verdict {
        Verdict::Pass => match &record.variant.scope {
            Scope::None { .. } => "pass (nothing in scope)".to_string(),
            _ => "pass".to_string(),
        },
        Verdict::Fail => format!("fail ({})", plural(record.failing.len(), "item", "items")),
        Verdict::NoVerdict(reason) => format!("no verdict: {}", reason.text()),
    }
}

fn row_line(
    row: &Row,
    set: &[Row],
    task_id: &str,
    records: &[EvidenceRecord],
    current: &Current,
    live: Option<&Live>,
) -> SectionRow {
    let name = &row.decl.name;
    let status = row_status(row, records, current, set, task_id);
    let state = current.checks.get(name);
    // The latest finished, non-parameterized record of this check, fresh or not.
    let latest = records
        .iter()
        .rev()
        .find(|r| &r.check == name && r.is_finished() && r.variant.params.is_none());
    // The record the row's result is about: the one that satisfied it, else the latest.
    let shown = match &status {
        RowStatus::Satisfied {
            accepted_by: None,
            record_id,
        } => records.iter().find(|r| &r.id == record_id),
        _ => latest,
    };

    let (result, tone) = match (&status, live) {
        (_, Some(live @ Live::Waiting { .. })) => (live.text(), Tone::Muted),
        (_, Some(live @ Live::Running { .. })) => (live.text(), Tone::Normal),
        (RowStatus::Satisfied { accepted_by, .. }, None) => (
            match (accepted_by, shown) {
                (Some(by), _) => format!("accepted by {by}"),
                (None, Some(record)) => record_result(record),
                (None, None) => "pass".to_string(),
            },
            Tone::Success,
        ),
        (RowStatus::Failing, None) => {
            (shown.map_or("fail".to_string(), record_result), Tone::Error)
        }
        (RowStatus::NoVerdict, None) => (
            shown.map_or("no verdict".to_string(), record_result),
            Tone::Warning,
        ),
        (RowStatus::Narrow, None) => (
            "pass at a narrower scope than declared, not enough".to_string(),
            Tone::Muted,
        ),
        (RowStatus::Stale(why), None) => (format!("stale: {}", why.text()), Tone::Muted),
        (RowStatus::Missing, None) => ("not run".to_string(), Tone::Muted),
    };

    let mut fields = vec![
        name.clone(),
        source_text(&row.source),
        if row.decl.is_required() {
            "required".to_string()
        } else {
            "advisory".to_string()
        },
        result,
    ];
    if let Some(record) = shown {
        fields.push(format!("scope {}", scope_text(record)));
        let key = record.tree_key.chars().take(8).collect::<String>();
        fields.push(format!("tree {key}"));
        if let Some(ended) = &record.ended {
            fields.push(ended.clone());
        }
        if let Some(ms) = record.duration_ms {
            fields.push(duration_text(ms));
        }
        if record.variant.parts > 1 {
            fields.push(format!("{} parts", record.variant.parts));
        }
    }
    if let Some(by) = &row.decl.or_accepted {
        fields.push(format!("or accepted by {by}"));
    }

    let mut detail = Vec::new();
    if let Some(record) = latest {
        // A no verdict or stale row also shows the last real result.
        let stale = freshness(record, state) != crate::evidence::Freshness::Fresh;
        let no_verdict = matches!(record.verdict, Verdict::NoVerdict(_));
        if matches!(status, RowStatus::Stale(_) | RowStatus::NoVerdict) && (stale || no_verdict) {
            let real = records.iter().rev().find(|r| {
                &r.check == name
                    && r.is_finished()
                    && r.variant.params.is_none()
                    && !matches!(r.verdict, Verdict::NoVerdict(_))
            });
            if let Some(real) = real {
                detail.push(format!("last result: {}", record_result(real)));
            }
        }
    }
    if let Some(record) = shown {
        if let Some(summary) = &record.summary {
            detail.push(summary.clone());
        }
        detail.extend(record.failing.iter().map(|item| format!("failing: {item}")));
        match &record.output {
            crate::evidence::Output::Path(path) => detail.push(format!("output: {path}")),
            crate::evidence::Output::Deleted => {
                detail.push("output deleted after 30 days".to_string())
            }
        }
    }

    SectionRow {
        id: name.clone(),
        text: fields.join(" | "),
        tone,
        detail,
    }
}

/// The Checks section of a task: `set` its effective set, `records` its evidence, `current`
/// the state of each check on the tree now, `live` what is running or waiting by check name. A dropped row is
/// shown as dropped here.
pub fn section(
    set: &EffectiveSet,
    records: &[EvidenceRecord],
    current: &Current,
    tree: &TreeState,
    live: &BTreeMap<String, Live>,
) -> TaskSection {
    let records = records_of_task(records, &set.task_id);
    let records = records.as_slice();
    let sat = satisfied(set.judged(), records, current, &set.task_id);
    let judged = set.judged();
    let mut rows: Vec<SectionRow> = judged
        .iter()
        .map(|row| {
            row_line(
                row,
                judged,
                &set.task_id,
                records,
                current,
                live.get(&row.decl.name),
            )
        })
        .collect();
    if set.applicability == Applicability::Checks {
        for dropped in &set.dropped {
            rows.push(SectionRow {
                id: format!("dropped/{}", dropped.row.decl.name),
                text: format!("{} | dropped here by {}", dropped.row.decl.name, dropped.by),
                tone: Tone::Muted,
                detail: Vec::new(),
            });
        }
    }
    TaskSection {
        title: "Checks".to_string(),
        header: header(set, tree, &sat, live),
        rows,
    }
}

fn declared_text(row: &Row) -> String {
    match row.declared.line {
        Some(line) => format!("{}:{line}", row.declared.file),
        None => row.declared.file.clone(),
    }
}

/// The rows `task_detail: toggle sources` adds under the Checks section: where each row of the
/// effective set is declared and which layer it came from, then each row the task dropped and
/// the task that dropped it. The ids start with `source/`, so they never name a check.
pub fn source_rows(set: &EffectiveSet) -> Vec<SectionRow> {
    let mut rows = Vec::new();
    let used = |rows: &mut Vec<SectionRow>, layer: &str, list: &[Row]| {
        for row in list {
            rows.push(SectionRow {
                id: format!("source/{layer}/{}", row.decl.name),
                text: format!(
                    "{} | uses: {} | declared in {}",
                    row.decl.name,
                    source_text(&row.source),
                    declared_text(row)
                ),
                tone: Tone::Muted,
                detail: Vec::new(),
            });
        }
    };
    if set.applicability == Applicability::NoChecksApply
        || set.applicability == Applicability::UnknownTask
    {
        return rows;
    }
    used(&mut rows, "rows", &set.rows);
    if set.is_group {
        used(&mut rows, "closing", &set.closing);
    }
    for dropped in &set.dropped {
        rows.push(SectionRow {
            id: format!("source/dropped/{}", dropped.row.decl.name),
            text: format!(
                "{} | dropped by {} | was {} | declared in {}",
                dropped.row.decl.name,
                dropped.by,
                source_text(&dropped.row.source),
                declared_text(&dropped.row)
            ),
            tone: Tone::Muted,
            detail: Vec::new(),
        });
    }
    rows
}
