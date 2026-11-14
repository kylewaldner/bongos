//! Reading definitions: the parsed `definitions/` folder of one commit or checkout
//! ([`DefinitionTree`]), and joining it with state into the tree the Work view serves
//! ([`work_tree_from`]). Pure apart from [`DefinitionTree::parse_dir`], which only reads files,
//! and [`work_tree_from`], which only reads the files folders.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use bongos_work_model::layout::ARTIFACTS_FILE;
use bongos_work_model::slug::slugify;
use bongos_work_model::store::{FileError, Project, Task, WorkTree, collect_files};
use bongos_work_model::{ProjectFile, parse_artifacts, parse_project};

use crate::layout::{PROJECT_FILE, TASK_FILE, TASKS_DIR};
use crate::split::{TaskDefinition, TaskStateRecord, join, reserved_keys_in};

/// A path relative to `definitions/`, with `/` separators, no empty, `.` or `..` parts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(String);

impl RelPath {
    /// `None` when `text` is absolute or names a part that is empty, `.` or `..`.
    pub fn new(text: impl Into<String>) -> Option<Self> {
        let text = text.into();
        let valid = !text.is_empty()
            && text
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        valid.then_some(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The folder holding the file, `""` for a file directly in `definitions/`.
    pub fn parent(&self) -> &str {
        self.0.rsplit_once('/').map_or("", |(parent, _)| parent)
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit_once('/').map_or(&self.0, |(_, name)| name)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a file of `definitions/` could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Relative to `definitions/`.
    pub path: String,
    pub reason: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

/// One task folder: the top group (`folder` empty) or `.../tasks/<slug>`.
#[derive(Clone, Debug, PartialEq)]
pub struct DefinitionNode {
    /// The folder relative to `definitions/`, `""` for the top group.
    pub folder: String,
    pub slug: String,
    pub definition: TaskDefinition,
    /// Child tasks, ordered by slug.
    pub children: Vec<DefinitionNode>,
}

impl DefinitionNode {
    fn walk<'a>(&'a self, nodes: &mut Vec<&'a DefinitionNode>) {
        nodes.push(self);
        for child in &self.children {
            child.walk(nodes);
        }
    }
}

/// The parsed `definitions/` of one commit: `project.toml` and every `task.toml`. Empty (no
/// project, no top group) before anything is loaded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DefinitionTree {
    pub project: Option<ProjectFile>,
    pub top: Option<DefinitionNode>,
}

impl DefinitionTree {
    /// Reads a materialized or checked-out `definitions/` folder. Only `project.toml` and the
    /// `task.toml` files are read; `.git` is skipped.
    pub fn parse_dir(root: &Path) -> Result<Self, Vec<Problem>> {
        let mut files = Vec::new();
        let mut problems = Vec::new();
        read_definition_files(root, "", &mut files, &mut problems);
        if !problems.is_empty() {
            return Err(problems);
        }
        Self::parse_files(files)
    }

    /// Parses the files of `definitions/`, as blobs read from git. Files other than
    /// `project.toml` and the `task.toml` of the task folders are ignored.
    pub fn parse_files(
        files: impl IntoIterator<Item = (RelPath, Vec<u8>)>,
    ) -> Result<Self, Vec<Problem>> {
        let mut problems = Vec::new();
        let mut project = None;
        // Task folder -> its parsed definition.
        let mut tasks: BTreeMap<String, TaskDefinition> = BTreeMap::new();
        let mut folders: BTreeSet<String> = BTreeSet::new();
        for (path, bytes) in files {
            let parent = path.parent().to_string();
            match path.file_name() {
                PROJECT_FILE if parent.is_empty() => match text(&path, &bytes) {
                    Ok(text) => match parse_project(text) {
                        Ok(file) => project = Some(file),
                        Err(error) => problems.push(problem(&path, error.to_string())),
                    },
                    Err(error) => problems.push(error),
                },
                TASK_FILE if is_task_folder(&parent) => {
                    folders.insert(parent.clone());
                    match text(&path, &bytes).and_then(|text| parse_definition(&path, text)) {
                        Ok(definition) => {
                            tasks.insert(parent, definition);
                        }
                        Err(error) => problems.push(error),
                    }
                }
                _ => {}
            }
        }
        if project.is_none() {
            problems.push(Problem {
                path: PROJECT_FILE.to_string(),
                reason: format!("{PROJECT_FILE} is missing"),
            });
        }
        if !folders.contains("") {
            problems.push(Problem {
                path: TASK_FILE.to_string(),
                reason: format!("the top group's {TASK_FILE} is missing"),
            });
        }
        // Every task folder's parent task folder must have a task.toml too.
        for folder in &folders {
            if let Some(parent) = parent_task_folder(folder)
                && !folders.contains(parent)
            {
                problems.push(Problem {
                    path: join_path(parent, TASK_FILE),
                    reason: format!("{TASK_FILE} is missing"),
                });
            }
        }
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for (folder, definition) in &tasks {
            if let Some(other) = seen.insert(&definition.id, folder) {
                problems.push(Problem {
                    path: join_path(folder, TASK_FILE),
                    reason: format!(
                        "duplicate id {}, also used by {}",
                        definition.id,
                        join_path(other, TASK_FILE)
                    ),
                });
            }
        }
        if !problems.is_empty() {
            problems.sort_by(|a, b| a.path.cmp(&b.path));
            problems.dedup();
            return Err(problems);
        }
        let top = build_node("", &mut tasks);
        Ok(Self { project, top })
    }

    /// Every task, depth first from the top group.
    pub fn nodes(&self) -> Vec<&DefinitionNode> {
        let mut nodes = Vec::new();
        if let Some(top) = &self.top {
            top.walk(&mut nodes);
        }
        nodes
    }

    pub fn node(&self, id: &str) -> Option<&DefinitionNode> {
        self.nodes()
            .into_iter()
            .find(|node| node.definition.id == id)
    }

    /// The definition of the task `id`.
    pub fn task(&self, id: &str) -> Option<&TaskDefinition> {
        self.node(id).map(|node| &node.definition)
    }

    /// The ids of the tasks whose definitions differ between `self` and `other`, including tasks
    /// only one of them has.
    pub fn differing_ids(&self, other: &DefinitionTree) -> BTreeSet<String> {
        let mine: BTreeMap<&str, &DefinitionNode> = self
            .nodes()
            .into_iter()
            .map(|node| (node.definition.id.as_str(), node))
            .collect();
        let theirs: BTreeMap<&str, &DefinitionNode> = other
            .nodes()
            .into_iter()
            .map(|node| (node.definition.id.as_str(), node))
            .collect();
        let ids: BTreeSet<&str> = mine.keys().chain(theirs.keys()).copied().collect();
        ids.into_iter()
            .filter(|id| {
                let a = mine.get(id);
                let b = theirs.get(id);
                a.map(|node| (&node.folder, &node.definition))
                    != b.map(|node| (&node.folder, &node.definition))
            })
            .map(str::to_string)
            .collect()
    }
}

fn problem(path: &RelPath, reason: String) -> Problem {
    Problem {
        path: path.to_string(),
        reason,
    }
}

fn text<'a>(path: &RelPath, bytes: &'a [u8]) -> Result<&'a str, Problem> {
    std::str::from_utf8(bytes).map_err(|_| problem(path, "not UTF-8 text".to_string()))
}

