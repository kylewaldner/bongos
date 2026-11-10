//! Loading the work tree from disk.
//!
//! [`load`] first finishes any `.op.toml` journal left by an interrupted multi-file operation, then
//! reads `projects/<slug>/` and `oneoffs/<activity>/<YYYY-MM-DD>-<slug>/` with their nested
//! `tasks/<slug>/` directories, recording the mtime of every file each task owns.
//!
//! Invalid files are never repaired or overwritten. When `task.toml`, `project.toml` or
//! `artifacts.toml` fails to parse, or `task.toml` breaks a structural rule (a cycle over the
//! sibling edges), the task keeps the last good parse from the previous tree given to [`reload`],
//! gets an error badge naming the file, line and reason, and [`Task::check_writable`] refuses every
//! write to it until the file is valid again. An edge to a missing sibling is a broken edge, not an
//! error, because an agent may create the sibling a moment later.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::Error;
use crate::files::content_hash;
use crate::io::{parse_artifacts_at, parse_project_at, parse_task_at};
use crate::journal::finish_journal;
use crate::layout::{self, OP_FILE, TASKS_DIR, is_temp_file};
use crate::types::{ArtifactsFile, ProjectFile, TaskFile};

/// A problem with one file, shown as an error badge: the file, the line when known, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileError {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub reason: String,
}

impl FileError {
    fn from_error(path: &Path, error: &Error) -> Self {
        Self {
            path: error.path.clone().unwrap_or_else(|| path.to_path_buf()),
            line: error.line,
            reason: error.kind.to_string(),
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        write!(f, ": {}", self.reason)
    }
}

/// A write refused because the task has an error badge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteRefused {
    pub task_dir: PathBuf,
    pub errors: Vec<FileError>,
}

impl fmt::Display for WriteRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} cannot be changed until its files are fixed: ",
            self.task_dir.display()
        )?;
        for (index, error) in self.errors.iter().enumerate() {
            if index > 0 {
                write!(f, "; ")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WriteRefused {}

/// One task directory: a project's top group, a one-off, or a nested task under `tasks/`.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    /// The directory name: a slug, or `<YYYY-MM-DD>-<slug>` for a one-off.
    pub slug: String,
    pub dir: PathBuf,
    /// The last good parse of `task.toml`; `None` when it has never parsed.
    pub file: Option<TaskFile>,
    /// The last good parse of `artifacts.toml`; `None` when it is absent or has never parsed.
    pub artifacts: Option<ArtifactsFile>,
    /// Error badges. Any badge refuses writes to this task.
    pub errors: Vec<FileError>,
    /// Edge targets (in `after` or `stacked_on`) that name no sibling.
    pub broken_edges: Vec<String>,
    /// The mtime of every file the task owns, relative to `dir`: its own files and those under
    /// `chats/`, `runs/` and any other subdirectory, but not `tasks/`, `.git/`, temporary files or
    /// a journal.
    pub files: BTreeMap<PathBuf, SystemTime>,
    /// The [`content_hash`] of each model file (`task.toml`, `artifacts.toml`, and for a
    /// project's top group `project.toml`) exactly as the loader read it, by file name, whether or
    /// not it parsed. A file that was missing has no entry. A writer compares these with the files
    /// on disk to tell whether someone changed them since this parse.
    pub hashes: BTreeMap<String, u64>,
    pub children: Vec<Task>,
}

impl Task {
    pub fn id(&self) -> Option<&str> {
        self.file.as_ref().map(|file| file.id.as_str())
    }

    /// The goal from the last good parse, or an empty string.
    pub fn goal(&self) -> &str {
        self.file.as_ref().map_or("", |file| file.goal.as_str())
    }

    /// The goal as lists and headers name the task: `Goal (SEQ-121)` for a task with a `linear`
    /// identifier (synced or hand-set, as the row shows it), the bare goal otherwise.
    pub fn named_goal(&self) -> String {
        let goal = self.goal();
        match self.file.as_ref().and_then(|file| file.linear.as_deref()) {
            Some(identifier) if goal.is_empty() => identifier.to_string(),
            Some(identifier) => format!("{goal} ({identifier})"),
            None => goal.to_string(),
        }
    }

    /// The write guard every command calls before changing this task's files.
    pub fn check_writable(&self) -> Result<(), WriteRefused> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(WriteRefused {
                task_dir: self.dir.clone(),
                errors: self.errors.clone(),
            })
        }
    }

    /// The hash of the model file `name` (such as `task.toml`) as last read, `None` when it was
    /// missing.
    pub fn read_hash(&self, name: &str) -> Option<u64> {
        self.hashes.get(name).copied()
    }

    /// The newest mtime among the task's own files.
    pub fn newest_mtime(&self) -> Option<SystemTime> {
        self.files.values().max().copied()
    }

    /// This task and every descendant, depth first.
    pub fn walk(&self) -> Vec<&Task> {
        let mut tasks = vec![self];
        for child in &self.children {
            tasks.extend(child.walk());
        }
        tasks
    }

    fn walk_mut(&mut self, visit: &mut impl FnMut(&mut Task)) {
        visit(self);
        for child in &mut self.children {
            child.walk_mut(visit);
        }
    }
}

