//! The Codex runner (feature 20, D8): one headless `codex exec --json` process per agent node try,
//! in the node's worktree, with the rendered prompt on stdin. It has the shape of
//! [`ClaudeRunner`](crate::runner_claude::ClaudeRunner): `start`, `poll` and `stop`.
//!
//! Codex was not installed where this was written, so every flag and event name below is the
//! design's assumption about the Codex CLI, kept in plain constants and exercised by
//! `test-fixtures/fake-codex-exec` and the `codex-*.jsonl` fixtures (written from the documented
//! event shapes, not recorded). [`CodexCli::check`] reads `codex exec --help` and refuses a CLI
//! that lacks a flag the runner needs, so a wrong assumption stops a start instead of running
//! unenforced.
//!
//! What a node declares, on Codex:
//! - the permission mode becomes the sandbox ([`sandbox_for`]): `plan` is `read-only`,
//!   `acceptEdits`, `auto`, `manual` and `dontAsk` are `workspace-write`, `bypassPermissions` is
//!   `danger-full-access`; any other mode is refused;
//! - the output schema is written to `output-schema.json` in the try directory and passed with
//!   `--output-schema`; the result is the last `agent_message` of the stream, which the schema
//!   forces to be JSON, checked again with [`schema::check`]. Free text is never accepted;
//! - MCP is limited by server: every installed server that no allowed entry names, and every
//!   excluded server, is turned off with `-c mcp_servers.<name>.enabled=false`;
//! - the turn cap is counted from the stream's turn events and enforced by killing the process;
//! - there is no cost: Codex reports none, so the cost is 0 and the cost cap is not enforced;
//! - there is no way to append a system prompt, so the task context is put in front of the prompt
//!   on stdin (and also kept in `system.md`).
//!
//! Failures are the same closed list as the Claude runner's; `permission denied` is never reported
//! because the sandbox blocks quietly and Codex has no denial record. The environment is the same
//! as the Claude runner's: only `PATH`, `HOME`, `LANG`, `TMPDIR` and the node's `BONGOS_*` names.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use bongos_chat::model::{Agent, Capabilities};
use bongos_chat::{AgentCommand, Health, HealthState, find_program, fix_logged_out, fix_not_found};
use bongos_workflow_model::definition::{AgentKind, Node, OutputField};
use bongos_workflow_model::{Definition, Failure, FailureReason, NodeKey, schema};
use serde_json::Value;

use crate::mcp;
use crate::process::{NodeProcess, ProcessRecord, group_alive, stop_all};
use crate::runner_claude::{NodeCaps, NodeEnv, STDERR_TAIL, minutes, node_env, read_tail};
use crate::runtime::Finish;

/// The files a try writes in its directory (the stream and the result are named as the Claude
/// runner's, so a try directory reads the same for both).
pub use crate::runner_claude::{PROMPT_FILE, RESULT_FILE, STDERR_FILE, STREAM_FILE, SYSTEM_FILE};

/// The output schema handed to `--output-schema`.
pub const SCHEMA_FILE: &str = "output-schema.json";

/// The flags `codex exec` needs, each as the alternatives that count (`--help` of `exec`).
pub const REQUIRED_FLAGS: &[&[&str]] = &[
    &["--json"],
    &["--output-schema"],
    &["--sandbox", "-s"],
    &["--cd", "-C"],
    &["--model", "-m"],
    &["--config", "-c"],
];

/// What a node may do and be given, resolved by the engine. The caps are [`NodeCaps`]; its
/// `model` is unused because Codex falls back to its own default model (see [`own_model`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexNode {
    pub caps: NodeCaps,
    /// `-m <model>`; `None` leaves the model to the CLI.
    pub model: Option<String>,
    pub output: Vec<OutputField>,
    /// Servers `workflows.mcp_exclude` removes; disabled even when the node names them.
    pub mcp_exclude: Vec<String>,
    /// The MCP servers discovered for Codex (`mcp::discover`).
    pub mcp_installed: BTreeSet<String>,
    pub prompt: String,
    /// The task context, put in front of the prompt.
    pub system: String,
    pub worktree: PathBuf,
    pub try_dir: PathBuf,
    pub task_id: String,
    pub task_dir: PathBuf,
    pub node_result: PathBuf,
}

/// The model a node names itself or through `[defaults]`; Codex has no default alias, so none is
/// passed otherwise.
pub fn own_model(definition: &Definition, node: &Node) -> Option<String> {
    node.settings
        .model
        .clone()
        .or_else(|| definition.defaults.model.clone())
        .filter(|model| !model.trim().is_empty())
}