fn parse_definition(path: &RelPath, text: &str) -> Result<TaskDefinition, Problem> {
    let definition: TaskDefinition =
        toml::from_str(text).map_err(|error| problem(path, error.message().to_string()))?;
    let reserved = reserved_keys_in(&definition);
    if !reserved.is_empty() {
        return Err(problem(
            path,
            format!(
                "holds state, which belongs to the state store: {}",
                reserved.join(", ")
            ),
        ));
    }
    Ok(definition)
}

/// Whether `folder` is the top group (`""`) or a `tasks/<slug>` folder, at any depth.
fn is_task_folder(folder: &str) -> bool {
    if folder.is_empty() {
        return true;
    }
    let parts: Vec<&str> = folder.split('/').collect();
    parts.len().is_multiple_of(2) && parts.chunks(2).all(|pair| pair[0] == TASKS_DIR)
}

/// The task folder holding the task folder `folder`; `None` for the top group.
fn parent_task_folder(folder: &str) -> Option<&str> {
    if folder.is_empty() {
        return None;
    }
    let without_slug = folder.rsplit_once('/').map_or("", |(rest, _)| rest);
    Some(
        without_slug
            .strip_suffix(TASKS_DIR)
            .map_or("", |rest| rest.trim_end_matches('/')),
    )
}

fn join_path(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_string()
    } else {
        format!("{folder}/{name}")
    }
}

fn build_node(
    folder: &str,
    tasks: &mut BTreeMap<String, TaskDefinition>,
) -> Option<DefinitionNode> {
    let definition = tasks.remove(folder)?;
    let prefix = join_path(folder, TASKS_DIR);
    let child_folders: Vec<String> = tasks
        .keys()
        .filter(|candidate| {
            candidate
                .strip_prefix(&prefix)
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|slug| !slug.contains('/'))
        })
        .cloned()
        .collect();
    let children = child_folders
        .iter()
        .filter_map(|child| build_node(child, tasks))
        .collect();
    let slug = folder.rsplit('/').next().unwrap_or_default().to_string();
    Some(DefinitionNode {
        folder: folder.to_string(),
        slug,
        definition,
        children,
    })
}

