//! Where things live under the work root (`~/.bongos` in the app). Every function is pure: the
//! root or directory is an argument, and nothing here touches the disk.
//!
//! ```text
//! projects/<slug>/                 project.toml, task.toml, plan.md, prompts.txt, artifacts.toml, tasks/<slug>/...
//! oneoffs/<activity>/<date>-<slug>/ task.toml, prompts.txt, artifacts.toml, tasks/<slug>/...
//! ```

use std::path::{Path, PathBuf};

pub const TASK_FILE: &str = "task.toml";
pub const PROJECT_FILE: &str = "project.toml";
pub const ARTIFACTS_FILE: &str = "artifacts.toml";
pub const HISTORY_OLD_FILE: &str = "history.old.toml";
pub const OP_FILE: &str = ".op.toml";
pub const PLAN_FILE: &str = "plan.md";
pub const PROMPTS_FILE: &str = "prompts.txt";
pub const TASKS_DIR: &str = "tasks";
pub const CHATS_DIR: &str = "chats";
pub const RUNS_DIR: &str = "runs";
/// The directory of a task's proposal files, `<n>.proposal.txt`.
pub const PROPOSALS_DIR: &str = "proposals";
pub const PROJECTS_DIR: &str = "projects";
pub const ONEOFFS_DIR: &str = "oneoffs";
/// The suffix of a link file such as `design.link.toml`.
pub const LINK_SUFFIX: &str = ".link.toml";

const RESERVED: &[&str] = &[
    TASK_FILE,
    PROJECT_FILE,
    ARTIFACTS_FILE,
    HISTORY_OLD_FILE,
    OP_FILE,
    TASKS_DIR,
    CHATS_DIR,
    RUNS_DIR,
    ".git",
];

/// Whether `name`, an entry of a task directory, is the model's own and never an artifact: one of
/// the reserved names (a trailing `/` is ignored) or a temporary file matching `.*.tmp-*`.
pub fn is_reserved(name: &str) -> bool {
    let name = name.trim_end_matches('/');
    RESERVED.contains(&name) || is_temp_file(name)
}

/// Whether `name` matches `.*.tmp-*`, the temporary files of [`crate::files::write_atomic`].
pub fn is_temp_file(name: &str) -> bool {
    name.strip_prefix('.')
        .is_some_and(|rest| rest.contains(".tmp-"))
}

pub fn projects_dir(root: &Path) -> PathBuf {
    root.join(PROJECTS_DIR)
}

pub fn project_dir(root: &Path, project_slug: &str) -> PathBuf {
    projects_dir(root).join(project_slug)
}

pub fn oneoffs_dir(root: &Path) -> PathBuf {
    root.join(ONEOFFS_DIR)
}

pub fn activity_dir(root: &Path, activity_slug: &str) -> PathBuf {
    oneoffs_dir(root).join(activity_slug)
}

/// A one-off's directory; `dir_name` comes from [`crate::slug::oneoff_dir_name`].
pub fn oneoff_dir(root: &Path, activity_slug: &str, dir_name: &str) -> PathBuf {
    activity_dir(root, activity_slug).join(dir_name)
}

/// The directory holding a task's children.
pub fn children_dir(task_dir: &Path) -> PathBuf {
    task_dir.join(TASKS_DIR)
}

pub fn child_dir(task_dir: &Path, child_slug: &str) -> PathBuf {
    children_dir(task_dir).join(child_slug)
}

pub fn task_file(task_dir: &Path) -> PathBuf {
    task_dir.join(TASK_FILE)
}

pub fn project_file(project_dir: &Path) -> PathBuf {
    project_dir.join(PROJECT_FILE)
}

pub fn artifacts_file(dir: &Path) -> PathBuf {
    dir.join(ARTIFACTS_FILE)
}

pub fn history_old_file(task_dir: &Path) -> PathBuf {
    task_dir.join(HISTORY_OLD_FILE)
}

/// The journal of a multi-file operation in progress, kept in the source parent.
pub fn op_file(dir: &Path) -> PathBuf {
    dir.join(OP_FILE)
}

pub fn plan_file(project_dir: &Path) -> PathBuf {
    project_dir.join(PLAN_FILE)
}

pub fn prompts_file(dir: &Path) -> PathBuf {
    dir.join(PROMPTS_FILE)
}

pub fn chats_dir(task_dir: &Path) -> PathBuf {
    task_dir.join(CHATS_DIR)
}

pub fn runs_dir(task_dir: &Path) -> PathBuf {
    task_dir.join(RUNS_DIR)
}

/// `<dir>/proposals/<n>.proposal.txt`.
pub fn proposal_file(dir: &Path, n: u32) -> PathBuf {
    dir.join(PROPOSALS_DIR).join(format!("{n}.proposal.txt"))
}
