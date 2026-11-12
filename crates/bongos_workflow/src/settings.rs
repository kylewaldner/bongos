use std::path::PathBuf;

use bongos_workflow_model::Limits;
use settings::{RegisterSetting, Settings, SettingsContent};

use crate::runner_program::ProgramSettings;

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
    /// The run, session and branch limits and the retry and width defaults.
    pub limits: Limits,
    /// How program nodes run and poll (`workflows.programs`, `workflows.poll_interval_minutes`,
    /// `workflows.wait_give_up_days`).
    pub programs: ProgramSettings,
}

impl Settings for WorkflowsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let workflows = content
            .workflows
            .clone()
            .expect("default.json defines `workflows`");
        let defaults = Limits::default();
        let program_defaults = ProgramSettings::default();
        let programs = workflows.programs.clone().unwrap_or_default();
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
            limits: Limits {
                max_concurrent_runs: workflows
                    .max_concurrent_runs
                    .unwrap_or(defaults.max_concurrent_runs)
                    .max(1),
                max_concurrent_sessions: workflows
                    .max_concurrent_sessions
                    .map(|sessions| sessions.max(1)),
                max_parallel_branches: workflows
                    .max_parallel_branches
                    .unwrap_or(defaults.max_parallel_branches)
                    .max(1),
                fan_out_width: workflows
                    .fan_out_width
                    .unwrap_or(defaults.fan_out_width)
                    .max(1),
                retries: workflows.retries.unwrap_or(defaults.retries),
            },
            programs: ProgramSettings {
                python: programs.python.filter(|python| !python.trim().is_empty()),
                max_minutes: programs
                    .max_minutes
                    .unwrap_or(program_defaults.max_minutes)
                    .max(1),
                max_output_mb: programs
                    .max_output_mb
                    .unwrap_or(program_defaults.max_output_mb)
                    .max(1),
                poll_interval_minutes: workflows
                    .poll_interval_minutes
                    .unwrap_or(program_defaults.poll_interval_minutes)
                    .max(1),
                wait_give_up_days: workflows
                    .wait_give_up_days
                    .unwrap_or(program_defaults.wait_give_up_days),
            },
        }
    }
}