/// The sandbox a permission mode runs in, or why there is none.
pub fn sandbox_for(mode: &str) -> Result<&'static str, String> {
    match mode {
        "plan" => Ok("read-only"),
        "acceptEdits" | "auto" | "manual" | "dontAsk" => Ok("workspace-write"),
        "bypassPermissions" => Ok("danger-full-access"),
        other => Err(format!("permission mode '{other}' has no Codex sandbox")),
    }
}

/// The Codex CLI as the runner uses it.
#[derive(Debug, Clone)]
pub struct CodexCli {
    pub command: String,
    pub program: PathBuf,
    pub extra_args: Vec<String>,
    /// From `codex --help`: the version and the subcommands (`exec`, `mcp`).
    pub caps: Capabilities,
    /// From `codex exec --help`: the flags.
    pub exec_caps: Capabilities,
}

impl CodexCli {
    /// The CLI for a start, or why none may start. `exec_help` is the output of `codex exec
    /// --help`, which the health probe does not read. Every refusal is `agent not installed or
    /// logged out` with the fix text as detail.
    pub fn check(
        command: &AgentCommand,
        health: &Health,
        exec_help: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, Failure> {
        let refuse = |detail: String| {
            Failure::new(FailureReason::AgentNotInstalledOrLoggedOut, None, detail)
        };
        match &health.state {
            HealthState::NotFound => {
                return Err(refuse(fix_not_found(Agent::Codex, &command.command)));
            }
            HealthState::LoggedOut => {
                return Err(refuse(fix_logged_out(Agent::Codex, &command.command)));
            }
            HealthState::Unchecked | HealthState::Ready { .. } => {}
        }
        let Some(caps) = health.caps.clone() else {
            return Err(refuse(format!(
                "Codex was not checked yet, so its flags are unknown; {}",
                if health.fix.is_empty() {
                    "wait for the check or fix chats.agents.codex.command"
                } else {
                    &health.fix
                }
            )));
        };
        let version = if caps.version().is_empty() {
            "(unknown version)".to_string()
        } else {
            caps.version().to_string()
        };
        if !caps.has_subcommand("exec") {
            return Err(refuse(format!(
                "Codex CLI {version} has no `exec` command; update it"
            )));
        }
        let exec_caps = Capabilities::from_help(caps.version(), exec_help, &[]);
        for alternatives in REQUIRED_FLAGS {
            if !exec_caps.has_any_flag(alternatives) {
                return Err(refuse(format!(
                    "Codex CLI {version} lacks {}; update it",
                    alternatives[0]
                )));
            }
        }
        let program = find_program(&command.command, env)
            .ok_or_else(|| refuse(fix_not_found(Agent::Codex, &command.command)))?;
        Ok(Self {
            command: command.command.clone(),
            program,
            extra_args: command.extra_args.clone(),
            caps,
            exec_caps,
        })
    }

    /// Whether `codex mcp list` can be used for discovery.
    pub fn has_mcp(&self) -> bool {
        self.caps.has_subcommand("mcp")
    }

    /// The arguments of one try (D8), after the program: `exec --json --output-schema <file>
    /// [-m <model>] --sandbox <mode> -C <worktree>`, one `-c mcp_servers.<name>.enabled=false` per
    /// disabled server, the configured extra arguments, and `-` (the prompt comes on stdin).
    pub fn args(&self, node: &CodexNode, schema_file: &Path) -> Result<Vec<String>, String> {
        let sandbox = sandbox_for(&node.caps.permission_mode)?;
        let mut args: Vec<String> = [
            "exec",
            "--json",
            "--output-schema",
            &schema_file.to_string_lossy(),
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        if let Some(model) = &node.model {
            args.push("-m".to_string());
            args.push(model.clone());
        }
        args.push("--sandbox".to_string());
        args.push(sandbox.to_string());
        args.push("-C".to_string());
        args.push(node.worktree.to_string_lossy().into_owned());
        args.extend(mcp::codex_disable_args(
            &node.mcp_installed,
            &node.caps.allowed_tools,
            &node.mcp_exclude,
        ));
        args.extend(self.extra_args.iter().cloned());
        args.push("-".to_string());
        Ok(args)
    }
}

/// The environment of a try, as the Claude runner's.
pub fn launch_env(
    base: &BTreeMap<String, String>,
    run: &str,
    key: &NodeKey,
    node: &CodexNode,
) -> BTreeMap<String, String> {
    node_env(
        base,
        run,
        key,
        &NodeEnv {
            task_id: &node.task_id,
            task_dir: &node.task_dir,
            worktree: &node.worktree,
            node_result: &node.node_result,
        },
    )
}

/// The text a node's process reads: the task context, then the prompt.
pub fn stdin_text(node: &CodexNode) -> String {
    if node.system.trim().is_empty() {
        node.prompt.clone()
    } else {
        format!("{}\n\n---\n\n{}", node.system.trim_end(), node.prompt)
    }
}

/// How one try ended, as the Claude runner's [`Outcome`](crate::runner_claude::Outcome) with no
/// cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub outcome: Result<Value, Failure>,
    pub turns: Option<u32>,
    pub session_id: Option<String>,
}

/// What [`classify`] needs to know about how the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed by a signal, or its status could not be read.
    Abnormal,
    /// The engine's timer stopped it.
    TimedOut,
    /// The runner stopped it at the turn cap.
    TurnCap,
}

