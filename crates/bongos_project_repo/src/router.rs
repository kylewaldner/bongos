//! The router: divides a command's [`Change`] on a new-format project into definition
//! operations (which need a working copy and a landing), state operations (one transaction) and
//! writes into the files folders (`files/<task id>/`).
//!
//! Commands run against the tree the loader built, where a task's directory is its files folder
//! `files/<id>/`. A path under a task's directory maps to that task by id: inside the definitions
//! to the task's definition folder, inside the files to `files/<id>/`. A new task's directory
//! (`<parent dir>/tasks/<slug>`, from the nested layout) maps to `<parent definition
//! folder>/tasks/<slug>`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use bongos_project_repo_model::layout::{PROJECT_FILE, TASK_FILE};
use bongos_project_repo_model::{Author, RelPath, TaskStateRecord, TodoStateRecord, split};
use bongos_work_model::commands::{Change, Write, create_prompts};
use bongos_work_model::files::write_atomic;
use bongos_work_model::layout::ARTIFACTS_FILE;
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{
    ProjectFile, TaskFile, TaskId, new_id, serialize_artifacts, serialize_link, serialize_project,
};
use serde::{Deserialize, Serialize};

use crate::state::StateWriter;
use crate::state_ops::{SETTABLE_FIELDS, StateOp, move_work, work_of};

/// A change to the files of `definitions/`, relative to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DefinitionOp {
    Write {
        path: RelPath,
        bytes: Vec<u8>,
    },
    /// A task folder (relative to `definitions/`) and everything in it.
    Move {
        from: String,
        to: String,
    },
    Delete {
        path: RelPath,
    },
}

/// A change in the files folders, relative to the files root (`<task id>/...`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileOp {
    Write {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    Move {
        from: PathBuf,
        to: PathBuf,
    },
    /// An empty `prompts.txt` in a new task's files folder.
    CreatePrompts {
        task: TaskId,
    },
}

/// Where a change goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Routed {
    /// Only state and files: one transaction, no working copy, no commit.
    StateOnly {
        state: Vec<StateOp>,
        files: Vec<FileOp>,
    },
    /// The definitions change: the definition ops land through a working copy, and the state
    /// ops are staged until they do.
    Definition {
        definition: Vec<DefinitionOp>,
        state: Vec<StateOp>,
        files: Vec<FileOp>,
    },
}

/// One known task directory, by its files folder.
struct Place {
    dir: PathBuf,
    id: TaskId,
    /// Relative to `definitions/`.
    folder: String,
}

struct Places(Vec<Place>);

impl Places {
    fn new(tree: &WorkTree) -> Self {
        let places = tree
            .tasks()
            .into_iter()
            .filter_map(|task| {
                Some(Place {
                    dir: task.dir.clone(),
                    id: task.id()?.to_string(),
                    folder: task.definition.as_ref()?.to_str()?.to_string(),
                })
            })
            .collect();
        Self(places)
    }

    /// The place whose directory is the longest prefix of `path`, and the rest of `path`.
    fn owner<'a>(&'a self, path: &'a Path) -> Option<(&'a Place, &'a Path)> {
        self.0
            .iter()
            .filter_map(|place| Some((place, path.strip_prefix(&place.dir).ok()?)))
            .min_by_key(|(_, rest)| rest.components().count())
    }

    /// The definition folder a task directory maps to.
    fn folder_of(&self, dir: &Path) -> Result<String, String> {
        let (place, rest) = self
            .owner(dir)
            .ok_or_else(|| format!("{} is not in a new-format project", dir.display()))?;
        Ok(join_folder(&place.folder, rest))
    }

    /// The path under the files root a path maps to: `<id>/<rest>`. A path that would leave
    /// the task's files folder (a `..` part) is refused.
    fn files_path(&self, path: &Path) -> Result<PathBuf, String> {
        let (place, rest) = self
            .owner(path)
            .ok_or_else(|| format!("{} is not in a new-format project", path.display()))?;
        if !is_plain(rest) || !is_plain(Path::new(&place.id)) {
            return Err(format!("{} is not a files path", path.display()));
        }
        Ok(Path::new(&place.id).join(rest))
    }
}

