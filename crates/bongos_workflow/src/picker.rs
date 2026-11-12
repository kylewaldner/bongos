//! The picker and review behind `workflow: run` (feature 20, step 17). The logic is plain data so
//! it can be tested without a window: the candidate definitions with their layers, the one the
//! todo names preselected, the review of a pick (the [`Summary`], the rows of the review list,
//! one input overridden at a time) and what Enter does: start with the overrides, or refuse while
//! an input is missing.
//!
//! Nothing here writes. Choosing another workflow than the todo's names it only in the
//! [`StartRequest`]; `task.toml` is left as it is.

use std::collections::BTreeMap;

use bongos_work::WorkStore;
use bongos_work_model::store::WorkTree;
use bongos_workflow_model::inputs::resolve;
use bongos_workflow_model::layers::{self, DEFINITION_FILE};
use bongos_workflow_model::{InputValue, LayerDirs, parse};
use gpui::{AppContext as _, Context, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::WorkflowsSettings;
use crate::actions::show_toast;
use crate::commands::open_report;
use crate::engine::WorkflowEngine;
use crate::lifecycle::{StartRequest, open_todo};
use crate::runner_claude::AgentDefaults;
use crate::runner_program::ProgramSettings;
use crate::summary::{SandboxState, Summary};

/// One definition the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    /// `name (layer)`, with the layers it shadows.
    pub label: String,
}

/// The definitions of every layer, by name, each labeled with its winning layer and the lower
/// layers it shadows.
pub fn candidates(dirs: &LayerDirs) -> Vec<Candidate> {
    layers::resolve(dirs)
        .resolved
        .into_iter()
        .map(|resolved| {
            let mut label = format!("{} ({}", resolved.name, resolved.layer.label());
            if !resolved.overrides.is_empty() {
                let shadowed: Vec<&str> = resolved
                    .overrides
                    .iter()
                    .map(|layer| layer.label())
                    .collect();
                label.push_str(&format!(", shadows {}", shadowed.join(", ")));
            }
            label.push(')');
            Candidate {
                name: resolved.name,
                label,
            }
        })
        .collect()
}

/// The list of definitions with the todo's workflow selected first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub candidates: Vec<Candidate>,
    selected: usize,
}

impl Picker {
    /// A picker over `candidates` with the one named `todo_workflow` selected; the first when the
    /// todo names none or the definition is gone.
    pub fn new(candidates: Vec<Candidate>, todo_workflow: Option<&str>) -> Self {
        let selected = todo_workflow
            .and_then(|name| candidates.iter().position(|found| found.name == name))
            .unwrap_or(0);
        Self {
            candidates,
            selected,
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn labels(&self) -> Vec<String> {
        self.candidates.iter().map(|c| c.label.clone()).collect()
    }

    pub fn select(&mut self, index: usize) {
        if index < self.candidates.len() {
            self.selected = index;
        }
    }

    /// The name of the selected definition.
    pub fn chosen(&self) -> Option<&str> {
        self.candidates
            .get(self.selected)
            .map(|found| found.name.as_str())
    }
}

/// The workflow the open todo (the one a run completes) of `task_id` names.
pub fn todo_workflow(tree: &WorkTree, task_id: &str) -> Option<String> {
    let task = tree.task_by_id(task_id)?;
    open_todo(task, None)?.1.workflow.clone()
}

/// A row of the review list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Start,
    Override(String),
}

/// What choosing a row does.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Start(StartRequest),
    /// Enter is refused until these inputs have a value.
    Blocked(Vec<String>),
    /// Ask for a new value of this input, starting from `initial`.
    Ask {
        name: String,
        initial: String,
    },
}

/// A workflow chosen for a task, with the inputs overridden so far.
#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub task_id: String,
    pub workflow: String,
    pub overrides: BTreeMap<String, InputValue>,
}

