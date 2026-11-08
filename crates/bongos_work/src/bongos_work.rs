//! The app side of bongos projects and tasks: the `work` settings and [`WorkStore`], the global
//! entity that loads the tree under `~/.bongos` on the background executor, watches it for edits
//! made by agents and editors, and writes every change through one serial writer. The rules and
//! file formats live in `bongos_work_model`, which has no UI dependency.

pub mod actions;
mod commands;
mod modals;
mod settings;
mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ::settings::Settings as _;
pub use bongos_work_model as model;
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Global};

pub use actions::{task, todo};
pub use commands::{DEFAULT_ACTIVITY, WorkToast, action_types, init_palette, register};
pub use settings::WorkSettings;
pub use store::{
    ECHO_TTL, MAX_RETRIES, Stats, WATCH_LATENCY, WorkCommand, WorkStore, WorkStoreEvent, WriteError,
};

/// Where the work tree lives. The app passes `~/.bongos`; tests pass a temporary directory, so no
/// test touches the real home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkDirs {
    pub root: PathBuf,
}

impl WorkDirs {
    /// The directory names under `home`: `<home>/.bongos`.
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".bongos"),
        }
    }
}

struct GlobalWorkStore(Entity<WorkStore>);

impl Global for GlobalWorkStore {}

/// Registers the `work` settings and creates the global [`WorkStore`]. The store reads nothing
/// until `work.enabled` is true.
pub fn init(fs: Arc<dyn Fs>, dirs: WorkDirs, cx: &mut App) {
    WorkSettings::register(cx);
    let store = cx.new(|cx| WorkStore::new(fs, dirs, cx));
    cx.set_global(GlobalWorkStore(store));
}

impl WorkStore {
    /// The global store, once [`init`] has run.
    pub fn global(cx: &App) -> Option<Entity<WorkStore>> {
        cx.try_global::<GlobalWorkStore>()
            .map(|global| global.0.clone())
    }
}

#[cfg(test)]
mod command_tests;
#[cfg(test)]
mod store_tests;
