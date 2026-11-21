//! Running one process of a check (one part in one tree): resolving the program from the pin or
//! `PATH`, building its environment, streaming its output to a file, reading its result file,
//! enforcing the time budget, stopping it with everything it started, and judging it.
//!
//! The process starts in its own session (its pid is its process group). A stop sends SIGTERM
//! to the group and to every process found carrying the part's `BONGOS_CHECK_TOKEN` (Linux) or
//! descending from it; one second before the grace ends it sends SIGKILL to all of them, so every
//! process is gone by the grace. A process that left the session is found on Linux by the token
//! in its environment; elsewhere only processes still linked to the part by parent links are
//! found.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use bongos_checks_model::decl::CheckDecl;
use bongos_checks_model::evidence::{PartRecord, TreeKind};
use bongos_checks_model::verdict::{
    Exit, NoVerdictReason, ResultFile, Verdict, judge, parse_result_file,
};
use bongos_confine::{
    Confinement, HostGit, JobDeclaration, JobKind, JobTree, TreeAccess, TreeGit, annotate,
};
use bongos_project_repo_model::RelPath;
use bongos_workflow::process::{
    NodeProcess, OrphanOutcome, ProcessRecord, kill_orphans, start_time, stop_all_with,
};
use sandbox::{CommandAndArgs, Sandbox, SandboxNetPolicy, SandboxPolicy};

use crate::output::{OutputSink, OutputSummary};
use crate::pin::{Pin, PinPathError};
use crate::trees::EnvBase;

/// The variable names this feature sets; inherited values of them never reach a check.
pub const CHECK_PREFIX: &str = "BONGOS_CHECK_";
/// Set for a scope script only.
pub const SCOPE_RESULT_VAR: &str = "BONGOS_SCOPE_RESULT";
/// Set for a scope script only.
pub const CHANGED_FILES_VAR: &str = "BONGOS_CHANGED_FILES";
/// Set for a part plan script only.
pub const DURATIONS_VAR: &str = "BONGOS_CHECK_DURATIONS";
pub const TOKEN_VAR: &str = "BONGOS_CHECK_TOKEN";

const POLL: Duration = Duration::from_millis(20);
/// How long the output may stay open after the process ended (a child still holding it).
const DRAIN: Duration = Duration::from_secs(2);
const READ_BUFFER: usize = 64 * 1024;
/// The longest line kept to look for a refused write in.
const LINE_CAP: usize = 64 * 1024;

/// A switch the owner of a running part flips to stop it (cancel, halt, quit).
#[derive(Clone, Debug, Default)]
pub struct StopHandle(Arc<StopState>);

#[derive(Debug, Default)]
struct StopState {
    stopped: AtomicBool,
    /// Woken by the stop: everyone awaiting [`StopHandle::stopped`].
    wakers: Mutex<Vec<std::task::Waker>>,
}

impl StopHandle {
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
        let wakers = std::mem::take(
            &mut *self
                .0
                .wakers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for waker in wakers {
            waker.wake();
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::SeqCst)
    }

    /// Resolves once the handle is stopped (at once when it already is).
    pub fn stopped(&self) -> impl std::future::Future<Output = ()> + 'static {
        let state = self.0.clone();
        std::future::poll_fn(move |cx| {
            if state.stopped.load(Ordering::SeqCst) {
                return std::task::Poll::Ready(());
            }
            let mut wakers = state
                .wakers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Checked again under the lock: a stop between the two reads takes the wakers
            // after this one is in.
            if state.stopped.load(Ordering::SeqCst) {
                return std::task::Poll::Ready(());
            }
            if !wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
            std::task::Poll::Pending
        })
    }
}

/// What the part's process is told about its scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartScope {
    /// The check declares no scope: no `BONGOS_CHECK_SCOPE`.
    Unscoped,
    /// `BONGOS_CHECK_SCOPE=all`.
    All,
    /// `BONGOS_CHECK_SCOPE=items` and `BONGOS_CHECK_ITEMS` naming this file.
    Items(PathBuf),
}

/// Which process of a check run this is: one part of the check, its scope script or its part
/// plan script. The scripts get the common environment plus their own files, not the part's
/// number or result file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProcessRole {
    #[default]
    Part,
    /// `BONGOS_CHANGED_FILES` and `BONGOS_SCOPE_RESULT` name these files.
    Scope {
        changed_files: PathBuf,
        result: PathBuf,
    },
    /// `BONGOS_CHECK_DURATIONS` names this file.
    Plan { durations: PathBuf },
}

/// What identifies a started part's process, recorded before its output is read so a crash
/// leaves enough to stop it at the next load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartStart {
    pub pid: u32,
    /// In clock ticks since boot (Linux); 0 when unknown.
    pub pid_start_time: u64,
    pub token: String,
}

/// Whether the OS sandbox can be created for a policy in a working directory, or why not.
pub type SandboxProbe = Arc<dyn Fn(&SandboxPolicy, &Path) -> Result<(), String> + Send + Sync>;