/// `projects/<slug>/`: `project.toml` and the top group's `task.toml`. Errors in `project.toml`
/// are badges on [`Project::task`], so its write guard covers both files.
#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub slug: String,
    /// The last good parse of `project.toml`.
    pub file: Option<ProjectFile>,
    pub task: Task,
}

/// `oneoffs/<activity>/`: the one-offs of one activity.
#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub slug: String,
    pub dir: PathBuf,
    pub oneoffs: Vec<Task>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkTree {
    pub root: PathBuf,
    pub projects: Vec<Project>,
    pub activities: Vec<Activity>,
    /// Problems not tied to one task's own files, such as a journal that could not be finished or
    /// a directory that could not be read. A journal problem is also a badge on the task whose
    /// directory holds the journal.
    pub errors: Vec<FileError>,
}

impl WorkTree {
    /// Every task in the tree: project top groups, one-offs and their descendants.
    pub fn tasks(&self) -> Vec<&Task> {
        let mut tasks = Vec::new();
        for project in &self.projects {
            tasks.extend(project.task.walk());
        }
        for activity in &self.activities {
            for oneoff in &activity.oneoffs {
                tasks.extend(oneoff.walk());
            }
        }
        tasks
    }

    pub fn task_by_id(&self, id: &str) -> Option<&Task> {
        self.tasks().into_iter().find(|task| task.id() == Some(id))
    }

    pub fn task_by_dir(&self, dir: &Path) -> Option<&Task> {
        self.tasks().into_iter().find(|task| task.dir == dir)
    }

    fn tasks_mut(&mut self, visit: &mut impl FnMut(&mut Task)) {
        for project in &mut self.projects {
            project.task.walk_mut(visit);
        }
        for activity in &mut self.activities {
            for oneoff in &mut activity.oneoffs {
                oneoff.walk_mut(visit);
            }
        }
    }
}

/// Loads the tree under `root` with no earlier parse to fall back on.
pub fn load(root: &Path) -> WorkTree {
    load_with(root, None)
}

/// Loads the tree under `root` again. A task whose file is now invalid keeps its parse from
/// `previous`, matched by directory.
pub fn reload(root: &Path, previous: &WorkTree) -> WorkTree {
    load_with(root, Some(previous))
}

fn load_with(root: &Path, previous: Option<&WorkTree>) -> WorkTree {
    let mut errors = Vec::new();
    let failed_journals = finish_journals(root, &mut errors);

    let previous = Previous::new(previous);
    let mut tree = WorkTree {
        root: root.to_path_buf(),
        projects: Vec::new(),
        activities: Vec::new(),
        errors,
    };

    for (slug, dir) in subdirs(&layout::projects_dir(root), &mut tree.errors) {
        tree.projects
            .push(load_project(slug, dir, &previous, &mut tree.errors));
    }
    for (slug, dir) in subdirs(&layout::oneoffs_dir(root), &mut tree.errors) {
        let mut oneoffs = Vec::new();
        for (name, oneoff_dir) in subdirs(&dir, &mut tree.errors) {
            oneoffs.push(load_task(name, oneoff_dir, &previous, &mut tree.errors));
        }
        check_siblings(&mut oneoffs, &previous);
        tree.activities.push(Activity { slug, dir, oneoffs });
    }

    mark_duplicate_ids(&mut tree);
    for (journal_dir, error) in failed_journals {
        // The task whose directory, or whose `tasks/` directory, holds the journal.
        let owner = if journal_dir
            .file_name()
            .is_some_and(|name| name == TASKS_DIR)
        {
            journal_dir.parent().map(Path::to_path_buf)
        } else {
            Some(journal_dir)
        };
        if let Some(owner) = owner {
            tree.tasks_mut(&mut |task| {
                if task.dir == owner {
                    task.errors.push(error.clone());
                }
            });
        }
    }
    tree
}

