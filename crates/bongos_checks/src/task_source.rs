//! What the engine needs to know about a task: its effective set of checks, its project, its
//! repository and the `worktree` of its file. The seam keeps the engine free of the work tree, so
//! a test supplies a task without one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_checks_model::{EffectiveSet, effective_set};
use bongos_project_repo::{ProjectRepo, StateWriter};
use bongos_project_repo_model::DefinitionTree;
use bongos_work::model::derive::{self, State};
use bongos_work::{WorkSettings, WorkStore};
use gpui::App;
use settings::Settings as _;

use crate::definitions::named_sets;
use crate::pin::master_definitions;
use crate::settings::TaskChecksSettings;

/// The folder inside the scratch folder that holds the private indexes of tree keys.
pub(crate) const TREE_KEYS_FOLDER: &str = "tree-keys";

/// One task as the engine runs its checks.
#[derive(Clone)]
pub struct TaskChecks {
    pub task_id: String,
    /// How messages name the task.
    pub name: String,
    /// The task's own repository, as `work.repositories` names it.
    pub repo: Option<String>,
    /// The `worktree` field of the task's file.
    pub worktree: Option<PathBuf>,
    pub set: EffectiveSet,
    pub project: ProjectRepo,
    /// The task's base branch.
    pub base_branch: Option<String>,
    /// The task's output folder.
    pub output_dir: PathBuf,
    /// Where the private indexes of tree keys go.
    pub index_dir: PathBuf,
}

impl TaskChecks {
    /// The writer of the project's state store.
    pub fn writer(&self) -> Option<Arc<StateWriter>> {
        self.project.state()?.writer().cloned()
    }
}

/// Finds a task's checks context.
pub trait TaskSource: 'static {
    /// The task, or `None` when it is unknown or has no project repository.
    fn task(&self, task_id: &str, cx: &App) -> Option<TaskChecks>;

    /// The ids of the tasks that are Done or cancelled now, for the question of which child of
    /// a group would carry its closing checks. None by default.
    fn done_tasks(&self, cx: &App) -> HashSet<String> {
        let _ = cx;
        HashSet::new()
    }

    /// The primary checkout of the code repository `name` (as `work.repositories` names it),
    /// whose object store holds the trees of the tasks in it. None by default.
    fn code_repository(&self, name: &str, cx: &App) -> Option<PathBuf> {
        let _ = (name, cx);
        None
    }
}

/// [`TaskSource`] over the Work tree and the open project repositories.
pub struct WorkTaskSource;

impl TaskSource for WorkTaskSource {
    fn task(&self, task_id: &str, cx: &App) -> Option<TaskChecks> {
        let tree = WorkStore::global(cx)?.read(cx).snapshot()?;
        let task = tree.task_by_id(task_id)?;
        let file = task.file.as_ref()?;
        let project = bongos_work::project_repo_of(task, cx)?;
        let set = effective_set_in(&master_definitions(&project), task_id, &|id| {
            tree.task_by_id(id).is_some_and(|other| {
                matches!(derive::state(other, &tree), State::Done | State::Cancelled)
            })
        })?;
        let repo = bongos_work::resolve_repository(
            &tree,
            task_id,
            &WorkSettings::get_global(cx).repositories,
        )
        .map(|repository| repository.name.clone());
        let settings = TaskChecksSettings::get_global(cx);
        Some(TaskChecks {
            task_id: task_id.to_string(),
            name: task.named_goal(),
            repo,
            worktree: file.worktree.as_deref().map(PathBuf::from),
            set,
            base_branch: file.base.clone(),
            output_dir: settings.output_folder(&project.location().files, task_id),
            index_dir: settings.scratch_folder().join(TREE_KEYS_FOLDER),
            project,
        })
    }

    fn done_tasks(&self, cx: &App) -> HashSet<String> {
        let Some(tree) = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot()) else {
            return HashSet::new();
        };
        tree.tasks()
            .into_iter()
            .filter(|task| matches!(derive::state(task, &tree), State::Done | State::Cancelled))
            .filter_map(|task| task.id().map(str::to_string))
            .collect()
    }

    fn code_repository(&self, name: &str, cx: &App) -> Option<PathBuf> {
        let repositories = &WorkSettings::get_global(cx).repositories;
        let repository = bongos_work::find_repository(repositories, name)?;
        Some(bongos_work::expand_home(&repository.path))
    }
}

/// The effective set of `task_id` in the definitions at `definitions`; `None` when they do not
/// parse.
pub(crate) fn effective_set_in(
    definitions: &Path,
    task_id: &str,
    done: &dyn Fn(&str) -> bool,
) -> Option<EffectiveSet> {
    let tree = DefinitionTree::parse_dir(definitions).ok()?;
    Some(effective_set(
        task_id,
        &tree,
        &named_sets(definitions),
        done,
    ))
}
