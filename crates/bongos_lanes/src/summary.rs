//! The pre-run summary `lanes: start` shows before anything starts: the lane table with its
//! effective counts and their layers, every workflow a lane may start, every step that merges or
//! deploys without a gate, the unattended choice per kind with its source, the bound, the
//! sections other features register, and the handover note. The dispatcher never approves a
//! gate; the summary only tells what the person is about to start.

use std::fmt::Write as _;
use std::sync::Arc;

use gpui::{App, Global};

/// One feature's section of the summary (pools, queues, triggers, graph-edit declarations).
pub trait SummarySection {
    /// The heading of the section.
    fn title(&self) -> String;
    /// The section's lines; none leaves the section out.
    fn lines(&self, cx: &App) -> Vec<String>;
}

/// The registered summary sections, in the order they were registered.
#[derive(Default, Clone)]
pub struct SummarySections {
    sections: Vec<Arc<dyn SummarySection>>,
}

impl Global for SummarySections {}

impl SummarySections {
    pub fn register(section: Arc<dyn SummarySection>, cx: &mut App) {
        cx.default_global::<Self>().sections.push(section);
    }

    /// Every registered section.
    pub fn sections(cx: &App) -> Vec<Arc<dyn SummarySection>> {
        cx.try_global::<Self>()
            .map(|sections| sections.sections.clone())
            .unwrap_or_default()
    }
}

/// What a window says before lanes move into a separate engine host: the runs it holds finish
/// their current step first, and the person chooses how the handover goes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoverNote {
    /// Lanes start in a separate engine host and this window becomes its client.
    pub window_mode: bool,
    /// Runs in progress in this window.
    pub runs_in_progress: usize,
}

impl HandoverNote {
    /// The note's lines; none when lanes already run in the host this window talks to.
    pub fn lines(&self) -> Vec<String> {
        if !self.window_mode {
            return Vec::new();
        }
        let mut lines =
            vec!["Lanes run in a separate engine host; this window becomes its client".to_string()];
        match self.runs_in_progress {
            0 => {}
            1 => lines.push("1 run finishes its current step first".to_string()),
            n => lines.push(format!("{n} runs finish their current step first")),
        }
        lines
    }
}

/// A kind's effective count and the layer that decided it ("project profile invoicing").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindCount {
    pub kind: String,
    pub count: u32,
    pub layer: String,
}

/// A workflow a lane may start, with the steps that merge or deploy without a gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowSummary {
    pub name: String,
    /// Node keys with `use = "merge"` or a `wait` of kind `deploy` and no gate.
    pub ungated: Vec<String>,
}

/// The unattended choice of a kind and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattendedChoice {
    pub kind: String,
    pub unattended: bool,
    pub source: String,
}

/// What the summary lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryInputs {
    /// Empty: one lane takes every eligible task.
    pub counts: Vec<KindCount>,
    pub workflows: Vec<WorkflowSummary>,
    pub unattended: Vec<UnattendedChoice>,
    /// The bound the dispatcher runs within, as text; `None` is the whole project.
    pub bound: Option<String>,
    pub handover: HandoverNote,
}

/// The summary text, with the lines of `sections`.
pub fn summary_text(
    inputs: &SummaryInputs,
    sections: &[Arc<dyn SummarySection>],
    cx: &App,
) -> String {
    let mut out = String::new();
    if inputs.counts.is_empty() {
        writeln!(out, "no lane table: one lane takes every eligible task").ok();
    } else {
        writeln!(out, "lanes:").ok();
        for count in &inputs.counts {
            writeln!(
                out,
                "  {} {}, from {}",
                count.count, count.kind, count.layer
            )
            .ok();
        }
    }
    writeln!(out, "workflows a lane may start:").ok();
    for workflow in &inputs.workflows {
        writeln!(out, "  {}", workflow.name).ok();
        for node in &workflow.ungated {
            writeln!(out, "    {node} merges or deploys without a gate").ok();
        }
    }
    writeln!(out, "unattended:").ok();
    for choice in &inputs.unattended {
        writeln!(
            out,
            "  {}: {}, from {}",
            choice.kind,
            if choice.unattended { "yes" } else { "no" },
            choice.source
        )
        .ok();
    }
    writeln!(
        out,
        "bound: {}",
        inputs.bound.as_deref().unwrap_or("the whole project")
    )
    .ok();
    for section in sections {
        let lines = section.lines(cx);
        if lines.is_empty() {
            continue;
        }
        writeln!(out, "{}:", section.title()).ok();
        for line in lines {
            writeln!(out, "  {line}").ok();
        }
    }
    for line in inputs.handover.lines() {
        writeln!(out, "{line}").ok();
    }
    out.trim_end().to_string()
}
