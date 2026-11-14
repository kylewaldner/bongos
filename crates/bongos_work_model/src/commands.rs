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

mod artifacts;
mod links;
mod projects;
mod structure;
mod tasks;
mod todos;

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

pub use artifacts::*;
pub use links::*;
pub use projects::*;
pub use structure::*;
pub use tasks::*;
pub use todos::*;

use crate::author::{Author, PersonAtKeyboard};
use crate::derive::{self, Shape};
use crate::error::{Error, ErrorKind, Result};
use crate::files::write_atomic;
use crate::io::serialize_history;
use crate::io::{
    parse_history_at, serialize_artifacts, serialize_link, serialize_project, serialize_task,
};
use crate::journal::{self, Journal, Step};
use crate::layout;
use crate::rules::{Place, Refusal};
use crate::store::WorkTree;
use crate::types::TaskId;
use crate::types::{
    ArtifactsFile, Extra, HISTORY_LIMIT, HistoryEntry, HistoryFile, Kind, LinkFile, ProjectFile,
    Status, TaskFile, Timestamp, now,
};

/// Who runs a command and when. `by` is what history entries record (such as `workflow:<name>`,
/// `agent` or `tree-run`); `author` says who wrote the change.
///
/// A context built from a [`PersonAtKeyboard`] holds it as its attestation. A clone never does,
/// and `Debug` and `PartialEq` ignore it.
pub struct Context {
    pub by: String,
    pub at: Timestamp,
    pub author: Author,
    attestation: Option<PersonAtKeyboard>,
}

impl Clone for Context {
    fn clone(&self) -> Self {
        Self {
            by: self.by.clone(),
            at: self.at,
            author: self.author.clone(),
            attestation: None,
        }
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("by", &self.by)
            .field("at", &self.at)
            .field("author", &self.author)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Context {
    fn eq(&self, other: &Self) -> bool {
        self.by == other.by && self.at == other.at && self.author == other.author
    }
}

impl Context {
    /// A context whose author is unattributed, named `by` verbatim.
    pub fn new(by: impl Into<String>, at: Timestamp) -> Self {
        let by = by.into();
        Self {
            author: Author::unattributed(by.clone()),
            by,
            at,
            attestation: None,
        }
    }

    /// A person at the keyboard, now. The marker moves into the context as its attestation.
    pub fn at_keyboard(marker: PersonAtKeyboard) -> Self {
        let name = person_name();
        Self {
            author: Author::person(name.clone()),
            by: name,
            at: now(),
            attestation: Some(marker),
        }
    }

    /// The same context written by `author`; `by` becomes the author's name.
    pub fn with_author(mut self, author: Author) -> Self {
        self.by = author.name.clone();
        self.author = author;
        self
    }

    /// Moves the attestation out, so it is used at most once.
    pub fn take_attestation(&mut self) -> Option<PersonAtKeyboard> {
        self.attestation.take()
    }

    /// Whether the context still holds its attestation.
    pub fn has_attestation(&self) -> bool {
        self.attestation.is_some()
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

/// The name of the person at the keyboard: the OS user name, else "person".
fn person_name() -> String {
    ["USER", "LOGNAME"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "person".to_string())
}

/// The whole new content of one file.
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    /// `task.toml` in `dir`.
    Task { dir: PathBuf, file: TaskFile },
    /// `project.toml` in `dir`.
    Project { dir: PathBuf, file: ProjectFile },
    /// `artifacts.toml` in `dir`.
    Artifacts { dir: PathBuf, file: ArtifactsFile },
    /// A link file (`<name>.link.toml`) at `path`.
    Link { path: PathBuf, file: LinkFile },
}

impl Write {
    pub fn path(&self) -> PathBuf {
        match self {
            Write::Task { dir, .. } => layout::task_file(dir),
            Write::Project { dir, .. } => layout::project_file(dir),
            Write::Artifacts { dir, .. } => layout::artifacts_file(dir),
            Write::Link { path, .. } => path.clone(),
        }
    }

    pub fn content(&self) -> Result<String> {
        match self {
            Write::Task { file, .. } => serialize_task(file),
            Write::Project { file, .. } => serialize_project(file),
            Write::Artifacts { file, .. } => serialize_artifacts(file),
            Write::Link { file, .. } => serialize_link(file),
        }
    }
}

/// A directory (or file) that a multi-file operation renames, with everything under it.
#[derive(Clone, Debug, PartialEq)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// What a command changes.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// The directory of the task the command acted on (for a new task, its new directory; for a
    /// moved task, where it goes).
    pub dir: PathBuf,
    /// The files to replace, the task's own first. A file in a moved directory is named by its
    /// path after the move.
    pub writes: Vec<Write>,
    /// The history entry recorded on that task, also present in its written `task.toml`. A
    /// change that writes no `task.toml` (the artifact commands) records it nowhere.
    pub entry: HistoryEntry,
    /// The renames of a multi-file operation (`split`, `move`, `move to project`, `make one
    /// off`), done before the writes. A source that is gone is skipped.
    pub moves: Vec<Move>,
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
            _ => None,
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
        moves: Vec::new(),
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

/// Writes `change`. One file is replaced with [`write_atomic`]; several files, or any move, go
/// through an `.op.toml` journal (D4), so a crash leaves all of them or none. A `task.toml` holding
/// more than [`HISTORY_LIMIT`] history entries has its oldest moved to `history.old.toml` in the
/// same step.
///
/// Every task the change creates (a `task.toml` that did not exist) gets an empty `prompts.txt`
/// once the change is written (D22).
///
/// The write guard is the command's job; this only writes. Every path must be under `root`, no
/// move may land on a path that exists, and no unfinished journal may sit where the new one goes.
pub fn apply(root: &Path, change: &Change) -> Result<()> {
    let created: Vec<PathBuf> = change
        .writes
        .iter()
        .filter_map(|write| match write {
            Write::Task { dir, .. } => Some(dir),
            _ => None,
        })
        .filter(|dir| {
            layout::task_file(&before_moves(change, dir))
                .symlink_metadata()
                .is_err()
        })
        .cloned()
        .collect();
    match plan(root, change)? {
        Plan::One { path, content } => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
            }
            write_atomic(&path, content.as_bytes()).map_err(|error| Error::io(&path, error))?;
        }
        Plan::Journal { dir, journal } => {
            std::fs::create_dir_all(&dir).map_err(|error| Error::io(&dir, error))?;
            journal::run_operation(root, &dir, &journal)?;
        }
    }
    for dir in created {
        create_prompts(&dir)?;
    }
    Ok(())
}

/// Creates an empty `prompts.txt` in `dir` unless one is there.
pub fn create_prompts(dir: &Path) -> Result<()> {
    let path = layout::prompts_file(dir);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(Error::io(&path, error)),
    }
}

