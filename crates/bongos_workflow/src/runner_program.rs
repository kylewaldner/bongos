//! The program runner (feature 20, D9): `runner::program` runs a node's Python script with no
//! model, as `<python> -I -B shim.py <policy.json> <script>` in the node's worktree with stdin
//! closed. It has the shape of the agent runners: `start`, `poll` and `stop`.
//!
//! - Interpreter: `workflows.programs.python`, or `python3` then `python` on `PATH`, at least
//!   Python 3.8 ([`Interpreter::find`]); otherwise nothing starts ("program interpreter missing").
//! - Inputs: the read-only state document (`state.json`, mode 0444, in the try directory) at
//!   `$BONGOS_STATE`; the script writes its typed result to `$BONGOS_RESULT` in the node's scratch
//!   directory. The run record is never in a writable path.
//! - Permissions (`permissions = { commands, network, mcp }`) are enforced twice. First by the OS
//!   sandbox (Bubblewrap on Linux): writes only in the node's worktree, its scratch directory and
//!   the repository's common git directory (and the sandbox's own `/tmp`), network blocked unless
//!   declared and then restricted to the declared hosts. When the sandbox cannot be created the
//!   script runs with the second layer alone and the try's `summary.json` says
//!   "not sandboxed: <reason>". Second by the embedded shim's audit hook (`shim.py`), which denies
//!   unlisted programs, writes outside the writable paths and undeclared connections. A denial
//!   reaches bongos through a pipe (file descriptor 3, kept in `denials.jsonl`), so it fails the
//!   node even when the script caught the `PermissionError`. MCP is refused in v1.
//! - Caps: `max_minutes` (the node's, else `workflows.programs.max_minutes`), the captured output
//!   (`workflows.programs.max_output_mb`; the rest is dropped and the process stopped) and the
//!   result size ([`RESULT_CAP`]). Halt and cancel stop the process group within 5 s.
//! - Polling: a result whose field equals the `polls` value is reported (the pass is recorded and
//!   the scheduler does not route it), then the node sleeps with no process and no cost and the
//!   runner reruns it after the interval, until a result other than the value or until the
//!   give-up time passes ("wait timed out").
//!
//! The environment is the agent runners' (`PATH`, `HOME`, `LANG`, the node's `BONGOS_*` names)
//! plus `BONGOS_STATE` and `BONGOS_RESULT`; `TMPDIR` points at `tmp/` in the node's scratch
//! directory, the one temporary directory both layers let it write. A program costs nothing.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bongos_chat::find_program;
use bongos_workflow_model::definition::{Node, OutputField, Permissions, Polls};
use bongos_workflow_model::{Failure, FailureReason, NodeKey, StateDoc, TypedResult, schema};
use sandbox::{
    CommandAndArgs, GitSandboxPolicy, Sandbox, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy,
};
use serde_json::{Value, json};

use crate::process::{NodeProcess, ProcessRecord, group_alive, stop_all};
use crate::runner_claude::{NodeEnv, STDERR_TAIL, minutes, node_env, read_tail};
use crate::runtime::Finish;

/// The shim every program runs under, embedded and written into each try directory.
pub const SHIM: &str = include_str!("shim.py");

/// The files a try writes in its directory.
pub const SHIM_FILE: &str = "shim.py";
pub const POLICY_FILE: &str = "policy.json";
pub const STATE_FILE: &str = "state.json";
pub const STDOUT_FILE: &str = "stdout.log";
pub const STDERR_FILE: &str = "stderr.log";
pub const DENIALS_FILE: &str = "denials.jsonl";
pub const SUMMARY_FILE: &str = "summary.json";
/// The checked result, kept once the try succeeded.
pub const RESULT_FILE: &str = "result.json";

/// The file in the node's scratch directory the script writes its result to (`$BONGOS_RESULT`).
pub const SCRATCH_RESULT_FILE: &str = "result.json";

/// The variables the program runner sets besides the agent runners' `BONGOS_*` names.
pub const PROGRAM_ENV_NAMES: &[&str] = &["BONGOS_STATE", "BONGOS_RESULT"];

/// The largest result a script may write.
pub const RESULT_CAP: u64 = 256 * 1024;

/// The oldest Python the shim runs on.
pub const MIN_PYTHON: (u32, u32) = (3, 8);