fn kind(event: &Value) -> &str {
    event.get("type").and_then(Value::as_str).unwrap_or("")
}

/// The message of an `error` or `turn.failed` event.
fn error_message(event: &Value) -> Option<String> {
    match kind(event) {
        "error" => event.get("message").and_then(Value::as_str),
        "turn.failed" => event
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str),
        _ => None,
    }
    .map(str::to_string)
}

/// The server named by "MCP client for `<server>` failed to start".
fn failed_server(message: &str) -> Option<&str> {
    let rest = message.split_once("MCP client for `")?.1;
    let (server, after) = rest.split_once('`')?;
    after.contains("failed to start").then_some(server)
}

/// Reads a finished try's stream and stderr into its outcome. Pure. Lines that are not JSON are
/// skipped.
pub fn classify(
    key: &NodeKey,
    node: &CodexNode,
    command: &str,
    stream: &str,
    stderr: &str,
    exit: Exit,
) -> Outcome {
    let events: Vec<Value> = stream
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let session_id = events
        .iter()
        .find(|event| kind(event) == "thread.started")
        .and_then(|event| event.get("thread_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let turns = events
        .iter()
        .filter(|event| kind(event) == "turn.completed")
        .count() as u32;
    let done = |outcome: Result<Value, Failure>| Outcome {
        outcome,
        turns: Some(turns),
        session_id: session_id.clone(),
    };
    let fail = |reason: FailureReason, detail: String| {
        done(Err(Failure::new(reason, Some(key.clone()), detail)))
    };
    let turn_cap = || {
        fail(
            FailureReason::TurnCapHit,
            format!("max_turns {}", node.caps.max_turns),
        )
    };

    if exit == Exit::TimedOut {
        return fail(
            FailureReason::TimeCapHit,
            format!("stopped after {}", minutes(node.caps.max_time)),
        );
    }
    if exit == Exit::TurnCap {
        return turn_cap();
    }
    let errors: Vec<String> = events.iter().filter_map(error_message).collect();
    let allowed = node.caps.mcp_servers();
    if let Some(server) = errors
        .iter()
        .filter_map(|message| failed_server(message))
        .find(|server| allowed.iter().any(|allowed| allowed == server))
    {
        return fail(FailureReason::McpServerUnavailable, server.to_string());
    }
    // A server the node does not use that failed to start does not matter.
    let errors: Vec<String> = errors
        .into_iter()
        .filter(|message| failed_server(message).is_none())
        .collect();
    if errors.iter().any(|message| says_logged_out(message))
        || (exit != Exit::Code(0) && says_logged_out(stderr))
    {
        return fail(
            FailureReason::AgentNotInstalledOrLoggedOut,
            fix_logged_out(Agent::Codex, command),
        );
    }
    if turns > node.caps.max_turns {
        return turn_cap();
    }
    if exit != Exit::Code(0) {
        let detail = match errors.last() {
            Some(message) => format!("{}: {message}", exit_status(exit)),
            None => exit_detail(exit, stderr),
        };
        return fail(FailureReason::AgentExitedNonzero, detail);
    }
    if let Some(message) = errors.last() {
        return fail(
            FailureReason::NodeOutputInvalid,
            format!("the agent reported an error: {message}"),
        );
    }
    let final_message = events
        .iter()
        .rev()
        .filter(|event| kind(event) == "item.completed")
        .filter_map(|event| event.get("item"))
        .find(|item| kind(item) == "agent_message")
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str);
    let Some(text) = final_message else {
        return fail(
            FailureReason::NodeOutputInvalid,
            "the agent ended without a final message".to_string(),
        );
    };
    let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
        return fail(
            FailureReason::NodeOutputInvalid,
            "the final message is not JSON".to_string(),
        );
    };
    match schema::check(&node.output, &value) {
        Ok(typed) => done(Ok(typed.to_json())),
        Err(problems) => fail(FailureReason::NodeOutputInvalid, problems),
    }
}

