//! Changed-file counts for the live modes of a leaf (D13): how many files differ in its worktree.
//!
//! When a window has the worktree open, the count comes from that project's git store, which
//! keeps its own status up to date. Otherwise `git status --porcelain` runs on the background
//! executor, at most once per [`CHANGES_INTERVAL`] per worktree; in between, the last count is
//! served. Views read [`WorkStore::changed_files`] and ask for a fresh one with
//! [`WorkStore::refresh_changed_files`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context};
use util::ResultExt as _;
use workspace::MultiWorkspace;

use crate::WorkStore;
use crate::worktrees::expand_home;

/// The shortest time between two `git status` runs in the same worktree.
pub const CHANGES_INTERVAL: Duration = Duration::from_secs(10);

/// The last count of one worktree.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChangedCount {
    at: Instant,
    count: Option<usize>,
    /// A `git status` is running.
    checking: bool,
}

impl ChangedCount {
    #[cfg(test)]
    pub(crate) fn new(at: Instant, count: Option<usize>, checking: bool) -> Self {
        Self {
            at,
            count,
            checking,
        }
    }

    /// Whether another `git status` may start at `now`.
    pub(crate) fn due(&self, now: Instant) -> bool {
        !self.checking && now.saturating_duration_since(self.at) >= CHANGES_INTERVAL
    }
}

/// The number of entries in the output of `git status --porcelain`: one per changed file.
pub(crate) fn count_porcelain(output: &str) -> usize {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

/// Runs `git status --porcelain` in `directory`. Untracked files are listed one by one, as the
/// git panel counts them.
async fn git_status_count(directory: &Path) -> anyhow::Result<usize> {
    let git_binary = which::which("git")?;
    let output = util::command::new_command(git_binary)
        .current_dir(directory)
        .args(["status", "--porcelain", "--untracked-files=all"].map(OsString::from))
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "git status failed in {}: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(count_porcelain(&String::from_utf8_lossy(&output.stdout)))
}

/// The changed-file count of the repository at `path` when a window has it open.
fn open_repository_count(path: &Path, cx: &App) -> Option<usize> {
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Ok(multi) = window.read(cx) else {
            continue;
        };
        for workspace in multi.workspaces() {
            let project = workspace.read(cx).project().read(cx);
            for repository in project.repositories(cx).values() {
                let repository = repository.read(cx);
                if repository.work_directory_abs_path.as_ref() == path {
                    return Some(repository.cached_status().count());
                }
            }
        }
    }
    None
}

impl WorkStore {
    /// The worktree of the leaf `id`, when it has one.
    fn worktree_path(&self, id: &str) -> Option<PathBuf> {
        let tree = self.snapshot()?;
        let worktree = tree.task_by_id(id)?.file.as_ref()?.worktree.clone()?;
        Some(expand_home(&worktree))
    }

    /// The last known number of changed files in the worktree of the task `id`. `None` for a
    /// task with no worktree and before the first count.
    pub fn changed_files(&self, id: &str) -> Option<usize> {
        self.changed.get(&self.worktree_path(id)?)?.count
    }

    /// Counts the changed files of the worktree of the task `id` again: from the git store of a
    /// project that has it open, else by running git, at most once per [`CHANGES_INTERVAL`].
    pub fn refresh_changed_files(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(path) = self.worktree_path(id) else {
            return;
        };
        if let Some(count) = open_repository_count(&path, cx) {
            self.set_changed(path, Some(count), cx);
            return;
        }
        let now = Instant::now();
        let previous = self.changed.get(&path).copied();
        if previous.is_some_and(|previous| !previous.due(now)) {
            return;
        }
        self.changed.insert(
            path.clone(),
            ChangedCount {
                at: now,
                count: previous.and_then(|previous| previous.count),
                checking: true,
            },
        );
        self.status_runs += 1;
        cx.spawn(async move |this, cx| {
            let directory = path.clone();
            let count = cx
                .background_spawn(async move { git_status_count(&directory).await.log_err() })
                .await;
            this.update(cx, |store, cx| store.set_changed(path, count, cx))
                .ok();
        })
        .detach();
    }

    /// How many times `git status` ran, for tests.
    pub fn status_runs(&self) -> usize {
        self.status_runs
    }

    fn set_changed(&mut self, path: PathBuf, count: Option<usize>, cx: &mut Context<Self>) {
        let previous = self.changed.get(&path).and_then(|entry| entry.count);
        let count = count.or(previous);
        self.changed.insert(
            path,
            ChangedCount {
                at: Instant::now(),
                count,
                checking: false,
            },
        );
        if count != previous {
            cx.emit(crate::WorkStoreEvent::ChangedFilesChanged);
            cx.notify();
        }
    }
}
