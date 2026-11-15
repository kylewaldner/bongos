//! The two diffs of a change. The raw diff is what `git diff` shows between the change's base and
//! its end. The task-aware diff says the same thing in terms of tasks: which were added, removed
//! or moved to another folder, whose todos, tags or edges changed, and the state edits the change
//! staged (prompts it creates, files it moves, fields it sets). A change an amending check edited
//! also has the diff of the amend commits alone.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::{ChangeSummary, DefinitionTree, summarize};

use crate::changes::{ChangeRow, ChangeState};
use crate::create::{MAIN_BRANCH, ProjectRepo};
use crate::git::{CommitId, TreeRef};
use crate::loader::load_definition_tree;
use crate::quick_edit::check_commit_id;
use crate::state_ops::StateOp;

/// A task whose folder differs between the two sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskMove {
    pub task: String,
    /// Folders relative to `definitions/`, `""` for the top group.
    pub from: String,
    pub to: String,
}

/// What a change does, in terms of tasks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskAwareDiff {
    pub summary: ChangeSummary,
    pub moves: Vec<TaskMove>,
    /// Tasks whose prompts file the change creates when it lands.
    pub prompts_created: Vec<String>,
    /// The other staged state ops, described. Only a change that has not landed has any.
    pub state_edits: Vec<String>,
}

impl TaskAwareDiff {
    /// The diff as lines of text, one fact per line.
    pub fn lines(&self) -> Vec<String> {
        let summary = &self.summary;
        let mut lines = vec![summary.row_text()];
        let mut push = |what: &str, ids: &[String]| {
            lines.extend(ids.iter().map(|id| format!("task {id} {what}")));
        };
        push("added", &summary.tasks_added);
        push("removed", &summary.tasks_removed);
        push("retagged", &summary.retagged);
        push("todos changed", &summary.todos_changed);
        for task in &self.moves {
            lines.push(format!(
                "task {} moved: {} -> {}",
                task.task,
                folder_name(&task.from),
                folder_name(&task.to)
            ));
        }
        for edge in &summary.edges_added {
            lines.push(format!(
                "edge added: {} {:?} {}",
                edge.task, edge.kind, edge.target
            ));
        }
        for edge in &summary.edges_removed {
            lines.push(format!(
                "edge removed: {} {:?} {}",
                edge.task, edge.kind, edge.target
            ));
        }
        lines.extend(
            self.prompts_created
                .iter()
                .map(|task| format!("prompts created for task {task}")),
        );
        lines.extend(self.state_edits.iter().cloned());
        lines
    }

    /// The ids of the tasks the change touches: added, removed, redefined or moved.
    pub fn tasks(&self) -> Vec<String> {
        let mut tasks: Vec<String> = self
            .summary
            .tasks_added
            .iter()
            .chain(&self.summary.tasks_removed)
            .chain(&self.summary.tasks_redefined)
            .chain(self.moves.iter().map(|moved| &moved.task))
            .chain(&self.prompts_created)
            .cloned()
            .collect();
        tasks.sort();
        tasks.dedup();
        tasks
    }
}

fn folder_name(folder: &str) -> &str {
    if folder.is_empty() { "(top)" } else { folder }
}

fn moves_between(old: &DefinitionTree, new: &DefinitionTree) -> Vec<TaskMove> {
    let folders = |tree: &DefinitionTree| -> BTreeMap<String, String> {
        tree.nodes()
            .into_iter()
            .map(|node| (node.definition.id.clone(), node.folder.clone()))
            .collect()
    };
    let (old, new) = (folders(old), folders(new));
    old.into_iter()
        .filter_map(|(task, from)| {
            let to = new.get(&task)?;
            (*to != from).then(|| TaskMove {
                task,
                from,
                to: to.clone(),
            })
        })
        .collect()
}

fn describe_op(op: &StateOp) -> Option<String> {
    Some(match op {
        StateOp::MoveTodo {
            from_task,
            to_task,
            todo_id,
        } => format!("moves todo {todo_id} from task {from_task} to task {to_task}"),
        StateOp::MoveWork { from_task, to_task } => {
            format!("moves the work of task {from_task} to task {to_task}")
        }
        StateOp::MoveFiles { from, to } => {
            format!("moves files {} -> {}", from.display(), to.display())
        }
        StateOp::CreatePrompts { .. } => return None,
        StateOp::WriteFile { path, .. } => format!("writes file {}", path.display()),
        StateOp::Set { task, field, .. } => format!("sets {field} of task {task}"),
        StateOp::SetTodo { task, value, .. } => {
            format!(
                "sets todo {} of task {task}",
                value.id.as_deref().unwrap_or("?")
            )
        }
        StateOp::AppendHistory { task, entry } => {
            format!("adds history entry {:?} to task {task}", entry.event)
        }
        StateOp::Delete { task } => format!("removes the state of task {task}"),
        StateOp::RecordProposal { root, .. } => format!("records a proposal on task {root}"),
    })
}

