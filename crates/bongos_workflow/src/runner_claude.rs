//! The Claude runner (feature 20, D7): one headless `claude -p` process per agent node try, in the
//! node's worktree, with the rendered prompt on stdin and the task context in an appended system
//! file.
//!
//! Every limit the node declares is handed to the CLI or enforced here, and a node that cannot be
//! enforced does not start: [`ClaudeRunner::new`] refuses a CLI that is missing, logged out, not
//! checked yet, or lacks a flag the runner needs ("Claude Code 2.1.0 lacks --json-schema; update
//! it"). The result is only ever the `structured_output` of the final `result` event, checked
//! against the node's declared fields with [`schema::check`]; the free text of the answer is never
//! parsed. Each try ends with the typed result or one failure:
//!
//! - `time cap hit`: the engine's own timer on `max_minutes` stopped the process;
//! - `mcp server unavailable: <server>`: a server the node allows reported `failed` at init;
//! - `agent not installed or logged out`: the program is gone or the CLI says nobody is logged in;
//! - `permission denied: <tool>`: the first entry of `permission_denials`, even when the agent
//!   carried on and produced a result;
//! - `turn cap hit`, `cost cap hit`: the CLI stopped at `--max-turns` or `--max-budget-usd` (or,
//!   when the CLI has no budget flag, the cost passed the cap by the end);
//! - `agent exited nonzero`;
//! - `node output invalid`: no result, no structured result, or one that fails the schema.
//!
//! The process environment is cleared, then given only `PATH`, `HOME`, `LANG` and `TMPDIR` from
//! the base environment and the node's `BONGOS_*` names ([`ENV_NAMES`]); no secret is added.
//!
//! Recorded against Claude Code 2.1.322: `--max-turns` and `--append-system-prompt-file` work but
//! are missing from `--help` ([`HIDDEN_FLAGS`]); `--allowedTools` only pre-approves tools, so the
//! runner also passes `--permission-prompts none` when the CLI lists it, which denies whatever
//! would have asked.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bongos_chat::model::{Agent, Capabilities, HiddenFlag, version_at_least};
use bongos_chat::{AgentCommand, Health, HealthState, find_program, fix_logged_out, fix_not_found};
use bongos_workflow_model::definition::{Node, OutputField};
use bongos_workflow_model::{Definition, Failure, FailureReason, NodeKey, schema};
use serde_json::Value;

use crate::process::{NodeProcess, ProcessRecord, stop_all};
use crate::runtime::Finish;

/// The variables taken from the base environment; everything else in it is dropped.
pub const PASSED_ENV: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR"];

/// The variables the runner sets for a node (D7). Feature 21 knows a workflow caller by
/// `BONGOS_RUN_ID`.
pub const ENV_NAMES: &[&str] = &[
    "BONGOS_RUN_ID",
    "BONGOS_RUN_NODE",
    "BONGOS_TASK_ID",
    "BONGOS_TASK_DIR",
    "BONGOS_WORKTREE",
    "BONGOS_NODE_RESULT",
];

/// The flags every node needs; a CLI without one of them is refused.
pub const REQUIRED_FLAGS: &[&str] = &[
    "--print",
    "--output-format",
    "--verbose",
    "--model",
    "--permission-mode",
    "--allowedTools",
    "--max-turns",
    "--json-schema",
    "--append-system-prompt-file",
    "--session-id",
];

/// Flags Claude Code accepts but leaves out of `--help` (checked on 2.1.322).
pub const HIDDEN_FLAGS: &[HiddenFlag] = &[
    HiddenFlag {
        flag: "--append-system-prompt-file",
        min_version: "2.1.319",
    },
    HiddenFlag {
        flag: "--max-turns",
        min_version: "2.1.322",
    },
];

/// The files a try writes in its directory.
pub const PROMPT_FILE: &str = "prompt.md";
pub const SYSTEM_FILE: &str = "system.md";
pub const STREAM_FILE: &str = "stream.jsonl";
pub const STDERR_FILE: &str = "stderr.log";
pub const RESULT_FILE: &str = "result.json";

/// The agent defaults a node falls back to after its own settings and the definition's
/// `[defaults]`; the values of `workflows.agents` in the settings design (D19).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDefaults {
    pub model: String,
    pub permission_mode: String,
    pub max_turns: u32,
    pub max_cost_usd: f64,
    pub max_minutes: u32,
}