/// Whether `path` is relative and has only plain parts (no `..`, `.` or root).
pub(crate) fn is_plain(path: &Path) -> bool {
    path.components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
}

/// `folder` (relative to `definitions/`) as a definitions path, refused when it is empty or
/// leaves `definitions/`.
fn checked_folder(folder: String) -> Result<String, String> {
    RelPath::new(folder.clone())
        .map(|_| folder.clone())
        .ok_or_else(|| format!("{folder} is not a definitions folder"))
}

fn join_folder(folder: &str, rest: &Path) -> String {
    let rest: Vec<String> = rest
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let mut parts: Vec<&str> = Vec::new();
    if !folder.is_empty() {
        parts.push(folder);
    }
    parts.extend(rest.iter().map(String::as_str));
    parts.join("/")
}

fn file_in(folder: &str, name: &str) -> Result<RelPath, String> {
    let path = if folder.is_empty() {
        name.to_string()
    } else {
        format!("{folder}/{name}")
    };
    RelPath::new(path.clone()).ok_or_else(|| format!("{path} is not a definitions path"))
}

/// Divides `change`, computed by a command on `tree`, into definition ops, state ops and file
/// ops. Every task the change writes must be in a new-format project of `tree`, or new under
/// one. Refuses with a reason the UI shows.
pub fn route(tree: &WorkTree, change: &Change) -> Result<Routed, String> {
    let mut places = Places::new(tree);
    let mut definition = Vec::new();
    let mut state = Vec::new();
    let mut files = Vec::new();
    let mut task_states = Vec::new();

    // New tasks first, parents before children, so a child's directory maps through its
    // parent's.
    let mut new_tasks: Vec<(&PathBuf, &TaskFile)> = change
        .writes
        .iter()
        .filter_map(|write| match write {
            Write::Task { dir, file } if tree.task_by_id(&file.id).is_none() => Some((dir, file)),
            _ => None,
        })
        .collect();
    new_tasks.sort_by_key(|(dir, _)| dir.components().count());
    for (dir, file) in new_tasks {
        let folder = places.folder_of(dir)?;
        places.0.push(Place {
            dir: dir.clone(),
            id: file.id.clone(),
            folder,
        });
    }

    // Moved tasks, outer before inner, so a path at a task's new place maps to the task's own
    // files folder, never through its new parent's.
    let mut moved_tasks: Vec<(&PathBuf, &str)> = change
        .moves
        .iter()
        .filter_map(|moved| {
            let task = tree.task_by_dir(&moved.from)?;
            task.definition.as_ref()?;
            Some((&moved.to, task.id()?))
        })
        .collect();
    moved_tasks.sort_by_key(|(to, _)| to.components().count());
    for (to, id) in moved_tasks {
        let folder = places.folder_of(to)?;
        places.0.push(Place {
            dir: to.clone(),
            id: id.to_string(),
            folder,
        });
    }

    for write in &change.writes {
        match write {
            Write::Task { dir, file } => {
                let mut file = file.clone();
                for todo in &mut file.todos {
                    if todo.id.is_none() {
                        todo.id = Some(new_id());
                    }
                }
                let old = tree.task_by_id(&file.id);
                let folder = match old {
                    Some(task) if task.dir == *dir => task
                        .definition
                        .as_ref()
                        .and_then(|folder| folder.to_str())
                        .map(str::to_string)
                        .ok_or_else(|| format!("{} is in the old format", task.slug))?,
                    _ => places.folder_of(dir)?,
                };
                let (new_definition, new_state) = split(&file);
                let (old_definition, old_state) = match old.and_then(|task| task.file.as_ref()) {
                    Some(old_file) => {
                        let (definition, state) = split(old_file);
                        (Some(definition), state)
                    }
                    None => (None, TaskStateRecord::default()),
                };
                if old.is_none() {
                    files.push(FileOp::CreatePrompts {
                        task: file.id.clone(),
                    });
                }
                if old_definition.as_ref() != Some(&new_definition) {
                    let bytes = toml::to_string(&new_definition)
                        .map_err(|error| format!("writing the definition: {error}"))?;
                    definition.push(DefinitionOp::Write {
                        path: file_in(&folder, TASK_FILE)?,
                        bytes: bytes.into_bytes(),
                    });
                }
                task_states.push((file.id.clone(), old_state, new_state));
            }
            Write::Project { dir, file } => {
                if project_of(tree, dir) != Some(file) {
                    let text = serialize_project(file).map_err(|error| error.to_string())?;
                    definition.push(DefinitionOp::Write {
                        path: file_in("", PROJECT_FILE)?,
                        bytes: text.into_bytes(),
                    });
                }
            }
            Write::Artifacts { dir, file } => {
                let text = serialize_artifacts(file).map_err(|error| error.to_string())?;
                files.push(FileOp::Write {
                    path: places.files_path(&dir.join(ARTIFACTS_FILE))?,
                    bytes: text.into_bytes(),
                });
            }
            Write::Link { path, file } => {
                let text = serialize_link(file).map_err(|error| error.to_string())?;
                files.push(FileOp::Write {
                    path: places.files_path(path)?,
                    bytes: text.into_bytes(),
                });
            }
        }
    }

    state.extend(state_ops(task_states)?);

    for moved in &change.moves {
        let task_folder = tree
            .task_by_dir(&moved.from)
            .and_then(|task| task.definition.as_ref());
        match task_folder {
            Some(from) => {
                let from = checked_folder(from.to_string_lossy().into_owned())?;
                let to = checked_folder(places.folder_of(&moved.to)?)?;
                if from != to {
                    definition.push(DefinitionOp::Move { from, to });
                }
            }
            None => files.push(FileOp::Move {
                from: places.files_path(&moved.from)?,
                to: places.files_path(&moved.to)?,
            }),
        }
    }

    Ok(if definition.is_empty() {
        Routed::StateOnly { state, files }
    } else {
        Routed::Definition {
            definition,
            state,
            files,
        }
    })
}