/// Runs the part's process in a confinement (`confinement.task_checks`): the tree writable with
/// its git read-only, the part's own scratch and the lane's declared directories (the ones
/// [`EnvBase::declared_dirs`] names) writable, nothing else. A sandbox that cannot be created is
/// no verdict, never an unconfined run.
///
/// The scratch is made fresh for every process (whatever a former run left there is removed
/// first), and the files bongos reads back (the result file, the scope script's answer) are read
/// from it before it is removed. The output folder, where bongos writes the process's output and
/// its own files, is never writable.
#[derive(Clone)]
pub struct PartConfinement {
    /// `None` asks the OS sandbox itself.
    pub probe: Option<SandboxProbe>,
    /// The checks' output folder; no declared directory may equal, contain or lie inside it.
    pub records_dir: Option<PathBuf>,
    /// The git directories bongos trusts for the part's tree (see [`host_git_dirs`]), taken
    /// from the git directory it recorded for the tree; the error says why it trusts none.
    pub git_dirs: Result<Vec<PathBuf>, String>,
}

impl Default for PartConfinement {
    fn default() -> Self {
        Self {
            probe: None,
            records_dir: None,
            git_dirs: Ok(Vec::new()),
        }
    }
}

/// Everything one part needs.
pub struct PartSpec {
    pub decl: CheckDecl,
    /// Held by the whole check run; `run` and every named file resolve in it.
    pub pin: Arc<Pin>,
    /// `definitions/` of the master checkout (`BONGOS_CHECK_MASTER`).
    pub master: PathBuf,
    /// The tree the process runs in (its working directory).
    pub tree: PathBuf,
    pub tree_kind: TreeKind,
    pub tree_key: String,
    /// Part K of N, from 1.
    pub part: u32,
    pub parts: u32,
    pub scope: PartScope,
    pub role: ProcessRole,
    /// Where standard error goes when it must stay out of the output (a plan script's answer
    /// is its standard output); `None` merges it into the output.
    pub stderr: Option<PathBuf>,
    pub env: EnvBase,
    /// The output file; the result and params files go beside it.
    pub output: PathBuf,
    /// `BONGOS_CHECK_EVIDENCE`, when the run wrote one.
    pub evidence: Option<PathBuf>,
    /// The most output bytes kept; `None` keeps all.
    pub output_limit: Option<u64>,
    pub result_limit: usize,
    /// `None`: no time budget.
    pub budget: Option<Duration>,
    /// Everything is gone this long after a stop starts.
    pub kill_grace: Duration,
    pub stop: StopHandle,
    /// Called with the pid, its start time and the token before any output is read.
    pub on_start: Option<Box<dyn FnOnce(&PartStart) + Send>>,
    /// `Some` while `confinement.task_checks` is on.
    pub confinement: Option<PartConfinement>,
}

/// How one part ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartOutcome {
    pub verdict: Verdict,
    pub exit: Option<Exit>,
    /// The valid result file, when the check declares one and it was valid.
    pub result: Option<ResultFile>,
    pub start: Option<PartStart>,
    /// The argv as run (the program resolved); empty when nothing was resolved.
    pub command: Vec<String>,
    pub output: OutputSummary,
    pub duration: Duration,
    /// A scope script's answer file as it was when the script ended, at most `result_limit` + 1
    /// bytes; `None` when it wrote none (or for any other role).
    pub scope_answer: Option<Vec<u8>>,
}

impl PartOutcome {
    fn no_verdict(reason: NoVerdictReason, command: Vec<String>, started: Instant) -> Self {
        Self {
            verdict: Verdict::NoVerdict(reason),
            exit: None,
            result: None,
            scope_answer: None,
            start: None,
            command,
            output: OutputSummary::default(),
            duration: started.elapsed(),
        }
    }
}

/// `<output dir>/<record id>/part-<K>.log`.
pub fn part_output_path(output_dir: &Path, record_id: &str, part: u32) -> PathBuf {
    output_dir.join(record_id).join(format!("part-{part}.log"))
}

/// The time budget of `decl`: its own `budget_minutes`, else the default; `None` is none.
pub fn budget(decl: &CheckDecl, default_minutes: Option<u32>) -> Option<Duration> {
    decl.budget_minutes
        .or(default_minutes)
        .map(|minutes| Duration::from_secs(u64::from(minutes) * 60))
}

/// The environment of a check process: the base, without any inherited variable of this
/// feature's own and minus every name in `unset`, plus `vars`. `unset` applies to the base
/// only, so the variables this feature sets (its token, result and params files) are always
/// present.
pub fn part_env(
    base: &EnvBase,
    vars: &[(String, OsString)],
    unset: &[String],
) -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = match base {
        EnvBase::Lane(lane) => lane
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
        EnvBase::Declared { env, .. } if !env.is_empty() => env
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
        EnvBase::Own | EnvBase::Declared { .. } => std::env::vars_os().collect(),
    };
    env.retain(|name, _| !is_own_variable(name));
    for name in unset {
        env.remove(OsStr::new(name));
    }
    for (name, value) in vars {
        env.insert(OsString::from(name), value.clone());
    }
    env
}