/// What to do when no usable interpreter is found.
pub const INTERPRETER_FIX: &str = "install Python 3.8+ or set workflows.programs.python";

/// Why a node with MCP permissions does not start in v1.
pub const MCP_REFUSAL: &str = "MCP from program nodes needs the bongos MCP server (feature 21)";

/// The file descriptor the shim writes denials to.
const DENIALS_FD: i32 = 3;

/// The most of `denials.jsonl` that is kept.
const DENIALS_CAP: u64 = 1024 * 1024;

/// How long a finished try waits for its output to drain after the process group is gone.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

const MB: u64 = 1024 * 1024;

/// The program settings (`workflows.programs.*`, `workflows.poll_interval_minutes` and
/// `workflows.wait_give_up_days`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramSettings {
    /// The interpreter; `None` looks up `python3`, then `python`.
    pub python: Option<String>,
    pub max_minutes: u32,
    pub max_output_mb: u64,
    pub poll_interval_minutes: u32,
    pub wait_give_up_days: u32,
}

impl Default for ProgramSettings {
    fn default() -> Self {
        Self {
            python: None,
            max_minutes: 10,
            max_output_mb: 10,
            poll_interval_minutes: 5,
            wait_give_up_days: 7,
        }
    }
}

/// The Python a program runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct Interpreter {
    pub program: PathBuf,
    pub version: (u32, u32),
}

impl Interpreter {
    /// The configured interpreter, or `python3` then `python` on the `PATH` of `env`, when it is
    /// Python 3.8 or newer; otherwise `program interpreter missing` with what to do.
    pub fn find(configured: Option<&str>, env: &BTreeMap<String, String>) -> Result<Self, Failure> {
        let candidates: Vec<&str> = match configured.map(str::trim).filter(|c| !c.is_empty()) {
            Some(configured) => vec![configured],
            None => vec!["python3", "python"],
        };
        let mut too_old = None;
        for candidate in candidates {
            let Some(program) = find_program(candidate, env) else {
                continue;
            };
            match python_version(&program, env) {
                Some(version) if version >= MIN_PYTHON => return Ok(Self { program, version }),
                Some((major, minor)) => {
                    too_old = Some(format!(
                        "{} is Python {major}.{minor}; {INTERPRETER_FIX}",
                        program.display()
                    ));
                }
                None => {}
            }
        }
        Err(Failure::new(
            FailureReason::ProgramInterpreterMissing,
            None,
            too_old.unwrap_or_else(|| INTERPRETER_FIX.to_string()),
        ))
    }
}

/// The `major.minor` version `program` reports, or `None` when it does not run as Python.
#[allow(
    clippy::disallowed_methods,
    reason = "a short probe of the interpreter when a program node starts"
)]
fn python_version(program: &Path, env: &BTreeMap<String, String>) -> Option<(u32, u32)> {
    let output = Command::new(program)
        .args([
            "-I",
            "-c",
            "import sys; sys.stdout.write('%d.%d' % sys.version_info[:2])",
        ])
        .env_clear()
        .envs(
            env.iter()
                .filter(|(name, _)| *name == "PATH" || *name == "LANG"),
        )
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (major, minor) = text.trim().split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// What a program node may spend.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramCaps {
    pub max_minutes: u32,
    /// The runner's timer. `max_minutes` unless a test shortens it.
    pub max_time: Duration,
    /// `workflows.programs.max_output_mb` in bytes: stdout and stderr together.
    pub max_output_bytes: u64,
}

impl ProgramCaps {
    /// The node's `max_minutes`, else the setting; the output cap from the setting.
    pub fn resolve(node: &Node, settings: &ProgramSettings) -> Self {
        let max_minutes = node.settings.max_minutes.unwrap_or(settings.max_minutes);
        Self {
            max_minutes,
            max_time: Duration::from_secs(u64::from(max_minutes) * 60),
            max_output_bytes: settings.max_output_mb.saturating_mul(MB),
        }
    }
}

/// A node's `polls`, with the settings' defaults filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct PollPlan {
    /// The result value that means "not yet".
    pub value: String,
    pub interval: Duration,
    pub give_up: Duration,
    pub interval_minutes: u32,
    pub give_up_days: u32,
}