pub(crate) fn project_of<'a>(tree: &'a WorkTree, dir: &Path) -> Option<&'a ProjectFile> {
    tree.projects
        .iter()
        .find(|project| project.task.dir == dir)
        .and_then(|project| project.file.as_ref())
}

fn state_table(id: &str, record: &TaskStateRecord) -> Result<toml::Table, String> {
    match toml::Value::try_from(record) {
        Ok(toml::Value::Table(table)) => Ok(table),
        Ok(_) => Err(format!("the state of {id} is not a table")),
        Err(error) => Err(format!("reading the state of {id}: {error}")),
    }
}

fn has_todo(record: &TaskStateRecord, todo_id: &str) -> bool {
    record
        .todos
        .iter()
        .any(|todo| todo.id.as_deref() == Some(todo_id))
}

/// The state ops of the written tasks, each given as (id, state as the command read it, new
/// state): first the moves between them, a `MoveTodo` for a todo that left one written task for
/// another and a `MoveWork` for the work a split hands to its keeper, so they move whatever the
/// rows hold when applied; then each task's own diff against its state with those moves made, so
/// a moved value is never also set (and a value changed in the move is set only if it is still
/// the one moved).
fn state_ops(
    mut states: Vec<(TaskId, TaskStateRecord, TaskStateRecord)>,
) -> Result<Vec<StateOp>, String> {
    let mut ops = Vec::new();
    for to in 0..states.len() {
        let arrived: Vec<String> = states[to]
            .2
            .todos
            .iter()
            .filter_map(|todo| todo.id.clone())
            .filter(|todo_id| !has_todo(&states[to].1, todo_id))
            .collect();
        for todo_id in arrived {
            let from = (0..states.len()).find(|&from| {
                from != to
                    && has_todo(&states[from].1, &todo_id)
                    && !has_todo(&states[from].2, &todo_id)
            });
            let Some(from) = from else {
                continue;
            };
            let old = &mut states[from].1.todos;
            let Some(position) = old
                .iter()
                .position(|todo| todo.id.as_deref() == Some(todo_id.as_str()))
            else {
                continue;
            };
            let record = old.remove(position);
            states[to].1.todos.push(record);
            ops.push(StateOp::MoveTodo {
                from_task: states[from].0.clone(),
                to_task: states[to].0.clone(),
                todo_id,
            });
        }
    }
    let none = TaskStateRecord::default();
    for from in 0..states.len() {
        let work = work_of(&states[from].1);
        if work == none || work_of(&states[from].2) != none {
            continue;
        }
        let to = (0..states.len()).find(|&to| {
            to != from && work_of(&states[to].1) == none && work_of(&states[to].2) == work
        });
        let Some(to) = to else {
            continue;
        };
        let mut from_old = states[from].1.clone();
        move_work(&mut from_old, &mut states[to].1);
        states[from].1 = from_old;
        ops.push(StateOp::MoveWork {
            from_task: states[from].0.clone(),
            to_task: states[to].0.clone(),
        });
    }
    for (id, old, new) in &states {
        ops.extend(diff_state(id, old, new)?);
    }
    Ok(ops)
}