impl Default for AgentDefaults {
    fn default() -> Self {
        Self {
            model: "sonnet".to_string(),
            permission_mode: "acceptEdits".to_string(),
            max_turns: 40,
            max_cost_usd: 5.0,
            max_minutes: 30,
        }
    }
}

/// What one node may do and spend: its settings over the definition's defaults over
/// [`AgentDefaults`].
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCaps {
    pub model: String,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
    pub max_turns: u32,
    pub max_cost_usd: f64,
    pub max_minutes: u32,
    /// The engine's timer. `max_minutes` unless a test shortens it.
    pub max_time: Duration,
}

impl NodeCaps {
    pub fn resolve(definition: &Definition, node: &Node, defaults: &AgentDefaults) -> Self {
        let own = &node.settings;
        let shared = &definition.defaults;
        let max_minutes = own
            .max_minutes
            .or(shared.max_minutes)
            .unwrap_or(defaults.max_minutes);
        Self {
            model: own
                .model
                .clone()
                .or_else(|| shared.model.clone())
                .unwrap_or_else(|| defaults.model.clone()),
            permission_mode: own
                .permission_mode
                .clone()
                .or_else(|| shared.permission_mode.clone())
                .unwrap_or_else(|| defaults.permission_mode.clone()),
            allowed_tools: own
                .allowed_tools
                .clone()
                .or_else(|| shared.allowed_tools.clone())
                .unwrap_or_default(),
            max_turns: own
                .max_turns
                .or(shared.max_turns)
                .unwrap_or(defaults.max_turns),
            max_cost_usd: own
                .max_cost_usd
                .or(shared.max_cost_usd)
                .unwrap_or(defaults.max_cost_usd),
            max_minutes,
            max_time: Duration::from_secs(u64::from(max_minutes) * 60),
        }
    }

    /// The MCP servers the allowed tools name (`mcp__<server>__<tool>` or `mcp__<server>__*`).
    pub fn mcp_servers(&self) -> Vec<String> {
        let mut servers: Vec<String> = self
            .allowed_tools
            .iter()
            .filter_map(|tool| tool.strip_prefix("mcp__"))
            .filter_map(|rest| rest.split_once("__").map(|(server, _)| server.to_string()))
            .collect();
        servers.sort();
        servers.dedup();
        servers
    }
}

/// Everything one try needs, prepared by the engine: the caps, the declared output, the rendered
/// prompt and system text, and where it runs and records.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeNode {
    pub caps: NodeCaps,
    pub output: Vec<OutputField>,
    /// Servers `workflows.mcp_exclude` removes (`--disallowedTools mcp__<name>__*`).
    pub mcp_exclude: Vec<String>,
    pub prompt: String,
    /// The task context appended to the system prompt (D6).
    pub system: String,
    /// The node's worktree: the process's working directory.
    pub worktree: PathBuf,
    /// The try's record directory (`nodes/<node>/<branch>/<pass>-<try>/`).
    pub try_dir: PathBuf,
    pub task_id: String,
    pub task_dir: PathBuf,
    /// `BONGOS_NODE_RESULT`.
    pub node_result: PathBuf,
    /// The session id passed with `--session-id` (a UUID).
    pub session_id: String,
}

/// The Claude CLI as the runner uses it: the configured command (04's `chats.agents.claude`), the
/// program it resolved to, and its capabilities.
#[derive(Debug, Clone)]
pub struct ClaudeCli {
    pub command: String,
    pub program: PathBuf,
    pub extra_args: Vec<String>,
    pub caps: Capabilities,
}

