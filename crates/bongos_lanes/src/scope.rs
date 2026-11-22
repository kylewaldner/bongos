//! The `lane-run-scope` approval hook, one of the `declared-scope` rule's hooks: a change by a
//! workflow run that a lane runs as its job is approved when everything it touches is inside the
//! `documents/` or `tasks/` folder of the top-level task its lane owns. The lane and its task come
//! from the lanes state ([`LaneRuns::owned_scope`]), never from the change. An edit of the owned
//! task's own definition, or anything outside the scope, waits for a person. A run no lane holds
//! (one started by hand) gets no opinion here, so without another rule it waits for a person too.

use bongos_project_repo::{ApprovalHook, ApprovalHooks, ApprovalInput, ApprovalOpinion, AutomaticRule};
use bongos_project_repo_model::AuthorKind;
use bongos_project_repo_model::layout::{DOCUMENTS_DIR, TASK_FILE, TASKS_DIR};
use gpui::App;

use crate::runs::LaneRuns;

/// Judges lane runs' changes against their owned scope.
pub struct LaneRunScope {
    runs: LaneRuns,
}

impl LaneRunScope {
    pub const NAME: &'static str = "lane-run-scope";

    pub fn new(runs: LaneRuns) -> Self {
        Self { runs }
    }

    /// Registers the hook over `runs`.
    pub fn register(runs: LaneRuns, cx: &mut App) {
        ApprovalHooks::register(std::sync::Arc::new(Self::new(runs)), cx);
    }
}

/// `folder` with a trailing slash, or nothing for the top group's folder.
fn prefix(folder: &str) -> String {
    if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    }
}

/// Whether the file `path` (relative to `definitions/`) is inside the `documents/` or `tasks/`
/// folder of the task whose folder is `folder`.
fn in_owned_scope(folder: &str, path: &str) -> bool {
    if path.split('/').any(|part| part == "..") {
        return false;
    }
    let Some(rest) = path.strip_prefix(&prefix(folder)) else {
        return false;
    };
    rest.starts_with(&format!("{DOCUMENTS_DIR}/")) || rest.starts_with(&format!("{TASKS_DIR}/"))
}

/// Whether a task whose folder is `task_folder` is strictly below the task whose folder is
/// `folder`, in its `tasks/` folder.
fn below(folder: &str, task_folder: &str) -> bool {
    task_folder.starts_with(&format!("{}{TASKS_DIR}/", prefix(folder)))
}

impl ApprovalHook for LaneRunScope {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn rule(&self) -> Option<AutomaticRule> {
        Some(AutomaticRule::DeclaredScope)
    }

    fn judge(&self, input: &ApprovalInput) -> ApprovalOpinion {
        let author = &input.meta.author;
        if author.kind() != AuthorKind::Workflow {
            return ApprovalOpinion::NoOpinion;
        }
        let Some(run) = author.run.as_deref() else {
            return ApprovalOpinion::NoOpinion;
        };
        let Some(scope) = self.runs.owned_scope(input.project_id, run) else {
            return ApprovalOpinion::NoOpinion;
        };
        let (lane, task) = (&scope.lane, scope.task.0.as_str());
        let Some(root) = input.master.node(task) else {
            return ApprovalOpinion::NeedsPerson(format!(
                "lane {lane} owns task {task}, which is not in master"
            ));
        };
        let folder = root.folder.as_str();
        let own_file = format!("{}{TASK_FILE}", prefix(folder));
        let touched = input
            .summary
            .tasks_added
            .iter()
            .chain(&input.summary.tasks_removed)
            .chain(&input.summary.tasks_redefined)
            .chain(&input.summary.reparented);
        for id in touched {
            if id == task {
                return ApprovalOpinion::NeedsPerson(format!(
                    "lane {lane} edits {own_file}, the definition of its task {task}"
                ));
            }
            for tree in [input.master, input.change] {
                if let Some(node) = tree.node(id)
                    && !below(folder, &node.folder)
                {
                    return ApprovalOpinion::NeedsPerson(format!(
                        "task {id} is outside the task {task} lane {lane} owns"
                    ));
                }
            }
        }
        for (path, _) in &input.summary.files {
            if *path == own_file {
                return ApprovalOpinion::NeedsPerson(format!(
                    "lane {lane} edits {own_file}, the definition of its task {task}"
                ));
            }
            if !in_owned_scope(folder, path) {
                return ApprovalOpinion::NeedsPerson(format!(
                    "{path} is outside the documents and tasks of task {task} that lane {lane} owns"
                ));
            }
        }
        ApprovalOpinion::Approve(format!(
            "lane {lane} run {run} changed only the documents and tasks of its task {task}"
        ))
    }
}