fn is_own_variable(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.starts_with(CHECK_PREFIX) || name == SCOPE_RESULT_VAR || name == CHANGED_FILES_VAR
    })
}

/// Runs one part to its end, a stop or its time budget, on a thread of its own, and judges it.
pub async fn run_part(spec: PartSpec) -> PartOutcome {
    let started = Instant::now();
    let (sender, receiver) = futures::channel::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("check part".to_string())
        .spawn(move || {
            sender.send(run_blocking(spec)).ok();
        });
    if let Err(error) = spawned {
        return PartOutcome::no_verdict(
            NoVerdictReason::StartFailed(format!("no thread for the part: {error}")),
            Vec::new(),
            started,
        );
    }
    receiver.await.unwrap_or_else(|_| {
        PartOutcome::no_verdict(NoVerdictReason::Interrupted, Vec::new(), started)
    })
}

enum Program {
    Resolved(PathBuf, Vec<String>),
    Refused(NoVerdictReason),
}

fn resolve_program(spec: &PartSpec, env: &BTreeMap<OsString, OsString>) -> Program {
    for path in spec.decl.named_files() {
        if RelPath::new(path).is_none() {
            return Program::Refused(NoVerdictReason::StartFailed(
                PinPathError::Escapes(path.to_string()).to_string(),
            ));
        }
    }
    match (&spec.decl.run, &spec.decl.command) {
        (Some(run), None) => match spec.pin.resolve(run) {
            Ok(path) => Program::Resolved(path, Vec::new()),
            Err(PinPathError::Missing(_)) => {
                Program::Refused(NoVerdictReason::ToolNotFound(run.clone()))
            }
            Err(error) => Program::Refused(NoVerdictReason::StartFailed(error.to_string())),
        },
        (None, Some(command)) if !command.is_empty() => {
            let name = &command[0];
            let path = env.get(OsStr::new("PATH")).cloned();
            match which::which_in(name, path, &spec.tree) {
                Ok(program) => Program::Resolved(program, command[1..].to_vec()),
                Err(_) => Program::Refused(NoVerdictReason::ToolNotFound(name.clone())),
            }
        }
        _ => Program::Refused(NoVerdictReason::StartFailed(
            "the check needs exactly one of run and command".to_string(),
        )),
    }
}

/// The files beside the output file.
struct PartFiles {
    result: PathBuf,
    params: PathBuf,
}

impl PartFiles {
    /// Named after the output file: `part-3.log` gives `part-3.result.json`.
    fn of(output: &Path) -> Self {
        let dir = output.parent().unwrap_or(Path::new("."));
        let stem = output
            .file_stem()
            .map_or_else(|| "part".into(), |stem| stem.to_string_lossy());
        Self {
            result: dir.join(format!("{stem}.result.json")),
            params: dir.join(format!("{stem}.params.json")),
        }
    }

    /// Removes any result file left before and writes the params file as a new file, never
    /// through an entry (a symlink) already at its path.
    fn prepare(&self, decl: &CheckDecl) -> std::io::Result<()> {
        if let Some(dir) = self.result.parent() {
            std::fs::create_dir_all(dir)?;
        }
        remove_entry(&self.result)?;
        remove_entry(&self.params)?;
        let params = decl.params.clone().unwrap_or_default();
        let json = serde_json::to_vec(&params).map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.params)?;
        std::io::Write::write_all(&mut file, &json)
    }
}

