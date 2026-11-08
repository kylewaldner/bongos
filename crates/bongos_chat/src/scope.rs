//! Where a new chat belongs and where it runs (design D6).
//!
//! The scope is the task the chat is on: its record goes in that task's `chats/`. A chat on a
//! project's top task is a project chat, and its record goes in the project directory. The
//! working directory, first match wins:
//! 1. the task's recorded worktree, when it exists (a group has none);
//! 2. for a project chat, the project's repository (its `repository`, else the one the project
//!    lists, else the first it lists) at its `path`, when that directory exists;
//! 3. otherwise the scope directory itself (groups, documents, spikes before start, `other`).
//!
//! An implement chat on a pull request leaf with no worktree is refused: it would otherwise run in
//! the task directory, not in the code.

use std::path::PathBuf;

use bongos_work::{Repository, expand_home, find_repository, is_project_top, resolve_repository};
use bongos_work_model::Kind;
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;

/// The session type that needs a worktree on a pull request leaf.
pub const IMPLEMENT_TYPE: &str = "implement";

/// Why an implement chat on a pull request leaf without a worktree is refused.
pub const START_FIRST: &str =
    "Start the task first (task: start, C-c w s); implement chats run in its worktree";

/// What the working directory of a new chat is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CwdSource {
    /// The task's worktree.
    Worktree,
    /// The repository of this name in `work.repositories`.
    Repository(String),
    /// The scope directory.
    ScopeDir,
}

/// Where a new chat's record goes and where its CLI runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatScope {
    /// The task the chat is on (a project's top task for a project chat).
    pub task_id: String,
    /// The directory whose `chats/` holds the record.
    pub scope_dir: PathBuf,
    /// Where the CLI runs.
    pub cwd: PathBuf,
    pub cwd_source: CwdSource,
    /// The chat is on a project's top task.
    pub project_chat: bool,
}

/// The scope of a new `chat_type` chat on the task `task_id`. `repositories` is
/// `work.repositories`; `repository` names another of them for a project chat (`chat: new with
/// options`). Refused, with the text to show, when no task has the id or for an implement chat on
/// a pull request leaf without a worktree.
pub fn resolve_scope(
    tree: &WorkTree,
    task_id: &str,
    chat_type: &str,
    repositories: &[Repository],
    repository: Option<&str>,
) -> Result<ChatScope, String> {
    let place = Place::locate(tree, task_id)?;
    let task = place.task;
    let shape = derive::shape(task, tree);
    let project_chat = is_project_top(tree, task_id);
    let scope = |cwd: PathBuf, cwd_source: CwdSource| ChatScope {
        task_id: task_id.to_string(),
        scope_dir: task.dir.clone(),
        cwd,
        cwd_source,
        project_chat,
    };

    let worktree = (shape == Shape::Leaf)
        .then(|| task.file.as_ref()?.worktree.as_deref())
        .flatten()
        .map(expand_home)
        .filter(|path| path.is_dir());
    if let Some(worktree) = worktree {
        return Ok(scope(worktree, CwdSource::Worktree));
    }
    let pull_request = task
        .file
        .as_ref()
        .is_some_and(|file| matches!(file.kind, Kind::PrShip | Kind::PrMerge));
    if chat_type == IMPLEMENT_TYPE && shape == Shape::Leaf && pull_request {
        return Err(START_FIRST.to_string());
    }
    if project_chat {
        let chosen = match repository {
            Some(name) => find_repository(repositories, name),
            None => resolve_repository(tree, task_id, repositories).or_else(|| {
                project_repositories(tree, task_id)
                    .iter()
                    .find_map(|name| find_repository(repositories, name))
            }),
        };
        if let Some(chosen) = chosen {
            let path = expand_home(&chosen.path);
            if path.is_dir() {
                return Ok(scope(path, CwdSource::Repository(chosen.name.clone())));
            }
        }
    }
    Ok(scope(task.dir.clone(), CwdSource::ScopeDir))
}

/// The repositories the project of `task_id` lists, in its order; empty for a one-off.
pub fn project_repositories(tree: &WorkTree, task_id: &str) -> Vec<String> {
    Place::locate(tree, task_id)
        .ok()
        .and_then(|place| Some(place.project?.file.as_ref()?.repositories.clone()))
        .unwrap_or_default()
}
