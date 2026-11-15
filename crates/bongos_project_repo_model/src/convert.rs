//! The plan of converting a project of the old plain-file layout (`projects/<slug>/` with
//! nested `tasks/<slug>/` folders, or the one-offs under `oneoffs/<activity>/`) into a project
//! repository. Pure: [`plan`] and [`plan_oneoffs`] read only the tree the old loader produced;
//! the executor reads and copies the files and checks every name again before it writes.
//!
//! Everything that comes from an old folder is untrusted. A task id, slug or file path that
//! could name a place outside the new project, or a `.git` entry, is never planned: the plan
//! either refuses the whole project (a blocker) or leaves the entry out with a reason (a
//! rejected entry), and the preview shows both.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use bongos_work_model::layout as old_layout;
use bongos_work_model::slug::slugify;
use bongos_work_model::store::{Activity, Project, Task};
use bongos_work_model::{Kind, ProjectFile, TaskFile, Timestamp};
use serde::{Deserialize, Serialize};

use crate::layout::{PLAN_FILE, PROJECT_FILE, TASK_FILE, TASKS_DIR, WORKFLOWS_DIR};
use crate::split::{TaskDefinition, TaskStateRecord, reserved_keys_in, split};

/// The message of the one commit a conversion makes.
pub const CONVERSION_COMMIT_MESSAGE: &str = "Convert from the plain-file layout";
/// The start of the name of the folder a conversion builds in, beside the project folders.
pub const CONVERTING_PREFIX: &str = ".converting-";
/// The title of the project the old one-offs convert into, unless the setting names another.
pub const DEFAULT_ONEOFFS_TITLE: &str = "One-offs";
/// The longest file or folder name a conversion writes, in bytes.
const MAX_NAME_BYTES: usize = 255;

/// Where a file of an old task goes in the new project.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "to", content = "path", rename_all = "kebab-case")]
pub enum Target {
    /// Relative to `definitions/` in the repository (the plan and the workflows).
    Definitions(String),
    /// Relative to the task's files folder, `files/<task id>/`.
    Files(String),
}

/// One file to copy: `from` relative to the old task folder, `/`-separated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCopy {
    pub from: String,
    pub to: Target,
}

/// One task of the new project.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedTask {
    pub id: String,
    /// The old task folder, as the old loader read it.
    pub old_dir: PathBuf,
    /// The definition folder relative to `definitions/`, `""` for the top group.
    pub folder: String,
    pub definition: TaskDefinition,
    /// Its state, before `history.old.toml` is folded in.
    pub state: TaskStateRecord,
    /// Whether the old folder has a `history.old.toml` whose entries go before `state.history`.
    pub fold_old_history: bool,
    pub copies: Vec<FileCopy>,
    /// Files of the old folder that become the definition or the state instead of a copy,
    /// relative to it.
    pub consumed: Vec<String>,
    /// The slugs of the child task folders the old loader read.
    pub child_slugs: Vec<String>,
    /// Whether this task's folder was made by the conversion (an activity group of the
    /// one-offs), so it has no old folder of its own.
    pub synthetic: bool,
}

/// An entry of the old folder that is not converted, and why.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Rejected {
    pub path: PathBuf,
    pub reason: String,
}

/// What converting one old project does.
#[derive(Clone, Debug, PartialEq)]
pub struct ConversionPlan {
    /// The old folder: `projects/<slug>` or, for the one-offs, `oneoffs`.
    pub old_dir: PathBuf,
    /// The name of the new project folder before any collision is resolved.
    pub slug: String,
    /// The new `project.toml`, with an id.
    pub project: ProjectFile,
    /// Every task, parents before children, the top group first.
    pub tasks: Vec<PlannedTask>,
    pub rejected: Vec<Rejected>,
    /// Why the project cannot be converted as it is; empty when it can.
    pub blockers: Vec<String>,
}

/// The preview of one conversion: what is there and what will be made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionSummary {
    pub old_dir: PathBuf,
    pub title: String,
    pub slug: String,
    pub tasks: usize,
    pub todos: usize,
    pub workflows: usize,
    pub chats: usize,
    /// Files copied into the files folders.
    pub files: usize,
    /// What the conversion makes, one line each.
    pub creates: Vec<String>,
    pub rejected: Vec<Rejected>,
    pub blockers: Vec<String>,
}

