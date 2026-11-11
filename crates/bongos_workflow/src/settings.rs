use std::path::PathBuf;

use settings::{RegisterSetting, Settings, SettingsContent};

/// The `workflows` settings (user level only).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct WorkflowsSettings {
    /// Whether workflows are on. While false, bongos starts no run, loads no definition and every
    /// workflow command is hidden and only says "Workflows are off (workflows.enabled)".
    pub enabled: bool,
    /// The directory holding run and branch worktrees; `None` means
    /// `<repo>/.bongos-worktrees/.runs/`.
    pub worktrees_dir: Option<PathBuf>,
    /// The least free space, in MB, on the disk holding the run worktrees for a run to start.
    pub min_free_disk_mb: u64,
    /// Days after which finished runs are offered to `workflow: clear finished`.
    pub keep_finished_days: u64,
}

impl Settings for WorkflowsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let workflows = content
            .workflows
            .clone()
            .expect("default.json defines `workflows`");
        Self {
            enabled: workflows
                .enabled
                .expect("default.json defines `workflows.enabled`"),
            worktrees_dir: workflows
                .worktrees_dir
                .filter(|dir| !dir.trim().is_empty())
                .map(PathBuf::from),
            min_free_disk_mb: workflows
                .min_free_disk_mb
                .expect("default.json defines `workflows.min_free_disk_mb`"),
            keep_finished_days: workflows
                .keep_finished_days
                .expect("default.json defines `workflows.keep_finished_days`"),
        }
    }
}