/// Removes the file or symlink at `path` (the link itself, never its target); nothing there is
/// fine.
fn remove_entry(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// A part's scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    /// Makes `path` a new empty directory: whatever is there first (a directory a former run
    /// left, a symlink, a file) is removed, a symlink as the link itself.
    fn fresh(path: &Path) -> std::io::Result<Self> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
            Ok(_) => std::fs::remove_file(path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir(path)?;
        Ok(Self(path.to_path_buf()))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn check_vars(
    spec: &PartSpec,
    files: &PartFiles,
    scope_answer: Option<&Path>,
    token: &str,
) -> Vec<(String, OsString)> {
    let mut vars = vec![
        ("BONGOS_CHECK_NAME", OsString::from(&spec.decl.name)),
        ("BONGOS_CHECK_PARAMS", files.params.clone().into_os_string()),
        (
            "BONGOS_CHECK_TREE",
            OsString::from(match spec.tree_kind {
                TreeKind::Tip => "tip",
                TreeKind::Base => "base",
            }),
        ),
        ("BONGOS_CHECK_TREE_KEY", OsString::from(&spec.tree_key)),
        (
            "BONGOS_CHECK_PINNED",
            spec.pin.definitions().into_os_string(),
        ),
        ("BONGOS_CHECK_MASTER", spec.master.clone().into_os_string()),
        (TOKEN_VAR, OsString::from(token)),
    ];
    match &spec.role {
        ProcessRole::Part => {
            vars.push(("BONGOS_CHECK_PART", OsString::from(spec.part.to_string())));
            vars.push(("BONGOS_CHECK_PARTS", OsString::from(spec.parts.to_string())));
            vars.push(("BONGOS_CHECK_RESULT", files.result.clone().into_os_string()));
        }
        ProcessRole::Scope {
            changed_files,
            result,
        } => {
            vars.push((CHANGED_FILES_VAR, changed_files.clone().into_os_string()));
            let answer = scope_answer.unwrap_or(result);
            vars.push((SCOPE_RESULT_VAR, answer.as_os_str().to_os_string()));
        }
        ProcessRole::Plan { durations } => {
            vars.push((DURATIONS_VAR, durations.clone().into_os_string()));
        }
    }
    match &spec.scope {
        PartScope::Unscoped => {}
        PartScope::All => vars.push(("BONGOS_CHECK_SCOPE", OsString::from("all"))),
        PartScope::Items(items) => {
            vars.push(("BONGOS_CHECK_SCOPE", OsString::from("items")));
            vars.push(("BONGOS_CHECK_ITEMS", items.clone().into_os_string()));
        }
    }
    if let Some(evidence) = &spec.evidence {
        vars.push(("BONGOS_CHECK_EVIDENCE", evidence.clone().into_os_string()));
    }
    vars.into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

/// Why the supervision loop ended.
enum Ending {
    Exited,
    Stopped(NoVerdictReason),
}

fn run_blocking(mut spec: PartSpec) -> PartOutcome {
    let started = Instant::now();
    let refuse = |reason, command| PartOutcome::no_verdict(reason, command, started);
    if !spec.tree.is_dir() {
        return refuse(
            NoVerdictReason::StartFailed(format!("the tree {} is gone", spec.tree.display())),
            Vec::new(),
        );
    }
    let token = uuid::Uuid::new_v4().to_string();
    // A confined part writes its result and params files (and a scope script its answer) in its
    // own scratch, the one place besides the tree and the lane's directories it may write.
    let scratch = spec.confinement.as_ref().map(|_| scratch_of(&spec.output));
    let files = match &scratch {
        Some(scratch) => PartFiles::of(&scratch.join(spec.output.file_name().unwrap_or_default())),
        None => PartFiles::of(&spec.output),
    };
    let scope_answer = match (&spec.role, &scratch) {
        (ProcessRole::Scope { result, .. }, Some(scratch)) => Some(
            scratch.join(
                result
                    .file_name()
                    .unwrap_or(OsStr::new("scope-answer.json")),
            ),
        ),
        (ProcessRole::Scope { result, .. }, None) => Some(result.clone()),
        _ => None,
    };
    let mut env = part_env(
        &spec.env,
        &check_vars(&spec, &files, scope_answer.as_deref(), &token),
        &spec.decl.unset,
    );
    if let Some(scratch) = &scratch {
        env.insert(OsString::from("TMPDIR"), scratch.clone().into_os_string());
    }
    let (program, args) = match resolve_program(&spec, &env) {
        Program::Resolved(program, args) => (program, args),
        Program::Refused(reason) => return refuse(reason, Vec::new()),
    };
    let argv: Vec<String> = std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .collect();
    // Removed on every path out, after the confinement (declared later) is finished.
    let _scratch = match scratch.as_deref().map(Scratch::fresh).transpose() {
        Ok(scratch) => scratch,
        Err(error) => {
            return refuse(
                NoVerdictReason::StartFailed(format!("making the part's scratch: {error}")),
                argv,
            );
        }
    };
    if let Err(error) = files.prepare(&spec.decl) {
        return refuse(
            NoVerdictReason::StartFailed(format!("preparing the part's files: {error}")),
            argv,
        );
    }
    let summary = spec
        .decl
        .summary
        .as_deref()
        .and_then(|pattern| regex::Regex::new(pattern).ok());
    let sink = match OutputSink::create(&spec.output, spec.output_limit, summary) {
        Ok(sink) => Arc::new(Mutex::new(sink)),
        Err(error) => {
            return refuse(
                NoVerdictReason::StartFailed(format!(
                    "creating the output file {}: {error}",
                    spec.output.display()
                )),
                argv,
            );
        }
    };
    let (pipe_read, pipe_write) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => {
            return refuse(
                NoVerdictReason::StartFailed(format!("no pipe for the output: {error}")),
                argv,
            );
        }
    };
    let stderr = match &spec.stderr {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => Stdio::from(file),
            Err(error) => {
                return refuse(
                    NoVerdictReason::StartFailed(format!(
                        "creating the error output file {}: {error}",
                        path.display()
                    )),
                    argv,
                );
            }
        },
        None => match pipe_write.try_clone() {
            Ok(pipe) => Stdio::from(pipe),
            Err(error) => {
                return refuse(
                    NoVerdictReason::StartFailed(format!("no pipe for the output: {error}")),
                    argv,
                );
            }
        },
    };
    // Dropped after the process group is gone (declared before the process): the confinement
    // is finished then, before the scratch is removed.
    let mut confined = None;
    let command = match (&spec.confinement, &scratch) {
        (Some(options), Some(scratch)) => {
            match confine_command(&spec, options, scratch, &program, &args, &env) {
                Ok((command, held)) => {
                    confined = Some(held);
                    command
                }
                Err(reason) => return refuse(reason, argv),
            }
        }
        _ => {
            let mut command = Command::new(&program);
            command
                .args(&args)
                .current_dir(&spec.tree)
                .env_clear()
                .envs(&env);
            command
        }
    };
    let roots = confined
        .as_ref()
        .map(|held| held.confinement.writable_roots().to_vec());
    let spawned = NodeProcess::spawn(command, Stdio::null(), Stdio::from(pipe_write), stderr);
    let mut process = match spawned {
        Ok(process) => process,
        Err(error) => return refuse(spawn_failure(&error, &argv[0]), argv),
    };
    let record = process.record();
    let start = PartStart {
        pid: record.pid,
        pid_start_time: record.start_time,
        token: token.clone(),
    };
    if let Some(on_start) = spec.on_start.take() {
        on_start(&start);
    }
    let (done_sender, done_receiver) = mpsc::channel();
    let reader_sink = sink.clone();
    let reader = std::thread::Builder::new()
        .name("check output".to_string())
        .spawn(move || {
            read_output(pipe_read, &reader_sink, roots.as_deref());
            done_sender.send(()).ok();
        });
    if let Err(error) = &reader {
        log::warn!("checks: no thread for the output of {}: {error}", argv[0]);
    }

    let deadline = spec.budget.map(|budget| started + budget);
    let stop = |process: &mut NodeProcess, reason| {
        let began = Instant::now();
        stop_part(process, &token, spec.kill_grace);
        (
            Ending::Stopped(reason),
            Some(stop_deadline(began, spec.kill_grace)),
        )
    };
    let (ending, stop_over) = loop {
        if process.has_exited() {
            break (Ending::Exited, None);
        }
        if spec.stop.is_stopped() {
            break stop(&mut process, NoVerdictReason::Cancelled);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break stop(&mut process, NoVerdictReason::TimeBudgetReached);
        }
        std::thread::sleep(POLL);
    };
    let exit = exit_of(&mut process);
    // On a stop the output gets only what is left of the grace.
    let drain = stop_over.map_or(DRAIN, |over| {
        DRAIN.min(over.saturating_duration_since(Instant::now()))
    });
    if reader.is_ok() && done_receiver.recv_timeout(drain).is_err() {
        log::info!(
            "checks: the output of {} is still open after it ended; reading stops",
            argv[0]
        );
    }
    let output = lock(&sink).finish();
    let scope_answer = match (&ending, &scope_answer) {
        (Ending::Exited, Some(path)) => read_capped(path, spec.result_limit),
        _ => None,
    };
    let (verdict, result) = match ending {
        Ending::Stopped(reason) => (Verdict::NoVerdict(reason), None),
        Ending::Exited => {
            let read = spec
                .decl
                .writes_result_file()
                .then(|| read_result(&files.result, spec.result_limit))
                .flatten();
            let result = read.as_ref().and_then(|read| read.as_ref().ok()).cloned();
            let verdict = match exit {
                Some(exit) => judge(exit, &spec.decl, output.summary_seen, read),
                None => Verdict::NoVerdict(NoVerdictReason::Interrupted),
            };
            (verdict, result)
        }
    };
    PartOutcome {
        verdict,
        exit,
        result,
        start: Some(start),
        command: argv,
        output,
        duration: started.elapsed(),
        scope_answer,
    }
}

/// A part's confinement and sandbox, held while its process group lives; dropping it finishes
/// the confinement.
struct Confined {
    confinement: Confinement,
    // Kept alive while the process runs.
    _sandbox: Option<Sandbox>,
}

impl Drop for Confined {
    fn drop(&mut self) {
        if let Err(error) = self.confinement.finish() {
            log::error!("checks: cannot finish the confinement of a part: {error}");
        }
    }
}

/// `<output dir>/<output stem>.scratch`, the part's own scratch directory.
fn scratch_of(output: &Path) -> PathBuf {
    let dir = output.parent().unwrap_or(Path::new("."));
    let stem = output
        .file_stem()
        .map_or_else(|| "part".into(), |stem| stem.to_string_lossy());
    dir.join(format!("{stem}.scratch"))
}

/// The git directories bongos trusts for a tree whose git directory it recorded as `git_dir`
/// (never read from the tree's own `.git`, which the job may have rewritten): a linked
/// worktree's git directory, its common directory and the repository's other worktrees' git
/// directories, or the `.git` directory of a plain repository.
pub fn host_git_dirs(git_dir: &Path) -> Vec<PathBuf> {
    let mut git_dirs = Vec::new();
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|common| git_dir.join(common.trim_end_matches(['\n', '\r'])));
    if let Some(common) = &common
        && let Ok(entries) = std::fs::read_dir(common.join("worktrees"))
    {
        git_dirs.extend(entries.flatten().map(|entry| entry.path()));
    }
    git_dirs.push(git_dir.to_path_buf());
    git_dirs.extend(common);
    git_dirs
}

