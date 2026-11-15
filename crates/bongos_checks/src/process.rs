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
use bongos_project_repo_model::RelPath;
use bongos_workflow::process::{
    NodeProcess, OrphanOutcome, ProcessRecord, kill_orphans, start_time, stop_all_with,
};

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
}

impl PartOutcome {
    fn no_verdict(reason: NoVerdictReason, command: Vec<String>, started: Instant) -> Self {
        Self {
            verdict: Verdict::NoVerdict(reason),
            exit: None,
            result: None,
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
        EnvBase::Own => std::env::vars_os().collect(),
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

    fn prepare(&self, decl: &CheckDecl) -> std::io::Result<()> {
        if let Some(dir) = self.result.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match std::fs::remove_file(&self.result) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let params = decl.params.clone().unwrap_or_default();
        let json = serde_json::to_vec(&params).map_err(std::io::Error::other)?;
        std::fs::write(&self.params, json)
    }
}

fn check_vars(spec: &PartSpec, files: &PartFiles, token: &str) -> Vec<(String, OsString)> {
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
            vars.push((SCOPE_RESULT_VAR, result.clone().into_os_string()));
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
    let files = PartFiles::of(&spec.output);
    let env = part_env(
        &spec.env,
        &check_vars(&spec, &files, &token),
        &spec.decl.unset,
    );
    let (program, args) = match resolve_program(&spec, &env) {
        Program::Resolved(program, args) => (program, args),
        Program::Refused(reason) => return refuse(reason, Vec::new()),
    };
    let argv: Vec<String> = std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .collect();
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
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(&spec.tree)
        .env_clear()
        .envs(&env);
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
            read_output(pipe_read, &reader_sink);
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
    }
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

fn read_output(mut pipe: std::io::PipeReader, sink: &Mutex<OutputSink>) {
    let mut buffer = vec![0u8; READ_BUFFER];
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
        sink.write(&buffer[..n]);
    }
}

/// The result file: `None` when there is none; refused when larger than `limit`.
fn read_result(path: &Path, limit: usize) -> Option<Result<ResultFile, String>> {
    let file = match std::fs::File::open(path) {
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
    let leader = process.record().pid;
    // Parent links break once the leader dies, so they are read first.
    let mut strays = descendants(leader);
    strays.extend(token_holders(token));
    for stray in &strays {
        signal_stray(*stray, Signal::Term);
    }
    stop_all_with(
        std::slice::from_mut(process),
        grace.saturating_sub(Duration::from_secs(1)),
    );
    // Whatever started since is found by the token again, and on every wait below, so a
    // process forking while the others die is killed too.
    strays.extend(token_holders(token));
    for stray in &strays {
        signal_stray(*stray, Signal::Kill);
    }
    let deadline = stop_deadline(began, grace);
    while Instant::now() < deadline {
        let late = token_holders(token);
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

/// Every process of this user whose environment holds `BONGOS_CHECK_TOKEN=<token>`. Linux only;
/// elsewhere the environment of another process cannot be read.
#[cfg(target_os = "linux")]
fn token_holders(token: &str) -> Vec<Stray> {
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
            let environ = std::fs::read(entry.path().join("environ")).ok()?;
            environ
                .split(|byte| *byte == 0)
                .any(|variable| variable == needle)
                .then(|| Stray {
                    pid,
                    start_time: start_time(pid),
                })
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn token_holders(_token: &str) -> Vec<Stray> {
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
            strays.extend(token_holders(token));
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
