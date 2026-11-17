//! The decisions of the task check commands, apart from the windows: which task a command acts
//! on, what it says when the run ends, and the text of `task: dump checks`.

use std::path::{Path, PathBuf};

use bongos_checks_model::verdict::Verdict;
use bongos_checks_model::{
    Current, EvidenceRecord, Freshness, Row, Source, TaskSection, freshness,
};

use crate::engine::{RunOutcome, RunReport};
use crate::task_source::TaskChecks;
use crate::task_tree::TaskTree;

/// The file `task: dump checks` writes in the logs directory.
pub const DUMP_FILE: &str = "task-checks.txt";

/// What a command that acts on one task does about which task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Act on this task.
    Task(String),
    /// Ask which of these.
    Pick(Vec<String>),
    /// No task to act on and none to pick.
    NoTasks,
}

/// The task a command acts on: the task at point (the selected row of the Work panel, else the
/// focused task), else a pick among `with_checks`.
pub fn choose_target(at_point: Option<String>, with_checks: Vec<String>) -> Target {
    match at_point {
        Some(id) => Target::Task(id),
        None if with_checks.is_empty() => Target::NoTasks,
        None => Target::Pick(with_checks),
    }
}

/// The toast when a command finds no task to act on.
pub const NO_TASK_MESSAGE: &str = "No task with checks to act on";

/// What `task: run checks` says once the run is over.
pub fn report_message(task: &str, report: &RunReport) -> String {
    let (results, cancelled) = match &report.outcome {
        RunOutcome::Refused(refusal) => return refusal.to_string(),
        RunOutcome::Ran { results, cancelled } => (results, *cancelled),
    };
    if cancelled {
        return format!("Checks of {task} stopped");
    }
    let named = |wanted: fn(&Verdict) -> bool| -> Vec<&str> {
        results
            .iter()
            .filter(|result| wanted(&result.verdict))
            .map(|result| result.check.as_str())
            .collect()
    };
    let failing = named(|verdict| *verdict == Verdict::Fail);
    let no_verdict = named(|verdict| matches!(verdict, Verdict::NoVerdict(_)));
    if failing.is_empty() && no_verdict.is_empty() {
        return format!("Checks of {task}: all {} pass", results.len());
    }
    let mut parts = Vec::new();
    if !failing.is_empty() {
        parts.push(format!("failing: {}", failing.join(", ")));
    }
    if !no_verdict.is_empty() {
        parts.push(format!("no verdict: {}", no_verdict.join(", ")));
    }
    format!("Checks of {task}: {}", parts.join("; "))
}

/// The class of a verdict, in a word.
fn verdict_class(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
        Verdict::NoVerdict(_) => "no verdict",
    }
}

fn source_name(source: &Source) -> String {
    match source {
        Source::Own => "own".to_string(),
        Source::Set { set } => format!("set {set}"),
        Source::Inherited { from } => format!("inherited from {from}"),
        Source::Overrides { from } => format!("overrides {}", source_name(from)),
        Source::Closing { group } => format!("closing check of {group}"),
    }
}

/// The text of `task: dump checks`: the task's checks, their rows, the last verdict class of
/// each and the inputs of its tree key. Never an environment value.
pub fn dump_text(
    task: &TaskChecks,
    tree: Option<&TaskTree>,
    section: &TaskSection,
    records: &[EvidenceRecord],
    current: &Current,
) -> String {
    let mut lines = vec![
        format!("task: {} ({})", task.name, task.task_id),
        format!(
            "tree: {}",
            tree.map_or("none".to_string(), |tree| tree.path.display().to_string())
        ),
        format!("header: {}", section.header),
    ];
    for row in task.set.judged() {
        lines.push(check_line(row, records, current));
    }
    lines.push("rows:".to_string());
    for row in &section.rows {
        lines.push(format!("  {}", row.text));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn check_line(row: &Row, records: &[EvidenceRecord], current: &Current) -> String {
    let name = &row.decl.name;
    let last = records
        .iter()
        .filter(|record| &record.check == name && record.is_finished())
        .max_by(|a, b| a.id.cmp(&b.id));
    let state = current.checks.get(name);
    let mut fields = vec![
        format!("check {name}"),
        source_name(&row.source),
        if row.decl.is_required() {
            "required".to_string()
        } else {
            "advisory".to_string()
        },
        format!(
            "last verdict {}",
            last.map_or("none", |r| verdict_class(&r.verdict))
        ),
    ];
    match state {
        Some(state) => {
            fields.push(format!("tree key {}", state.tree_key));
            fields.push(format!("key inputs {}", state.key_inputs));
            fields.push(format!("digest {}", state.digest));
            if let Some(record) = last {
                let fresh = freshness(record, Some(state)) == Freshness::Fresh;
                fields.push(if fresh { "fresh" } else { "stale" }.to_string());
            }
        }
        None => fields.push("tree key not computed yet".to_string()),
    }
    fields.join(" | ")
}

/// Writes `text` to [`DUMP_FILE`] in `logs_dir` and returns the path.
pub fn write_dump(logs_dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(logs_dir)?;
    let path = logs_dir.join(DUMP_FILE);
    std::fs::write(&path, text)?;
    Ok(path)
}