/// Text that says nobody is logged in to Codex (assumed wording).
fn says_logged_out(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("not logged in")
        || text.contains("codex login")
        || text.contains("401 unauthorized")
}

fn exit_status(exit: Exit) -> String {
    match exit {
        Exit::Code(code) => format!("exit code {code}"),
        Exit::Abnormal | Exit::TimedOut | Exit::TurnCap => "killed".to_string(),
    }
}

fn exit_detail(exit: Exit, stderr: &str) -> String {
    let status = exit_status(exit);
    match stderr.lines().rev().find(|line| !line.trim().is_empty()) {
        Some(line) => format!("{status}: {}", line.trim()),
        None => status,
    }
}

/// Reads a stream file line by line and keeps only what [`classify`] looks at: the first
/// `thread.started`, every `turn.completed`, every error, and the last completed `agent_message`.
fn read_stream(path: &Path) -> String {
    let Ok(file) = File::open(path) else {
        return String::new();
    };
    let mut reader = std::io::BufReader::new(file);
    let mut head: Option<String> = None;
    let mut message: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end();
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match kind(&event) {
            "thread.started" if head.is_none() => head = Some(line.to_string()),
            "turn.completed" | "turn.failed" | "error" => rest.push(line.to_string()),
            "item.completed" if event.get("item").map(kind) == Some("agent_message") => {
                message = Some(line.to_string());
            }
            _ => {}
        }
    }
    head.into_iter()
        .chain(rest)
        .chain(message)
        .collect::<Vec<_>>()
        .join("\n")
}

struct Try {
    run: String,
    key: NodeKey,
    try_number: u32,
    node: CodexNode,
    started: Instant,
    /// How far `stream.jsonl` has been scanned for turn events, and the partial line after it.
    offset: u64,
    partial: Vec<u8>,
    turns_started: u32,
}

impl Try {
    /// Counts the `turn.started` events written since the last scan.
    fn scan(&mut self) {
        let Ok(mut file) = File::open(self.node.try_dir.join(STREAM_FILE)) else {
            return;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return;
        }
        self.offset += bytes.len() as u64;
        self.partial.extend(bytes);
        while let Some(end) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            if let Ok(event) = serde_json::from_slice::<Value>(&line)
                && kind(&event) == "turn.started"
            {
                self.turns_started += 1;
            }
        }
    }
}

/// Runs Codex node tries.
pub struct CodexRunner {
    cli: CodexCli,
    base_env: BTreeMap<String, String>,
    tries: Vec<(Try, NodeProcess)>,
}

impl CodexRunner {
    pub fn new(cli: CodexCli, base_env: BTreeMap<String, String>) -> Self {
        Self {
            cli,
            base_env,
            tries: Vec::new(),
        }
    }

    pub fn cli(&self) -> &CodexCli {
        &self.cli
    }