/// Collects `project.toml` and every `task.toml` under `dir`, named relative to the root.
fn read_definition_files(
    dir: &Path,
    relative: &str,
    files: &mut Vec<(RelPath, Vec<u8>)>,
    problems: &mut Vec<Problem>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            problems.push(Problem {
                path: relative.to_string(),
                reason: error.to_string(),
            });
            return;
        }
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name == ".git" {
            continue;
        }
        let path = join_path(relative, &name);
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            read_definition_files(&entry.path(), &path, files, problems);
        } else if (name == TASK_FILE || name == PROJECT_FILE)
            && let Some(rel) = RelPath::new(path.clone())
        {
            match std::fs::read(entry.path()) {
                Ok(bytes) => files.push((rel, bytes)),
                Err(error) => problems.push(Problem {
                    path,
                    reason: error.to_string(),
                }),
            }
        }
    }
}

/// The current state of tasks, as the loader reads it.
pub trait StateLookup {
    /// The task's state row; `None` when it has none (it reads as the default: open, todos
    /// open).
    fn task_state(&self, id: &str) -> Result<Option<TaskStateRecord>, String>;
}

impl StateLookup for BTreeMap<String, TaskStateRecord> {
    fn task_state(&self, id: &str) -> Result<Option<TaskStateRecord>, String> {
        Ok(self.get(id).cloned())
    }
}

/// The work tree of one new-format project: each task's definition joined with its current
/// state, its directory the task's files folder `files_root/<id>`, its definition folder
/// relative to `definitions/`, and the mtimes of the files in its files folder (never under a
/// `.git`). The tree's root is `files_root`. A project with no top group yields no project.
pub fn work_tree_from(
    definitions: &DefinitionTree,
    state: &impl StateLookup,
    files_root: &Path,
) -> WorkTree {
    let mut tree = WorkTree {
        root: files_root.to_path_buf(),
        ..WorkTree::default()
    };
    let Some(top) = &definitions.top else {
        return tree;
    };
    let slug = definitions
        .project
        .as_ref()
        .map(|project| slugify(&project.title))
        .filter(|slug| !slug.is_empty())
        .unwrap_or_else(|| slugify(&top.definition.goal));
    let mut task = load_node(top, state, files_root, &mut tree.errors);
    task.slug = slug.clone();
    tree.projects.push(Project {
        slug,
        file: definitions.project.clone(),
        task,
    });
    tree
}

fn load_node(
    node: &DefinitionNode,
    state: &impl StateLookup,
    files_root: &Path,
    errors: &mut Vec<FileError>,
) -> Task {
    let id = &node.definition.id;
    let dir = files_root.join(id);
    let mut badges = Vec::new();
    let record = match state.task_state(id) {
        Ok(record) => record.unwrap_or_default(),
        Err(reason) => {
            badges.push(FileError {
                path: dir.clone(),
                line: None,
                reason: format!("reading the state of task {id}: {reason}"),
            });
            TaskStateRecord::default()
        }
    };
    let file = join(node.definition.clone(), record);

    let mut artifacts = None;
    let mut hashes = BTreeMap::new();
    let artifacts_path = dir.join(ARTIFACTS_FILE);
    match std::fs::read_to_string(&artifacts_path) {
        Ok(text) => {
            hashes.insert(
                ARTIFACTS_FILE.to_string(),
                bongos_work_model::files::content_hash(text.as_bytes()),
            );
            match parse_artifacts(&text) {
                Ok(parsed) => artifacts = Some(parsed),
                Err(error) => badges.push(FileError {
                    path: artifacts_path,
                    line: error.line,
                    reason: error.kind.to_string(),
                }),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => badges.push(FileError {
            path: artifacts_path,
            line: None,
            reason: error.to_string(),
        }),
    }

    let mut files = BTreeMap::new();
    if dir.is_dir() {
        collect_files(&dir, Path::new(""), &mut files, errors);
    }

    let children: Vec<Task> = node
        .children
        .iter()
        .map(|child| load_node(child, state, files_root, errors))
        .collect();
    let mut task = Task {
        slug: node.slug.clone(),
        dir,
        definition: Some(PathBuf::from(&node.folder)),
        pending: None,
        needs_conversion: false,
        file: Some(file),
        artifacts,
        errors: badges,
        broken_edges: Vec::new(),
        files,
        hashes,
        children,
    };
    mark_broken_edges(&mut task.children);
    task
}

/// An edge target that names no sibling is a broken edge.
fn mark_broken_edges(siblings: &mut [Task]) {
    let slugs: BTreeSet<String> = siblings.iter().map(|task| task.slug.clone()).collect();
    for task in siblings {
        let Some(file) = &task.file else {
            continue;
        };
        let mut broken = Vec::new();
        for target in file
            .after
            .iter()
            .map(|edge| &edge.task)
            .chain(file.stacked_on.iter().map(|edge| &edge.task))
        {
            if !slugs.contains(target) && !broken.contains(target) {
                broken.push(target.clone());
            }
        }
        task.broken_edges = broken;
    }
}