/// The last good parses of an earlier tree, by directory.
struct Previous<'a> {
    tasks: HashMap<&'a Path, &'a Task>,
    projects: HashMap<&'a Path, &'a ProjectFile>,
}

impl<'a> Previous<'a> {
    fn new(tree: Option<&'a WorkTree>) -> Self {
        let mut tasks = HashMap::new();
        let mut projects = HashMap::new();
        if let Some(tree) = tree {
            for task in tree.tasks() {
                tasks.insert(task.dir.as_path(), task);
            }
            for project in &tree.projects {
                if let Some(file) = &project.file {
                    projects.insert(project.task.dir.as_path(), file);
                }
            }
        }
        Self { tasks, projects }
    }

    fn task_file(&self, dir: &Path) -> Option<TaskFile> {
        self.tasks.get(dir).and_then(|task| task.file.clone())
    }

    fn artifacts(&self, dir: &Path) -> Option<ArtifactsFile> {
        self.tasks.get(dir).and_then(|task| task.artifacts.clone())
    }

    fn project(&self, dir: &Path) -> Option<ProjectFile> {
        self.projects.get(dir).map(|file| (*file).clone())
    }
}

/// Finds and finishes every `.op.toml` in the structural directories of the tree, before the tree
/// is read. A journal that cannot be finished is left in place; its directory and error are
/// returned.
fn finish_journals(root: &Path, errors: &mut Vec<FileError>) -> Vec<(PathBuf, FileError)> {
    let mut failed: Vec<(PathBuf, FileError)> = Vec::new();
    // Finishing one journal moves directories, so search again after each one.
    loop {
        let next = journal_dirs(root)
            .into_iter()
            .find(|dir| !failed.iter().any(|(failed_dir, _)| failed_dir == dir));
        let Some(dir) = next else {
            break;
        };
        let path = layout::op_file(&dir);
        if let Err(error) = finish_journal(root, &path) {
            let error = FileError::from_error(&path, &error);
            errors.push(error.clone());
            failed.push((dir, error));
        }
    }
    failed
}

/// The directories that may hold a journal and contain one now: the root, `projects/`,
/// `oneoffs/`, each activity, and every task directory and `tasks/` directory.
fn journal_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut ignored = Vec::new();
    let mut visit = |dir: &Path, found: &mut Vec<PathBuf>| {
        if layout::op_file(dir).symlink_metadata().is_ok() {
            found.push(dir.to_path_buf());
        }
    };
    visit(root, &mut found);
    let projects = layout::projects_dir(root);
    visit(&projects, &mut found);
    for (_, project) in subdirs(&projects, &mut ignored) {
        task_journal_dirs(&project, &mut found, &mut visit);
    }
    let oneoffs = layout::oneoffs_dir(root);
    visit(&oneoffs, &mut found);
    for (_, activity) in subdirs(&oneoffs, &mut ignored) {
        visit(&activity, &mut found);
        for (_, oneoff) in subdirs(&activity, &mut ignored) {
            task_journal_dirs(&oneoff, &mut found, &mut visit);
        }
    }
    found
}

fn task_journal_dirs(
    task_dir: &Path,
    found: &mut Vec<PathBuf>,
    visit: &mut impl FnMut(&Path, &mut Vec<PathBuf>),
) {
    visit(task_dir, found);
    let children = layout::children_dir(task_dir);
    visit(&children, found);
    for (_, child) in subdirs(&children, &mut Vec::new()) {
        task_journal_dirs(&child, found, visit);
    }
}

/// The subdirectories of `dir` that can be projects, activities or tasks, sorted by name: not
/// hidden, not temporary, not reserved. A missing `dir` has none.
fn subdirs(dir: &Path, errors: &mut Vec<FileError>) -> Vec<(String, PathBuf)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            errors.push(FileError::from_error(dir, &Error::io(dir, error)));
            return Vec::new();
        }
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || layout::is_reserved(&name) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            result.push((name, entry.path()));
        }
    }
    result.sort();
    result
}