impl ConversionPlan {
    pub fn project_id(&self) -> &str {
        self.project.id.as_deref().unwrap_or_default()
    }

    pub fn is_blocked(&self) -> bool {
        !self.blockers.is_empty()
    }

    /// Sorts the rejected entries, keeps one per path, and makes each path text (a name that is
    /// not UTF-8 shows with replacement characters) so the preview can be sent as data.
    pub fn tidy_rejected(&mut self) {
        for entry in &mut self.rejected {
            if entry.path.to_str().is_none() {
                entry.path = PathBuf::from(entry.path.to_string_lossy().into_owned());
            }
        }
        self.rejected.sort();
        self.rejected.dedup_by(|a, b| a.path == b.path);
    }

    pub fn summary(&self) -> ConversionSummary {
        let mut workflows = BTreeSet::new();
        let mut chats = 0;
        let mut files = 0;
        let mut has_plan = false;
        for task in &self.tasks {
            for copy in &task.copies {
                match &copy.to {
                    Target::Definitions(path) => {
                        if path == PLAN_FILE {
                            has_plan = true;
                        }
                        if let Some(rest) = path.strip_prefix(&format!("{WORKFLOWS_DIR}/"))
                            && let Some((name, _)) = rest.split_once('/')
                        {
                            workflows.insert(name.to_string());
                        }
                    }
                    Target::Files(path) => {
                        files += 1;
                        if let Some(rest) =
                            path.strip_prefix(&format!("{}/", old_layout::CHATS_DIR))
                            && !rest.contains('/')
                            && rest.ends_with(".toml")
                        {
                            chats += 1;
                        }
                    }
                }
            }
        }
        let todos = self
            .tasks
            .iter()
            .map(|task| task.definition.todos.len())
            .sum();
        let mut creates = vec![
            format!(
                "a project repository with {} task definition{}",
                self.tasks.len(),
                plural(self.tasks.len())
            ),
            format!(
                "{} plan.md",
                if has_plan {
                    "the project's"
                } else {
                    "an empty"
                }
            ),
        ];
        if !workflows.is_empty() {
            creates.push(format!(
                "{} workflow{} in definitions/workflows",
                workflows.len(),
                plural(workflows.len())
            ));
        }
        creates.push(format!(
            "{files} file{} copied into the files folders of the tasks",
            plural(files)
        ));
        creates.push(format!(
            "the state of {} task{} in a new state store",
            self.tasks.len(),
            plural(self.tasks.len())
        ));
        creates.push(format!("one commit: {CONVERSION_COMMIT_MESSAGE}"));
        ConversionSummary {
            old_dir: self.old_dir.clone(),
            title: self.project.title.clone(),
            slug: self.slug.clone(),
            tasks: self.tasks.len(),
            todos,
            workflows: workflows.len(),
            chats,
            files,
            creates,
            rejected: self.rejected.clone(),
            blockers: self.blockers.clone(),
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Why `name` cannot be one file or folder name the conversion writes: empty, `.` or `..`, a
/// separator, a control character (a newline among them), longer than 255 bytes, or `.git` in
/// any case.
pub fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("the name is empty".to_string());
    }
    if name == "." || name == ".." {
        return Some(format!("the name {name:?} names a folder above it"));
    }
    if name.contains('/') || name.contains('\\') {
        return Some(format!("the name {name:?} holds a path separator"));
    }
    if name.chars().any(char::is_control) {
        return Some(format!("the name {name:?} holds a control character"));
    }
    if name.len() > MAX_NAME_BYTES {
        return Some(format!("the name is longer than {MAX_NAME_BYTES} bytes"));
    }
    if name.eq_ignore_ascii_case(".git") {
        return Some("a git repository inside a task's files is not copied".to_string());
    }
    None
}

/// `path` as a `/`-separated relative path whose every part passes [`name_problem`], or why it
/// cannot be one.
pub fn relative_path(path: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| "the name is not UTF-8".to_string())?;
                if let Some(problem) = name_problem(part) {
                    return Err(problem);
                }
                parts.push(part);
            }
            Component::CurDir => return Err("the path holds a `.` part".to_string()),
            Component::ParentDir => {
                return Err("the path leads out of the folder with `..`".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("the path is absolute".to_string());
            }
        }
    }
    if parts.is_empty() {
        return Err("the path is empty".to_string());
    }
    Ok(parts.join("/"))
}

