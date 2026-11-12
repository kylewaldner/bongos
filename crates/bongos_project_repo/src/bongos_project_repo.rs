//! Project repositories: a project's definitions kept in a git repository, the per-machine
//! record of where each one lives, and the creation of a new one.

mod create;
mod git;
mod locations;
mod settings;

pub use create::{CreateError, CreateRequest, LayoutTemplates, ProjectRepo};
pub use git::{DEFAULT_IDENTITY_EMAIL, DEFAULT_IDENTITY_NAME, Git, GitError, Identity};
pub use locations::{Location, LocationError, RepositoryLocations};
pub use settings::ProjectRepositorySettings;

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
mod locations_tests;