/// `path` with its longest existing ancestor resolved (symlinks followed) and the rest appended;
/// refused when it is relative or names `.` or `..`.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    use std::path::Component;
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(format!(
            "the declared directory {} must be an absolute path without . or ..",
            path.display()
        ));
    }
    let mut rest = Vec::new();
    let mut existing = path;
    loop {
        match existing.canonicalize() {
            Ok(canonical) => {
                return Ok(rest
                    .iter()
                    .rev()
                    .fold(canonical, |path: PathBuf, part| path.join(part)));
            }
            Err(_) => match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_os_string());
                    existing = parent;
                }
                _ => return Ok(path.to_path_buf()),
            },
        }
    }
}

/// Refuses a declared directory that equals, contains or lies inside the checks' output folder
/// (`records_dir`) or bongos's data folder, or that equals or contains the home folder. Checked
/// before anything is made at a declared path.
fn check_declared_dirs(dirs: &[PathBuf], records_dir: Option<&Path>) -> Result<(), String> {
    let lenient = |path: &Path| resolved(path).unwrap_or_else(|_| path.to_path_buf());
    let mut both_ways = vec![("the bongos data folder", lenient(paths::data_dir()))];
    if let Some(records_dir) = records_dir {
        both_ways.push(("the checks' output folder", lenient(records_dir)));
    }
    let home = lenient(paths::home_dir());
    for dir in dirs {
        let path = resolved(dir)?;
        for (what, other) in &both_ways {
            if path.starts_with(other) || other.starts_with(&path) {
                return Err(format!(
                    "the declared directory {} overlaps {what} {}",
                    dir.display(),
                    other.display()
                ));
            }
        }
        if home.starts_with(&path) {
            return Err(format!(
                "the declared directory {} contains the home folder {}",
                dir.display(),
                home.display()
            ));
        }
    }
    Ok(())
}

