//! The app side of bongos projects and tasks: the `work` settings and [`WorkStore`], the global
//! entity that loads the tree under `~/.bongos` on the background executor, watches it for edits
//! made by agents and editors, and writes every change through one serial writer. The rules and
//! file formats live in `bongos_work_model`, which has no UI dependency.

pub mod actions;
mod artifacts;
mod changes;
mod commands;
mod filters;
mod focus;
mod landing_commands;
mod landing_view;
mod links;
mod master_files;
mod modals;
mod modes;
mod open;
mod overview;
mod panel;
mod project_repos;
mod projects;
mod rebase;
mod ref_watch;
mod registry;
mod repositories;
mod seams;
mod settings;
mod status;
mod store;
mod structure;
mod view;
mod working_copy_commands;
mod worktrees;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ::settings::Settings as _;
pub use bongos_work_model as model;
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Global};

pub use actions::{artifact, project, task, todo, work};
pub use artifacts::focused;
pub use changes::CHANGES_INTERVAL;
pub use commands::{DEFAULT_ACTIVITY, WorkToast, action_types, init_palette, register};
pub use commands::{ready, set_new_task_todos, show_toast, with_task};
pub use focus::{FOCUS_KEY, Focus, KVP_NAMESPACE, Target, focus_task, is_project_top, target};
pub use master_files::{MASTER_SUFFIX, MasterFile, master_file, master_file_at};
pub use modals::{Check, MINIBUFFER_NAME, Then, ask, ask_checked, pick};
pub use overview::{OldRootTask, RepositoryRow, RootProblem, RootSource, old_root_tasks};
pub use panel::{WorkPanel, init_panel, task_at_point};
pub use project_repos::{
    LandingVerdict, LandingWatch, abandon_proposal, approve_and_watch, approve_proposal, register_project_repo,
    watch_landings,
};
pub use rebase::RebaseState;
pub use ref_watch::{REF_WATCH_LATENCY, SLOW_POLL_INTERVAL};
pub use registry::{
    ActiveRun, LiveModes, LiveModesRegistry, LiveModesSource, NeedsYou, NeedsYouRegistry,
    NeedsYouSource, RowChipsRegistry, RowChipsSource, RunStart, observe_view_opened,
};
pub use repositories::{default_kind, find_repository, merge_is_ship, resolve_repository};
pub use seams::{
    ChangeId, DefinitionWriter, DefinitionWriters, ProjectSource, ProjectSources, WriteOutcome,
};
pub use settings::{DEFAULT_BRANCH_TEMPLATE, Repository, WorkSettings};
pub use status::{FocusStatus, breadcrumb};
pub use store::{
    ECHO_TTL, MAX_RETRIES, OLD_WORKTREE_NOTICE_KEY, Stats, WATCH_LATENCY, WorkCommand, WorkStore,
    WorkStoreEvent, WriteError,
};
pub use worktrees::expand_home;

/// Where the work tree lives. The app passes `~/.bongos`; tests pass a temporary directory, so no
/// test touches the real home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkDirs {
    pub root: PathBuf,
    /// The central directory for worktrees: `<data_dir>/worktrees`.
    pub worktrees: PathBuf,
}

impl WorkDirs {
    /// `<home>/.bongos` and `<data_dir>/worktrees`.
    pub fn new(home: &Path, data_dir: &Path) -> Self {
        Self {
            root: home.join(".bongos"),
            worktrees: data_dir.join("worktrees"),
        }
    }

    /// Dirs for a test: `worktrees` is a sibling of `root` named `central-worktrees`, outside it.
    pub fn for_test(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let worktrees = root.with_file_name("central-worktrees");
        Self { root, worktrees }
    }
}

/// The central worktrees directory of the global store, once [`init`] has run.
pub fn central_worktrees_dir(cx: &App) -> Option<PathBuf> {
    WorkStore::global(cx).map(|store| store.read(cx).worktrees().to_path_buf())
}

struct GlobalWorkStore(Entity<WorkStore>);

impl Global for GlobalWorkStore {}

/// Registers the `work` settings and creates the global [`WorkStore`] and [`Focus`]. The store
/// reads nothing until `work.enabled` is true.
pub fn init(fs: Arc<dyn Fs>, dirs: WorkDirs, cx: &mut App) {
    WorkSettings::register(cx);
    bongos_project_repo::init(cx);
    master_files::init(cx);
    let store = cx.new(|cx| WorkStore::new(fs, dirs, cx));
    cx.set_global(GlobalWorkStore(store));
    focus::init(cx);
    rebase::observe_focus(cx);
}

impl WorkStore {
    /// The global store, once [`init`] has run.
    pub fn global(cx: &App) -> Option<Entity<WorkStore>> {
        cx.try_global::<GlobalWorkStore>()
            .map(|global| global.0.clone())
    }
}

#[cfg(test)]
mod artifact_tests;
#[cfg(test)]
mod command_tests;
#[cfg(test)]
mod command_tests_more;
#[cfg(test)]
mod focus_tests;
#[cfg(test)]
mod landing_view_tests;
#[cfg(test)]
mod modal_route_tests;
#[cfg(test)]
mod mode_tests;
#[cfg(test)]
mod overview_tests;
#[cfg(test)]
mod panel_tests;
#[cfg(test)]
mod person_marker_tests;
#[cfg(test)]
mod privacy_tests;
#[cfg(test)]
mod rebase_tests;
#[cfg(test)]
mod ref_watch_tests;
#[cfg(test)]
mod registry_tests;
#[cfg(test)]
mod repository_tests;
#[cfg(test)]
mod seam_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod structure_tests;
#[cfg(test)]
mod top_level_tests;
#[cfg(test)]
mod working_copy_tests;
#[cfg(test)]
mod worktree_root_tests;
#[cfg(test)]
mod worktree_tests;