impl ProjectRepo {
    /// The commits the diff of `row` runs between. A landed change is its commit against that
    /// commit's first parent; any other is its branch tip against where it left `main`. Work not
    /// yet committed on the branch is not in it.
    async fn change_range(&self, row: &ChangeRow) -> Result<(String, String)> {
        let repository = &self.location.repository;
        match row.state {
            ChangeState::Landed => {
                let commit = row
                    .commit
                    .clone()
                    .ok_or_else(|| anyhow!("change {} has no commit", row.change_id))?;
                let parents = self.git.parents_of(repository, &commit).await?;
                let parent = parents
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("{commit} has no parent to compare with"))?;
                Ok((parent, commit))
            }
            ChangeState::Abandoned => bail!("change {} was abandoned and has no diff", row.name),
            ChangeState::Open | ChangeState::Requested => {
                let tip = self.git.rev_parse(repository, &row.branch).await?;
                let base = self.git.merge_base(repository, MAIN_BRANCH, &tip).await?;
                Ok((base, tip))
            }
        }
    }

    fn known_change(&self, change_id: &str) -> Result<ChangeRow> {
        check_change_id(change_id)?;
        self.change(change_id)?
            .ok_or_else(|| anyhow!("there is no change {change_id}"))
    }

    /// The raw diff of the change `change_id`.
    pub async fn raw_diff(&self, change_id: &str) -> Result<String> {
        let row = self.known_change(change_id)?;
        let (from, to) = self.change_range(&row).await?;
        Ok(self
            .git
            .diff_text(&self.location.repository, &from, &to)
            .await?)
    }

    /// The task-aware diff of the change `change_id`.
    pub async fn task_diff(&self, change_id: &str) -> Result<TaskAwareDiff> {
        let row = self.known_change(change_id)?;
        self.task_diff_of(&row).await
    }

    pub(crate) async fn task_diff_of(&self, row: &ChangeRow) -> Result<TaskAwareDiff> {
        let (from, to) = self.change_range(row).await?;
        let repository = &self.location.repository;
        let load = |commit: String| async move {
            load_definition_tree(&self.git, repository, &TreeRef::Commit(CommitId(commit))).await
        };
        let (old, new) = (load(from.clone()).await?, load(to.clone()).await?);
        let mut diff = TaskAwareDiff {
            summary: summarize(&old, &new),
            moves: moves_between(&old, &new),
            ..TaskAwareDiff::default()
        };
        if let Some(ops) = self.read_state(|read| read.staged(&row.change_id))? {
            for op in &ops {
                match op {
                    StateOp::CreatePrompts { task } => diff.prompts_created.push(task.to_string()),
                    other => diff.state_edits.extend(describe_op(other)),
                }
            }
        }
        Ok(diff)
    }

    /// The diff of the commits the amending checks made on the change `change_id`: from the
    /// parent of the first to the last. `commit` must be one of the change's recorded amend
    /// commits (a commit is an amendment because the landing engine recorded it, never because
    /// of its message); the diff runs to the last amendment from the first's parent when the
    /// change has several.
    pub async fn amend_diff(&self, change_id: &str, commit: &str) -> Result<String> {
        let row = self.known_change(change_id)?;
        check_commit_id(commit)?;
        let (first, last) = match row.amend_commits.as_slice() {
            [] => bail!("change {} was not amended by a check", row.name),
            [only] => (only, only),
            [first, .., last] => (first, last),
        };
        if !row.amend_commits.iter().any(|amend| amend.commit == commit) {
            bail!("{commit} is not an amend commit of change {}", row.name);
        }
        let repository = &self.location.repository;
        let parent = self
            .git
            .parents_of(repository, &first.commit)
            .await?
            .into_iter()
            .next()
            .with_context(|| format!("{} has no parent", first.commit))?;
        Ok(self
            .git
            .diff_text(repository, &parent, &last.commit)
            .await?)
    }
}

/// Refuses a change id that could not be one of ours: empty, long, or with a character other
/// than a letter, digit, `-` or `_`. Ids from other processes are checked here before they
/// reach a query, a path or a git command.
pub(crate) fn check_change_id(change_id: &str) -> Result<()> {
    let valid = !change_id.is_empty()
        && change_id.len() <= 64
        && change_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        bail!("{change_id:?} is not a change id")
    }
}