impl PollPlan {
    pub fn resolve(polls: &Polls, settings: &ProgramSettings) -> Self {
        let interval_minutes = polls
            .interval_minutes
            .unwrap_or(settings.poll_interval_minutes)
            .max(1);
        let give_up_days = polls.give_up_days.unwrap_or(settings.wait_give_up_days);
        Self {
            value: polls.value.clone(),
            interval: Duration::from_secs(u64::from(interval_minutes) * 60),
            give_up: Duration::from_secs(u64::from(give_up_days) * 24 * 60 * 60),
            interval_minutes,
            give_up_days,
        }
    }

    /// Whether `result` says "not yet": a field holds the value (as the scheduler reads it).
    pub fn is_waiting(&self, result: &TypedResult) -> bool {
        result
            .fields
            .iter()
            .any(|field| field.value.as_str() == Some(self.value.as_str()))
    }
}

/// Everything one program try needs, prepared by the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramNode {
    /// The script, resolved to a file.
    pub script: PathBuf,
    pub output: Vec<OutputField>,
    pub permissions: Permissions,
    pub caps: ProgramCaps,
    pub polls: Option<PollPlan>,
    /// The state document; its `paths` are `worktree` and `scratch`.
    pub state: StateDoc,
    /// The node's worktree: the working directory, writable.
    pub worktree: PathBuf,
    /// The node's scratch directory, writable; holds `$BONGOS_RESULT` and `TMPDIR`.
    pub scratch: PathBuf,
    /// The repository's common git directory, writable so the script can commit.
    pub git_common_dir: Option<PathBuf>,
    /// The try's record directory (`nodes/<node>/<branch>/<pass>-<try>/`); never writable.
    pub try_dir: PathBuf,
    pub task_id: String,
    pub task_dir: PathBuf,
    /// `BONGOS_NODE_RESULT`.
    pub node_result: PathBuf,
}

impl ProgramNode {
    /// `$BONGOS_RESULT`.
    pub fn result_path(&self) -> PathBuf {
        self.scratch.join(SCRATCH_RESULT_FILE)
    }

    /// The paths the script may write: the worktree, the scratch directory and the common git
    /// directory.
    pub fn writable_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.worktree.clone(), self.scratch.clone()];
        paths.extend(self.git_common_dir.clone());
        paths
    }

    /// The OS sandbox policy of the node.
    pub fn sandbox_policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: vec![self.worktree.clone(), self.scratch.clone()],
            },
            network: if self.permissions.network.is_empty() {
                SandboxNetPolicy::Blocked
            } else {
                SandboxNetPolicy::Restricted {
                    allowed_domains: self.permissions.network.clone(),
                }
            },
            git: match &self.git_common_dir {
                Some(dir) => GitSandboxPolicy::Allowed {
                    git_dirs: vec![dir.clone()],
                },
                None => GitSandboxPolicy::default(),
            },
        }
    }
}

/// The environment of a program: the agent runners' names, `BONGOS_STATE`, `BONGOS_RESULT` and
/// `TMPDIR` in the scratch directory.
pub fn launch_env(
    base: &BTreeMap<String, String>,
    run: &str,
    key: &NodeKey,
    node: &ProgramNode,
    state_file: &Path,
) -> BTreeMap<String, String> {
    let mut env = node_env(
        base,
        run,
        key,
        &NodeEnv {
            task_id: &node.task_id,
            task_dir: &node.task_dir,
            worktree: &node.worktree,
            node_result: &node.node_result,
        },
    );
    let path = |path: &Path| path.to_string_lossy().into_owned();
    env.insert("BONGOS_STATE".to_string(), path(state_file));
    env.insert("BONGOS_RESULT".to_string(), path(&node.result_path()));
    env.insert("TMPDIR".to_string(), path(&node.scratch.join("tmp")));
    env
}

/// The runner's clock; tests advance their own.
pub trait Clock {
    fn now(&self) -> Instant;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Whether the OS sandbox can be created for a policy in a working directory, or why not.
pub type SandboxProbe = Arc<dyn Fn(&SandboxPolicy, &Path) -> Result<(), String>>;

/// The real probe ([`Sandbox::can_create`]; on Linux a brief `bwrap` run).
pub fn os_sandbox_probe() -> SandboxProbe {
    Arc::new(|policy, cwd| {
        Sandbox::can_create(policy, Some(cwd)).map_err(|error| error.to_string())
    })
}

/// How the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed by a signal (the output cap stops it this way), or its status could not be read.
    Abnormal,
    /// The runner's timer stopped it.
    TimedOut,
}