/// The ops that turn the state `old` (as the command read it) into `new`: a `Set` per changed
/// field, a `SetTodo` per changed todo state and an `AppendHistory` per new history entry.
fn diff_state(
    id: &str,
    old: &TaskStateRecord,
    new: &TaskStateRecord,
) -> Result<Vec<StateOp>, String> {
    let mut ops = Vec::new();
    let old_table = state_table(id, old)?;
    let new_table = state_table(id, new)?;
    for field in SETTABLE_FIELDS {
        let before = old_table.get(field);
        let after = new_table.get(field);
        if before != after {
            ops.push(StateOp::Set {
                task: id.to_string(),
                field: field.to_string(),
                expected: before.cloned(),
                value: after.cloned(),
            });
        }
    }
    let old_todos: BTreeMap<&str, &TodoStateRecord> = old
        .todos
        .iter()
        .filter_map(|todo| Some((todo.id.as_deref()?, todo)))
        .collect();
    for todo in &new.todos {
        let Some(todo_id) = todo.id.as_deref() else {
            continue;
        };
        let before = old_todos.get(todo_id).copied();
        if before != Some(todo) {
            ops.push(StateOp::SetTodo {
                task: id.to_string(),
                expected: before.cloned(),
                value: todo.clone(),
            });
        }
    }
    let kept = old.history.len().min(new.history.len());
    if new.history[..kept] != old.history[..kept] {
        return Err(format!(
            "the history of {id} was rewritten, not appended to"
        ));
    }
    for entry in &new.history[kept..] {
        ops.push(StateOp::AppendHistory {
            task: id.to_string(),
            entry: entry.clone(),
        });
    }
    Ok(ops)
}

/// Applies a state-only change: its state ops in one transaction, labeled with `origin`, then
/// its file ops under `files_root`. Makes no working copy and no commit.
pub fn apply_state_only(
    writer: &StateWriter,
    files_root: &Path,
    state: &[StateOp],
    files: &[FileOp],
    origin: &Author,
) -> Result<()> {
    if !state.is_empty() {
        let notes = writer.transact_with(origin, state, |tx| tx.apply_ops(state))?;
        for note in notes {
            log::info!("{note}");
        }
    }
    for op in files {
        apply_file_op(files_root, op)?;
    }
    Ok(())
}