    /// Starts try `try_number` of `key`: checks the node's MCP tools against the installed
    /// servers, writes the prompt, system and schema files into the try directory, then spawns the
    /// CLI in the worktree with the prompt on stdin and its output captured.
    pub fn start(
        &mut self,
        run: &str,
        key: &NodeKey,
        try_number: u32,
        node: CodexNode,
    ) -> Result<ProcessRecord, Failure> {
        let refuse =
            |reason: FailureReason, detail: String| Failure::new(reason, Some(key.clone()), detail);
        mcp::check_start(
            AgentKind::Codex,
            Some(key),
            &node.caps.allowed_tools,
            &node.mcp_installed,
            &node.mcp_exclude,
        )?;
        let io = |error: std::io::Error| {
            refuse(
                FailureReason::WorktreeUnavailable,
                format!("{}: {error}", node.try_dir.display()),
            )
        };
        fs::create_dir_all(&node.try_dir).map_err(io)?;
        let schema_file = node.try_dir.join(SCHEMA_FILE);
        let args = self
            .cli
            .args(&node, &schema_file)
            .map_err(|detail| refuse(FailureReason::PermissionDenied, detail))?;
        let prompt_file = node.try_dir.join(PROMPT_FILE);
        fs::write(&prompt_file, &node.prompt).map_err(io)?;
        fs::write(node.try_dir.join(SYSTEM_FILE), &node.system).map_err(io)?;
        let schema_text =
            serde_json::to_string_pretty(&schema::json_schema(&node.output)).unwrap_or_default();
        fs::write(&schema_file, schema_text).map_err(io)?;
        let stdin_file = node.try_dir.join("stdin.txt");
        fs::write(&stdin_file, stdin_text(&node)).map_err(io)?;
        let stdin = File::open(&stdin_file).map_err(io)?;
        let stdout = File::create(node.try_dir.join(STREAM_FILE)).map_err(io)?;
        let stderr = File::create(node.try_dir.join(STDERR_FILE)).map_err(io)?;

        let mut command = Command::new(&self.cli.program);
        command
            .args(args)
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
                    fix_not_found(Agent::Codex, &self.cli.command),
                )
            } else {
                refuse(FailureReason::AgentExitedNonzero, format!("{error:#}"))
            }
        })?;
        let record = process.record();
        self.tries.push((
            Try {
                run: run.to_string(),
                key: key.clone(),
                try_number,
                node,
                started: Instant::now(),
                offset: 0,
                partial: Vec::new(),
                turns_started: 0,
            },
            process,
        ));
        Ok(record)
    }

    /// The tries that ended since the last call, and those the time cap or the turn cap stopped.
    pub fn poll(&mut self) -> Vec<Finish> {
        let mut ended: Vec<(Try, NodeProcess, Exit)> = Vec::new();
        let mut index = 0;
        while index < self.tries.len() {
            let (entry, process) = &mut self.tries[index];
            let exited = process.has_exited();
            let timed_out = entry.started.elapsed() >= entry.node.caps.max_time;
            // A turn that starts after the cap's turns have all completed is over the cap.
            let over_turns = if exited || timed_out {
                false
            } else {
                entry.scan();
                entry.turns_started > entry.node.caps.max_turns
            };
            if !exited && !timed_out && !over_turns {
                index += 1;
                continue;
            }
            let (entry, mut process) = self.tries.remove(index);
            let exit = if exited {
                match process.child().try_status() {
                    Ok(Some(status)) => status.code().map_or(Exit::Abnormal, Exit::Code),
                    _ => Exit::Abnormal,
                }
            } else if timed_out {
                Exit::TimedOut
            } else {
                Exit::TurnCap
            };
            ended.push((entry, process, exit));
        }
        // One shared stop for the tries the engine ended and for exited ones whose group still has
        // a live member; a group with no live member is left alone (its id may be reused).
        let mut stopping: Vec<NodeProcess> = Vec::new();
        let mut done: Vec<(Try, Exit)> = Vec::new();
        for (entry, process, exit) in ended {
            if matches!(exit, Exit::TimedOut | Exit::TurnCap) || group_alive(process.record().pid) {
                stopping.push(process);
            }
            done.push((entry, exit));
        }
        stop_all(&mut stopping);
        done.into_iter()
            .map(|(entry, exit)| self.finish(entry, exit))
            .collect()
    }

    /// Stops these tries; they are never reported by [`CodexRunner::poll`].
    pub fn stop(&mut self, tries: &[(String, NodeKey)]) {
        let (stopped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.tries)
            .into_iter()
            .partition(|(entry, _)| {
                tries
                    .iter()
                    .any(|(run, key)| *run == entry.run && *key == entry.key)
            });
        self.tries = kept;
        let mut processes: Vec<NodeProcess> =
            stopped.into_iter().map(|(_, process)| process).collect();
        stop_all(&mut processes);
    }

    /// The tries in flight, as `(run, key)`.
    pub fn in_flight(&self) -> Vec<(String, NodeKey)> {
        self.tries
            .iter()
            .map(|(entry, _)| (entry.run.clone(), entry.key.clone()))
            .collect()
    }

    fn finish(&self, entry: Try, exit: Exit) -> Finish {
        let classified = classify(
            &entry.key,
            &entry.node,
            &self.cli.command,
            &read_stream(&entry.node.try_dir.join(STREAM_FILE)),
            &read_tail(&entry.node.try_dir.join(STDERR_FILE), STDERR_TAIL),
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
            cost_usd: 0.0,
            turns: classified.turns,
            elapsed_ms: entry.started.elapsed().as_millis() as u64,
            session_id: classified.session_id,
        }
    }
}
