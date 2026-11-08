//! "Rebase needed" and "retarget needed" (D16), for stacked tasks only.
//!
//! A task stacked on a sibling needs a rebase when the sibling's branch moved since this task
//! was cut from it, the task's own branch does not contain the sibling's new head, and the owner
//! has not acknowledged that head with `task: clear rebase`. After a real rebase the ancestor
//! test clears the state by itself. A task whose parent is cancelled needs a retarget instead.
//!
//! [`WorkStore`] keeps the result, rechecked after every load or write, when a window gains
//! focus, and every [`CHECK_INTERVAL`] while bongos runs. Views read it with
//! [`WorkStore::rebase_state`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_work_model::Status;
use bongos_work_model::commands as model;
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;
use fs::Fs;
use gpui::{App, AppContext as _, Context};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::Workspace;

use crate::actions::task;
use crate::commands::{show_toast, submit, with_task};
use crate::repositories::resolve_repository;
use crate::settings::Repository;
use crate::worktrees::{expand_home, resolve_sha};
use crate::{WorkSettings, WorkStore};

/// How often the states are rechecked while bongos runs.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);

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
        Box::new(|id, workspace, _, cx| {
            let state =
                WorkStore::global(cx).and_then(|store| store.read(cx).rebase_state(&id).cloned());
            match state {
                Some(RebaseState::RebaseNeeded { parent_head }) => {
                    submit(workspace, cx, move |tree, ctx| {
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
    /// for the tasks that need it.
    pub fn refresh_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(tree) = self.snapshot() else {
            self.set_rebase(HashMap::new(), cx);
            return;
        };
        let repositories = WorkSettings::get_global(cx).repositories.clone();
        let (mut states, probes) = candidates(&tree, &repositories);
        if probes.is_empty() {
            self.rebase_check = None;
            self.set_rebase(states, cx);
            return;
        }
        let fs = self.fs();
        // A newer check replaces this one.
        self.rebase_check = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move { probe_all(fs, probes).await })
                .await;
            states.extend(found);
            this.update(cx, |store, cx| store.set_rebase(states, cx))
                .ok();
        }));
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