fn load_project(
    slug: String,
    dir: PathBuf,
    previous: &Previous,
    errors: &mut Vec<FileError>,
) -> Project {
    let mut task = load_task(slug.clone(), dir, previous, errors);
    let project_path = layout::project_file(&task.dir);
    let text = read_text(&project_path);
    record_hash(&mut task.hashes, layout::PROJECT_FILE, &text);
    let file = match text {
        Ok(Some(text)) => match parse_project_at(&text, &project_path) {
            Ok(file) => Some(file),
            Err(error) => {
                task.errors
                    .push(FileError::from_error(&project_path, &error));
                previous.project(&task.dir)
            }
        },
        Ok(None) => {
            task.errors.push(missing(&project_path));
            previous.project(&task.dir)
        }
        Err(error) => {
            task.errors.push(error);
            previous.project(&task.dir)
        }
    };
    Project { slug, file, task }
}

fn missing(path: &Path) -> FileError {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    FileError {
        path: path.to_path_buf(),
        line: None,
        reason: format!("{name} is missing"),
    }
}

/// The text of `path`, or `None` when it does not exist.
fn read_text(path: &Path) -> Result<Option<String>, FileError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(FileError::from_error(path, &Error::io(path, error))),
    }
}

/// Records the hash of a model file's text, when it was read.
fn record_hash(
    hashes: &mut BTreeMap<String, u64>,
    name: &str,
    text: &Result<Option<String>, FileError>,
) {
    if let Ok(Some(text)) = text {
        hashes.insert(name.to_string(), content_hash(text.as_bytes()));
    }
}

fn load_task(slug: String, dir: PathBuf, previous: &Previous, errors: &mut Vec<FileError>) -> Task {
    let mut badges = Vec::new();
    let mut hashes = BTreeMap::new();

    let task_path = layout::task_file(&dir);
    let text = read_text(&task_path);
    record_hash(&mut hashes, layout::TASK_FILE, &text);
    let file = match text {
        Ok(Some(text)) => match parse_task_at(&text, &task_path) {
            Ok(file) => Some(file),
            Err(error) => {
                badges.push(FileError::from_error(&task_path, &error));
                previous.task_file(&dir)
            }
        },
        Ok(None) => {
            badges.push(missing(&task_path));
            previous.task_file(&dir)
        }
        Err(error) => {
            badges.push(error);
            previous.task_file(&dir)
        }
    };

    let artifacts_path = layout::artifacts_file(&dir);
    let text = read_text(&artifacts_path);
    record_hash(&mut hashes, layout::ARTIFACTS_FILE, &text);
    let artifacts = match text {
        Ok(Some(text)) => match parse_artifacts_at(&text, &artifacts_path) {
            Ok(file) => Some(file),
            Err(error) => {
                badges.push(FileError::from_error(&artifacts_path, &error));
                previous.artifacts(&dir)
            }
        },
        Ok(None) => None,
        Err(error) => {
            badges.push(error);
            previous.artifacts(&dir)
        }
    };

    let mut files = BTreeMap::new();
    collect_files(&dir, Path::new(""), &mut files, errors);

    let mut children = Vec::new();
    for (child_slug, child_dir) in subdirs(&layout::children_dir(&dir), errors) {
        children.push(load_task(child_slug, child_dir, previous, errors));
    }
    check_siblings(&mut children, previous);

    Task {
        slug,
        dir,
        file,
        artifacts,
        errors: badges,
        broken_edges: Vec::new(),
        files,
        hashes,
        children,
    }
}

/// Records the mtime of every file under `dir` that the task owns.
pub(crate) fn collect_files(
    dir: &Path,
    relative: &Path,
    files: &mut BTreeMap<PathBuf, SystemTime>,
    errors: &mut Vec<FileError>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(FileError::from_error(dir, &Error::io(dir, error)));
            return;
        }
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let at_top = relative.as_os_str().is_empty();
        if is_temp_file(&name) || name == OP_FILE || name == ".git" || (at_top && name == TASKS_DIR)
        {
            continue;
        }
        // `symlink_metadata`: a symbolic link is recorded, never followed.
        let Ok(metadata) = entry.path().symlink_metadata() else {
            continue;
        };
        let path = relative.join(&name);
        if metadata.is_dir() {
            collect_files(&entry.path(), &path, files, errors);
        } else if let Ok(modified) = metadata.modified() {
            files.insert(path, modified);
        }
    }
}

