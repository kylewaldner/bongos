//! Watching the git refs of the repositories that have stacked tasks (D9, D10), so "rebase
//! needed" follows a parent's branch when it moves rather than on a timer.
//!
//! Only repositories with at least one stacked task that has a worktree are watched: after every
//! full [`WorkStore::refresh_rebase`] the store reconciles the watched set with the common git
//! dirs its probes named. A repository is watched through its common git dir (shared by the
//! checkout and all its worktrees): the dir itself, `refs/`, every directory under `refs/heads`
//! and `refs/remotes`, and `reftable/` when present, because the Linux watcher is not
//! recursive. A batch that touches a ref (see [`names_a_ref`]) runs
//! [`WorkStore::refresh_rebase_for`] for that repository alone.
//!
//! A repository on a file system that delivers no change notifications (network mounts, most
//! FUSE file systems, or any repository while `ZED_FILE_WATCHER_MODE=poll`) is not watched;
//! a timer runs its check every [`SLOW_POLL_INTERVAL`] instead.
//!
//! The watches belong to the store's session, so turning `work.enabled` off drops every watcher
//! and timer.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs::{Fs, PathEvent, PathEventKind, Watcher};
use futures::StreamExt as _;
use gpui::{Context, Task};
use util::ResultExt as _;

use crate::WorkStore;
use crate::worktrees::common_git_dir;

/// How long the ref watcher gathers events before delivering them.
pub const REF_WATCH_LATENCY: Duration = Duration::from_millis(100);

/// How often a repository that cannot be watched is checked.
pub const SLOW_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Files directly in the common git dir whose change can move a branch or what it points at.
const REF_FILES: [&str; 4] = ["packed-refs", "HEAD", "FETCH_HEAD", "ORIG_HEAD"];

/// A repository's common git dir, canonicalized, and whether it needs the slow poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitDirInfo {
    pub(crate) common_dir: PathBuf,
    pub(crate) slow_poll: bool,
}

/// Finds the common git dir of the checkout at `repository` and asks `poll_decider` whether it
/// needs the slow poll. Blocking file system calls: run it on the background executor. `None`
/// when `repository` is not a git checkout.
pub(crate) async fn git_dir_info(
    fs: &dyn Fs,
    repository: &Path,
    poll_decider: fn(&Path) -> bool,
) -> Option<GitDirInfo> {
    let common_dir = common_git_dir(repository).ok()?;
    let common_dir = fs.canonicalize(&common_dir).await.unwrap_or(common_dir);
    Some(GitDirInfo {
        slow_poll: poll_decider(&common_dir),
        common_dir,
    })
}

/// Whether a change at `relative` (a path under the common git dir) can move a ref: anything
/// under `refs/` or `reftable/`, and `packed-refs`, `HEAD`, `FETCH_HEAD` and `ORIG_HEAD`. Lock
/// files, `index`, `objects/` and the rest cannot.
pub(crate) fn names_a_ref(relative: &Path) -> bool {
    if relative
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".lock"))
    {
        return false;
    }
    let mut components = relative.components();
    let Some(Component::Normal(first)) = components.next() else {
        return false;
    };
    if first == "refs" || first == "reftable" {
        return true;
    }
    components.next().is_none() && REF_FILES.iter().any(|name| first == *name)
}

/// Whether the directory at `relative` (under the common git dir) is one the watcher adds.
fn wants_watch(relative: &Path) -> bool {
    relative == Path::new("refs")
        || relative == Path::new("reftable")
        || relative.starts_with("refs/heads")
        || relative.starts_with("refs/remotes")
}

/// The directories to add to the watcher of `common_dir` besides itself.
fn ref_dirs(common_dir: &Path) -> BTreeSet<PathBuf> {
    let mut dirs = BTreeSet::new();
    let refs = common_dir.join("refs");
    if is_dir(&refs) {
        dirs.insert(refs.clone());
        for top in ["heads", "remotes"] {
            walk(&refs.join(top), &mut dirs);
        }
    }
    let reftable = common_dir.join("reftable");
    if is_dir(&reftable) {
        dirs.insert(reftable);
    }
    dirs
}

/// Whether `path` is a directory, not following a symbolic link.
fn is_dir(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.is_dir())
}

fn walk(dir: &Path, dirs: &mut BTreeSet<PathBuf>) {
    if !is_dir(dir) {
        return;
    }
    dirs.insert(dir.to_path_buf());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            walk(&entry.path(), dirs);
        }
    }
}

/// The watcher of one common git dir and the directories added to it.
pub(crate) struct RefDirs {
    common_dir: PathBuf,
    watcher: Arc<dyn Watcher>,
    dirs: BTreeSet<PathBuf>,
}

impl RefDirs {
    /// Watches the ref directories of `common_dir` with `watcher`, which already watches the
    /// dir itself. Blocking file system calls: run it on the background executor.
    pub(crate) fn new(common_dir: PathBuf, watcher: Arc<dyn Watcher>) -> Self {
        let mut dirs = Self {
            common_dir,
            watcher,
            dirs: BTreeSet::new(),
        };
        dirs.sync();
        dirs
    }