/// How [`apply`] writes a change.
#[derive(Debug)]
pub enum Plan {
    /// One file, replaced atomically.
    One { path: PathBuf, content: String },
    /// A journal to write into `dir` and run.
    Journal { dir: PathBuf, journal: Journal },
}

/// The writes of `change`, and for a multi-file change its journal: every move (creating the
/// target's parent first), then every file. Reads `history.old.toml` files and checks that no move
/// target exists, but writes nothing. Public for the crash tests of other crates, which run a
/// journal step by step.
pub fn plan(root: &Path, change: &Change) -> Result<Plan> {
    let files = rendered(change)?;
    if change.moves.is_empty()
        && let [(path, content)] = files.as_slice()
    {
        return Ok(Plan::One {
            path: path.clone(),
            content: content.clone(),
        });
    }

    let mut steps = Vec::new();
    let mut made = HashSet::new();
    for moved in &change.moves {
        if moved.to.symlink_metadata().is_ok() {
            return Err(Error::new(ErrorKind::InvalidJournal(format!(
                "cannot move {} to {}: the target exists",
                moved.from.display(),
                moved.to.display()
            ))));
        }
        if let Some(parent) = moved.to.parent() {
            let parent = relative(root, parent)?;
            if made.insert(parent.clone()) {
                steps.push(Step::CreateDir { path: parent });
            }
        }
        steps.push(Step::Rename {
            from: relative(root, &moved.from)?,
            to: relative(root, &moved.to)?,
        });
    }
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
            _ => None,
        })
        .collect();
    // The journal of a move goes in the source parent, which no move of this change renames
    // (each moves a task directory out of it, or a directory out of the task). Otherwise it goes
    // in the nearest directory that exists: the task's own, or for a new task its parent's,
    // `tasks/`, the activity, `oneoffs/`, `projects/` or the root. The loader searches all of
    // them for journals.
    let near = change
        .moves
        .first()
        .and_then(|moved| moved.from.parent())
        .unwrap_or(&change.dir);
    let dir = near
        .ancestors()
        .find(|dir| dir.starts_with(root) && dir.is_dir())
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf);
    // A journal left there by an operation that could not finish is never replaced.
    let stuck = layout::op_file(&dir);
    if stuck.symlink_metadata().is_ok() {
        return Err(Error::new(ErrorKind::InvalidJournal(format!(
            "{} holds an unfinished operation; reload to finish it, or fix it by hand, first",
            stuck.display()
        ))));
    }
    let journal = Journal::new(change.entry.event.clone(), ids, steps);
    Ok(Plan::Journal { dir, journal })
}

/// Where the directory `dir`, named by its path after `change`, is before it: inside a moved
/// directory, the matching path under the move's source.
fn before_moves(change: &Change, dir: &Path) -> PathBuf {
    change
        .moves
        .iter()
        .find_map(|moved| {
            dir.strip_prefix(&moved.to)
                .ok()
                .map(|rest| moved.from.join(rest))
        })
        .unwrap_or_else(|| dir.to_path_buf())
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
                // A moved task's `history.old.toml` is still at its source.
                let current = layout::history_old_file(&before_moves(change, dir));
                let mut old = match std::fs::read_to_string(&current) {
                    Ok(text) => parse_history_at(&text, &current)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        HistoryFile::default()
                    }
                    Err(error) => return Err(Error::io(&current, error)),
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