/// Checks the edges among siblings: a target that names no sibling is a broken edge; a cycle over
/// `after` and `stacked_on` is an error badge on every task in it, and each keeps its last good
/// parse.
fn check_siblings(siblings: &mut [Task], previous: &Previous) {
    let slugs: HashSet<String> = siblings.iter().map(|task| task.slug.clone()).collect();
    let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for task in siblings.iter_mut() {
        let Some(file) = &task.file else {
            graph.insert(task.slug.clone(), Vec::new());
            continue;
        };
        let targets: Vec<String> = file
            .after
            .iter()
            .map(|edge| edge.task.clone())
            .chain(file.stacked_on.iter().map(|edge| edge.task.clone()))
            .collect();
        let mut broken = Vec::new();
        let mut present = Vec::new();
        for target in targets {
            if slugs.contains(&target) {
                present.push(target);
            } else if !broken.contains(&target) {
                broken.push(target);
            }
        }
        task.broken_edges = broken;
        graph.insert(task.slug.clone(), present);
    }

    for (slug, chain) in find_cycles(&graph) {
        let Some(task) = siblings.iter_mut().find(|task| task.slug == slug) else {
            continue;
        };
        let task_path = layout::task_file(&task.dir);
        let next = &chain[1];
        let line = std::fs::read_to_string(&task_path)
            .ok()
            .and_then(|text| line_naming(&text, next));
        task.errors.push(FileError {
            path: task_path,
            line,
            reason: format!("cycle: {}", chain.join(" -> ")),
        });
        if let Some(good) = previous.task_file(&task.dir) {
            task.file = Some(good);
        }
    }
}

/// The 1-based line of the first `"<slug>"` in `text`.
fn line_naming(text: &str, slug: &str) -> Option<usize> {
    let quoted = format!("\"{slug}\"");
    text.lines()
        .position(|line| line.contains(&quoted))
        .map(|index| index + 1)
}

/// For each node on a cycle, the cycle starting and ending at it, such as `a -> b -> a`.
fn find_cycles(graph: &BTreeMap<String, Vec<String>>) -> BTreeMap<String, Vec<String>> {
    fn visit(
        node: &str,
        graph: &BTreeMap<String, Vec<String>>,
        stack: &mut Vec<String>,
        done: &mut HashSet<String>,
        cycles: &mut BTreeMap<String, Vec<String>>,
    ) {
        if let Some(start) = stack.iter().position(|entry| entry == node) {
            let cycle = &stack[start..];
            for (index, member) in cycle.iter().enumerate() {
                if cycles.contains_key(member) {
                    continue;
                }
                let mut chain: Vec<String> = cycle[index..]
                    .iter()
                    .chain(cycle[..index].iter())
                    .cloned()
                    .collect();
                chain.push(member.clone());
                cycles.insert(member.clone(), chain);
            }
            return;
        }
        if done.contains(node) {
            return;
        }
        stack.push(node.to_string());
        for next in graph.get(node).into_iter().flatten() {
            visit(next, graph, stack, done, cycles);
        }
        stack.pop();
        done.insert(node.to_string());
    }

    let mut cycles = BTreeMap::new();
    let mut done = HashSet::new();
    for node in graph.keys() {
        visit(node, graph, &mut Vec::new(), &mut done, &mut cycles);
    }
    cycles
}

/// Two task directories with the same `id` are both badged: an id must name one task.
fn mark_duplicate_ids(tree: &mut WorkTree) {
    let mut dirs_by_id: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for task in tree.tasks() {
        if let Some(id) = task.id() {
            dirs_by_id
                .entry(id.to_string())
                .or_default()
                .push(task.dir.clone());
        }
    }
    dirs_by_id.retain(|_, dirs| dirs.len() > 1);
    if dirs_by_id.is_empty() {
        return;
    }
    tree.tasks_mut(&mut |task| {
        let Some(dirs) = task.id().and_then(|id| dirs_by_id.get(id)) else {
            return;
        };
        let others: Vec<String> = dirs
            .iter()
            .filter(|dir| **dir != task.dir)
            .map(|dir| dir.display().to_string())
            .collect();
        task.errors.push(FileError {
            path: layout::task_file(&task.dir),
            line: None,
            reason: format!("duplicate id, also used by {}", others.join(", ")),
        });
    });
}
