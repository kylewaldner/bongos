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
    /// The largest text file a landing lets through.
    pub text_file_limit_bytes: u64,
    /// The largest image a landing lets through.
    pub image_limit_bytes: u64,
    /// The regular expressions the secrets check refuses in changed text, and a snapshot
    /// withholds.
    pub secret_patterns: Vec<String>,
    /// The branch snapshots of the state go on (without `refs/heads/`).
    pub state_branch: String,
    /// The snapshot interval of a project whose `project.toml` sets none.
    pub default_snapshot_interval_minutes: u32,
    /// The least time between two snapshots, but for `project: backup now`.
    pub min_snapshot_gap_seconds: u32,
    /// The first pause after a failed push of the state branch; it doubles up to the max.
    pub push_retry_min_minutes: u32,
    pub push_retry_max_minutes: u32,
    /// Snapshots older than this are thinned to the last of each day.
    pub thinning_after_days: u32,
    /// Where run records are copied; `None` keeps them on this machine only.
    pub run_record_copy_dir: Option<PathBuf>,
    pub run_record_copy_interval_minutes: u32,
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
            text_file_limit_bytes: settings
                .text_file_limit_bytes
                .expect("default.json defines `project_repository.text_file_limit_bytes`"),
            image_limit_bytes: settings
                .image_limit_bytes
                .expect("default.json defines `project_repository.image_limit_bytes`"),
            secret_patterns: settings.secret_patterns.unwrap_or_default(),
            state_branch: settings
                .state_branch
                .unwrap_or_else(|| crate::backup::DEFAULT_STATE_BRANCH.to_string()),
            default_snapshot_interval_minutes: settings
                .default_snapshot_interval_minutes
                .unwrap_or(crate::backup::DEFAULT_SNAPSHOT_INTERVAL_MINUTES),
            min_snapshot_gap_seconds: settings
                .min_snapshot_gap_seconds
                .unwrap_or(crate::backup::DEFAULT_MIN_SNAPSHOT_GAP_SECONDS),
            push_retry_min_minutes: settings
                .push_retry_min_minutes
                .unwrap_or(crate::backup::DEFAULT_PUSH_RETRY_MIN_MINUTES),
            push_retry_max_minutes: settings
                .push_retry_max_minutes
                .unwrap_or(crate::backup::DEFAULT_PUSH_RETRY_MAX_MINUTES),
            thinning_after_days: settings
                .thinning_after_days
                .unwrap_or(crate::backup::DEFAULT_THINNING_AFTER_DAYS),
            run_record_copy_dir: settings
                .run_record_copy_dir
                .filter(|dir| !dir.trim().is_empty())
                .map(PathBuf::from),
            run_record_copy_interval_minutes: settings
                .run_record_copy_interval_minutes
                .unwrap_or(crate::backup::DEFAULT_RUN_RECORD_COPY_INTERVAL_MINUTES),
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
