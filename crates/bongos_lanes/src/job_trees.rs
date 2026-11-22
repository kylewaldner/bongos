//! The trees of a lane job: one per repository of the job, the primary one and the extra ones
//! other features name through [`JobRepositories`]. A repository the binding lists as a
//! persistent tree runs in the lane's tree, reset onto the job's run branch; any other gets a
//! fresh tree, as a hand start does. The trees of a lane's current job are kept in
//! [`CurrentJobTrees`] for the features that read them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use bongos_lanes_model::{JobRecord, LaneId, WaitReason};
use bongos_work_model::store::WorkTree;
use bongos_workflow::worktrees::{Git, run_branch_name};
use gpui::{App, Global};

use crate::lane_tree::{Forbidden, LastRun, ResetRequest, Saved, lane_tree, reset_tree};
use crate::locations::{LaneLocations, LocationInputs};
use crate::task_trees::SecondTreeLeases;

/// Names the repositories a job works in besides its primary one.
pub type ExtraRepositories = fn(&JobRecord, &WorkTree) -> Vec<String>;

/// The registered sources of extra repositories. With none registered a job has only its
/// primary repository.
#[derive(Default)]
pub struct JobRepositories {
    sources: Vec<ExtraRepositories>,
}

impl Global for JobRepositories {}

impl JobRepositories {
    pub fn register(source: ExtraRepositories, cx: &mut App) {
        cx.default_global::<Self>().sources.push(source);
    }

    /// The extra repositories of `job`, each once, in the order the sources name them, without
    /// `primary`.
    pub fn extra(job: &JobRecord, tree: &WorkTree, primary: &str, cx: &App) -> Vec<String> {
        let mut repositories: Vec<String> = Vec::new();
        let Some(registered) = cx.try_global::<Self>() else {
            return repositories;
        };
        for source in &registered.sources {
            for repo in source(job, tree) {
                if repo != primary && !repositories.contains(&repo) {
                    repositories.push(repo);
                }
            }
        }
        repositories
    }
}

/// The tree of each repository of one job. The primary repository is always in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobTrees {
    primary: String,
    trees: BTreeMap<String, PathBuf>,
}

impl JobTrees {
    pub fn new(primary: impl Into<String>, path: PathBuf) -> Self {
        let primary = primary.into();
        Self {
            trees: BTreeMap::from([(primary.clone(), path)]),
            primary,
        }
    }

    /// The primary repository's name.
    pub fn primary_repo(&self) -> &str {
        &self.primary
    }

    /// The primary repository's tree: the run's worktree.
    pub fn primary(&self) -> &Path {
        &self.trees[&self.primary]
    }

    /// The tree of `repo` in this job.
    pub fn tip(&self, repo: &str) -> Option<&Path> {
        self.trees.get(repo).map(PathBuf::as_path)
    }

    /// Every `(repository, tree)`, by repository name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.trees
            .iter()
            .map(|(repo, path)| (repo.as_str(), path.as_path()))
    }

    fn insert(&mut self, repo: String, path: PathBuf) {
        self.trees.insert(repo, path);
    }
}

/// The trees of each lane's current job, in memory only. Clones share one map, so a reader that
/// has no `App` (the task trees the checks ask) sees what the job start sets.
#[derive(Clone, Default)]
pub struct CurrentJobTrees(Arc<RwLock<BTreeMap<LaneId, JobTrees>>>);

impl Global for CurrentJobTrees {}

impl CurrentJobTrees {
    /// The app's map, made empty on first use.
    pub fn global(cx: &mut App) -> Self {
        cx.default_global::<Self>().clone()
    }

    pub fn set(lane: LaneId, trees: JobTrees, cx: &mut App) {
        Self::global(cx).insert(lane, trees);
    }

    pub fn of(lane: &LaneId, cx: &App) -> Option<JobTrees> {
        cx.try_global::<Self>()?.get(lane)
    }

    /// Forgets the trees of `lane`'s job when it ends.
    pub fn clear(lane: &LaneId, cx: &mut App) {
        if let Some(map) = cx.try_global::<Self>() {
            map.remove(lane);
        }
    }

    pub fn insert(&self, lane: LaneId, trees: JobTrees) {
        self.0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(lane, trees);
    }

    pub fn get(&self, lane: &LaneId) -> Option<JobTrees> {
        self.0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(lane)
            .cloned()
    }

    pub fn remove(&self, lane: &LaneId) {
        self.0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(lane);
    }
}

/// One repository of a job and where its run branch starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRepository {
    pub name: String,
    /// The primary checkout.
    pub repository: PathBuf,
    /// What a missing lane tree is added at.
    pub add_from: String,
    /// What the run branch starts from.
    pub start: String,
    pub run_branch: String,
    /// The run that last used the lane's tree of this repository.
    pub last: Option<LastRun>,
}

impl JobRepository {
    /// A repository of a task job: the run branch starts at the head of the task branch and is
    /// named from it.
    pub fn for_task(
        name: impl Into<String>,
        repository: PathBuf,
        task_branch: &str,
        run_id: &str,
        attempt: u32,
        last: Option<LastRun>,
    ) -> Self {
        Self {
            name: name.into(),
            repository,
            add_from: task_branch.to_string(),
            start: format!("refs/heads/{task_branch}"),
            run_branch: run_branch_name(task_branch, run_id, attempt),
            last,
        }
    }

