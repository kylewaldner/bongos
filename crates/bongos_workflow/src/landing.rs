//! Workflows in the landing of a project repository: check 5, `workflow-valid`, which refuses a
//! change that leaves a workflow invalid, and the `workflow` check kind, which starts a workflow
//! on a change and takes its end for the check's verdict. Both are registered with the landing
//! checks of `bongos_project_repo` by [`register`].

use std::path::{Component, Path};
use std::sync::{Arc, RwLock};

use bongos_project_repo::{LandingChecks, WorkflowResolver};
use bongos_project_repo::{CheckInput, CheckKinds, CheckOutcome, CheckPhase, LandingCheck};
use bongos_project_repo_model::ChangeSummary;
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, WORKFLOWS_DIR};
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::{Layer, LayerDirs, Resolved, Severity, parse};
use bongos_workflow_model::layers::resolve;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::App;

use crate::definitions::check_definition;

/// The name of check 5.
pub const WORKFLOW_VALID_CHECK: &str = "workflow-valid";
/// The key of a `[[landing.checks]]` entry's `run` table that names a workflow.
pub const WORKFLOW_KIND: &str = "workflow";
/// The longest workflow name a `workflow` check accepts; the same bound as `workflow: new`.
const MAX_WORKFLOW_NAME: usize = 80;

/// Registers check 5 and the `workflow` check kind. Call before a project repository is opened
/// ([`bongos_project_repo::ProjectRepo::use_registered_checks`] copies what is registered then).
pub fn register(cx: &mut App) {
    LandingChecks::register(Arc::new(WorkflowValid), cx);
    CheckKinds::register(WORKFLOW_KIND, parse_workflow, cx);
    LandingChecks::register_workflow_resolver(workflow_resolver(), cx);
}

static RESOLVER_DIRS: RwLock<Option<LayerDirs>> = RwLock::new(None);

/// Sets the shipped and user workflow folders the landing resolver reads (the engine does this
/// when it starts); `None` makes the resolver accept every name again.
pub fn set_resolver_dirs(dirs: Option<LayerDirs>) {
    *RESOLVER_DIRS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = dirs;
}

/// Resolves a workflow name in the user's and the shipped workflows for check 4. The project's
/// own workflows are looked up by the check itself in the change. With no folders set, every
/// name resolves, since nothing can be said.
pub fn workflow_resolver() -> WorkflowResolver {
    Arc::new(|name: &str| {
        let dirs = RESOLVER_DIRS
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(mut dirs) = dirs else {
            return true;
        };
        dirs.project = None;
        resolve(&dirs).get(name).is_some()
    })
}

/// Check 5, `workflow-valid` (`InQueue`): every workflow folder the change touches, which covers
/// a changed prompt or script since those live in the folder, is validated as `workflow:
/// validate` validates it. An error reads "workflow <name>: node <node>, line <line>: <message>".
pub struct WorkflowValid;

impl LandingCheck for WorkflowValid {
    fn name(&self) -> &str {
        WORKFLOW_VALID_CHECK
    }

    fn phase(&self) -> CheckPhase {
        CheckPhase::InQueue
    }

    fn applies(&self, summary: &ChangeSummary) -> bool {
        !summary.workflows_touched.is_empty()
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        let workflows = input.root.join(DEFINITIONS_DIR).join(WORKFLOWS_DIR);
        let touched = input.summary.workflows_touched.clone();
        async move { CheckOutcome::from_refusals(problems(&workflows, &touched)) }.boxed()
    }
}

/// The errors of the workflows in `touched` (folder names under `workflows`), one line each.
/// A folder with no `workflow.toml` (a workflow the change removed) has nothing to validate.
pub fn problems(workflows: &Path, touched: &[String]) -> Vec<String> {
    let mut folders: Vec<&str> = touched.iter().map(String::as_str).collect();
    folders.sort_unstable();
    folders.dedup();
    let mut lines = Vec::new();
    for folder in folders {
        // The folder names come from paths in a change; only one plain name stays under
        // `workflows`.
        let mut parts = Path::new(folder).components();
        if !matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        ) {
            lines.push(format!(
                "workflow folder {folder:?} is not a plain folder name"
            ));
            continue;
        }
        let dir = workflows.join(folder);
        let Ok(text) = std::fs::read_to_string(dir.join(DEFINITION_FILE)) else {
            continue;
        };
        let name = parse(&text)
            .map(|definition| definition.name)
            .unwrap_or_else(|_| folder.to_string());
        let report = check_definition(&Resolved {
            name: name.clone(),
            layer: Layer::Project,
            dir,
            overrides: Vec::new(),
        });
        if let Some(error) = &report.load_error {
            lines.push(format!("workflow {name}: {error}"));
        }
        for diagnostic in &report.diagnostics {
            if diagnostic.severity != Severity::Error {
                continue;
            }
            let node = match &diagnostic.node {
                Some(node) => format!("node {node}, "),
                None => String::new(),
            };
            lines.push(format!(
                "workflow {name}: {node}line {}: {}",
                diagnostic.line, diagnostic.message
            ));
        }
    }
    lines
}