/// What `$BONGOS_RESULT` held after the script exited.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultFile {
    Missing,
    /// Over [`RESULT_CAP`]; the size.
    TooLarge(u64),
    Text(String),
}

impl ResultFile {
    pub fn read(path: &Path) -> Self {
        let Ok(file) = File::open(path) else {
            return Self::Missing;
        };
        let size = file.metadata().map_or(0, |metadata| metadata.len());
        if size > RESULT_CAP {
            return Self::TooLarge(size);
        }
        let mut bytes = Vec::new();
        if file.take(RESULT_CAP + 1).read_to_end(&mut bytes).is_err() {
            return Self::Missing;
        }
        if bytes.len() as u64 > RESULT_CAP {
            return Self::TooLarge(bytes.len() as u64);
        }
        Self::Text(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn size_text(bytes: u64) -> String {
    if bytes >= MB && bytes.is_multiple_of(MB) {
        format!("{} MB", bytes / MB)
    } else {
        format!("{bytes} bytes")
    }
}

/// The outcome of a finished try. Pure. In order: the time cap, the output cap, the first denial,
/// a nonzero exit, then the result (missing, too large, not JSON or not matching the output).
pub fn classify(
    key: &NodeKey,
    node: &ProgramNode,
    exit: Exit,
    output_capped: bool,
    denials: &[String],
    result: ResultFile,
    stderr: &str,
) -> Result<TypedResult, Failure> {
    let fail = |reason: FailureReason, detail: String| {
        Err(Failure::new(reason, Some(key.clone()), detail))
    };
    if exit == Exit::TimedOut {
        return fail(
            FailureReason::TimeCapHit,
            format!(
                "stopped at the time cap of {} (max_minutes)",
                minutes(node.caps.max_time)
            ),
        );
    }
    if output_capped {
        return fail(
            FailureReason::OutputCapHit,
            format!(
                "the output passed {} (workflows.programs.max_output_mb); the rest was dropped",
                size_text(node.caps.max_output_bytes)
            ),
        );
    }
    if let Some(what) = denials.first() {
        return fail(FailureReason::PermissionDenied, what.clone());
    }
    if exit != Exit::Code(0) {
        let status = match exit {
            Exit::Code(code) => format!("exit code {code}"),
            Exit::Abnormal | Exit::TimedOut => "killed".to_string(),
        };
        let detail = match stderr.lines().rev().find(|line| !line.trim().is_empty()) {
            Some(line) => format!("{status}: {}", line.trim()),
            None => status,
        };
        return fail(FailureReason::ProgramExitedNonzero, detail);
    }
    let text = match result {
        ResultFile::Missing => {
            return fail(
                FailureReason::ProgramResultInvalid,
                "the script wrote no result to $BONGOS_RESULT".to_string(),
            );
        }
        ResultFile::TooLarge(size) => {
            return fail(
                FailureReason::ProgramResultInvalid,
                format!(
                    "the result is {}, over the cap of {}",
                    size_text(size),
                    size_text(RESULT_CAP)
                ),
            );
        }
        ResultFile::Text(text) => text,
    };
    let value = match serde_json::from_str::<Value>(text.trim()) {
        Ok(value) => value,
        Err(error) => {
            return fail(
                FailureReason::ProgramResultInvalid,
                format!("the result is not JSON: {error}"),
            );
        }
    };
    schema::check(&node.output, &value)
        .or_else(|problems| fail(FailureReason::ProgramResultInvalid, problems))
}

/// The denials the shim reported, one `what` per line of `denials.jsonl`.
pub fn read_denials(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| {
                    value
                        .get("what")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| line.trim().to_string())
        })
        .collect()
}

/// The output of one process: bytes seen and whether the cap dropped some.
#[derive(Default)]
struct Capture {
    bytes: AtomicU64,
    capped: AtomicBool,
}

/// Copies `from` into `path`, keeping at most `cap` bytes across every stream sharing `capture`;
/// the rest is read and dropped so the process never blocks on a full pipe.
fn copy_capped(
    mut from: impl Read + Send + 'static,
    path: PathBuf,
    capture: Arc<Capture>,
    cap: u64,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut file = File::create(&path).ok();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = match from.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let before = capture.bytes.fetch_add(read as u64, Ordering::SeqCst);
            let keep = cap.saturating_sub(before).min(read as u64) as usize;
            if keep < read {
                capture.capped.store(true, Ordering::SeqCst);
            }
            if keep > 0
                && let Some(file) = file.as_mut()
                && file.write_all(&buffer[..keep]).is_err()
            {
                log::error!("workflows: could not write {}", path.display());
            }
        }
    })
}

