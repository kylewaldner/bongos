//! Every command that changes one task or project, as a pure transformation of the loaded tree.
//!
//! A command takes the tree, a [`Context`] (who acts, and when) and its arguments, and returns a
//! [`Change`]: the new content of each file it replaces, with a history entry appended to the task
//! it acted on. Or it returns the [`Refusal`] the UI shows. Every command calls
//! [`crate::store::Task::check_writable`] on each task whose file it replaces before anything
//! else, so a task with an error badge is never written. Commands read nothing from disk; [`apply`]
//! writes a change with the atomic writers.
//!
//! Tasks are named by id; edge targets that may name no sibling are named by slug. Todos are
//! named by their index in the task's list, and matched by role, never by name.

mod links;
mod projects;
mod tasks;
mod todos;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub use links::*;
pub use projects::*;
pub use tasks::*;
pub use todos::*;

use crate::derive::{self, Shape};
use crate::error::{Error, ErrorKind, Result};
use crate::files::write_atomic;
use crate::io::serialize_history;
use crate::io::{parse_history_at, serialize_project, serialize_task};
use crate::journal::{self, Journal, Step};
use crate::layout;
use crate::rules::{Place, Refusal};
use crate::store::WorkTree;
use crate::types::TaskId;
use crate::types::{
    Extra, HISTORY_LIMIT, HistoryEntry, HistoryFile, Kind, ProjectFile, Status, TaskFile,
    Timestamp, now,
};

/// Who runs a command and when: `by` is `kyle`, `workflow:<name>`, `agent` or `tree-run`.
#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    pub by: String,
    pub at: Timestamp,
}

impl Context {
    pub fn new(by: impl Into<String>, at: Timestamp) -> Self {
        Self { by: by.into(), at }
    }

    /// Kyle, now.
    pub fn kyle() -> Self {
        Self::new("kyle", now())
    }

    fn entry(&self, event: &str, detail: Option<String>) -> HistoryEntry {
        HistoryEntry {
            at: self.at,
            by: self.by.clone(),
            event: event.to_string(),
            detail,
            extra: Extra::new(),
        }
    }
}

/// The whole new content of one file.
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    /// `task.toml` in `dir`.
    Task { dir: PathBuf, file: TaskFile },
    /// `project.toml` in `dir`.
    Project { dir: PathBuf, file: ProjectFile },
}

impl Write {
    pub fn path(&self) -> PathBuf {
        match self {
            Write::Task { dir, .. } => layout::task_file(dir),
            Write::Project { dir, .. } => layout::project_file(dir),
        }
    }

    pub fn content(&self) -> Result<String> {
        match self {
            Write::Task { file, .. } => serialize_task(file),
            Write::Project { file, .. } => serialize_project(file),
        }
    }
}

/// What a command changes.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// The directory of the task the command acted on (for a new task, its new directory).
    pub dir: PathBuf,
    /// The files to replace, the task's own first.
    pub writes: Vec<Write>,
    /// The history entry recorded on that task, also present in its written `task.toml`.
    pub entry: HistoryEntry,
}

impl Change {
    /// The new `task.toml` of the task in `dir`, when the change writes it.
    pub fn task_file(&self, dir: &Path) -> Option<&TaskFile> {
        self.writes.iter().find_map(|write| match write {
            Write::Task { dir: written, file } if written == dir => Some(file),
            _ => None,
        })
    }

    /// The new `project.toml`, when the change writes one.
    pub fn project_file(&self) -> Option<&ProjectFile> {
        self.writes.iter().find_map(|write| match write {
            Write::Project { file, .. } => Some(file),
            Write::Task { .. } => None,
        })
    }

    /// The new `task.toml` of the task the command acted on.
    pub fn file(&self) -> Option<&TaskFile> {
        self.task_file(&self.dir)
    }
}

/// A command's result: the change to write, or why it was refused.
pub type Outcome = std::result::Result<Change, Refusal>;

/// The task with `id`, after its write guard, and a copy of its file to change.
fn writable<'a>(
    tree: &'a WorkTree,
    id: &str,
) -> std::result::Result<(Place<'a>, TaskFile), Refusal> {
    let place = Place::locate(tree, id)?;
    place
        .task
        .check_writable()
        .map_err(|refused| refused.to_string())?;
    let file = place
        .task
        .file
        .clone()
        .ok_or_else(|| format!("{} has no task.toml", place.slug()))?;
    Ok((place, file))
}

/// A change to one task's `task.toml`, with `event` recorded in its history.
fn single(
    ctx: &Context,
    place: &Place,
    mut file: TaskFile,
    event: &str,
    detail: Option<String>,
) -> Change {
    let entry = ctx.entry(event, detail);
    file.history.push(entry.clone());
    let dir = place.task.dir.clone();
    Change {
        dir: dir.clone(),
        writes: vec![Write::Task { dir, file }],
        entry,
    }
}