/// How a run a landing check started ended, or why none started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingRunEnd {
    Succeeded,
    /// The run failed; what it says.
    Failed(String),
    Cancelled,
    /// No run started; why.
    NotStarted(String),
}

/// Starts workflow runs for landing checks. The app's implementation hands each request to the
/// engine on the main thread ([`crate::engine`]); tests give their own.
pub trait LandingRuns: Send + Sync + 'static {
    /// Starts `workflow` on the task `task_id` with `StartedBy::Landing` and resolves when the
    /// run ends.
    fn run(&self, workflow: &str, task_id: &str) -> BoxFuture<'static, LandingRunEnd>;
}

static LANDING_RUNS: RwLock<Option<Arc<dyn LandingRuns>>> = RwLock::new(None);

/// Sets who starts the runs of `workflow` checks; `None` when the engine is gone.
pub fn set_landing_runs(runs: Option<Arc<dyn LandingRuns>>) {
    *LANDING_RUNS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = runs;
}

pub(crate) fn landing_runs() -> Option<Arc<dyn LandingRuns>> {
    LANDING_RUNS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// A `[[landing.checks]]` entry `run = { workflow = "<name>" }`: starts that workflow on the
/// first task the change adds or redefines and passes when the run succeeds, fails when it
/// fails, and has no verdict when it is cancelled, when no run could start, or when the engine
/// is not running. Its runs start with `StartedBy::Landing`.
pub struct WorkflowCheck {
    workflow: String,
    name: String,
}

/// Parses the `workflow` value of a `run` table.
pub fn parse_workflow(value: &toml::Value) -> Result<Arc<dyn LandingCheck>, String> {
    let workflow = value
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or("workflow must be the name of a workflow")?;
    if workflow.chars().count() > MAX_WORKFLOW_NAME || workflow.chars().any(char::is_control) {
        return Err(format!(
            "workflow must be a name of at most {MAX_WORKFLOW_NAME} characters with no control characters"
        ));
    }
    Ok(Arc::new(WorkflowCheck {
        name: format!("workflow {workflow}"),
        workflow: workflow.to_string(),
    }))
}

/// The task a change's workflow check runs on: the first task it adds, else the first it
/// redefines.
fn run_task(summary: &ChangeSummary) -> Option<&str> {
    summary
        .tasks_added
        .first()
        .or_else(|| summary.tasks_redefined.first())
        .map(String::as_str)
}

impl LandingCheck for WorkflowCheck {
    fn name(&self) -> &str {
        &self.name
    }

    fn phase(&self) -> CheckPhase {
        CheckPhase::BeforeQueue
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        let workflow = self.workflow.clone();
        let task = run_task(input.summary).map(str::to_string);
        async move {
            let Some(runs) = landing_runs() else {
                return CheckOutcome::no_verdict("the workflow engine is not running");
            };
            let Some(task) = task else {
                return CheckOutcome::no_verdict(format!(
                    "the change adds or redefines no task for workflow {workflow} to run on"
                ));
            };
            match runs.run(&workflow, &task).await {
                LandingRunEnd::Succeeded => CheckOutcome::Pass,
                LandingRunEnd::Failed(detail) => {
                    CheckOutcome::fail(format!("workflow {workflow} failed: {detail}"))
                }
                LandingRunEnd::Cancelled => {
                    CheckOutcome::no_verdict(format!("workflow {workflow} was cancelled"))
                }
                LandingRunEnd::NotStarted(why) => {
                    CheckOutcome::no_verdict(format!("workflow {workflow} did not start: {why}"))
                }
            }
        }
        .boxed()
    }
}