/// The command that runs the part's process in the sandbox, and what to hold while it lives.
/// Every failure is a start failure: a part is never run unconfined once the switch is on.
fn confine_command(
    spec: &PartSpec,
    options: &PartConfinement,
    scratch: &Path,
    program: &Path,
    args: &[String],
    env: &BTreeMap<OsString, OsString>,
) -> Result<(Command, Confined), NoVerdictReason> {
    let failed = |why: String| NoVerdictReason::StartFailed(why);
    let git_dirs = options
        .git_dirs
        .clone()
        .map_err(|why| failed(format!("the sandbox cannot be created: {why}")))?;
    let mut declared_dirs = spec.env.declared_dirs().to_vec();
    check_declared_dirs(&declared_dirs, options.records_dir.as_deref())
        .map_err(|why| failed(format!("the sandbox cannot be created: {why}")))?;
    // Only once they are checked; one that cannot be made is not writable.
    declared_dirs.retain(|dir| std::fs::create_dir_all(dir).is_ok());
    let declaration = JobDeclaration {
        kind: JobKind::Check,
        trees: vec![JobTree {
            path: spec.tree.clone(),
            access: TreeAccess::Writable,
            git: TreeGit::ReadOnly,
        }],
        scratch: scratch.to_path_buf(),
        declared_dirs,
        extra_writable: Vec::new(),
        network: SandboxNetPolicy::Unrestricted,
    };
    let host = HostGit {
        git_dirs,
        records_dir: None,
    };
    let confinement = Confinement::build(&declaration, &host)
        .map_err(|error| failed(format!("the sandbox cannot be created: {error}")))?;
    // From here the guard finishes the confinement on every path out.
    let mut confined = Confined {
        confinement,
        _sandbox: None,
    };
    let policy = confined.confinement.sandbox_policy();
    let probed = match &options.probe {
        Some(probe) => probe(&policy, &spec.tree),
        None => Sandbox::can_create(&policy, Some(&spec.tree)).map_err(|error| error.to_string()),
    };
    probed.map_err(|why| failed(format!("the sandbox cannot be created: {why}")))?;
    let mut sandbox = Sandbox::new(policy)
        .map_err(|error| failed(format!("the sandbox cannot be created: {error}")))?;
    let lossy = |text: &OsStr| text.to_string_lossy().into_owned();
    let command = CommandAndArgs {
        program: lossy(program.as_os_str()),
        args: args.to_vec(),
        env: env
            .iter()
            .map(|(name, value)| (lossy(name), lossy(value)))
            .collect(),
        cwd: Some(spec.tree.clone()),
    };
    let wrapped = futures::executor::block_on(sandbox.wrap(&command))
        .map_err(|error| failed(format!("the sandbox cannot be created: {error}")))?;
    confined._sandbox = Some(sandbox);
    let mut process = Command::new(&wrapped.program);
    process
        .args(&wrapped.args)
        .current_dir(wrapped.cwd.as_ref().unwrap_or(&spec.tree))
        .env_clear()
        .envs(&wrapped.env);
    Ok((process, confined))
}

