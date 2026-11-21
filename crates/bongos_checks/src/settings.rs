use std::path::{Path, PathBuf};

use settings::{RegisterSetting, Settings, SettingsContent};

/// The `task_checks` settings (user level only).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct TaskChecksSettings {
    /// Whether task checks are on. While false, no check runs, no record is written, and every
    /// task check command is hidden and only says "Task checks are off (task_checks.enabled)".
    pub enabled: bool,
    /// Where output files go; `None` is the task's files folder.
    pub output_dir: Option<PathBuf>,
    pub output_retention_days: u32,
    pub output_limit_mb: u64,
    pub result_file_limit_kb: u64,
    /// The time budget of a check that sets none; `None` is no budget.
    pub default_budget_minutes: Option<u32>,
    pub no_verdict_reruns: u32,
    pub no_verdict_rerun_minutes: u32,
    pub pool_wait_limit_hours: u32,
    pub kill_grace_seconds: u32,
    pub parallel_parts_max: u32,
    /// Where scratch trees for base runs go; `None` is `<data dir>/checks-scratch`.
    pub scratch_dir: Option<PathBuf>,
    pub tree_key_debounce_ms: u32,
    /// At most one live-state write per task per this many milliseconds.
    pub live_write_ms: u32,
    /// `confinement.task_checks`: run each part's process in the sandbox.
    pub confine: bool,
}

impl TaskChecksSettings {
    /// The folder output files of `task_id` go in: `output_dir` when set, else the task's
    /// folder in the project's `files` folder.
    pub fn output_folder(&self, files: &Path, task_id: &str) -> PathBuf {
        match &self.output_dir {
            Some(dir) => dir.join(task_id),
            None => files.join(task_id).join("checks"),
        }
    }

    /// `output_limit_mb` in bytes.
    pub fn output_limit_bytes(&self) -> u64 {
        self.output_limit_mb.saturating_mul(1024 * 1024)
    }

    /// The folder scratch trees go in.
    pub fn scratch_folder(&self) -> PathBuf {
        self.scratch_dir
            .clone()
            .unwrap_or_else(|| paths::data_dir().join("checks-scratch"))
    }
}

fn path(value: Option<String>) -> Option<PathBuf> {
    value
        .filter(|dir| !dir.trim().is_empty())
        .map(PathBuf::from)
}

impl Settings for TaskChecksSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let checks = content
            .task_checks
            .clone()
            .expect("default.json defines `task_checks`");
        Self {
            enabled: checks
                .enabled
                .expect("default.json defines `task_checks.enabled`"),
            output_dir: path(checks.output_dir),
            output_retention_days: checks
                .output_retention_days
                .expect("default.json defines `task_checks.output_retention_days`"),
            output_limit_mb: checks
                .output_limit_mb
                .expect("default.json defines `task_checks.output_limit_mb`"),
            result_file_limit_kb: checks
                .result_file_limit_kb
                .expect("default.json defines `task_checks.result_file_limit_kb`"),
            default_budget_minutes: checks.default_budget_minutes,
            no_verdict_reruns: checks
                .no_verdict_reruns
                .expect("default.json defines `task_checks.no_verdict_reruns`"),
            no_verdict_rerun_minutes: checks
                .no_verdict_rerun_minutes
                .expect("default.json defines `task_checks.no_verdict_rerun_minutes`"),
            pool_wait_limit_hours: checks
                .pool_wait_limit_hours
                .expect("default.json defines `task_checks.pool_wait_limit_hours`"),
            kill_grace_seconds: checks
                .kill_grace_seconds
                .expect("default.json defines `task_checks.kill_grace_seconds`"),
            parallel_parts_max: checks
                .parallel_parts_max
                .expect("default.json defines `task_checks.parallel_parts_max`"),
            scratch_dir: path(checks.scratch_dir),
            tree_key_debounce_ms: checks
                .tree_key_debounce_ms
                .expect("default.json defines `task_checks.tree_key_debounce_ms`"),
            live_write_ms: checks
                .live_write_ms
                .expect("default.json defines `task_checks.live_write_ms`"),
            confine: content
                .confinement
                .as_ref()
                .and_then(|confinement| confinement.task_checks)
                .unwrap_or(false),
        }
    }
}
