//! The text of `task: show check output`: the evidence record as a header block, then the output
//! of the check's parts. Pure apart from reading the output files.

use std::path::{Path, PathBuf};

use bongos_checks_model::evidence::{EvidenceRecord, Output, Scope};
use bongos_checks_model::verdict::Verdict;

/// What the view says when the output files are gone.
pub(crate) const DELETED_MESSAGE: &str = "output deleted after 30 days";

/// The part logs in `folder`, in part order.
fn part_logs(folder: &Path) -> Vec<(u32, PathBuf)> {
    let mut logs: Vec<(u32, PathBuf)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let n = name.strip_prefix("part-")?.strip_suffix(".log")?;
            Some((n.parse().ok()?, entry.path()))
        })
        .collect();
    logs.sort();
    logs
}

/// The output of the parts of `record`, each under a line naming it when there are several;
/// `None` when the record says it was deleted or its folder holds no log.
pub(crate) fn read_output(record: &EvidenceRecord) -> Option<String> {
    let Output::Path(folder) = &record.output else {
        return None;
    };
    let logs = part_logs(Path::new(folder));
    if logs.is_empty() {
        return None;
    }
    let several = logs.len() > 1;
    let mut text = String::new();
    for (n, path) in logs {
        if several {
            text.push_str(&format!("--- part {n} ---\n"));
        }
        text.push_str(&String::from_utf8_lossy(
            &std::fs::read(path).unwrap_or_default(),
        ));
        if several && !text.ends_with('\n') {
            text.push('\n');
        }
    }
    Some(text)
}

fn verdict_text(record: &EvidenceRecord) -> String {
    if !record.is_finished() {
        return "running".to_string();
    }
    match &record.verdict {
        Verdict::Pass => "pass".to_string(),
        Verdict::Fail => "fail".to_string(),
        Verdict::NoVerdict(reason) => format!("no verdict: {}", reason.text()),
    }
}

/// The evidence record as lines, for the top of the view.
pub(crate) fn header_block(record: &EvidenceRecord) -> String {
    let mut lines = vec![
        format!("check: {}", record.check),
        format!("task: {}", record.task_id),
        format!("result: {}", verdict_text(record)),
        format!("started: {}", record.started),
    ];
    if let Some(ended) = &record.ended {
        lines.push(format!("ended: {ended}"));
    }
    if let Some(ms) = record.duration_ms {
        lines.push(format!("duration: {ms}ms"));
    }
    lines.push(match &record.variant.scope {
        Scope::All => "scope: all".to_string(),
        Scope::Items { count, .. } => format!("scope: {count} items"),
        Scope::None { reason } => format!("scope: nothing in scope: {reason}"),
        Scope::Unscoped => "scope: unscoped".to_string(),
    });
    lines.push(format!("tree: {}", record.tree_key));
    if record.variant.parts > 1 {
        lines.push(format!("parts: {}", record.variant.parts));
    }
    if !record.command.is_empty() {
        lines.push(format!("command: {}", record.command.join(" ")));
    }
    if let Some(summary) = &record.summary {
        lines.push(format!("summary: {summary}"));
    }
    lines.extend(record.failing.iter().map(|item| format!("failing: {item}")));
    if record.cut > 0 {
        lines.push(format!("cut: {} bytes", record.cut));
    }
    lines.join("\n")
}

/// The whole view of `record`: the header block, a rule, then `output`, or the message that it
/// was deleted.
pub(crate) fn view_text(record: &EvidenceRecord, output: Option<&str>) -> String {
    let body = match output {
        Some(output) => output,
        None if record.is_finished() || matches!(record.output, Output::Deleted) => DELETED_MESSAGE,
        None => "",
    };
    format!("{}\n{}\n{body}", header_block(record), "-".repeat(40))
}

/// The latest record of each check that has one, by check name.
pub(crate) fn latest_per_check(records: &[EvidenceRecord]) -> Vec<&EvidenceRecord> {
    let mut latest: std::collections::BTreeMap<&str, &EvidenceRecord> = Default::default();
    for record in records {
        let slot = latest.entry(record.check.as_str()).or_insert(record);
        if record.id > slot.id {
            *slot = record;
        }
    }
    latest.into_values().collect()
}
