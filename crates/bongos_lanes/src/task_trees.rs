//! A task's trees as the lanes serve them, keyed by task and repository so a check started by
//! hand finds them too: the tip tree of the lane whose current job is on the task, its complete
//! environment, and the second tree of the lane that owns the task's top-level task, lent
//! exclusively. Leases live only in this process's memory and are never run state; one ends when
//! its value drops. Lending never makes, resets or cleans a tree: the lane's next job start
//! resets what a lease left.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use bongos_checks::{SecondTree, SecondTreeLease, TaskTrees, TaskTreesRegistry};
use bongos_lanes_model::table::Binding;
use bongos_lanes_model::{EnvEdit, LaneId, OwnerState, TaskId, TokenError, TokenValues};
use bongos_work::OpenProjectRepos;
use gpui::{App, Global};

use crate::create::lexical;
use crate::env::{apply_env, second_tree_env, tip_env};
use crate::job_trees::CurrentJobTrees;
use crate::lane_tree::{Forbidden, check_apart_from_tips, check_own_tree, lane_tree};
use crate::locations::LaneLocations;
use crate::runs::LaneRuns;

/// The top-level task above a task (itself for a top-level task); `None` for a task not known.
pub type TopLevelOf = Arc<dyn Fn(&str) -> Option<TaskId> + Send + Sync>;

/// The tasks that still owe checks (an owed marker and no valid done record), each project's
/// read in one snapshot of its run state; an error when a read failed.
pub type OwedTasks = Arc<dyn Fn() -> anyhow::Result<BTreeSet<String>> + Send + Sync>;

/// Knows no task: nothing is lent until a top-level lookup is set.
pub fn no_top_level() -> TopLevelOf {
    Arc::new(|_| None)
}

/// The tasks owing checks in every open project repository whose run state this process has.
pub fn open_projects_owed(repos: OpenProjectRepos) -> OwedTasks {
    Arc::new(move || {
        let mut owed = BTreeSet::new();
        for loaded in repos.list() {
            let Some(state) = loaded.repo().state() else {
                continue;
            };
            owed.extend(state.read(bongos_checks::owing_tasks)?);
        }
        Ok(owed)
    })
}

/// The environment a lane's edits apply to: bongos's own process environment in the app.
pub type ProcessEnv = Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>;

/// Bongos's own process environment; a variable whose name or value is not UTF-8 is left out.
pub fn process_env() -> ProcessEnv {
    Arc::new(|| {
        std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .collect()
    })
}

/// The second trees held now, by path: by a lease, or by the job-start reset of the tree.
#[derive(Clone, Default)]
pub struct SecondTreeLeases(Arc<Mutex<BTreeSet<PathBuf>>>);

impl SecondTreeLeases {
    fn lock(&self) -> MutexGuard<'_, BTreeSet<PathBuf>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Holds `path` until the returned hold drops; `None` while something else holds it. Exactly
    /// one of any number of racing callers gets it.
    pub fn try_hold(&self, path: &Path) -> Option<TreeHold> {
        let key = lexical(path);
        if !self.lock().insert(key.clone()) {
            return None;
        }
        Some(TreeHold {
            path: key,
            leases: self.clone(),
        })
    }

    pub fn is_held(&self, path: &Path) -> bool {
        self.lock().contains(&lexical(path))
    }
}

/// One hold of a second tree, released when dropped (on any thread, and on unwinding).
pub struct TreeHold {
    path: PathBuf,
    leases: SecondTreeLeases,
}

impl TreeHold {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TreeHold {
    fn drop(&mut self) {
        self.leases.lock().remove(&self.path);
    }
}

/// What a lane's trees offer outside its jobs, set at each job start: its binding, its locations
/// as prepared for the job, its token values and the project profile's edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneTreeSetup {
    pub binding: Binding,
    pub locations: LaneLocations,
    /// The lane's token values; `repo` and `dirs` are set for each tree.
    pub values: TokenValues,
    /// The project profile's edits for the lane, applied last.
    pub profile: Vec<EnvEdit>,
    /// What the job start refused to reset; a second tree that is, holds or sits inside any of
    /// it is never lent.
    pub forbidden: Forbidden,
    /// The repositories whose second tree the job start checked
    /// ([`crate::StartedTrees::second_checked`]); only those are lent.
    pub second_checked: BTreeSet<String>,
}

impl LaneTreeSetup {
    fn values(&self, repo: &str) -> TokenValues {
        TokenValues {
            repo: repo.to_string(),
            dirs: self.locations.dir_values(&[]),
            ..self.values.clone()
        }
    }

    /// The edits of the tip tree of `repo`.
    pub fn tip_edits(&self, repo: &str) -> Result<Vec<EnvEdit>, TokenError> {
        tip_env(&self.binding, &self.values(repo), &self.profile)
    }

