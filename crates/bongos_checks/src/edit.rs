//! Where `task: edit checks` opens: the file that holds a task's checks and the line of its
//! `checks` table. Pure.

use std::path::{Path, PathBuf};

use bongos_project_repo_model::layout::TASK_FILE;

use crate::definitions::{CHECKS_FOLDER, SETS_FOLDER};

/// The file to edit, relative to `definitions/`: the named set `set` when the checks come from
/// one, else the `task.toml` in the task's `folder` (none for the top of the project).
pub(crate) fn edit_target(folder: Option<&Path>, set: Option<&str>) -> PathBuf {
    match set {
        Some(set) => Path::new(CHECKS_FOLDER)
            .join(SETS_FOLDER)
            .join(format!("{set}.toml")),
        None => folder.unwrap_or(Path::new("")).join(TASK_FILE),
    }
}

/// The 0-based line of the first `checks` table in `text` (`[checks]`, `[checks.x]` or
/// `[[checks.check]]`); 0 when there is none, so the file opens at its top.
pub(crate) fn checks_table_row(text: &str) -> u32 {
    text.lines()
        .position(|line| {
            let line = line.trim();
            let Some(rest) = line.strip_prefix('[') else {
                return false;
            };
            let rest = rest.strip_prefix('[').unwrap_or(rest).trim_start();
            rest.strip_prefix("checks")
                .is_some_and(|after| after.starts_with(['.', ']', ' ']))
        })
        .unwrap_or(0) as u32
}