    /// Adds the directories that appeared and removes the ones that are gone, so a directory
    /// deleted and made again (`git pack-refs` prunes empty ones) is watched again.
    fn sync(&mut self) {
        let wanted = ref_dirs(&self.common_dir);
        for gone in self.dirs.difference(&wanted) {
            self.watcher.remove(gone).log_err();
        }
        for new in wanted.difference(&self.dirs) {
            self.watcher.add(new).log_err();
        }
        self.dirs = wanted;
    }

    /// Whether `batch` touches a ref. A rescan always counts. Brings the watched directories up
    /// to date when one appeared or went away.
    pub(crate) fn handle(&mut self, batch: &[PathEvent]) -> bool {
        let mut matters = false;
        let mut resync = false;
        for event in batch {
            if event.kind == Some(PathEventKind::Rescan) {
                matters = true;
                resync = true;
                continue;
            }
            let Ok(relative) = event.path.strip_prefix(&self.common_dir) else {
                continue;
            };
            matters |= names_a_ref(relative);
            if self.dirs.contains(&event.path) || (wants_watch(relative) && is_dir(&event.path)) {
                resync = true;
            }
        }
        if resync {
            self.sync();
        }
        matters
    }
}

/// The ref watches of one session, by common git dir. Dropping it drops every watcher and timer.
#[derive(Default)]
pub(crate) struct RefWatches {
    watches: HashMap<PathBuf, RefWatch>,
}

struct RefWatch {
    slow_poll: bool,
    _task: Task<()>,
}

impl RefWatches {
    /// The watched common git dirs, each with whether it is on the slow poll.
    pub(crate) fn watched(&self) -> BTreeMap<PathBuf, bool> {
        self.watches
            .iter()
            .map(|(dir, watch)| (dir.clone(), watch.slow_poll))
            .collect()
    }
}

impl WorkStore {
    /// The common git dirs watched for ref changes, each with whether it is on the slow poll
    /// instead of a watcher. Empty while `work.enabled` is false.
    pub fn watched_git_dirs(&self) -> BTreeMap<PathBuf, bool> {
        self.ref_watches()
            .map(RefWatches::watched)
            .unwrap_or_default()
    }

    /// Makes the watched set `wanted` (common git dir to slow poll): new dirs are watched, dirs
    /// no longer wanted are dropped. Does nothing while disabled.
    pub(crate) fn reconcile_ref_watches(
        &mut self,
        wanted: BTreeMap<PathBuf, bool>,
        cx: &mut Context<Self>,
    ) {
        let fs = self.fs();
        let Some(watches) = self.ref_watches_mut() else {
            return;
        };
        watches
            .watches
            .retain(|dir, watch| wanted.get(dir) == Some(&watch.slow_poll));
        for (dir, slow_poll) in wanted {
            if watches.watches.contains_key(&dir) {
                continue;
            }
            let task = if slow_poll {
                slow_poll_task(dir.clone(), cx)
            } else {
                watch_task(fs.clone(), dir.clone(), cx)
            };
            watches.watches.insert(
                dir,
                RefWatch {
                    slow_poll,
                    _task: task,
                },
            );
        }
    }
}

/// Watches `common_dir` and checks its repository after every batch that touches a ref.
fn watch_task(fs: Arc<dyn Fs>, common_dir: PathBuf, cx: &mut Context<WorkStore>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let background = cx.background_executor().clone();
        let (mut events, mut dirs) = background
            .spawn({
                let common_dir = common_dir.clone();
                async move {
                    let (events, watcher) = fs.watch(&common_dir, REF_WATCH_LATENCY).await;
                    (events, RefDirs::new(common_dir, watcher))
                }
            })
            .await;
        // A ref may have moved between the probe that asked for this watch and the watch itself.
        if this
            .update(cx, |store, cx| store.refresh_rebase_for(&common_dir, cx))
            .is_err()
        {
            return;
        }
        while let Some(batch) = events.next().await {
            let (matters, returned) = background
                .spawn(async move {
                    let matters = dirs.handle(&batch);
                    (matters, dirs)
                })
                .await;
            dirs = returned;
            if !matters {
                continue;
            }
            let updated = this.update(cx, |store, cx| {
                store.stats.ref_events += 1;
                store.refresh_rebase_for(&common_dir, cx);
            });
            if updated.is_err() {
                return;
            }
        }
        log::error!(
            "the ref watcher of {} stopped; rebase states there update on focus only",
            common_dir.display()
        );
    })
}

/// Checks the repository of `common_dir` every [`SLOW_POLL_INTERVAL`].
fn slow_poll_task(common_dir: PathBuf, cx: &mut Context<WorkStore>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(SLOW_POLL_INTERVAL).await;
            if this
                .update(cx, |store, cx| store.refresh_rebase_for(&common_dir, cx))
                .is_err()
            {
                return;
            }
        }
    })
}