/// Why the process could not start, from the program and the io error only: the spawn error's
/// context prints the whole command with its environment, which may hold secrets.
fn spawn_failure(error: &anyhow::Error, program: &str) -> NoVerdictReason {
    let io = error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .next();
    match io {
        Some(io) if io.kind() == std::io::ErrorKind::NotFound => {
            NoVerdictReason::ToolNotFound(program.to_string())
        }
        Some(io) => NoVerdictReason::StartFailed(format!("starting {program}: {io}")),
        None => NoVerdictReason::StartFailed(format!("starting {program} failed")),
    }
}

fn lock(sink: &Mutex<OutputSink>) -> std::sync::MutexGuard<'_, OutputSink> {
    sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_output(mut pipe: std::io::PipeReader, sink: &Mutex<OutputSink>, roots: Option<&[PathBuf]>) {
    let mut buffer = vec![0u8; READ_BUFFER];
    // The part of the current line already written, for the annotation of a confined part.
    let mut line: Vec<u8> = Vec::new();
    loop {
        let n = match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut sink = lock(sink);
        if sink.is_finished() {
            return;
        }
        let Some(roots) = roots else {
            sink.write(&buffer[..n]);
            continue;
        };
        for piece in buffer[..n].split_inclusive(|byte| *byte == b'\n') {
            sink.write(piece);
            if line.len() < LINE_CAP {
                line.extend_from_slice(piece);
            }
            if piece.ends_with(b"\n") {
                if let Some(note) = annotation(&line, roots) {
                    sink.write(note.as_bytes());
                }
                line.clear();
            }
        }
    }
    if let Some(roots) = roots
        && !line.is_empty()
        && let Some(note) = annotation(&line, roots)
    {
        let mut sink = lock(sink);
        if !sink.is_finished() {
            sink.write(format!("\n{note}").as_bytes());
        }
    }
}

/// The note after an output line that reports a write the sandbox refused, with its newline.
fn annotation(line: &[u8], roots: &[PathBuf]) -> Option<String> {
    annotate::refusal(&String::from_utf8_lossy(line), roots).map(|note| format!("{note}\n"))
}

/// Opens `path` for reading, refusing a symlink there: a file the process wrote in its scratch
/// is read as it is, never through a link to somewhere else.
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// At most `limit` + 1 bytes of the file at `path` (no symlink); `None` when it cannot be read.
fn read_capped(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let file = open_no_follow(path).ok()?;
    let mut bytes = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    file.take(cap).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// The result file: `None` when there is none; refused when larger than `limit`.
fn read_result(path: &Path, limit: usize) -> Option<Result<ResultFile, String>> {
    let file = match open_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => return Some(Err(format!("reading it: {error}"))),
    };
    let mut bytes = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    if let Err(error) = file.take(cap).read_to_end(&mut bytes) {
        return Some(Err(format!("reading it: {error}")));
    }
    Some(parse_result_file(&bytes, limit))
}

fn exit_of(process: &mut NodeProcess) -> Option<Exit> {
    let status = process.child().try_status().ok().flatten()?;
    if let Some(code) = status.code() {
        return Some(Exit::Code(code));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().map(Exit::Signal)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// A process found outside the part's group, with its start time when the platform gives one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stray {
    pid: u32,
    start_time: Option<u64>,
}

/// Stops a running part within `grace`: SIGTERM to its group and to every process carrying its
/// token or descending from it, SIGKILL to all of them one second before the grace ends.
fn stop_part(process: &mut NodeProcess, token: &str, grace: Duration) {
    let began = Instant::now();
    let record = process.record();
    let leader = record.pid;
    // Every holder of the token started after the leader, which carried it first.
    let since = Some(record.start_time).filter(|start| *start != 0);
    // Parent links break once the leader dies, so they are read first.
    let mut strays = descendants(leader);
    strays.extend(token_holders(token, since));
    for stray in &strays {
        signal_stray(*stray, Signal::Term);
    }
    stop_all_with(
        std::slice::from_mut(process),
        grace.saturating_sub(Duration::from_secs(1)),
    );
    // Whatever started since is found by the token again, and on every wait below, so a
    // process forking while the others die is killed too.
    strays.extend(token_holders(token, since));
    for stray in &strays {
        signal_stray(*stray, Signal::Kill);
    }
    let deadline = stop_deadline(began, grace);
    while Instant::now() < deadline {
        let late = token_holders(token, since);
        for stray in &late {
            signal_stray(*stray, Signal::Kill);
        }
        for stray in &late {
            if !strays.contains(stray) {
                strays.push(*stray);
            }
        }
        if late.is_empty() && !strays.iter().any(|stray| stray_alive(*stray)) {
            break;
        }
        std::thread::sleep(POLL);
    }
}

/// When a stop begun at `began` must be over.
fn stop_deadline(began: Instant, grace: Duration) -> Instant {
    began + grace.max(Duration::from_secs(1))
}

/// Every live process below `leader` by parent links.
fn descendants(leader: u32) -> Vec<Stray> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
    let refresh = ProcessRefreshKind::nothing();
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children
                .entry(parent.as_u32())
                .or_default()
                .push(pid.as_u32());
        }
    }
    let mut found = Vec::new();
    let mut next = vec![leader];
    while let Some(pid) = next.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if !found.iter().any(|stray: &Stray| stray.pid == *child) {
                found.push(Stray {
                    pid: *child,
                    start_time: start_time(*child),
                });
                next.push(*child);
            }
        }
    }
    found
}