/// What to spawn for one pass, and the sandbox it runs in.
struct Launch {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    sandbox: Option<Sandbox>,
    sandbox_note: String,
}

/// One pass of a program node in flight.
struct Pass {
    run: String,
    key: NodeKey,
    try_number: u32,
    node: ProgramNode,
    /// This pass's record directory: the try directory, then `polls/<n>/` inside it.
    dir: PathBuf,
    /// 1 for the first run, then one more per poll rerun.
    number: u32,
    /// When the first `waiting` result came, for the give-up time.
    first_waiting: Option<Instant>,
    started: Instant,
    started_wall: Instant,
    capture: Arc<Capture>,
    readers: Vec<JoinHandle<()>>,
    sandbox_note: String,
    /// Kept alive while the process runs: it owns the network proxy.
    sandbox: Option<Sandbox>,
}

/// A polling node between passes: no process, no cost.
struct Sleeper {
    run: String,
    key: NodeKey,
    try_number: u32,
    node: ProgramNode,
    number: u32,
    first_waiting: Instant,
    due: Instant,
}

/// Runs program node tries.
pub struct ProgramRunner {
    interpreter: Interpreter,
    base_env: BTreeMap<String, String>,
    clock: Arc<dyn Clock>,
    probe: SandboxProbe,
    passes: Vec<(Pass, NodeProcess)>,
    sleepers: Vec<Sleeper>,
}

