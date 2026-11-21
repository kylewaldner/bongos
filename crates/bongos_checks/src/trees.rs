//! The trees a check runs in and their environments, as a feature that binds trees to lanes
//! provides them. This crate defines the traits; that feature implements them. With no
//! implementation a check runs in the task's own tree with bongos's own environment.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{App, Global};

/// Where a task's trees are, and the environment a process in each gets.
pub trait TaskTrees: 'static {
    /// The tree of `task_id` in repository `repo` that a lane serves, if any.
    fn tip(&self, task_id: &str, repo: &str) -> Option<PathBuf>;
    /// A second tree of the lane that owns the task, lent exclusively while the lease lives.
    fn second_tree(&self, task_id: &str, repo: &str) -> SecondTree;
    /// The complete environment of the tip tree of `task_id` in `repo`: bongos's own with the
    /// lane's edits applied (a variable the lane removes is absent). Empty when no lane serves
    /// the task.
    fn env(&self, task_id: &str, repo: &str) -> Vec<(String, String)>;
    /// The directories the lane declares it writes (its build directory, index directory and so
    /// on) for the tip tree of `task_id` in `repo`. A confined check may write them.
    fn declared_dirs(&self, _task_id: &str, _repo: &str) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// The answer to [`TaskTrees::second_tree`].
pub enum SecondTree {
    /// A tree lent to the caller now.
    Lease(Box<dyn SecondTreeLease>),
    /// A lane could lend one, but it is leased now.
    Busy,
    /// No lane can lend a tree for this task.
    Unavailable,
}

/// A second tree lent to a check run; released when dropped.
pub trait SecondTreeLease {
    fn path(&self) -> &Path;
    /// The complete environment of the leased tree, like [`TaskTrees::env`].
    fn env(&self) -> Vec<(String, String)>;
    /// The directories the lane declares it writes, like [`TaskTrees::declared_dirs`].
    fn declared_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// The environment a check process starts from, before the check's own variables are added and
/// its `unset` names removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvBase {
    /// A lane's complete environment: only these variables.
    Lane(Vec<(String, String)>),
    /// Bongos's own environment.
    Own,
    /// A lane's environment (bongos's own when `env` is empty) and the directories the lane
    /// declares it writes.
    Declared {
        env: Vec<(String, String)>,
        dirs: Vec<PathBuf>,
    },
}

impl EnvBase {
    /// A lane's environment when it is non-empty, else bongos's own.
    pub fn from_lane(env: Vec<(String, String)>) -> Self {
        if env.is_empty() {
            Self::Own
        } else {
            Self::Lane(env)
        }
    }

    /// [`EnvBase::from_lane`], remembering the lane's declared directories.
    fn from_lane_with_dirs(env: Vec<(String, String)>, dirs: Vec<PathBuf>) -> Self {
        if dirs.is_empty() {
            Self::from_lane(env)
        } else {
            Self::Declared { env, dirs }
        }
    }

    /// The base for the tip tree of `task_id` in `repo`; bongos's own with no implementation.
    pub fn for_tip(trees: Option<&dyn TaskTrees>, task_id: &str, repo: &str) -> Self {
        trees.map_or(Self::Own, |trees| {
            Self::from_lane_with_dirs(trees.env(task_id, repo), trees.declared_dirs(task_id, repo))
        })
    }

    /// The base for a leased second tree.
    pub fn for_lease(lease: &dyn SecondTreeLease) -> Self {
        Self::from_lane_with_dirs(lease.env(), lease.declared_dirs())
    }

    /// The directories the lane declares it writes; none for bongos's own environment.
    pub fn declared_dirs(&self) -> &[PathBuf] {
        match self {
            Self::Declared { dirs, .. } => dirs,
            Self::Lane(_) | Self::Own => &[],
        }
    }
}

/// The registered tree provider; none until a feature that binds trees to lanes sets one.
#[derive(Default)]
pub struct TaskTreesRegistry {
    trees: Option<Rc<dyn TaskTrees>>,
}

impl Global for TaskTreesRegistry {}

impl TaskTreesRegistry {
    pub fn set(trees: Rc<dyn TaskTrees>, cx: &mut App) {
        cx.default_global::<Self>().trees = Some(trees);
    }

    /// The registered implementation; `None` runs a check in the task's own tree.
    pub fn get(cx: &App) -> Option<Rc<dyn TaskTrees>> {
        cx.try_global::<Self>()
            .and_then(|registry| registry.trees.clone())
    }
}