/// Runs `change` on the task with `id`: locate it, check its write guard, let `change` edit a copy
/// of its file and name the event, then record that event.
fn edit_task(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    change: impl FnOnce(
        &Place,
        &mut TaskFile,
    ) -> std::result::Result<(&'static str, Option<String>), Refusal>,
) -> Outcome {
    let (place, mut file) = writable(tree, id)?;
    let (event, detail) = change(&place, &mut file)?;
    Ok(single(ctx, &place, file, event, detail))
}

/// Refuses on a group, which has no `what` (todos, result).
fn require_leaf(place: &Place, tree: &WorkTree, what: &str) -> std::result::Result<(), Refusal> {
    if derive::shape(place.task, tree) == Shape::Group {
        Err(format!(
            "{} is a group, and a group has no {what}",
            place.slug()
        ))
    } else {
        Ok(())
    }
}

/// Refuses on a task that is done or cancelled.
fn require_open(slug: &str, file: &TaskFile) -> std::result::Result<(), Refusal> {
    match file.status {
        Status::Open => Ok(()),
        Status::Done => Err(format!("{slug} is done; reopen it first")),
        Status::Cancelled => Err(format!("{slug} is cancelled; reopen it first")),
    }
}

pub fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::PrShip => "pr-ship",
        Kind::PrMerge => "pr-merge",
        Kind::Document => "document",
        Kind::Spike => "spike",
        Kind::Babysit => "babysit",
        Kind::Other => "other",
    }
}

/// Writes `change`. One file is replaced with [`write_atomic`]; several go through an `.op.toml`
/// journal (D4), so a crash leaves all of them or none. A `task.toml` holding more than
/// [`HISTORY_LIMIT`] history entries has its oldest moved to `history.old.toml` in the same step.
///
/// The write guard is the command's job; this only writes. Every path must be under `root`.
pub fn apply(root: &Path, change: &Change) -> Result<()> {
    let files = rendered(change)?;
    if let [(path, content)] = files.as_slice() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        }
        return write_atomic(path, content.as_bytes()).map_err(|error| Error::io(path, error));
    }

    let mut steps = Vec::new();
    let mut made = HashSet::new();
    for (path, content) in files {
        if let Some(parent) = path.parent() {
            let parent = relative(root, parent)?;
            if made.insert(parent.clone()) {
                steps.push(Step::CreateDir { path: parent });
            }
        }
        steps.push(Step::WriteFile {
            path: relative(root, &path)?,
            content,
        });
    }
    let ids: Vec<TaskId> = change
        .writes
        .iter()
        .filter_map(|write| match write {
            Write::Task { file, .. } => Some(file.id.clone()),
            Write::Project { .. } => None,
        })
        .collect();
    // The journal goes in the nearest directory that exists: the task's own, or for a new task
    // its parent's, `tasks/`, the activity, `oneoffs/`, `projects/` or the root, all of which the
    // loader searches for journals.
    let journal_dir = change
        .dir
        .ancestors()
        .find(|dir| dir.starts_with(root) && dir.is_dir())
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf);
    std::fs::create_dir_all(&journal_dir).map_err(|error| Error::io(&journal_dir, error))?;
    let journal = Journal::new(change.entry.event.clone(), ids, steps);
    journal::run_operation(root, &journal_dir, &journal)
}

/// The path and content of every file `change` writes, with history beyond the limit moved out.
fn rendered(change: &Change) -> Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();
    for write in &change.writes {
        match write {
            Write::Task { dir, file } if file.history.len() > HISTORY_LIMIT => {
                let mut file = file.clone();
                let overflow = file.history.len() - HISTORY_LIMIT;
                let moved: Vec<HistoryEntry> = file.history.drain(..overflow).collect();
                let old_path = layout::history_old_file(dir);
                let mut old = match std::fs::read_to_string(&old_path) {
                    Ok(text) => parse_history_at(&text, &old_path)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        HistoryFile::default()
                    }
                    Err(error) => return Err(Error::io(&old_path, error)),
                };
                old.history.extend(moved);
                files.push((old_path, serialize_history(&old)?));
                files.push((layout::task_file(dir), serialize_task(&file)?));
            }
            _ => files.push((write.path(), write.content()?)),
        }
    }
    Ok(files)
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(root)
        .ok()
        .and_then(|relative| relative.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            Error::new(ErrorKind::InvalidJournal(format!(
                "{} is not a UTF-8 path under the work root {}",
                path.display(),
                root.display()
            )))
        })
}
