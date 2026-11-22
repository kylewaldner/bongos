//! The dry-run buffer: what each lane would pick right now, the best-ranked other candidates and
//! why each waits, then the lines pick extensions add. Pure over what it is given: it takes the
//! picker's inputs by reference and writes nothing, so the store generation and every file stay
//! as they were.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use bongos_lanes_model::{
    AssignedWork, DryRun, LaneId, LaneTable, LanesState, PickGraph, PickOptions, PickOutcome,
    WaitReason, Waiting, explain,
};
use gpui::App;

use crate::pick_extension::{PickExtension, PickExtensions};
use crate::wait_text::WaitTextFormatters;

/// What the dry run reads.
pub struct DryRunInputs<'a> {
    pub graph: &'a PickGraph,
    pub state: &'a LanesState,
    pub table: &'a LaneTable,
    pub options: &'a PickOptions,
}

/// The buffer text of a dry run over `inputs`, with the lines of `extensions`.
pub fn dry_run_buffer(
    inputs: &DryRunInputs,
    extensions: &[Arc<dyn PickExtension>],
    wait_text: &WaitTextFormatters,
) -> String {
    let dry = explain(inputs.graph, inputs.state, inputs.table, inputs.options);
    let extension_lines: Vec<String> = extensions
        .iter()
        .flat_map(|extension| extension.explain(inputs.state))
        .collect();
    dry_run_text(inputs.graph, &dry, &extension_lines, wait_text)
}

/// [`dry_run_buffer`] with the extensions and formatters registered in `cx`.
pub fn dry_run_buffer_in(inputs: &DryRunInputs, cx: &App) -> String {
    dry_run_buffer(
        inputs,
        &PickExtensions::extensions(cx),
        &WaitTextFormatters::global(cx),
    )
}

fn name_of(graph: &PickGraph, task: &bongos_lanes_model::TaskId) -> String {
    graph
        .task(task)
        .map_or_else(|| task.0.clone(), |task| task.name.clone())
}

/// Renders a finished [`DryRun`].
pub fn dry_run_text(
    graph: &PickGraph,
    dry: &DryRun,
    extension_lines: &[String],
    wait_text: &WaitTextFormatters,
) -> String {
    let mut out = String::new();
    for (lane, outcome) in &dry.per_lane {
        match outcome {
            PickOutcome::Assign(assignment) => {
                let work = match &assignment.work {
                    AssignedWork::Leaf { task, .. } => name_of(graph, task),
                    AssignedWork::Extension { kind, task, .. } => match task {
                        Some(task) => format!("{kind} {}", name_of(graph, task)),
                        None => kind.clone(),
                    },
                };
                // A lane takes the best-ranked candidate, so its pick is rank 1.
                writeln!(
                    out,
                    "{lane} would pick: {work}    workflow: {}    rank 1",
                    assignment.workflow
                )
                .ok();
            }
            PickOutcome::Nothing { .. } => {
                writeln!(out, "{lane} would pick: nothing").ok();
            }
            PickOutcome::Hold(hold) => {
                writeln!(
                    out,
                    "{lane} holds: {} {}",
                    name_of(graph, &hold.task),
                    hold.reason
                )
                .ok();
            }
        }
    }
    if !dry.waiting.is_empty() {
        writeln!(out, "waiting:").ok();
    }
    for waiting in &dry.waiting {
        writeln!(
            out,
            "  {}   {}",
            name_of(graph, &waiting.task),
            wait_text.text(&waiting.reason)
        )
        .ok();
    }
    for line in extension_lines {
        writeln!(out, "{line}").ok();
    }
    write!(
        out,
        "ready set: {}   critical path: {} jobs   lanes that can be busy at once: {}",
        dry.ready, dry.critical_path, dry.parallel
    )
    .ok();
    out
}

/// Why a lane finds nothing, in one phrase: "nothing eligible: 4 tasks wait for dependencies, 2
/// share area `editor` with a task in flight".
pub fn nothing_reason(reasons: &[Waiting], wait_text: &WaitTextFormatters) -> String {
    if reasons.is_empty() {
        return "nothing to do".to_string();
    }
    let mut dependencies = 0;
    let mut areas: BTreeMap<&str, usize> = BTreeMap::new();
    let mut others: BTreeMap<String, usize> = BTreeMap::new();
    for waiting in reasons {
        match &waiting.reason {
            WaitReason::Dependency { .. } => dependencies += 1,
            WaitReason::Area { tag, .. } => *areas.entry(tag).or_insert(0) += 1,
            other => *others.entry(wait_text.text(other)).or_insert(0) += 1,
        }
    }
    let mut parts = Vec::new();
    if dependencies > 0 {
        parts.push(format!(
            "{dependencies} {} for dependencies",
            if dependencies == 1 {
                "task waits"
            } else {
                "tasks wait"
            }
        ));
    }
    for (tag, count) in areas {
        parts.push(format!("{count} share area `{tag}` with a task in flight"));
    }
    for (text, count) in others {
        parts.push(format!("{count} {text}"));
    }
    format!("nothing eligible: {}", parts.join(", "))
}

/// The idle reason of every lane the dry run found nothing for.
pub fn idle_reasons(dry: &DryRun, wait_text: &WaitTextFormatters) -> BTreeMap<LaneId, String> {
    dry.per_lane
        .iter()
        .filter_map(|(lane, outcome)| match outcome {
            PickOutcome::Nothing { reasons, .. } => {
                Some((lane.clone(), nothing_reason(reasons, wait_text)))
            }
            _ => None,
        })
        .collect()
}