    /// The lane's second tree of `repo`, when the binding gives one.
    pub fn second_tree(&self, repo: &str) -> Option<&Path> {
        lane_tree(&self.locations, repo, true)
            .ok()
            .map(|location| location.path.as_path())
    }

    /// The edits of the second tree of `repo`: `{dir.<name>}` is the `<name>-base` directory for
    /// each name its `base_tree.dirs` lists, and the tip's directory for the rest.
    pub fn second_edits(&self, repo: &str) -> Result<Vec<EnvEdit>, TokenError> {
        second_tree_env(
            &self.binding,
            repo,
            &self.locations,
            &self.values(repo),
            &self.profile,
        )
    }
}

struct Inner {
    runs: LaneRuns,
    jobs: CurrentJobTrees,
    top_level: RwLock<TopLevelOf>,
    /// `None` until set: a `Done` owner then lends nothing.
    owed: RwLock<Option<OwedTasks>>,
    process_env: ProcessEnv,
    setups: RwLock<BTreeMap<LaneId, LaneTreeSetup>>,
    leases: SecondTreeLeases,
}

impl Global for LaneTaskTrees {}

/// The trees of tasks as lanes serve them, over the lanes state ([`LaneRuns`]), the trees of
/// each lane's current job ([`CurrentJobTrees`]) and each lane's setup. Clones share everything.
#[derive(Clone)]
pub struct LaneTaskTrees {
    inner: Arc<Inner>,
}

