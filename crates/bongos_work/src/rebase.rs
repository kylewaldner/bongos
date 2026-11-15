//! "Rebase needed" and "retarget needed" (D16), for stacked tasks only.
//!
//! A task stacked on a sibling needs a rebase when the sibling's branch moved since this task
//! was cut from it, the task's own branch does not contain the sibling's new head, and the owner
//! has not acknowledged that head with `task: clear rebase`. After a real rebase the ancestor
//! test clears the state by itself. A task whose parent is cancelled needs a retarget instead.
//!
//! [`WorkStore`] keeps the result. Every stacked task is rechecked after every load or write and
//! when a window gains focus ([`WorkStore::refresh_rebase`]); the tasks of one repository are
//! rechecked when its refs change, or every [`SLOW_POLL_INTERVAL`] where that cannot be watched
//! ([`WorkStore::refresh_rebase_for`], fed by `ref_watch`). Nothing else is periodic. Views read
//! the result with [`WorkStore::rebase_state`].
//!
//! At most one full check and one check per repository run at a time. A request for a check
//! that is already running is held and runs once more after it ends. A repository's check never
//! cancels the full one, and one requested while the full one runs waits for it: the full
//! check replaces every state, and its probes may have run before the ref change that asked for
//! the repository's check.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_work_model::Status;
use bongos_work_model::commands as model;
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;
use fs::Fs;
use gpui::{App, AppContext as _, Context, Task};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::Workspace;

use crate::actions::task;
use crate::commands::{show_toast, submit, with_task};
use crate::ref_watch::{GitDirInfo, git_dir_info};
use crate::repositories::resolve_repository;
use crate::settings::Repository;
use crate::worktrees::{expand_home, resolve_sha};
use crate::{WorkSettings, WorkStore};

#[cfg(doc)]
use crate::ref_watch::SLOW_POLL_INTERVAL;

/// What a stacked task needs, when it needs something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseState {
    /// The parent's branch moved to `parent_head`, which this task's branch does not contain.
    RebaseNeeded { parent_head: String },
    /// The parent is cancelled.
    RetargetNeeded,
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|w, _: &task::ClearRebase, window, cx| clear_rebase(w, window, cx));
}

/// Rechecks every stacked task whenever a window becomes active.
pub(crate) fn observe_focus(cx: &mut App) {
    cx.observe_new(|_: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        cx.observe_window_activation(window, |_, window, cx| {
            if !window.is_window_active() {
                return;
            }
            if let Some(store) = WorkStore::global(cx) {
                store.update(cx, |store, cx| store.refresh_rebase(cx));
            }
        })
        .detach();
    })
    .detach();
}

