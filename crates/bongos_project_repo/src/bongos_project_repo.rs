//! Project repositories: a project's definitions kept in a git repository, the per-machine
//! record of where each one lives, the creation of a new one, and its live state store.

mod create;
mod git;
mod loader;
mod locations;
mod registries;
pub mod router;
mod settings;
mod sink;
mod state;
mod state_hooks;
mod state_ops;

pub use create::{CreateError, CreateRequest, LayoutTemplates, ProjectRepo};
pub use create::MAIN_BRANCH;
pub use git::{
    CommitId, DEFAULT_IDENTITY_EMAIL, DEFAULT_IDENTITY_NAME, Git, GitError, Identity, TreeEntry,
    TreeId, TreeRef,
};
pub use loader::{LoadedProject, StateHandle, load_definition_tree};
pub use locations::{Location, LocationError, RepositoryLocations};
pub use registries::{ActivitySource, ActivitySources, StateExporter, StateExporters};
pub use settings::ProjectRepositorySettings;
pub use sink::{LocalSink, RefusingSink, RepoCommand, RepoReply, StateSink};
pub use state::{
    KvRead, KvTx, OpenError, StateEvent, StateRead, StateReader, StateStore, StateTx, StateWriter,
    VERIFIED_COMMIT_KEY,
};
pub use state_hooks::{StateApplyChange, StateApplyHook, StateApplyHooks, TaskTouch};
pub use state_ops::{SETTABLE_FIELDS, StateOp};

/// Registers the settings of project repositories.
pub fn init(cx: &mut gpui::App) {
    use ::settings::Settings as _;
    ProjectRepositorySettings::register(cx);
}

#[cfg(test)]
mod create_tests;
#[cfg(test)]
mod git_tests;
#[cfg(test)]
mod loader_tests;
#[cfg(test)]
mod locations_tests;
#[cfg(test)]
mod router_tests;
#[cfg(test)]
mod sink_tests;
#[cfg(test)]
mod state_hooks_tests;
#[cfg(test)]
mod state_ops_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod test_support;