impl LaneTaskTrees {
    pub fn new(
        runs: LaneRuns,
        jobs: CurrentJobTrees,
        top_level: TopLevelOf,
        process_env: ProcessEnv,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                runs,
                jobs,
                top_level: RwLock::new(top_level),
                owed: RwLock::new(None),
                process_env,
                setups: RwLock::default(),
                leases: SecondTreeLeases::default(),
            }),
        }
    }

    /// The app's trees, once [`Self::register`] ran.
    pub fn global(cx: &App) -> Option<Self> {
        cx.try_global::<Self>().cloned()
    }

    /// Makes these the app's trees and the checks' tree provider.
    pub fn register(&self, cx: &mut App) {
        cx.set_global(self.clone());
        TaskTreesRegistry::set(Rc::new(self.clone()), cx);
    }

    /// Replaces the lookup of a task's top-level task, as the dispatcher's graph moves.
    pub fn set_top_level(&self, top_level: TopLevelOf) {
        *self
            .inner
            .top_level
            .write()
            .unwrap_or_else(|error| error.into_inner()) = top_level;
    }

    /// Sets how the tasks that still owe checks are read; until then a `Done` owner lends nothing.
    pub fn set_owed(&self, owed: OwedTasks) {
        *self
            .inner
            .owed
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(owed);
    }

    fn top_level_of(&self, task_id: &str) -> Option<TaskId> {
        let top_level = self
            .inner
            .top_level
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        top_level(task_id)
    }

    /// Whether `top` or a task under it still owes checks, from one read of the markers and done
    /// records. A read that fails, or no way to read them, owes nothing here: lending is refused.
    fn owes_checks_under(&self, top: &TaskId) -> bool {
        let owed = self
            .inner
            .owed
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(owed) = owed else {
            return false;
        };
        match owed() {
            Ok(tasks) => tasks
                .iter()
                .any(|task| self.top_level_of(task).as_ref() == Some(top)),
            Err(error) => {
                log::error!("reading the checks owed under {top}: {error:#}");
                false
            }
        }
    }

    /// Whether the owner of `top`, in `state`, lends its lane's second tree: an owner that holds
    /// the task, or a `Done` one while checks are still owed under its task.
    fn lends_second_tree(&self, top: &TaskId, state: OwnerState) -> bool {
        match state {
            OwnerState::Active | OwnerState::Stopped | OwnerState::Halted => true,
            OwnerState::Done => self.owes_checks_under(top),
            OwnerState::Ahead | OwnerState::Prepared => false,
        }
    }

    /// The lanes with a setup.
    pub fn lanes(&self) -> Vec<LaneId> {
        self.inner
            .setups
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// The holds on the lanes' second trees, which the job start shares.
    pub fn leases(&self) -> SecondTreeLeases {
        self.inner.leases.clone()
    }

    /// Sets `lane`'s setup at its job start, only after [`crate::start_job_trees`] returned `Ok`
    /// for the same locations and forbidden places, so a lane whose trees were refused lends none.
    /// The dispatcher calls it when the job's run started.
    pub fn set_lane(&self, lane: LaneId, setup: LaneTreeSetup) {
        self.inner
            .setups
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(lane, setup);
    }

    /// Forgets `lane`, as when it leaves the lane table; a lease already given stays valid until
    /// it drops, and no new one is given.
    pub fn forget_lane(&self, lane: &LaneId) {
        self.inner
            .setups
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(lane);
    }

    fn setup(&self, lane: &LaneId) -> Option<LaneTreeSetup> {
        self.inner
            .setups
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(lane)
            .cloned()
    }

    /// The lane whose current job is on `task_id`, and its tree of `repo`.
    fn tip_of(&self, task_id: &str, repo: &str) -> Option<(LaneId, PathBuf)> {
        let lane = self.inner.runs.job_lane(task_id)?;
        let path = self.inner.jobs.get(&lane)?.tip(repo)?.to_path_buf();
        Some((lane, path))
    }

    /// The tree of `repo` of the lane whose current job is on `task_id`.
    pub fn tip(&self, task_id: &str, repo: &str) -> Option<PathBuf> {
        self.tip_of(task_id, repo).map(|(_, path)| path)
    }

    /// The complete environment of [`Self::tip`]'s tree: bongos's own with the binding's `env`
    /// and the profile's edits applied. Empty when no lane's job is on the task.
    pub fn env(&self, task_id: &str, repo: &str) -> Vec<(String, String)> {
        let Some((lane, _)) = self.tip_of(task_id, repo) else {
            return Vec::new();
        };
        let edits = match self.setup(&lane).map(|setup| setup.tip_edits(repo)) {
            Some(Ok(edits)) => edits,
            Some(Err(error)) => {
                log::error!("{lane}: the environment of its tree of {repo}: {error}");
                Vec::new()
            }
            None => Vec::new(),
        };
        apply_env((self.inner.process_env)(), &edits)
    }

    /// Lends the second tree of `repo` of the lane named in the owner record of `task_id`'s
    /// top-level task, whether or not that lane's current job is on this task: while the owner
    /// is active, stopped or halted, or done while checks are still owed under the task.
    pub fn second_tree(&self, task_id: &str, repo: &str) -> SecondTree {
        let Some(top) = self.top_level_of(task_id) else {
            return SecondTree::Unavailable;
        };
        let Some((lane, state, exists)) = self.inner.runs.owner_lane(&top) else {
            return SecondTree::Unavailable;
        };
        if !exists || !self.lends_second_tree(&top, state) {
            return SecondTree::Unavailable;
        }
        let Some(setup) = self.setup(&lane) else {
            return SecondTree::Unavailable;
        };
        if !setup.second_checked.contains(repo) {
            return SecondTree::Unavailable;
        }
        let Some(path) = setup.second_tree(repo) else {
            return SecondTree::Unavailable;
        };
        // The holder checks out and cleans in the lent tree: it gets the same checks as a reset.
        if let Err(message) = check_own_tree(path, path, &setup.forbidden)
            .and_then(|_| check_apart_from_tips(path, &setup.locations))
        {
            log::error!("{lane}: {message}");
            return SecondTree::Unavailable;
        }
        let edits = match setup.second_edits(repo) {
            Ok(edits) => edits,
            Err(error) => {
                log::error!("{lane}: the environment of its second tree of {repo}: {error}");
                return SecondTree::Unavailable;
            }
        };
        let Some(hold) = self.inner.leases.try_hold(path) else {
            return SecondTree::Busy;
        };
        // Lending makes nothing: a second tree not made yet is made by the lane's next job start.
        // Only a linked worktree is lent (its `.git` is a file); a checkout of its own is not.
        let is_dir = |path: &Path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
        };
        let is_linked = std::fs::symlink_metadata(hold.path().join(".git"))
            .is_ok_and(|metadata| metadata.file_type().is_file());
        if !is_dir(hold.path()) {
            return SecondTree::Unavailable;
        }
        if !is_linked {
            log::error!(
                "{lane}: {} is not a linked worktree; it is not lent",
                hold.path().display()
            );
            return SecondTree::Unavailable;
        }
        let env = apply_env((self.inner.process_env)(), &edits);
        SecondTree::Lease(Box::new(LaneLease { hold, env }))
    }
}

/// A lane's second tree lent to one caller; released when dropped.
pub struct LaneLease {
    hold: TreeHold,
    env: Vec<(String, String)>,
}

impl SecondTreeLease for LaneLease {
    fn path(&self) -> &Path {
        self.hold.path()
    }

    fn env(&self) -> Vec<(String, String)> {
        self.env.clone()
    }
}

impl TaskTrees for LaneTaskTrees {
    fn tip(&self, task_id: &str, repo: &str) -> Option<PathBuf> {
        LaneTaskTrees::tip(self, task_id, repo)
    }

    fn second_tree(&self, task_id: &str, repo: &str) -> SecondTree {
        LaneTaskTrees::second_tree(self, task_id, repo)
    }

    fn env(&self, task_id: &str, repo: &str) -> Vec<(String, String)> {
        LaneTaskTrees::env(self, task_id, repo)
    }
}