impl Review {
    pub fn new(task_id: impl Into<String>, workflow: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            workflow: workflow.into(),
            overrides: BTreeMap::new(),
        }
    }

    /// The summary of a run of the workflow on the task, with the overrides applied.
    pub fn summary(
        &self,
        dirs: &LayerDirs,
        tree: &WorkTree,
        sandbox: &SandboxState,
        agents: &AgentDefaults,
        programs: &ProgramSettings,
    ) -> Result<Summary, String> {
        let task = tree
            .task_by_id(&self.task_id)
            .ok_or_else(|| format!("No task {} is loaded.", self.task_id))?;
        let todo = open_todo(task, None)
            .map(|(index, _)| index)
            .ok_or("This task has no open todo for a workflow to complete.")?;
        let resolved = layers::resolve(dirs)
            .get(&self.workflow)
            .cloned()
            .ok_or_else(|| format!("No workflow is named '{}'.", self.workflow))?;
        let file = resolved.dir.join(DEFINITION_FILE);
        let text = std::fs::read_to_string(&file)
            .map_err(|error| format!("{}: {error}", file.display()))?;
        let definition = parse(&text).map_err(|error| error.to_string())?;
        let inputs = resolve(&definition, task, tree, Some(todo), &self.overrides);
        Ok(Summary::build(
            &definition,
            resolved.layer,
            inputs,
            sandbox,
            agents,
            programs,
        ))
    }

    /// The rows of the review list: Start, then one row per input to override.
    pub fn rows(&self, summary: &Summary) -> Vec<(Row, String)> {
        let start = if summary.can_start() {
            format!("Start the run of {}", summary.workflow)
        } else {
            let names: Vec<&str> = summary.missing().iter().map(|i| i.name.as_str()).collect();
            format!("Start (blocked, missing: {})", names.join(", "))
        };
        let mut rows = vec![(Row::Start, start)];
        for input in &summary.inputs {
            let value = match &input.value {
                Some(value) => value
                    .to_text()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                None => "(no value)".to_string(),
            };
            rows.push((
                Row::Override(input.name.clone()),
                format!(
                    "Override input {}: {value} [from {}]",
                    input.name, input.source
                ),
            ));
        }
        rows
    }

    /// The row highlighted first: the first missing input, else Start.
    pub fn first_row(&self, summary: &Summary) -> usize {
        summary
            .inputs
            .iter()
            .position(|input| input.missing)
            .map_or(0, |position| position + 1)
    }

    /// What choosing `row` does.
    pub fn choose(&self, row: &Row, summary: &Summary) -> Choice {
        match row {
            Row::Start => {
                if summary.can_start() {
                    Choice::Start(self.request())
                } else {
                    Choice::Blocked(summary.missing().iter().map(|i| i.name.clone()).collect())
                }
            }
            Row::Override(name) => Choice::Ask {
                name: name.clone(),
                initial: summary
                    .inputs
                    .iter()
                    .find(|input| &input.name == name)
                    .and_then(|input| input.value.as_ref())
                    .map(InputValue::to_text)
                    .unwrap_or_default(),
            },
        }
    }

    /// Overrides input `name` with `text`; an empty text drops the override.
    pub fn set_override(&mut self, name: &str, text: &str) {
        if text.trim().is_empty() {
            self.overrides.remove(name);
        } else {
            self.overrides
                .insert(name.to_string(), InputValue::Text(text.trim().to_string()));
        }
    }

    /// The request that starts the run: the chosen workflow and the overrides, on the task's
    /// current todo. Nothing is written to the task.
    pub fn request(&self) -> StartRequest {
        StartRequest {
            workflow: Some(self.workflow.clone()),
            overrides: self.overrides.clone(),
            ..StartRequest::new(self.task_id.clone())
        }
    }
}

/// The title of the buffer holding the summary of a run.
pub const SUMMARY_TITLE: &str = "Workflow run summary";