/// The file ops among staged `ops`, which wait for the landing.
pub(crate) fn staged_file_ops(ops: &[StateOp]) -> Vec<FileOp> {
    ops.iter()
        .filter_map(|op| match op {
            StateOp::MoveFiles { from, to } => Some(FileOp::Move {
                from: from.clone(),
                to: to.clone(),
            }),
            StateOp::CreatePrompts { task } => Some(FileOp::CreatePrompts { task: task.clone() }),
            StateOp::WriteFile { path, bytes } => Some(FileOp::Write {
                path: path.clone(),
                bytes: bytes.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The staged op that waits for the landing to make `op`.
pub(crate) fn staged_file_op(op: &FileOp) -> StateOp {
    match op {
        FileOp::Move { from, to } => StateOp::MoveFiles {
            from: from.clone(),
            to: to.clone(),
        },
        FileOp::CreatePrompts { task } => StateOp::CreatePrompts { task: task.clone() },
        FileOp::Write { path, bytes } => StateOp::WriteFile {
            path: path.clone(),
            bytes: bytes.clone(),
        },
    }
}

/// Refuses a move of files that would leave the files root (a `..` part, an absolute path) or
/// overwrite what is at the destination. A source that is not there is not refused: there is
/// nothing to move (or the move was done).
pub(crate) fn check_file_move(files_root: &Path, from: &Path, to: &Path) -> Result<()> {
    if !is_plain(from) || !is_plain(to) || from.as_os_str().is_empty() || to.as_os_str().is_empty()
    {
        anyhow::bail!(
            "{} to {} is not a move inside the files folders",
            from.display(),
            to.display()
        );
    }
    if files_root.join(from).symlink_metadata().is_ok()
        && files_root.join(to).symlink_metadata().is_ok()
    {
        anyhow::bail!("{} already exists", to.display());
    }
    Ok(())
}

/// Refuses a file op that waits for a landing when it would leave the files root or overwrite.
pub(crate) fn check_file_op(files_root: &Path, op: &FileOp) -> Result<()> {
    match op {
        FileOp::Move { from, to } => check_file_move(files_root, from, to),
        FileOp::CreatePrompts { task } => {
            let path = Path::new(task);
            if !is_plain(path) || path.components().count() != 1 {
                anyhow::bail!("{task} is not a task id");
            }
            Ok(())
        }
        FileOp::Write { path, .. } => {
            if !is_plain(path) || path.as_os_str().is_empty() {
                anyhow::bail!("{} is not a files path", path.display());
            }
            Ok(())
        }
    }
}

/// Makes each op of `ops` that passes [`check_file_op`], one at a time: one that fails does not
/// stop the others. Returns the ops that failed, each with why. Running it again changes nothing.
pub(crate) fn run_file_ops(files_root: &Path, ops: &[FileOp]) -> Vec<(FileOp, String)> {
    ops.iter()
        .filter_map(|op| {
            check_file_op(files_root, op)
                .and_then(|()| apply_file_op(files_root, op))
                .err()
                .map(|error| (op.clone(), format!("{error:#}")))
        })
        .collect()
}

pub(crate) fn apply_file_op(files_root: &Path, op: &FileOp) -> Result<()> {
    match op {
        FileOp::Write { path, bytes } => {
            let path = files_root.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            write_atomic(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        }
        FileOp::Move { from, to } => {
            let from = files_root.join(from);
            let to = files_root.join(to);
            if from.symlink_metadata().is_err() {
                return Ok(());
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        }
        FileOp::CreatePrompts { task } => {
            let dir = files_root.join(task);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            create_prompts(&dir).map_err(|error| anyhow::anyhow!("{error}"))?;
        }
    }
    Ok(())
}

/// Whether `change` writes anything in the new-format project `project_id` of `tree`: a task,
/// a path under one of its tasks' files folders, or a new task under one.
pub fn owns(tree: &WorkTree, project_id: &str, change: &Change) -> bool {
    let Some(project) = tree.projects.iter().find(|project| {
        project.task.definition.is_some()
            && project.file.as_ref().and_then(|file| file.id.as_deref()) == Some(project_id)
    }) else {
        return false;
    };
    let dirs: Vec<&Path> = project
        .task
        .walk()
        .into_iter()
        .map(|task: &Task| task.dir.as_path())
        .collect();
    let mut paths = vec![change.dir.clone()];
    for write in &change.writes {
        paths.push(match write {
            Write::Task { dir, .. } | Write::Project { dir, .. } | Write::Artifacts { dir, .. } => {
                dir.clone()
            }
            Write::Link { path, .. } => path.clone(),
        });
    }
    for moved in &change.moves {
        paths.push(moved.from.clone());
        paths.push(moved.to.clone());
    }
    paths
        .iter()
        .any(|path| dirs.iter().any(|dir| path.starts_with(dir)))
}

/// `tree` with each new-format task's directory set to its definition folder inside `root` (a
/// working copy's `definitions/`), so the path arithmetic of the nested layout yields
/// working-copy paths.
pub fn project_into(tree: &WorkTree, root: &Path) -> WorkTree {
    let mut projected = tree.clone();
    for project in &mut projected.projects {
        project.task.walk_mut(&mut |task| {
            if let Some(folder) = &task.definition {
                task.dir = root.join(folder);
            }
        });
    }
    projected
}