impl ClaudeCli {
    /// The CLI for a start, or why none may start: not installed, logged out, not checked yet, or
    /// a required flag missing. Every refusal is `agent not installed or logged out` with the fix
    /// text as detail; nothing runs unenforced.
    pub fn check(
        command: &AgentCommand,
        health: &Health,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, Failure> {
        let refuse = |detail: String| {
            Failure::new(FailureReason::AgentNotInstalledOrLoggedOut, None, detail)
        };
        match &health.state {
            HealthState::NotFound => {
                return Err(refuse(fix_not_found(Agent::Claude, &command.command)));
            }
            HealthState::LoggedOut => {
                return Err(refuse(fix_logged_out(Agent::Claude, &command.command)));
            }
            HealthState::Unchecked | HealthState::Ready { .. } => {}
        }
        let Some(caps) = health.caps.clone() else {
            return Err(refuse(format!(
                "Claude Code was not checked yet, so its flags are unknown; {}",
                if health.fix.is_empty() {
                    "wait for the check or fix chats.agents.claude.command"
                } else {
                    &health.fix
                }
            )));
        };
        if let Some(flag) = missing_flag(&caps, REQUIRED_FLAGS) {
            return Err(refuse(lacks(&caps, flag)));
        }
        let program = find_program(&command.command, env)
            .ok_or_else(|| refuse(fix_not_found(Agent::Claude, &command.command)))?;
        Ok(Self {
            command: command.command.clone(),
            program,
            extra_args: command.extra_args.clone(),
            caps,
        })
    }

    /// Whether the CLI offers `flag`, in `--help` or as a known hidden flag of its version.
    pub fn has_flag(&self, flag: &str) -> bool {
        has_flag(&self.caps, flag)
    }

    /// The arguments of one try (D7), after the program: `-p --output-format stream-json
    /// --verbose --model --permission-mode --allowedTools [--disallowedTools] --max-turns
    /// [--max-budget-usd] --json-schema --append-system-prompt-file --session-id
    /// [--permission-prompts none]`, then the configured extra arguments.
    pub fn args(&self, node: &ClaudeNode, system_file: &Path) -> Vec<String> {
        let caps = &node.caps;
        let mut args: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            &caps.model,
            "--permission-mode",
            &caps.permission_mode,
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        // Variadic: each tool is its own argument, so a pattern holding a comma or a space stays
        // whole. A flag always follows.
        args.push("--allowedTools".to_string());
        args.extend(caps.allowed_tools.iter().cloned());
        if !node.mcp_exclude.is_empty() {
            args.push("--disallowedTools".to_string());
            args.extend(
                node.mcp_exclude
                    .iter()
                    .map(|server| format!("mcp__{server}__*")),
            );
        }
        args.push("--max-turns".to_string());
        args.push(caps.max_turns.to_string());
        if self.has_flag("--max-budget-usd") {
            args.push("--max-budget-usd".to_string());
            args.push(caps.max_cost_usd.to_string());
        }
        args.push("--json-schema".to_string());
        args.push(schema::json_schema(&node.output).to_string());
        args.push("--append-system-prompt-file".to_string());
        args.push(system_file.to_string_lossy().into_owned());
        args.push("--session-id".to_string());
        args.push(node.session_id.clone());
        if self.has_flag("--permission-prompts") {
            args.push("--permission-prompts".to_string());
            args.push("none".to_string());
        }
        args.extend(self.extra_args.iter().cloned());
        args
    }
}

fn has_flag(caps: &Capabilities, flag: &str) -> bool {
    caps.has_flag(flag)
        || HIDDEN_FLAGS.iter().any(|hidden| {
            hidden.flag == flag && version_at_least(caps.version(), hidden.min_version)
        })
}

fn missing_flag<'a>(caps: &Capabilities, flags: &[&'a str]) -> Option<&'a str> {
    flags.iter().copied().find(|flag| !has_flag(caps, flag))
}

fn lacks(caps: &Capabilities, flag: &str) -> String {
    let version = if caps.version().is_empty() {
        "(unknown version)"
    } else {
        caps.version()
    };
    format!("Claude Code {version} lacks {flag}; update it")
}

/// The environment of a try: `PATH`, `HOME`, `LANG` and `TMPDIR` from `base` when set, and the
/// node's [`ENV_NAMES`]. Nothing else.
pub fn launch_env(
    base: &BTreeMap<String, String>,
    run: &str,
    key: &NodeKey,
    node: &ClaudeNode,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = base
        .iter()
        .filter(|(name, _)| PASSED_ENV.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let path = |path: &Path| path.to_string_lossy().into_owned();
    for (name, value) in [
        ("BONGOS_RUN_ID", run.to_string()),
        ("BONGOS_RUN_NODE", key.to_string()),
        ("BONGOS_TASK_ID", node.task_id.clone()),
        ("BONGOS_TASK_DIR", path(&node.task_dir)),
        ("BONGOS_WORKTREE", path(&node.worktree)),
        ("BONGOS_NODE_RESULT", path(&node.node_result)),
    ] {
        env.insert(name.to_string(), value);
    }
    env
}

/// How one try ended, read from its stream: the typed result or the failure, with what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub outcome: Result<Value, Failure>,
    pub cost_usd: f64,
    pub turns: Option<u32>,
    pub session_id: Option<String>,
}