/// Plans the conversion of the old project `old`. `new_id` makes the ids the plan needs: the
/// project's when it has none, and each todo's that has none or repeats another's.
pub fn plan(old: &Project, new_id: &mut impl FnMut() -> String) -> ConversionPlan {
    let mut plan = ConversionPlan {
        old_dir: old.task.dir.clone(),
        slug: old.slug.clone(),
        project: old
            .file
            .clone()
            .unwrap_or_else(|| ProjectFile::new(old.slug.clone())),
        tasks: Vec::new(),
        rejected: Vec::new(),
        blockers: Vec::new(),
    };
    if let Some(problem) = name_problem(&old.slug) {
        plan.blockers.push(format!(
            "the project folder's name cannot be used: {problem}"
        ));
    }
    if old.file.is_none() {
        plan.blockers.push(format!(
            "{}: {} has never parsed",
            old.task.dir.display(),
            PROJECT_FILE
        ));
    }
    if plan.project.title.trim().is_empty() {
        plan.blockers.push("the project has no title".to_string());
    }
    let project_id = plan
        .project
        .id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| new_id());
    if let Some(problem) = name_problem(&project_id) {
        plan.blockers
            .push(format!("the project id cannot be used: {problem}"));
    }
    plan.project.id = Some(project_id);

    let mut ids = BTreeMap::new();
    plan_task(&old.task, String::new(), true, &mut plan, &mut ids, new_id);
    plan.tidy_rejected();
    plan
}

/// Plans the conversion of the old one-offs, the activities under `oneoffs_dir`, into one
/// project titled `title`: each activity becomes a top-level group (created `now`) and each
/// one-off a child of it.
pub fn plan_oneoffs(
    oneoffs_dir: &Path,
    activities: &[Activity],
    title: &str,
    now: Timestamp,
    new_id: &mut impl FnMut() -> String,
) -> ConversionPlan {
    let slug = slugify(title);
    let mut project = ProjectFile::new(title.to_string());
    project.id = Some(new_id());
    let mut top_file = TaskFile::new(title.to_string(), Kind::Other);
    top_file.id = new_id();
    top_file.created = now;
    let mut made = BTreeSet::from([top_file.id.clone()]);
    let mut groups = Vec::new();
    for activity in activities {
        let mut file = TaskFile::new(activity.slug.clone(), Kind::Other);
        file.id = new_id();
        made.insert(file.id.clone());
        file.created = now;
        file.activity = Some(activity.slug.clone());
        groups.push(synthetic_task(
            &activity.slug,
            activity.dir.clone(),
            file,
            activity.oneoffs.clone(),
        ));
    }
    let top = synthetic_task(&slug, oneoffs_dir.to_path_buf(), top_file, groups);
    let mut plan = ConversionPlan {
        old_dir: oneoffs_dir.to_path_buf(),
        slug: slug.clone(),
        project,
        tasks: Vec::new(),
        rejected: Vec::new(),
        blockers: Vec::new(),
    };
    if let Some(problem) = name_problem(&slug) {
        plan.blockers.push(format!(
            "the one-offs project's title cannot be used: {problem}"
        ));
    }
    let mut ids = BTreeMap::new();
    plan_task(&top, String::new(), true, &mut plan, &mut ids, new_id);
    for task in &mut plan.tasks {
        // The top group and the activity groups are made here; the one-offs keep their folders.
        task.synthetic = made.contains(&task.id);
    }
    plan.tidy_rejected();
    plan
}

fn synthetic_task(slug: &str, dir: PathBuf, file: TaskFile, children: Vec<Task>) -> Task {
    Task {
        slug: slug.to_string(),
        dir,
        definition: None,
        pending: None,
        needs_conversion: false,
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        broken_edges: Vec::new(),
        files: BTreeMap::new(),
        hashes: BTreeMap::new(),
        children,
    }
}