/// Every process of this user whose environment holds `BONGOS_CHECK_TOKEN=<token>`, among those
/// started at or after `since` (a start time in clock ticks) when given. Linux only; elsewhere
/// the environment of another process cannot be read.
///
/// Reading a process's environment waits on its memory map lock, which a busy process (a
/// compiler, a linker) can hold for long on a loaded machine, so the start time, which needs no
/// such lock, rules out the older processes first.
#[cfg(target_os = "linux")]
fn token_holders(token: &str, since: Option<u64>) -> Vec<Stray> {
    use std::os::unix::fs::MetadataExt as _;
    let needle = format!("{TOKEN_VAR}={token}");
    let needle = needle.as_bytes();
    // SAFETY: geteuid and getpid only read this process's ids.
    let (uid, own) = unsafe { (libc::geteuid(), libc::getpid()) };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if i32::try_from(pid).ok()? == own {
                return None;
            }
            if entry.metadata().ok()?.uid() != uid {
                return None;
            }
            let started = start_time(pid);
            if let (Some(since), Some(started)) = (since, started)
                && started < since
            {
                return None;
            }
            let environ = std::fs::read(entry.path().join("environ")).ok()?;
            environ
                .split(|byte| *byte == 0)
                .any(|variable| variable == needle)
                .then_some(Stray {
                    pid,
                    start_time: started,
                })
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn token_holders(_token: &str, _since: Option<u64>) -> Vec<Stray> {
    Vec::new()
}

/// Whether the stray is still the same live process.
fn stray_alive(stray: Stray) -> bool {
    match (stray.start_time, start_time(stray.pid)) {
        (Some(recorded), Some(now)) => recorded == now && !is_zombie(stray.pid),
        // Never signalled on Linux (see `signal_stray`), so never waited for either.
        (None, _) => !cfg!(target_os = "linux") && pid_alive(stray.pid),
        (Some(_), None) => false,
    }
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|text| {
            let rest = &text[text.rfind(')')? + 1..];
            rest.split_whitespace().next().map(|state| state == "Z")
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn is_zombie(_pid: u32) -> bool {
    false
}

fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    pid > 1 && unsafe { libc::kill(pid, 0) } == 0
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Signals one stray process when it is still the one found (same start time where known).
/// Never pid 0 or 1, never bongos itself.
fn signal_stray(stray: Stray, signal: Signal) {
    let Ok(pid) = i32::try_from(stray.pid) else {
        return;
    };
    // SAFETY: getpid only reads this process's id.
    let own = unsafe { libc::getpid() };
    if pid <= 1 || pid == own {
        return;
    }
    // On Linux a live process always has a start time; one found without it may be gone, and
    // its pid reused by now.
    if cfg!(target_os = "linux") && stray.start_time.is_none() {
        return;
    }
    if let Some(recorded) = stray.start_time
        && start_time(stray.pid) != Some(recorded)
    {
        return;
    }
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: kill only sends a signal, to a pid above 1 that is not bongos.
    unsafe {
        libc::kill(pid, signal);
    }
}

/// Stops the processes of parts a former bongos left running: the parts of `parts` not ended,
/// matched by pid and start time like feature processes, plus (on Linux) every process still
/// carrying a part's token. One outcome per part given.
pub fn kill_orphan_parts(parts: &[PartRecord]) -> Vec<OrphanOutcome> {
    let open: Vec<&PartRecord> = parts.iter().filter(|part| part.ended.is_none()).collect();
    let records: Vec<ProcessRecord> = open
        .iter()
        .map(|part| ProcessRecord {
            pid: part.pid.unwrap_or(0),
            start_time: part.pid_start_time.unwrap_or(0),
        })
        .collect();
    let outcomes = kill_orphans(&records);
    let mut strays = Vec::new();
    for part in &open {
        if let Some(token) = part.token.as_deref().filter(|token| !token.is_empty()) {
            let since = part.pid_start_time.filter(|start| *start != 0);
            strays.extend(token_holders(token, since));
        }
    }
    for stray in &strays {
        signal_stray(*stray, Signal::Kill);
    }
    let mut by_part = outcomes.into_iter();
    parts
        .iter()
        .map(|part| {
            if part.ended.is_some() {
                OrphanOutcome::NotRunning
            } else {
                by_part.next().unwrap_or(OrphanOutcome::Unknown)
            }
        })
        .collect()
}