/// What [`classify`] needs to know about how the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The process exited with this code.
    Code(i32),
    /// Killed by a signal, or its status could not be read.
    Abnormal,
    /// The engine's timer stopped it.
    TimedOut,
}

/// Reads a finished try's stream (`stream.jsonl`) and stderr into its outcome. Pure. Lines that
/// are not JSON are skipped. `budget_enforced` says whether the CLI got `--max-budget-usd`;
/// without it the cost cap is checked here, after the fact.
pub fn classify(
    key: &NodeKey,
    node: &ClaudeNode,
    command: &str,
    budget_enforced: bool,
    stream: &str,
    stderr: &str,
    exit: Exit,
) -> Outcome {
    let events: Vec<Value> = stream
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let init = events.iter().find(|event| {
        event.get("type").and_then(Value::as_str) == Some("system")
            && event.get("subtype").and_then(Value::as_str) == Some("init")
    });
    let result = events
        .iter()
        .rev()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("result"));

    let session_id = result
        .or(init)
        .and_then(|event| event.get("session_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let cost_usd = result
        .and_then(|result| result.get("total_cost_usd"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let turns = result
        .and_then(|result| result.get("num_turns"))
        .and_then(Value::as_u64)
        .map(|turns| turns as u32);
    let done = |outcome: Result<Value, Failure>| Outcome {
        outcome,
        cost_usd,
        turns,
        session_id: session_id.clone(),
    };
    let fail = |reason: FailureReason, detail: String| {
        done(Err(Failure::new(reason, Some(key.clone()), detail)))
    };

    if exit == Exit::TimedOut {
        return fail(
            FailureReason::TimeCapHit,
            format!("stopped after {}", minutes(node.caps.max_time)),
        );
    }
    if let Some(server) = init.and_then(|init| unavailable_server(init, &node.caps)) {
        return fail(FailureReason::McpServerUnavailable, server);
    }

    let Some(result) = result else {
        if says_logged_out(stderr) {
            return fail(
                FailureReason::AgentNotInstalledOrLoggedOut,
                fix_logged_out(Agent::Claude, command),
            );
        }
        if exit != Exit::Code(0) {
            return fail(FailureReason::AgentExitedNonzero, exit_detail(exit, stderr));
        }
        return fail(
            FailureReason::NodeOutputInvalid,
            "the agent ended without a result".to_string(),
        );
    };

    let is_error = result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = result.get("result").and_then(Value::as_str).unwrap_or("");
    if is_error && (says_logged_out(text) || says_logged_out(stderr)) {
        return fail(
            FailureReason::AgentNotInstalledOrLoggedOut,
            fix_logged_out(Agent::Claude, command),
        );
    }
    if let Some(tool) = result
        .get("permission_denials")
        .and_then(Value::as_array)
        .and_then(|denials| denials.first())
        .map(|denial| {
            denial
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown tool")
                .to_string()
        })
    {
        return fail(FailureReason::PermissionDenied, tool);
    }
    let subtype = result.get("subtype").and_then(Value::as_str).unwrap_or("");
    let terminal = result
        .get("terminal_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    if subtype == "error_max_turns" || terminal == "max_turns" {
        return fail(
            FailureReason::TurnCapHit,
            format!("max_turns {}", node.caps.max_turns),
        );
    }
    if subtype == "error_max_budget_usd"
        || terminal == "budget_exhausted"
        || (!budget_enforced && cost_usd > node.caps.max_cost_usd)
    {
        return fail(
            FailureReason::CostCapHit,
            format!(
                "spent ${cost_usd:.2} of max_cost_usd {}",
                node.caps.max_cost_usd
            ),
        );
    }
    if exit != Exit::Code(0) {
        return fail(FailureReason::AgentExitedNonzero, exit_detail(exit, stderr));
    }
    if is_error || subtype != "success" {
        let errors = result
            .get("errors")
            .and_then(Value::as_array)
            .map(|errors| {
                errors
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        return fail(
            FailureReason::NodeOutputInvalid,
            format!("the agent ended with {subtype} {errors}")
                .trim()
                .to_string(),
        );
    }
    // Only the structured result counts; the answer's text is never read for one.
    match result.get("structured_output") {
        None | Some(Value::Null) => fail(
            FailureReason::NodeOutputInvalid,
            "the agent returned no structured result".to_string(),
        ),
        Some(value) => match schema::check(&node.output, value) {
            Ok(typed) => done(Ok(typed.to_json())),
            Err(problems) => fail(FailureReason::NodeOutputInvalid, problems),
        },
    }
}

/// The first server the node allows that the init event reports unusable.
fn unavailable_server(init: &Value, caps: &NodeCaps) -> Option<String> {
    let allowed = caps.mcp_servers();
    init.get("mcp_servers")?
        .as_array()?
        .iter()
        .filter(|server| {
            matches!(
                server.get("status").and_then(Value::as_str),
                Some("failed" | "needs-auth")
            )
        })
        .filter_map(|server| server.get("name").and_then(Value::as_str))
        .find(|name| allowed.iter().any(|allowed| allowed == name))
        .map(str::to_string)
}

fn says_logged_out(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("not logged in")
        || text.contains("invalid api key")
        || text.contains("please run /login")
}

fn exit_detail(exit: Exit, stderr: &str) -> String {
    let status = match exit {
        Exit::Code(code) => format!("exit code {code}"),
        Exit::Abnormal | Exit::TimedOut => "killed".to_string(),
    };
    match stderr.lines().rev().find(|line| !line.trim().is_empty()) {
        Some(line) => format!("{status}: {}", line.trim()),
        None => status,
    }
}

fn minutes(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{} s", duration.as_secs_f64())
    }
}

struct Try {
    run: String,
    key: NodeKey,
    try_number: u32,
    node: ClaudeNode,
    process: NodeProcess,
    started: Instant,
}

/// Runs Claude node tries: [`ClaudeRunner::start`], [`ClaudeRunner::poll`] and
/// [`ClaudeRunner::stop`] have the shape of the engine's [`Runner`](crate::runtime::Runner), which
/// dispatches agent nodes here.
pub struct ClaudeRunner {
    cli: ClaudeCli,
    base_env: BTreeMap<String, String>,
    tries: Vec<Try>,
}

impl ClaudeRunner {
    /// A runner for `cli`, whose processes get [`PASSED_ENV`] from `base_env`.
    pub fn new(cli: ClaudeCli, base_env: BTreeMap<String, String>) -> Self {
        Self {
            cli,
            base_env,
            tries: Vec::new(),
        }
    }

    pub fn cli(&self) -> &ClaudeCli {
        &self.cli
    }

    /// Starts try `try_number` of `key`: writes the prompt and system files into the try
    /// directory, then spawns the CLI in the worktree with the prompt on stdin and its output
    /// captured in `stream.jsonl` and `stderr.log`.
    pub fn start(
        &mut self,
        run: &str,
        key: &NodeKey,
        try_number: u32,
        node: ClaudeNode,
    ) -> Result<ProcessRecord, Failure> {
        let refuse =
            |reason: FailureReason, detail: String| Failure::new(reason, Some(key.clone()), detail);
        if !node.mcp_exclude.is_empty() && !self.cli.has_flag("--disallowedTools") {
            return Err(refuse(
                FailureReason::AgentNotInstalledOrLoggedOut,
                lacks(&self.cli.caps, "--disallowedTools"),
            ));
        }
        let io = |error: std::io::Error| {
            refuse(
                FailureReason::WorktreeUnavailable,
                format!("{}: {error}", node.try_dir.display()),
            )
        };
        fs::create_dir_all(&node.try_dir).map_err(io)?;
        let prompt_file = node.try_dir.join(PROMPT_FILE);
        let system_file = node.try_dir.join(SYSTEM_FILE);
        fs::write(&prompt_file, &node.prompt).map_err(io)?;
        fs::write(&system_file, &node.system).map_err(io)?;
        let stdin = File::open(&prompt_file).map_err(io)?;
        let stdout = File::create(node.try_dir.join(STREAM_FILE)).map_err(io)?;
        let stderr = File::create(node.try_dir.join(STDERR_FILE)).map_err(io)?;

        let mut command = Command::new(&self.cli.program);
        command
            .args(self.cli.args(&node, &system_file))
            .current_dir(&node.worktree)
            .env_clear()
            .envs(launch_env(&self.base_env, run, key, &node));
        let process = NodeProcess::spawn(
            command,
            Stdio::from(stdin),
            Stdio::from(stdout),
            Stdio::from(stderr),
        )
        .map_err(|error| {
            let not_found = error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|error| error.kind() == std::io::ErrorKind::NotFound);
            if not_found && !node.worktree.is_dir() {
                refuse(
                    FailureReason::WorktreeUnavailable,
                    format!("{} is missing", node.worktree.display()),
                )
            } else if not_found {
                refuse(
                    FailureReason::AgentNotInstalledOrLoggedOut,
                    fix_not_found(Agent::Claude, &self.cli.command),
                )
            } else {
                refuse(FailureReason::AgentExitedNonzero, format!("{error:#}"))
            }
        })?;
        let record = process.record();
        self.tries.push(Try {
            run: run.to_string(),
            key: key.clone(),
            try_number,
            node,
            process,
            started: Instant::now(),
        });
        Ok(record)
    }

    /// The tries that ended since the last call, and those the time cap stopped.
    pub fn poll(&mut self) -> Vec<Finish> {
        let mut finished = Vec::new();
        let mut index = 0;
        while index < self.tries.len() {
            let entry = &mut self.tries[index];
            let timed_out = entry.started.elapsed() >= entry.node.caps.max_time;
            if !entry.process.has_exited() && !timed_out {
                index += 1;
                continue;
            }
            let mut entry = self.tries.remove(index);
            let exit = if entry.process.has_exited() {
                match entry.process.child().try_status() {
                    Ok(Some(status)) => status.code().map_or(Exit::Abnormal, Exit::Code),
                    _ => Exit::Abnormal,
                }
            } else {
                entry.process.stop();
                Exit::TimedOut
            };
            finished.push(self.finish(entry, exit));
        }
        finished
    }

    /// Stops these tries (one shared grace period); they are never reported by
    /// [`ClaudeRunner::poll`].
    pub fn stop(&mut self, tries: &[(String, NodeKey)]) {
        let (stopped, kept): (Vec<Try>, Vec<Try>) = std::mem::take(&mut self.tries)
            .into_iter()
            .partition(|entry| {
                tries
                    .iter()
                    .any(|(run, key)| *run == entry.run && *key == entry.key)
            });
        self.tries = kept;
        let mut processes: Vec<NodeProcess> =
            stopped.into_iter().map(|entry| entry.process).collect();
        stop_all(&mut processes);
    }

    /// The tries in flight, as `(run, key)`.
    pub fn in_flight(&self) -> Vec<(String, NodeKey)> {
        self.tries
            .iter()
            .map(|entry| (entry.run.clone(), entry.key.clone()))
            .collect()
    }

    fn finish(&self, entry: Try, exit: Exit) -> Finish {
        let read =
            |name: &str| fs::read_to_string(entry.node.try_dir.join(name)).unwrap_or_default();
        let classified = classify(
            &entry.key,
            &entry.node,
            &self.cli.command,
            self.cli.has_flag("--max-budget-usd"),
            &read(STREAM_FILE),
            &read(STDERR_FILE),
            exit,
        );
        if let Ok(result) = &classified.outcome {
            let path = entry.node.try_dir.join(RESULT_FILE);
            let text = serde_json::to_string_pretty(result).unwrap_or_default();
            if let Err(error) = fs::write(&path, text) {
                log::error!("workflows: could not write {}: {error}", path.display());
            }
        }
        Finish {
            run: entry.run,
            key: entry.key,
            try_number: entry.try_number,
            outcome: classified.outcome,
            cost_usd: classified.cost_usd,
            turns: classified.turns,
            elapsed_ms: entry.started.elapsed().as_millis() as u64,
            session_id: classified
                .session_id
                .or(Some(entry.node.session_id.clone())),
        }
    }
}