impl ProgramRunner {
    /// A runner with the system clock and the real sandbox probe.
    pub fn new(interpreter: Interpreter, base_env: BTreeMap<String, String>) -> Self {
        Self {
            interpreter,
            base_env,
            clock: Arc::new(SystemClock),
            probe: os_sandbox_probe(),
            passes: Vec::new(),
            sleepers: Vec::new(),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_sandbox_probe(mut self, probe: SandboxProbe) -> Self {
        self.probe = probe;
        self
    }

    pub fn interpreter(&self) -> &Interpreter {
        &self.interpreter
    }

    /// Starts try `try_number` of `key`. Refuses MCP permissions (v1).
    pub fn start(
        &mut self,
        run: &str,
        key: &NodeKey,
        try_number: u32,
        node: ProgramNode,
    ) -> Result<ProcessRecord, Failure> {
        if !node.permissions.mcp.is_empty() {
            return Err(Failure::new(
                FailureReason::PermissionDenied,
                Some(key.clone()),
                MCP_REFUSAL,
            ));
        }
        let dir = node.try_dir.clone();
        self.launch(run, key, try_number, node, dir, 1, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn launch(
        &mut self,
        run: &str,
        key: &NodeKey,
        try_number: u32,
        node: ProgramNode,
        dir: PathBuf,
        number: u32,
        first_waiting: Option<Instant>,
    ) -> Result<ProcessRecord, Failure> {
        let refuse =
            |reason: FailureReason, detail: String| Failure::new(reason, Some(key.clone()), detail);
        let io = |path: &Path, error: std::io::Error| {
            refuse(
                FailureReason::WorktreeUnavailable,
                format!("{}: {error}", path.display()),
            )
        };
        if !node.worktree.is_dir() {
            return Err(refuse(
                FailureReason::WorktreeUnavailable,
                format!("{} is missing", node.worktree.display()),
            ));
        }
        fs::create_dir_all(&dir).map_err(|error| io(&dir, error))?;
        let tmp = node.scratch.join("tmp");
        fs::create_dir_all(&tmp).map_err(|error| io(&tmp, error))?;
        let result_path = node.result_path();
        match fs::remove_file(&result_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(&result_path, error)),
        }
        let shim = dir.join(SHIM_FILE);
        fs::write(&shim, SHIM).map_err(|error| io(&shim, error))?;
        let state_file = dir.join(STATE_FILE);
        node.state
            .write_readonly(&state_file)
            .map_err(|error| io(&state_file, error))?;
        let policy_file = dir.join(POLICY_FILE);
        let policy = json!({
            "commands": node.permissions.commands,
            "network": node.permissions.network,
            "writable": node.writable_paths(),
            "denials_fd": DENIALS_FD,
        });
        fs::write(
            &policy_file,
            serde_json::to_string_pretty(&policy).unwrap_or_default(),
        )
        .map_err(|error| io(&policy_file, error))?;

        let path = |path: &Path| path.to_string_lossy().into_owned();
        let args = vec![
            "-I".to_string(),
            "-B".to_string(),
            path(&shim),
            path(&policy_file),
            path(&node.script),
        ];
        let env = launch_env(&self.base_env, run, key, &node, &state_file);
        let Launch {
            program,
            args,
            env,
            sandbox,
            sandbox_note,
        } = self.wrap(&node, path(&self.interpreter.program), args, env);
        write_summary(&dir, &json!({ "pass": number, "sandbox": sandbox_note }));

        let (stdout_read, stdout_write) = std::io::pipe().map_err(|error| io(&dir, error))?;
        let (stderr_read, stderr_write) = std::io::pipe().map_err(|error| io(&dir, error))?;
        let (denials_read, denials_write) = std::io::pipe().map_err(|error| io(&dir, error))?;
        let mut command = Command::new(&program);
        command
            .args(&args)
            .current_dir(&node.worktree)
            .env_clear()
            .envs(&env);
        pass_denials_fd(&mut command, &denials_write);
        let spawned = NodeProcess::spawn(
            command,
            Stdio::null(),
            Stdio::from(stdout_write),
            Stdio::from(stderr_write),
        );
        drop(denials_write);
        let process = spawned.map_err(|error| {
            let not_found = error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|error| error.kind() == std::io::ErrorKind::NotFound);
            if not_found {
                refuse(
                    FailureReason::ProgramInterpreterMissing,
                    format!("{program}: {INTERPRETER_FIX}"),
                )
            } else {
                refuse(FailureReason::ProgramExitedNonzero, format!("{error:#}"))
            }
        })?;

        let capture = Arc::new(Capture::default());
        let cap = node.caps.max_output_bytes;
        let readers = vec![
            copy_capped(stdout_read, dir.join(STDOUT_FILE), capture.clone(), cap),
            copy_capped(stderr_read, dir.join(STDERR_FILE), capture.clone(), cap),
            copy_capped(
                denials_read,
                dir.join(DENIALS_FILE),
                Arc::new(Capture::default()),
                DENIALS_CAP,
            ),
        ];
        let record = process.record();
        self.passes.push((
            Pass {
                run: run.to_string(),
                key: key.clone(),
                try_number,
                node,
                dir,
                number,
                first_waiting,
                started: self.clock.now(),
                started_wall: Instant::now(),
                capture,
                readers,
                sandbox_note,
                sandbox,
            },
            process,
        ));
        Ok(record)
    }

    /// The command to spawn: inside the OS sandbox when it can be created, else as it is with
    /// the reason it is not sandboxed.
    fn wrap(
        &self,
        node: &ProgramNode,
        program: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Launch {
        let policy = node.sandbox_policy();
        let wrapped = (self.probe)(&policy, &node.worktree).and_then(|()| {
            let mut sandbox = Sandbox::new(policy).map_err(|error| error.to_string())?;
            let command = CommandAndArgs {
                program: program.clone(),
                args: args.clone(),
                env: env.clone().into_iter().collect::<HashMap<_, _>>(),
                cwd: Some(node.worktree.clone()),
            };
            let wrapped = futures::executor::block_on(sandbox.wrap(&command))
                .map_err(|error| error.to_string())?;
            Ok((wrapped, sandbox))
        });
        match wrapped {
            Ok((wrapped, sandbox)) => Launch {
                program: wrapped.program,
                args: wrapped.args,
                env: wrapped.env.into_iter().collect(),
                sandbox: Some(sandbox),
                sandbox_note: "sandboxed".to_string(),
            },
            Err(reason) => {
                log::warn!("workflows: a program runs without the OS sandbox: {reason}");
                Launch {
                    program,
                    args,
                    env,
                    sandbox: None,
                    sandbox_note: format!("not sandboxed: {reason}"),
                }
            }
        }
    }

    /// The passes that ended since the last call (a `waiting` result included: it is recorded,
    /// then the node sleeps), and the sleeping nodes that gave up. Reruns the sleeping nodes that
    /// are due.
    pub fn poll(&mut self) -> Vec<Finish> {
        let now = self.clock.now();
        let mut ended: Vec<(Pass, NodeProcess, Exit)> = Vec::new();
        let mut index = 0;
        while index < self.passes.len() {
            let (pass, process) = &mut self.passes[index];
            let exited = process.has_exited();
            let timed_out = now.saturating_duration_since(pass.started) >= pass.node.caps.max_time;
            let capped = pass.capture.capped.load(Ordering::SeqCst);
            if !exited && !timed_out && !capped {
                index += 1;
                continue;
            }
            let (pass, mut process) = self.passes.remove(index);
            let exit = if exited {
                match process.child().try_status() {
                    Ok(Some(status)) => status.code().map_or(Exit::Abnormal, Exit::Code),
                    _ => Exit::Abnormal,
                }
            } else if timed_out {
                Exit::TimedOut
            } else {
                Exit::Abnormal
            };
            ended.push((pass, process, exit));
        }
        // One shared stop for the passes the runner ended and for exited ones whose group still
        // has a live member; a group with no live member is left alone (its id may be reused).
        let mut stopping: Vec<NodeProcess> = Vec::new();
        let mut done: Vec<(Pass, Exit)> = Vec::new();
        for (pass, process, exit) in ended {
            if !matches!(exit, Exit::Code(_)) || group_alive(process.record().pid) {
                stopping.push(process);
            }
            done.push((pass, exit));
        }
        stop_all(&mut stopping);
        let mut finishes: Vec<Finish> = done
            .into_iter()
            .map(|(pass, exit)| self.finish(pass, exit, now))
            .collect();

        let (due, sleeping): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sleepers)
            .into_iter()
            .partition(|sleeper| now >= sleeper.due);
        self.sleepers = sleeping;
        for sleeper in due {
            let polls = sleeper.node.polls.clone().expect("a sleeper polls");
            if now.saturating_duration_since(sleeper.first_waiting) >= polls.give_up {
                finishes.push(gave_up(
                    sleeper.run,
                    sleeper.key,
                    sleeper.try_number,
                    &polls,
                ));
                continue;
            }
            let number = sleeper.number + 1;
            let dir = sleeper.node.try_dir.join("polls").join(number.to_string());
            if let Err(failure) = self.launch(
                &sleeper.run,
                &sleeper.key,
                sleeper.try_number,
                sleeper.node,
                dir,
                number,
                Some(sleeper.first_waiting),
            ) {
                finishes.push(Finish {
                    outcome: Err(failure),
                    ..Finish::ok(sleeper.run, sleeper.key, sleeper.try_number, Value::Null)
                });
            }
        }
        finishes
    }

    /// Stops these tries, running or sleeping; they are never reported by [`ProgramRunner::poll`].
    pub fn stop(&mut self, tries: &[(String, NodeKey)]) {
        let matches = |run: &str, key: &NodeKey| {
            tries
                .iter()
                .any(|(stopped_run, stopped_key)| stopped_run == run && stopped_key == key)
        };
        self.sleepers
            .retain(|sleeper| !matches(&sleeper.run, &sleeper.key));
        let (stopped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.passes)
            .into_iter()
            .partition(|(pass, _)| matches(&pass.run, &pass.key));
        self.passes = kept;
        let mut processes: Vec<NodeProcess> = Vec::new();
        let mut sandboxes = Vec::new();
        for (pass, process) in stopped {
            processes.push(process);
            sandboxes.push(pass.sandbox);
        }
        stop_all(&mut processes);
        drop(sandboxes);
    }

    /// The tries with a process, as `(run, key)`.
    pub fn in_flight(&self) -> Vec<(String, NodeKey)> {
        self.passes
            .iter()
            .map(|(pass, _)| (pass.run.clone(), pass.key.clone()))
            .collect()
    }

    /// The polling tries asleep between passes, as `(run, key)`.
    pub fn sleeping(&self) -> Vec<(String, NodeKey)> {
        self.sleepers
            .iter()
            .map(|sleeper| (sleeper.run.clone(), sleeper.key.clone()))
            .collect()
    }

    fn finish(&mut self, mut pass: Pass, exit: Exit, now: Instant) -> Finish {
        let deadline = Instant::now() + DRAIN_WAIT;
        for reader in pass.readers.drain(..) {
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                reader.join().ok();
            }
        }
        drop(pass.sandbox.take());
        let capped = pass.capture.capped.load(Ordering::SeqCst);
        let denials = read_denials(&pass.dir.join(DENIALS_FILE));
        let classified = classify(
            &pass.key,
            &pass.node,
            exit,
            capped,
            &denials,
            ResultFile::read(&pass.node.result_path()),
            &read_tail(&pass.dir.join(STDERR_FILE), STDERR_TAIL),
        );
        write_summary(
            &pass.dir,
            &json!({
                "pass": pass.number,
                "sandbox": pass.sandbox_note,
                "exit": match exit {
                    Exit::Code(code) => json!(code),
                    Exit::Abnormal => json!("killed"),
                    Exit::TimedOut => json!("timed out"),
                },
                "output_bytes": pass.capture.bytes.load(Ordering::SeqCst),
                "output_capped": capped,
                "denials": denials,
            }),
        );
        let elapsed_ms = pass.started_wall.elapsed().as_millis() as u64;
        let finish = |outcome: Result<Value, Failure>| Finish {
            outcome,
            elapsed_ms,
            ..Finish::ok(
                pass.run.clone(),
                pass.key.clone(),
                pass.try_number,
                Value::Null,
            )
        };
        let typed = match classified {
            Ok(typed) => typed,
            Err(failure) => return finish(Err(failure)),
        };
        let result = typed.to_json();
        let path = pass.dir.join(RESULT_FILE);
        if let Err(error) = fs::write(
            &path,
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        ) {
            log::error!("workflows: could not write {}: {error}", path.display());
        }
        if let Some(polls) = pass.node.polls.clone()
            && polls.is_waiting(&typed)
        {
            let first_waiting = pass.first_waiting.unwrap_or(now);
            if now.saturating_duration_since(first_waiting) >= polls.give_up {
                return gave_up(pass.run, pass.key, pass.try_number, &polls);
            }
            let reported = finish(Ok(result));
            self.sleepers.push(Sleeper {
                run: pass.run,
                key: pass.key,
                try_number: pass.try_number,
                node: pass.node,
                number: pass.number,
                first_waiting,
                due: now + polls.interval,
            });
            return reported;
        }
        finish(Ok(result))
    }
}

fn gave_up(run: String, key: NodeKey, try_number: u32, polls: &PollPlan) -> Finish {
    let failure = Failure::new(
        FailureReason::WaitTimedOut,
        Some(key.clone()),
        format!(
            "still {} after {} days (give_up_days)",
            polls.value, polls.give_up_days
        ),
    );
    Finish {
        outcome: Err(failure),
        ..Finish::ok(run, key, try_number, Value::Null)
    }
}

fn write_summary(dir: &Path, summary: &Value) {
    let path = dir.join(SUMMARY_FILE);
    if let Err(error) = fs::write(
        &path,
        serde_json::to_string_pretty(summary).unwrap_or_default(),
    ) {
        log::error!("workflows: could not write {}: {error}", path.display());
    }
}

/// Hands the write end of the denials pipe to the process as [`DENIALS_FD`].
#[cfg(unix)]
fn pass_denials_fd(command: &mut Command, pipe: &std::io::PipeWriter) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let fd = pipe.as_raw_fd();
    // SAFETY: only async-signal-safe calls (dup2, fcntl) run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if fd == DENIALS_FD {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(fd, DENIALS_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn pass_denials_fd(_command: &mut Command, _pipe: &std::io::PipeWriter) {}
