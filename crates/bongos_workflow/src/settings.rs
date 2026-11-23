use std::path::PathBuf;

use bongos_workflow_model::{Ceilings, Limits, ValidationContext};
use settings::{RegisterSetting, Settings, SettingsContent};

use crate::agent_confinement::AgentConfinementSettings;
use crate::runner_claude::AgentDefaults;
use crate::runner_program::ProgramSettings;

/// The `workflows` settings (user level only).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct WorkflowsSettings {
    /// Whether workflows are on (the default). Enabling starts nothing by itself: no run starts,
    /// no process starts and nothing is written under `runs/` until a workflow is run. While
    /// false, bongos starts no run, loads no definition and every workflow command is hidden and
    /// only says "Workflows are off (workflows.enabled)".
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
    /// The caps an agent node falls back to (`workflows.agents`).
    pub agents: AgentDefaults,
    /// What a definition may declare at most (`workflows.ceilings`).
    pub ceilings: Ceilings,
    /// MCP servers agent nodes never get (`workflows.mcp_exclude`).
    pub mcp_exclude: Vec<String>,
    /// The directory holding workflow definitions; `None` means `~/.bongos/workflows`.
    pub definitions_dir: Option<PathBuf>,
    /// Whether agent steps run confined, and the state paths of each CLI (`confinement.*`).
    pub agent_confinement: AgentConfinementSettings,
}

impl WorkflowsSettings {
    /// What validation needs from the settings: the ceilings, the widest fan-out and whether
    /// programs may use the network. The installed MCP servers, the shipped programs and the
    /// definition directory are the caller's to fill.
    pub fn validation_context(&self) -> ValidationContext {
        ValidationContext {
            ceilings: self.ceilings.clone(),
            fan_out_width: self.limits.fan_out_width,
            loop_bound: Some(self.limits.loop_bound),
            allow_network: self.programs.allow_network,
            ..ValidationContext::default()
        }
    }
}

/// The values of `default.json`, for an engine made before the settings are read.
impl Default for WorkflowsSettings {
    fn default() -> Self {
        Self {
            agents: AgentDefaults::default(),
            ceilings: Ceilings::default(),
            mcp_exclude: Vec::new(),
            definitions_dir: None,
            enabled: true,
            worktrees_dir: None,
            min_free_disk_mb: 2048,
            keep_finished_days: 14,
            limits: Limits::default(),
            programs: ProgramSettings::default(),
            agent_confinement: AgentConfinementSettings {
                agent_steps: true,
                claude_state_paths: vec!["~/.claude".to_string()],
                codex_state_paths: vec!["~/.codex".to_string()],
            },
        }
    }
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
        let agents = workflows.agents.clone().unwrap_or_default();
        let agent_defaults = AgentDefaults::default();
        let ceilings = workflows.ceilings.clone().unwrap_or_default();
        let ceiling_defaults = Ceilings::default();
        let confinement = content.confinement.clone().unwrap_or_default();
        let state_paths = confinement.agent_state_paths.unwrap_or_default();
        Self {
            agent_confinement: AgentConfinementSettings {
                agent_steps: confinement.agent_steps.unwrap_or(true),
                claude_state_paths: state_paths.claude.unwrap_or_default(),
                codex_state_paths: state_paths.codex.unwrap_or_default(),
            },
            agents: AgentDefaults {
                model: agents
                    .claude
                    .and_then(|claude| claude.model)
                    .filter(|model| !model.trim().is_empty())
                    .unwrap_or(agent_defaults.model),
                permission_mode: agent_defaults.permission_mode,
                max_turns: agents.max_turns.unwrap_or(agent_defaults.max_turns).max(1),
                max_cost_usd: agents.max_cost_usd.unwrap_or(agent_defaults.max_cost_usd),
                max_minutes: agents
                    .max_minutes
                    .unwrap_or(agent_defaults.max_minutes)
                    .max(1),
            },
            ceilings: Ceilings {
                max_turns: ceilings.max_turns.unwrap_or(ceiling_defaults.max_turns),
                max_cost_usd: ceilings
                    .max_cost_usd
                    .unwrap_or(ceiling_defaults.max_cost_usd),
                max_minutes: ceilings.max_minutes.unwrap_or(ceiling_defaults.max_minutes),
                max_passes: ceilings.max_passes.unwrap_or(ceiling_defaults.max_passes),
                retries: ceilings.retries.unwrap_or(ceiling_defaults.retries),
                program_max_minutes: ceilings
                    .program_max_minutes
                    .unwrap_or(ceiling_defaults.program_max_minutes),
                allow_bypass_permissions: ceilings
                    .allow_bypass_permissions
                    .unwrap_or(ceiling_defaults.allow_bypass_permissions),
            },
            mcp_exclude: workflows.mcp_exclude.clone().unwrap_or_default(),
            definitions_dir: workflows
                .definitions_dir
                .clone()
                .filter(|dir| !dir.trim().is_empty())
                .map(PathBuf::from),
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
                loop_bound: workflows.loop_bound.unwrap_or(defaults.loop_bound),
                run_max_cost_usd: workflows.run_max_cost_usd.filter(|cap| *cap > 0.0),
            },
            programs: ProgramSettings {
                allow_network: programs
                    .allow_network
                    .unwrap_or(program_defaults.allow_network),
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