    /// A repository of a project job: the run branch starts at `start` (the run's
    /// `RunTree::Lane.start`) or, unset, at the repository's base branch, and is named from it.
    pub fn for_project(
        name: impl Into<String>,
        repository: PathBuf,
        start: Option<&str>,
        base_branch: &str,
        run_id: &str,
        attempt: u32,
        last: Option<LastRun>,
    ) -> Self {
        let start = start.unwrap_or(base_branch);
        Self {
            name: name.into(),
            repository,
            add_from: start.to_string(),
            start: start.to_string(),
            run_branch: run_branch_name(start, run_id, attempt),
            last,
        }
    }
}

/// Everything a job start needs for its trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStart {
    /// The lane's locations as prepared for this job.
    pub locations: LaneLocations,
    /// The lanes of the table at their effective counts; a lane not among them is never reset.
    pub table_lanes: Vec<LaneId>,
    pub primary: JobRepository,
    pub extra: Vec<JobRepository>,
    pub save_max_file_bytes: u64,
    pub forbidden: Forbidden,
}

/// The trees of a started job, and the history entries of the saves made before the resets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedTrees {
    pub trees: JobTrees,
    pub saved: Vec<(String, Saved)>,
    /// The repositories whose second tree was leased at the start, so not reset now; the next
    /// job start resets it.
    pub second_leased: Vec<String>,
}

/// What a lane must never reset or clean, from the app's inputs: every primary checkout and the
/// project repository, every task worktree recorded in `tree`, and every location of the other
/// lanes in `all`.
pub fn forbidden_for(
    lane: &LaneId,
    inputs: &LocationInputs,
    all: &[LaneLocations],
    tree: &WorkTree,
) -> Forbidden {
    Forbidden {
        checkouts: inputs
            .repositories
            .iter()
            .map(|repository| repository.path.clone())
            .chain(inputs.project_repository.iter().cloned())
            .collect(),
        task_worktrees: tree
            .tasks()
            .into_iter()
            .filter_map(|task| task.file.as_ref()?.worktree.as_deref())
            .map(bongos_work::expand_home)
            .collect(),
        other_lanes: all
            .iter()
            .filter(|locations| &locations.lane != lane)
            .flat_map(|locations| locations.items.iter().map(|item| item.path.clone()))
            .collect(),
    }
}

/// Makes the trees of a job: for each repository, the lane's persistent tree reset onto the run
/// branch when the binding lists it, else a fresh tree from `fresh`; then the lane's second tree
/// of it, when the binding gives one, reset where it stands (`reset --hard`, `clean -fd`). A
/// second tree leased at that moment is left as it is and the start does not wait; the following
/// start resets it. The reset holds the tree like a lease, so none is lent while it runs. Any
/// failure refuses the start as an environment wait with nothing deleted; trees reset before the
/// failure stay as reset.
pub async fn start_job_trees<F>(
    git: &Git,
    start: &JobStart,
    leases: &SecondTreeLeases,
    fresh: &F,
) -> Result<StartedTrees, WaitReason>
where
    F: AsyncFn(&JobRepository) -> Result<PathBuf, String>,
{
    let lane = &start.locations.lane;
    if !start.table_lanes.contains(lane) {
        return Err(WaitReason::Environment(format!(
            "{lane} is not in the lane table; its trees are not reset"
        )));
    }
    let mut trees: Option<JobTrees> = None;
    let mut saved = Vec::new();
    let mut second_leased = Vec::new();
    for repository in std::iter::once(&start.primary).chain(&start.extra) {
        let request = |second: bool| ResetRequest {
            locations: start.locations.clone(),
            repo: repository.name.clone(),
            repository: repository.repository.clone(),
            second,
            add_from: repository.add_from.clone(),
            start: repository.start.clone(),
            run_branch: repository.run_branch.clone(),
            last: if second {
                None
            } else {
                repository.last.clone()
            },
            save_max_file_bytes: start.save_max_file_bytes,
            forbidden: start.forbidden.clone(),
        };
        let path = if lane_tree(&start.locations, &repository.name, false).is_ok() {
            let outcome = reset_tree(git, &request(false))
                .await
                .map_err(|message| WaitReason::Environment(format!("{lane}: {message}")))?;
            if let Some(save) = outcome.saved {
                saved.push((repository.name.clone(), save));
            }
            lane_tree(&start.locations, &repository.name, false)
                .map_err(WaitReason::Environment)?
                .path
                .clone()
        } else {
            fresh(repository)
                .await
                .map_err(|message| WaitReason::Environment(format!("{lane}: {message}")))?
        };
        if let Ok(second) = lane_tree(&start.locations, &repository.name, true) {
            match leases.try_hold(&second.path) {
                None => second_leased.push(repository.name.clone()),
                // A named binding, never `_`: the hold must last across the reset's await so no
                // lease is given while the tree is reset.
                Some(_hold) => {
                    reset_tree(git, &request(true))
                        .await
                        .map_err(|message| WaitReason::Environment(format!("{lane}: {message}")))?;
                }
            }
        }
        match &mut trees {
            None => trees = Some(JobTrees::new(repository.name.clone(), path)),
            Some(trees) => trees.insert(repository.name.clone(), path),
        }
    }
    let trees = trees.expect("the primary repository is always started");
    Ok(StartedTrees {
        trees,
        saved,
        second_leased,
    })
}