fn clear_rebase(workspace: &mut Workspace, window: &mut gpui::Window, cx: &mut Context<Workspace>) {
    let flagged: Vec<String> = WorkStore::global(cx)
        .map(|store| {
            store
                .read(cx)
                .rebase_states()
                .iter()
                .filter(|(_, state)| matches!(state, RebaseState::RebaseNeeded { .. }))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    with_task(
        workspace,
        window,
        cx,
        "Clear the rebase flag of which task?",
        "No task needs a rebase",
        move |task, _| task.id().is_some_and(|id| flagged.iter().any(|f| f == id)),
        Box::new(|id, workspace, window, cx| {
            let state =
                WorkStore::global(cx).and_then(|store| store.read(cx).rebase_state(&id).cloned());
            match state {
                Some(RebaseState::RebaseNeeded { parent_head }) => {
                    submit(workspace, window, cx, move |tree, ctx| {
                        model::clear_rebase(tree, ctx, &id, &parent_head)
                    })
                }
                _ => show_toast(workspace, format!("{id} no longer needs a rebase"), cx),
            }
        }),
    );
}

/// A stacked task whose parent's branch has to be looked at in git.
#[derive(Clone, Debug)]
pub(crate) struct Probe {
    id: String,
    repository: PathBuf,
    parent_branch: String,
    branch: String,
    base_sha: String,
    ack_sha: Option<String>,
    /// The repository's common git dir, once known: the dir its ref watch is keyed by.
    common_dir: Option<PathBuf>,
}

/// The rebase checks in flight (D9).
#[derive(Default)]
pub(crate) struct RebaseChecks {
    full: Option<Task<()>>,
    /// Another full check was asked for while one ran.
    full_pending: bool,
    /// The check of each repository, by common git dir.
    repos: HashMap<PathBuf, Task<()>>,
    /// Repositories whose check was asked for while it, or the full check, ran.
    repos_pending: BTreeSet<PathBuf>,
}

impl RebaseChecks {
    /// No check is running or held.
    #[cfg(test)]
    pub(crate) fn idle(&self) -> bool {
        self.full.is_none()
            && !self.full_pending
            && self.repos.is_empty()
            && self.repos_pending.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn full_running(&self) -> bool {
        self.full.is_some()
    }

    #[cfg(test)]
    pub(crate) fn is_pending(&self, common_dir: &Path) -> bool {
        self.repos_pending.contains(common_dir)
    }

    #[cfg(test)]
    pub(crate) fn repo_running(&self, common_dir: &Path) -> bool {
        self.repos.contains_key(common_dir)
    }
}

/// What the tree alone decides (a cancelled parent), and the tasks that need git to decide.
/// Only open tasks are checked: a closed or cancelled one has nothing left to rebase.
pub(crate) fn candidates(
    tree: &WorkTree,
    repositories: &[Repository],
) -> (HashMap<String, RebaseState>, Vec<Probe>) {
    let mut states = HashMap::new();
    let mut probes = Vec::new();
    for task in tree.tasks() {
        let (Some(file), Some(id)) = (task.file.as_ref(), task.id()) else {
            continue;
        };
        let Some(edge) = &file.stacked_on else {
            continue;
        };
        if file.status != Status::Open {
            continue;
        }
        let Ok(place) = Place::locate(tree, id) else {
            continue;
        };
        let Some(parent) = place.sibling(&edge.task).and_then(|p| p.file.as_ref()) else {
            continue;
        };
        if parent.status == Status::Cancelled {
            states.insert(id.to_string(), RebaseState::RetargetNeeded);
            continue;
        }
        let (Some(_), Some(branch), Some(base_sha), Some(parent_branch)) = (
            file.worktree.as_ref(),
            file.branch.clone(),
            file.base_sha.clone(),
            parent.branch.clone(),
        ) else {
            continue;
        };
        let Some(repository) = resolve_repository(tree, id, repositories) else {
            continue;
        };
        probes.push(Probe {
            id: id.to_string(),
            repository: expand_home(&repository.path),
            parent_branch,
            branch,
            base_sha,
            ack_sha: file.rebase_ack_sha.clone(),
            common_dir: None,
        });
    }
    (states, probes)
}

/// Runs git for every probe.
pub(crate) async fn probe_all(fs: Arc<dyn Fs>, probes: Vec<Probe>) -> HashMap<String, RebaseState> {
    let mut states = HashMap::new();
    for probe in probes {
        if let Some(state) = probe_one(fs.as_ref(), &probe).await {
            states.insert(probe.id, state);
        }
    }
    states
}

async fn probe_one(fs: &dyn Fs, probe: &Probe) -> Option<RebaseState> {
    let git_binary = which::which("git").ok()?;
    let repository = fs
        .open_repo(&probe.repository.join(".git"), Some(&git_binary))
        .log_err()?;
    let head = resolve_sha(repository.as_ref(), &probe.parent_branch).await?;
    if head == probe.base_sha || probe.ack_sha.as_deref() == Some(head.as_str()) {
        return None;
    }
    let contains = is_ancestor(
        &git_binary,
        &probe.repository,
        &head,
        &format!("refs/heads/{}", probe.branch),
    )
    .await
    .log_err()?;
    (!contains).then_some(RebaseState::RebaseNeeded { parent_head: head })
}

/// Whether `ancestor` is reachable from `descendant`, by `git merge-base --is-ancestor`: exit
/// code 0 is yes, 1 is no, anything else is an error (an unknown branch, say).
async fn is_ancestor(
    git_binary: &Path,
    directory: &Path,
    ancestor: &str,
    descendant: &str,
) -> anyhow::Result<bool> {
    let output = util::command::new_command(git_binary)
        .current_dir(directory)
        .args(["merge-base", "--is-ancestor", ancestor, descendant].map(OsString::from))
        .output()
        .await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(anyhow::anyhow!(
            "git merge-base --is-ancestor {ancestor} {descendant} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

impl WorkStore {
    /// What each stacked task, by id, needs. Tasks that need nothing are absent.
    pub fn rebase_states(&self) -> &HashMap<String, RebaseState> {
        &self.rebase
    }

    /// What the task `id` needs, if anything.
    pub fn rebase_state(&self, id: &str) -> Option<&RebaseState> {
        self.rebase.get(id)
    }

    /// Recomputes [`Self::rebase_states`] from the snapshot, with git on the background executor
    /// for the tasks that need it, then watches the repositories those tasks are in and no
    /// others.
    pub fn refresh_rebase(&mut self, cx: &mut Context<Self>) {
        if self.rebase_checks.full.is_some() {
            self.rebase_checks.full_pending = true;
            return;
        }
        let Some(tree) = self.snapshot() else {
            self.set_rebase(HashMap::new(), cx);
            self.reconcile_ref_watches(BTreeMap::new(), cx);
            return;
        };
        let repositories = WorkSettings::get_global(cx).repositories.clone();
        let (mut states, mut probes) = candidates(&tree, &repositories);
        if probes.is_empty() {
            self.set_rebase(states, cx);
            self.reconcile_ref_watches(BTreeMap::new(), cx);
            return;
        }
        let known = self.git_dirs.clone();
        let poll_decider = self.poll_decider;
        let fs = self.fs();
        self.stats.rebase_probes += probes.len();
        #[cfg(test)]
        let after_probe = self.after_full_probe.take();
        self.rebase_checks.full = Some(cx.spawn(async move |this, cx| {
            let (found, learned, wanted) = cx
                .background_spawn(async move {
                    let mut learned: HashMap<PathBuf, GitDirInfo> = HashMap::new();
                    let mut wanted = BTreeMap::new();
                    for probe in &mut probes {
                        let info = match known
                            .get(&probe.repository)
                            .or_else(|| learned.get(&probe.repository))
                        {
                            Some(info) => Some(info.clone()),
                            None => {
                                let info =
                                    git_dir_info(fs.as_ref(), &probe.repository, poll_decider)
                                        .await;
                                if let Some(info) = &info {
                                    learned.insert(probe.repository.clone(), info.clone());
                                }
                                info
                            }
                        };
                        if let Some(info) = info {
                            wanted.insert(info.common_dir.clone(), info.slow_poll);
                            probe.common_dir = Some(info.common_dir);
                        }
                    }
                    let found = probe_all(fs, probes).await;
                    #[cfg(test)]
                    if let Some(after_probe) = after_probe {
                        after_probe();
                    }
                    (found, learned, wanted)
                })
                .await;
            states.extend(found);
            this.update(cx, |store, cx| {
                store.rebase_checks.full = None;
                store.git_dirs.extend(learned);
                store.set_rebase(states, cx);
                store.reconcile_ref_watches(wanted, cx);
                if std::mem::take(&mut store.rebase_checks.full_pending) {
                    store.refresh_rebase(cx);
                } else {
                    for common_dir in std::mem::take(&mut store.rebase_checks.repos_pending) {
                        store.refresh_rebase_for(&common_dir, cx);
                    }
                }
            })
            .ok();
        }));
    }

    /// Rechecks only the stacked tasks in the repository whose common git dir is `common_dir`,
    /// keeping every other task's state. Held until the running full check, or this
    /// repository's running check, ends.
    pub(crate) fn refresh_rebase_for(&mut self, common_dir: &Path, cx: &mut Context<Self>) {
        if self.rebase_checks.full.is_some() || self.rebase_checks.repos.contains_key(common_dir) {
            self.rebase_checks
                .repos_pending
                .insert(common_dir.to_path_buf());
            return;
        }
        let Some(tree) = self.snapshot() else {
            return;
        };
        let repositories = WorkSettings::get_global(cx).repositories.clone();
        let (_, probes) = candidates(&tree, &repositories);
        let probes: Vec<Probe> = probes
            .into_iter()
            .map(|mut probe| {
                probe.common_dir = self
                    .git_dirs
                    .get(&probe.repository)
                    .map(|info| info.common_dir.clone());
                probe
            })
            .filter(|probe| probe.common_dir.as_deref() == Some(common_dir))
            .collect();
        if probes.is_empty() {
            return;
        }
        let ids: Vec<String> = probes.iter().map(|probe| probe.id.clone()).collect();
        let fs = self.fs();
        let key = common_dir.to_path_buf();
        self.stats.rebase_probes += probes.len();
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let found = cx
                    .background_spawn(async move { probe_all(fs, probes).await })
                    .await;
                this.update(cx, |store, cx| {
                    store.rebase_checks.repos.remove(&key);
                    let mut states = store.rebase.clone();
                    for id in &ids {
                        states.remove(id);
                    }
                    states.extend(found);
                    store.set_rebase(states, cx);
                    if store.rebase_checks.repos_pending.remove(&key) {
                        store.refresh_rebase_for(&key, cx);
                    }
                })
                .ok();
            }
        });
        self.rebase_checks.repos.insert(key, task);
    }

    pub(crate) fn set_rebase(
        &mut self,
        states: HashMap<String, RebaseState>,
        cx: &mut Context<Self>,
    ) {
        if states != self.rebase {
            self.rebase = states;
            cx.emit(crate::WorkStoreEvent::RebaseChanged);
            cx.notify();
        }
    }
}