fn join_folder(folder: &str, slug: &str) -> String {
    if folder.is_empty() {
        format!("{TASKS_DIR}/{slug}")
    } else {
        format!("{folder}/{TASKS_DIR}/{slug}")
    }
}

fn plan_task(
    task: &Task,
    folder: String,
    is_top: bool,
    plan: &mut ConversionPlan,
    ids: &mut BTreeMap<String, PathBuf>,
    new_id: &mut impl FnMut() -> String,
) {
    for error in &task.errors {
        plan.blockers.push(error.to_string());
    }
    let mut child_slugs = Vec::new();
    let mut children = Vec::new();
    for child in &task.children {
        match name_problem(&child.slug) {
            Some(problem) => plan.blockers.push(format!(
                "{}: the task folder's name cannot be used: {problem}",
                child.dir.display()
            )),
            None => {
                child_slugs.push(child.slug.clone());
                children.push(child);
            }
        }
    }

    match &task.file {
        None => plan.blockers.push(format!(
            "{}: {TASK_FILE} has never parsed",
            task.dir.display()
        )),
        Some(file) => {
            if let Some(problem) = name_problem(&file.id) {
                plan.blockers.push(format!(
                    "{}: the task id cannot be used: {problem}",
                    task.dir.display()
                ));
            } else if let Some(other) = ids.insert(file.id.clone(), task.dir.clone()) {
                plan.blockers.push(format!(
                    "{}: duplicate id {}, also used by {}",
                    task.dir.display(),
                    file.id,
                    other.display()
                ));
            }
            let mut file = file.clone();
            let mut todo_ids = BTreeSet::new();
            for todo in &mut file.todos {
                let usable = todo
                    .id
                    .as_ref()
                    .is_some_and(|id| !id.trim().is_empty() && !todo_ids.contains(id));
                if !usable {
                    todo.id = Some(new_id());
                }
                todo_ids.insert(todo.id.clone().unwrap_or_default());
            }
            let (definition, state) = split(&file);
            let reserved = reserved_keys_in(&definition);
            if !reserved.is_empty() {
                plan.blockers.push(format!(
                    "{}: holds keys of the state store among its own: {}",
                    task.dir.display(),
                    reserved.join(", ")
                ));
            }
            let (copies, consumed, fold_old_history) = plan_files(task, is_top, plan);
            plan.tasks.push(PlannedTask {
                id: definition.id.clone(),
                old_dir: task.dir.clone(),
                folder: folder.clone(),
                definition,
                state,
                fold_old_history,
                copies,
                consumed,
                child_slugs,
                synthetic: false,
            });
        }
    }

    for child in children {
        plan_task(
            child,
            join_folder(&folder, &child.slug),
            false,
            plan,
            ids,
            new_id,
        );
    }
}

/// The copies of a task's files, the files it consumes, and whether it folds an old history.
fn plan_files(
    task: &Task,
    is_top: bool,
    plan: &mut ConversionPlan,
) -> (Vec<FileCopy>, Vec<String>, bool) {
    let mut copies = Vec::new();
    let mut consumed = Vec::new();
    let mut fold = false;
    for path in task.files.keys() {
        let relative = match relative_path(path) {
            Ok(relative) => relative,
            Err(reason) => {
                plan.rejected.push(Rejected {
                    path: task.dir.join(path),
                    reason,
                });
                continue;
            }
        };
        match relative.as_str() {
            TASK_FILE => consumed.push(relative),
            PROJECT_FILE if is_top => consumed.push(relative),
            old_layout::HISTORY_OLD_FILE => {
                fold = true;
                consumed.push(relative);
            }
            PLAN_FILE if is_top => copies.push(FileCopy {
                to: Target::Definitions(relative.clone()),
                from: relative,
            }),
            _ if is_top && relative.starts_with(&format!("{WORKFLOWS_DIR}/")) => {
                copies.push(FileCopy {
                    to: Target::Definitions(relative.clone()),
                    from: relative,
                })
            }
            _ => copies.push(FileCopy {
                to: Target::Files(relative.clone()),
                from: relative,
            }),
        }
    }
    (copies, consumed, fold)
}
