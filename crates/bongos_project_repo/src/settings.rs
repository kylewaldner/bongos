use std::path::PathBuf;

use bongos_project_repo_model::layout;
use settings::{RegisterSetting, Settings, SettingsContent};

use crate::create::LayoutTemplates;

/// The `project_repository` settings (user level only: where projects live is per machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct ProjectRepositorySettings {
    /// While false, projects keep the old format and nothing here is used.
    pub enabled: bool,
    /// Where new project folders go; `None` is `<data dir>/projects`.
    pub projects_dir: Option<PathBuf>,
    pub layout: LayoutTemplates,
    /// The start of every working copy's branch name.
    pub branch_prefix: String,
    /// Whether a landed working copy stays instead of being removed.
    pub keep_landed: bool,
    /// Days of idleness after which a change is shown as stale.
    pub stale_after_days: u32,
    /// Whether a landing squashes a working copy's commits into one commit on master.
    pub squash: bool,
    /// How long a change may hold the landing queue before it is sent out.
    pub queue_limit_seconds: u32,
}

impl ProjectRepositorySettings {
    /// The folder of a new project named `slug`.
    pub fn project_folder(&self, slug: &str) -> PathBuf {
        self.projects_dir
            .clone()
            .unwrap_or_else(|| paths::data_dir().join("projects"))
            .join(slug)
    }
}

impl Settings for ProjectRepositorySettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let settings = content
            .project_repository
            .clone()
            .expect("default.json defines `project_repository`");
        let templates = settings
            .layout
            .expect("default.json defines `project_repository.layout`");
        Self {
            enabled: settings
                .enabled
                .expect("default.json defines `project_repository.enabled`"),
            projects_dir: settings
                .projects_dir
                .filter(|dir| !dir.trim().is_empty())
                .map(PathBuf::from),
            branch_prefix: settings
                .branch_prefix
                .expect("default.json defines `project_repository.branch_prefix`"),
            keep_landed: settings
                .keep_landed
                .expect("default.json defines `project_repository.keep_landed`"),
            stale_after_days: settings
                .stale_after_days
                .expect("default.json defines `project_repository.stale_after_days`"),
            squash: settings
                .squash
                .expect("default.json defines `project_repository.squash`"),
            queue_limit_seconds: settings
                .queue_limit_seconds
                .expect("default.json defines `project_repository.queue_limit_seconds`"),
            layout: LayoutTemplates {
                repository: templates
                    .repository
                    .unwrap_or_else(|| layout::DEFAULT_REPOSITORY_TEMPLATE.to_string()),
                working_copies: templates
                    .working_copies
                    .unwrap_or_else(|| layout::DEFAULT_WORKING_COPIES_TEMPLATE.to_string()),
                state: templates
                    .state
                    .unwrap_or_else(|| layout::DEFAULT_STATE_TEMPLATE.to_string()),
                files: templates
                    .files
                    .unwrap_or_else(|| layout::DEFAULT_FILES_TEMPLATE.to_string()),
            },
        }
    }
}