/// `workflow: run` on `task_id`: the picker of definitions with the todo's workflow selected,
/// then the review of the pick.
pub(crate) fn run_picker(
    engine: WorkflowEngine,
    task_id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot()) else {
        show_toast(
            workspace,
            "The task tree is not loaded yet.".to_string(),
            cx,
        );
        return;
    };
    let picker = Picker::new(
        candidates(engine.dirs()),
        todo_workflow(&tree, &task_id).as_deref(),
    );
    if picker.candidates.is_empty() {
        show_toast(
            workspace,
            "No workflows are defined (workflow: new)".to_string(),
            cx,
        );
        return;
    }
    let names: Vec<String> = picker
        .candidates
        .iter()
        .map(|found| found.name.clone())
        .collect();
    bongos_work::pick(
        workspace,
        "Run workflow",
        picker.labels(),
        picker.selected(),
        Box::new(move |index, workspace, window, cx| {
            let Some(name) = names.get(index).cloned() else {
                return;
            };
            probe_then_review(
                engine,
                Review::new(task_id, name),
                true,
                workspace,
                window,
                cx,
            );
        }),
        window,
        cx,
    );
}

/// The sandbox state of this machine, probed with the narrowest policy a program runs under.
fn probe_sandbox(cwd: std::path::PathBuf) -> SandboxState {
    use sandbox::{GitSandboxPolicy, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};
    let policy = SandboxPolicy {
        fs: SandboxFsPolicy::Restricted {
            writable_paths: Vec::new(),
        },
        network: SandboxNetPolicy::Blocked,
        git: GitSandboxPolicy::default(),
    };
    match crate::runner_program::os_sandbox_probe()(&policy, &cwd) {
        Ok(()) => SandboxState::Sandboxed,
        Err(reason) => SandboxState::NotSandboxed(reason),
    }
}

fn probe_then_review(
    engine: WorkflowEngine,
    review: Review,
    show_summary: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let probe = cx.background_spawn(async move { probe_sandbox(cwd) });
    cx.spawn_in(window, async move |workspace, cx| {
        let sandbox = probe.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                review_step(engine, review, sandbox, show_summary, workspace, window, cx)
            })
            .ok();
    })
    .detach();
}

/// Shows the summary (the first time) and the review list; each row does what [`Review::choose`]
/// says, and an override comes back here with the summary read again.
fn review_step(
    engine: WorkflowEngine,
    review: Review,
    sandbox: SandboxState,
    show_summary: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot()) else {
        show_toast(
            workspace,
            "The task tree is not loaded yet.".to_string(),
            cx,
        );
        return;
    };
    let programs = WorkflowsSettings::get_global(cx).programs.clone();
    let summary = match review.summary(
        engine.dirs(),
        &tree,
        &sandbox,
        &AgentDefaults::default(),
        &programs,
    ) {
        Ok(summary) => summary,
        Err(message) => {
            show_toast(workspace, message, cx);
            return;
        }
    };
    if show_summary {
        open_report(
            SUMMARY_TITLE.to_string(),
            summary.text(),
            workspace,
            window,
            cx,
        );
    }
    let rows = review.rows(&summary);
    let labels: Vec<String> = rows.iter().map(|(_, label)| label.clone()).collect();
    let first = review.first_row(&summary);
    bongos_work::pick(
        workspace,
        format!("Run {}", summary.workflow),
        labels,
        first,
        Box::new(move |index, workspace, window, cx| {
            let Some((row, _)) = rows.get(index) else {
                return;
            };
            match review.choose(row, &summary) {
                Choice::Start(request) => start(&engine, request, workspace, cx),
                Choice::Blocked(names) => {
                    show_toast(
                        workspace,
                        format!("Supply the missing input to start: {}", names.join(", ")),
                        cx,
                    );
                    review_step(engine, review, sandbox, false, workspace, window, cx);
                }
                Choice::Ask { name, initial } => {
                    let title = format!("Value of input {name}");
                    bongos_work::ask(
                        workspace,
                        title,
                        &initial,
                        true,
                        Box::new(move |text, workspace, window, cx| {
                            let mut review = review;
                            review.set_override(&name, &text);
                            review_step(engine, review, sandbox, false, workspace, window, cx);
                        }),
                        window,
                        cx,
                    );
                }
            }
        }),
        window,
        cx,
    );
}

fn start(
    engine: &WorkflowEngine,
    request: StartRequest,
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) {
    let message = match engine.start(request, cx) {
        Ok(_) => "Workflow run started".to_string(),
        Err(refusal) => refusal.to_string(),
    };
    show_toast(workspace, message, cx);
}
