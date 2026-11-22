//! The command line of the lanes: `bongos lanes ...`, `bongos run` and `bongos host ...`.
//!
//! Each command reaches the process that holds the project's state writer, whatever it is (a
//! window or an engine host process), over the project's socket. With no writer, `lanes start`
//! and `run` start an engine host; `status`, `dry-run` and `pick` run the app binary once in its
//! read mode; the other commands say no lanes are running. Everything that touches the machine
//! (settings, project list, `host.json`, the socket, the app binary) goes through [`Env`], so the
//! dispatch is tested with fakes and no display. This module links only serde and the wire
//! format of the project socket, never the app.
//!
//! Exit codes: 0 done; 1 refused (message on stderr); 2 usage or project not found; 3 the host
//! could not start (its log path is printed); 4 protocol or version mismatch.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::{CommandFactory as _, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

/// The first words that make `bongos` run this module instead of opening paths.
pub const WORDS: [&str; 3] = ["lanes", "run", "host"];

/// What the engine host answers a request with when the writer moves to another process.
pub const MOVING_REFUSAL: &str = "the writer is moving; retry";

/// The text of the refusal when `lanes.enabled` is off.
pub const LANES_OFF: &str = "lanes are off (lanes.enabled); open the project in a window";

/// The schema a `status --json` document names.
pub const STATUS_SCHEMA: &str = "lanes-status 1";

pub const EXIT_DONE: i32 = 0;
pub const EXIT_REFUSED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_HOST_START: i32 = 3;
pub const EXIT_PROTOCOL: i32 = 4;

/// Whether `first` (the argument after the program name) is one of the words of this module.
pub fn is_lanes_word(first: Option<&str>) -> bool {
    first.is_some_and(|first| WORDS.contains(&first))
}

#[derive(Parser, Debug)]
#[command(name = "bongos", disable_version_flag = true)]
struct LanesCli {
    /// The project, by name, id or folder (default: the project whose folder holds the current
    /// directory, else the only project).
    #[arg(long, global = true, value_name = "name|id|path")]
    project: Option<String>,
    #[command(subcommand)]
    word: Word,
}

#[derive(Subcommand, Debug)]
enum Word {
    /// Control the lanes of a project.
    Lanes {
        #[command(subcommand)]
        verb: LanesWord,
    },
    /// Start a workflow run by hand.
    Run {
        workflow: String,
        /// The task, by id or path.
        #[arg(long)]
        task: String,
        /// Wait for room instead of refusing when the workflow is at its limit.
        #[arg(long)]
        queue: bool,
        #[arg(long, value_enum)]
        unattended: Option<OnOff>,
    },
    /// Control the engine host process of a project.
    Host {
        #[command(subcommand)]
        verb: HostWord,
    },
}

#[derive(Subcommand, Debug)]
enum LanesWord {
    Start {
        /// Stop once this task is done.
        #[arg(long)]
        until: Option<String>,
        /// Stop after this many jobs.
        #[arg(long)]
        max_jobs: Option<u32>,
        /// Work only within this task.
        #[arg(long)]
        within: Option<String>,
        #[arg(long, value_enum)]
        unattended: Option<OnOff>,
    },
    /// Stop one lane, or every lane, at its job boundary.
    Stop {
        lane: Option<String>,
    },
    Pause {
        lane: String,
    },
    Resume {
        lane: String,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    DryRun {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        lane: Option<String>,
    },
    Pick {
        #[arg(long)]
        lane: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum HostWord {
    Status,
    Stop {
        /// Halt running jobs now (they are interrupted) instead of at their job boundary.
        #[arg(long)]
        now: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum OnOff {
    On,
    Off,
}

impl OnOff {
    fn text(self) -> &'static str {
        match self {
            OnOff::On => "on",
            OnOff::Off => "off",
        }
    }
}

/// A project on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRef {
    pub id: String,
    /// The folder's name.
    pub name: String,
    pub folder: PathBuf,
    /// The project's state folder, where `host.json` lives.
    pub state: PathBuf,
}

/// The process that holds a project's writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterInfo {
    pub pid: u32,
    /// Whether it is a window (`false`: an engine host process).
    pub window: bool,
    /// The socket it serves; `None` when it serves none.
    pub socket: Option<PathBuf>,
}

/// Why a host did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartFailure {
    pub message: String,
    /// The host's log.
    pub log: PathBuf,
}

/// What a request to the writer ended in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The writer refused, with its text.
    Refused(String),
    /// The protocol or the version differs, or the writer's answer cannot be read.
    Protocol(String),
    /// The writer cannot be reached.
    Unreachable(String),
}

/// A connection to the writer.
pub trait Session {
    /// Sends the command (a `HostCommand` as JSON) and returns the writer's answer.
    fn request(&self, command: Value) -> Result<Value, RequestError>;
}

/// What the app binary printed in its read mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Everything the commands need from the machine.
pub trait Env {
    /// `lanes.enabled` of the user settings.
    fn lanes_enabled(&self) -> bool;
    fn projects(&self) -> Result<Vec<ProjectRef>, String>;
    fn current_dir(&self) -> PathBuf;
    /// The live writer of the project, if any.
    fn writer(&self, project: &ProjectRef) -> Option<WriterInfo>;
    fn connect(&self, socket: &Path) -> Result<Box<dyn Session>, RequestError>;
    /// Starts an engine host. With `hand_over` a window holds the writer: the host takes it over,
    /// and this returns once the host holds it. Otherwise this returns once a writer is live.
    fn start_host(
        &self,
        project: &ProjectRef,
        reason: &str,
        hand_over: bool,
    ) -> Result<WriterInfo, StartFailure>;
    /// Runs the app binary once in its `--read` mode with `args` (`status`, `--json`, ...).
    fn read_mode(&self, project: &ProjectRef, args: &[String]) -> Result<ReadOutput, String>;
}

/// What a command printed and how it ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    fn done(text: impl Into<String>) -> Self {
        Self {
            code: EXIT_DONE,
            stdout: line(text.into()),
            stderr: String::new(),
        }
    }

    fn fail(code: i32, text: impl Into<String>) -> Self {
        Self {
            code,
            stdout: String::new(),
            stderr: line(text.into()),
        }
    }
}

fn line(mut text: String) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Runs the command `args` (the arguments after the program name) and says what it printed.
pub fn execute(args: &[String], env: &dyn Env) -> Outcome {
    let parsed = match LanesCli::try_parse_from(
        std::iter::once("bongos".to_string()).chain(args.iter().cloned()),
    ) {
        Ok(parsed) => parsed,
        Err(error) => {
            let text = error.render().to_string();
            return if error.use_stderr() {
                Outcome::fail(EXIT_USAGE, text)
            } else {
                Outcome::done(text)
            };
        }
    };
    if let Err(text) = check_lanes(&parsed.word) {
        return Outcome::fail(EXIT_USAGE, text);
    }
    if !env.lanes_enabled() {
        return Outcome::fail(EXIT_REFUSED, LANES_OFF);
    }
    let project = match select_project(parsed.project.as_deref(), env) {
        Ok(project) => project,
        Err(text) => return Outcome::fail(EXIT_USAGE, text),
    };
    let command = command_of(&parsed.word);
    run_command(&project, command, env)
}

/// The lane names in the arguments must look like `build-2`.
fn check_lanes(word: &Word) -> Result<(), String> {
    let lanes: Vec<&String> = match word {
        Word::Lanes { verb } => match verb {
            LanesWord::Stop { lane } => lane.iter().collect(),
            LanesWord::Pause { lane }
            | LanesWord::Resume { lane }
            | LanesWord::Pick { lane, .. } => {
                vec![lane]
            }
            LanesWord::DryRun { lane, .. } => lane.iter().collect(),
            LanesWord::Start { .. } | LanesWord::Status { .. } => Vec::new(),
        },
        _ => Vec::new(),
    };
    for lane in lanes {
        let valid = lane.rsplit_once('-').is_some_and(|(kind, index)| {
            !kind.is_empty() && index.parse::<u32>().is_ok_and(|index| index >= 1)
        });
        if !valid {
            return Err(format!("{lane} is not a lane (like build-2)"));
        }
    }
    Ok(())
}

/// What a parsed command asks for.
#[derive(Debug)]
enum Command {
    Start(Value),
    Run(Value),
    LaneVerb {
        verb: &'static str,
        lane: Option<String>,
        json: bool,
        /// What a window or host is told.
        payload: Value,
    },
    HostStatus,
    HostStop {
        now: bool,
    },
}

fn command_of(word: &Word) -> Command {
    match word {
        Word::Lanes { verb } => match verb {
            LanesWord::Start {
                until,
                max_jobs,
                within,
                unattended,
            } => Command::Start(json!({
                "until": until,
                "max_jobs": max_jobs,
                "within": within,
                "unattended": unattended.map(OnOff::text),
            })),
            LanesWord::Stop { lane } => lane_verb("stop", lane.clone(), false),
            LanesWord::Pause { lane } => lane_verb("pause", Some(lane.clone()), false),
            LanesWord::Resume { lane } => lane_verb("resume", Some(lane.clone()), false),
            LanesWord::Status { json } => lane_verb("status", None, *json),
            LanesWord::DryRun { json, lane } => lane_verb("dry-run", lane.clone(), *json),
            LanesWord::Pick { lane, json } => lane_verb("pick", Some(lane.clone()), *json),
        },
        Word::Run {
            workflow,
            task,
            queue,
            unattended,
        } => Command::Run(json!({
            "task_id": task,
            "workflow": workflow,
            "queue": queue,
            "unattended": unattended.map(OnOff::text),
        })),
        Word::Host { verb } => match verb {
            HostWord::Status => Command::HostStatus,
            HostWord::Stop { now } => Command::HostStop { now: *now },
        },
    }
}

fn lane_verb(verb: &'static str, lane: Option<String>, json: bool) -> Command {
    Command::LaneVerb {
        verb,
        payload: json!({ "lane": lane }),
        lane,
        json,
    }
}

/// The project named by `--project`, else the one whose folder holds the current directory, else
/// the only one.
fn select_project(named: Option<&str>, env: &dyn Env) -> Result<ProjectRef, String> {
    let projects = env.projects()?;
    if projects.is_empty() {
        return Err("no project is set up on this machine".to_string());
    }
    let list = |projects: &[ProjectRef]| {
        projects
            .iter()
            .map(|project| {
                format!(
                    "  {} ({}, {})",
                    project.name,
                    project.id,
                    project.folder.display()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    if let Some(named) = named {
        let cwd = env.current_dir();
        let as_path = cwd.join(named);
        let matches: Vec<&ProjectRef> = projects
            .iter()
            .filter(|project| {
                project.id == named
                    || project.folder == as_path
                    || project.folder == Path::new(named)
            })
            .collect();
        let matches = if matches.is_empty() {
            projects
                .iter()
                .filter(|project| project.name == named)
                .collect()
        } else {
            matches
        };
        return match matches.as_slice() {
            [project] => Ok((*project).clone()),
            [] => Err(format!(
                "no project {named}; the projects are:\n{}",
                list(&projects)
            )),
            several => Err(format!(
                "{named} names several projects; use an id:\n{}",
                list(
                    &several
                        .iter()
                        .map(|project| (*project).clone())
                        .collect::<Vec<_>>()
                )
            )),
        };
    }
    let cwd = env.current_dir();
    let containing = projects
        .iter()
        .filter(|project| cwd.starts_with(&project.folder))
        .max_by_key(|project| project.folder.components().count());
    if let Some(project) = containing {
        return Ok(project.clone());
    }
    match projects.as_slice() {
        [only] => Ok(only.clone()),
        _ => Err(format!(
            "the current directory is in no project; pass --project, one of:\n{}",
            list(&projects)
        )),
    }
}

fn run_command(project: &ProjectRef, command: Command, env: &dyn Env) -> Outcome {
    let writer = env.writer(project);
    match command {
        Command::Start(payload) => {
            let attach = match &writer {
                // A window holds the writer: a host takes it over.
                Some(writer) if writer.window => {
                    env.start_host(project, "bongos lanes start", true)
                }
                Some(writer) => Ok(writer.clone()),
                None => env.start_host(project, "bongos lanes start", false),
            };
            let writer = match attach {
                Ok(writer) => writer,
                Err(failure) => return start_failed(&failure),
            };
            send(
                env,
                &writer,
                host_command("lanes", "start", payload),
                |reply| message_of(&reply),
            )
        }
        Command::Run(payload) => {
            let writer = match writer {
                Some(writer) => writer,
                None => match env.start_host(project, "bongos run", false) {
                    Ok(writer) => writer,
                    Err(failure) => return start_failed(&failure),
                },
            };
            send(
                env,
                &writer,
                host_command("workflow", "start", payload),
                |reply| message_of(&reply),
            )
        }
        Command::LaneVerb {
            verb,
            lane,
            json,
            payload,
        } => match writer {
            Some(writer) => {
                let render_json = json;
                let shown_verb = verb;
                send(
                    env,
                    &writer,
                    host_command("lanes", verb, payload),
                    move |reply| render_lane_reply(shown_verb, reply, render_json),
                )
            }
            None if matches!(verb, "status" | "dry-run" | "pick") => {
                let mut args = vec![verb.to_string()];
                if json {
                    args.push("--json".to_string());
                }
                if let Some(lane) = lane {
                    args.push("--lane".to_string());
                    args.push(lane);
                }
                match env.read_mode(project, &args) {
                    Ok(output) => Outcome {
                        code: output.code,
                        stdout: output.stdout,
                        stderr: output.stderr,
                    },
                    Err(text) => Outcome::fail(EXIT_REFUSED, text),
                }
            }
            None => Outcome::done("no lanes are running"),
        },
        Command::HostStatus => match writer {
            Some(writer) => send(env, &writer, json!({ "host": "status" }), |reply| {
                render_object(&reply)
            }),
            None => Outcome::done(format!("no engine host runs for {}", project.name)),
        },
        Command::HostStop { now } => match writer {
            Some(writer) => send(
                env,
                &writer,
                json!({ "host": { "stop": { "at_job_boundary": !now } } }),
                |reply| message_of(&reply),
            ),
            None => Outcome::done("no engine host is running"),
        },
    }
}

fn start_failed(failure: &StartFailure) -> Outcome {
    Outcome::fail(
        EXIT_HOST_START,
        format!("{}; see {}", failure.message, failure.log.display()),
    )
}

/// `{ "<kind>": { "verb": ..., "payload": ... } }` as the wire's `HostCommand`.
fn host_command(kind: &str, verb: &str, payload: Value) -> Value {
    json!({ kind: { "verb": verb, "payload": payload } })
}

/// Sends `command` to `writer` and renders the answer with `render`.
fn send(
    env: &dyn Env,
    writer: &WriterInfo,
    command: Value,
    render: impl FnOnce(Value) -> Result<String, String>,
) -> Outcome {
    let Some(socket) = &writer.socket else {
        return Outcome::fail(
            EXIT_REFUSED,
            format!(
                "the process that holds this project (pid {}) serves no socket; start lanes in it from its window",
                writer.pid
            ),
        );
    };
    let session = match env.connect(socket) {
        Ok(session) => session,
        Err(error) => return request_failed(error),
    };
    match session.request(command) {
        Ok(reply) => match render(reply) {
            Ok(text) => Outcome::done(text),
            Err(text) => Outcome::fail(EXIT_PROTOCOL, text),
        },
        Err(error) => request_failed(error),
    }
}

fn request_failed(error: RequestError) -> Outcome {
    match error {
        RequestError::Refused(text) | RequestError::Unreachable(text) => {
            Outcome::fail(EXIT_REFUSED, text)
        }
        RequestError::Protocol(text) => Outcome::fail(EXIT_PROTOCOL, text),
    }
}

fn message_of(reply: &Value) -> Result<String, String> {
    Ok(match reply {
        Value::String(text) => text.clone(),
        Value::Object(map) => match map.get("message").and_then(Value::as_str) {
            Some(text) => text.to_string(),
            None => render_object(reply)?,
        },
        Value::Null => "done".to_string(),
        other => other.to_string(),
    })
}

fn render_object(reply: &Value) -> Result<String, String> {
    match reply {
        Value::Object(map) => Ok(map
            .iter()
            .map(|(key, value)| match value {
                Value::String(text) => format!("{key}: {text}"),
                other => format!("{key}: {other}"),
            })
            .collect::<Vec<_>>()
            .join("\n")),
        other => Ok(other.to_string()),
    }
}

fn render_lane_reply(verb: &str, reply: Value, as_json: bool) -> Result<String, String> {
    if verb == "status" {
        let doc = reply
            .get("status")
            .cloned()
            .ok_or_else(|| "the engine host sent no lanes status".to_string())?;
        if doc.get("schema").and_then(Value::as_str) != Some(STATUS_SCHEMA) {
            return Err(format!(
                "the engine host's lanes status is not {STATUS_SCHEMA}; use the same bongos version as the host"
            ));
        }
        return Ok(if as_json {
            serde_json::to_string_pretty(&doc).map_err(|error| error.to_string())?
        } else {
            render_status(&doc)
        });
    }
    if as_json {
        return serde_json::to_string_pretty(&reply).map_err(|error| error.to_string());
    }
    message_of(&reply)
}

/// The plain-text status of a `lanes-status 1` document.
pub fn render_status(doc: &Value) -> String {
    let mut out = String::new();
    let header = &doc["header"];
    let count = |key: &str| header[key].as_u64().unwrap_or(0);
    writeln!(
        out,
        "lanes: {} (dispatcher {}): {} running, {} idle, {} waiting, {} paused, {} stopped, {} halted",
        count("lanes"),
        header["dispatcher"].as_str().unwrap_or("unknown"),
        count("running"),
        count("idle"),
        count("waiting"),
        count("paused"),
        count("stopped"),
        count("halted"),
    )
    .ok();
    if let Some(split) = doc.get("master_split") {
        let _ = split;
        writeln!(
            out,
            "master is split: every landing waits until a person runs project: resolve split"
        )
        .ok();
    }
    for lane in doc["lanes"].as_array().into_iter().flatten() {
        let text = |key: &str| lane[key].as_str();
        let mut row = format!(
            "{}  {}",
            text("id").unwrap_or("?"),
            text("state").unwrap_or("?")
        );
        if let Some(task) = text("task") {
            write!(row, "  {task}").ok();
            if let Some(goal) = text("goal") {
                write!(row, " ({goal})").ok();
            }
        }
        if let Some(run) = text("run") {
            write!(row, "  run {run}").ok();
            if let Some(workflow) = text("workflow") {
                write!(row, " of {workflow}").ok();
            }
            if let Some(lifecycle) = text("run_lifecycle") {
                write!(row, " ({lifecycle})").ok();
            }
        }
        if let Some(reason) = text("reason") {
            write!(row, "  {reason}").ok();
        }
        write!(
            row,
            "  {} jobs done",
            lane["jobs_done"].as_u64().unwrap_or(0)
        )
        .ok();
        writeln!(out, "{row}").ok();
    }
    out.trim_end().to_string()
}

/// The usage text of the lanes words, for `bongos --help`.
pub fn usage() -> String {
    LanesCli::command().render_help().to_string()
}

#[cfg(unix)]
pub use real::RealEnv;

#[cfg(unix)]
pub(crate) mod real {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use serde::Deserialize;
    use serde_json::{Value, json};

    use super::{
        Env, MOVING_REFUSAL, ProjectRef, ReadOutput, RequestError, Session, StartFailure,
        WriterInfo,
    };

    /// The protocol of the project socket this build speaks.
    const PROTOCOL_VERSION: u32 = 1;
    const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(15);
    const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(25);
    const MOVING_RETRY: Duration = Duration::from_millis(50);

    /// The machine this process runs on.
    pub struct RealEnv {
        /// The app binary that runs engine hosts and the read mode.
        pub program: PathBuf,
        /// The version the project socket's server must run.
        pub bongos_version: String,
    }

    #[derive(Deserialize)]
    struct Record {
        #[serde(default, rename = "project")]
        projects: Vec<Location>,
    }

    #[derive(Deserialize)]
    struct Location {
        id: String,
        folder: PathBuf,
        state: PathBuf,
    }

    #[derive(Deserialize)]
    struct HostFile {
        pid: u32,
        start_time: Option<u64>,
        socket: Option<PathBuf>,
        mode: String,
        moving: Option<serde::de::IgnoredAny>,
    }

    fn user_settings() -> Value {
        std::fs::read_to_string(paths::settings_file())
            .ok()
            .and_then(|text| serde_json_lenient::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null)
    }

    fn start_timeout() -> Duration {
        user_settings()["lanes"]["host"]["start_timeout_seconds"]
            .as_f64()
            .filter(|seconds| *seconds > 0.0)
            .map_or(DEFAULT_START_TIMEOUT, Duration::from_secs_f64)
    }

    #[cfg(target_os = "linux")]
    fn start_time(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after = stat.rsplit_once(')')?.1;
        after.split_whitespace().nth(19)?.parse().ok()
    }

    #[cfg(not(target_os = "linux"))]
    fn start_time(_pid: u32) -> Option<u64> {
        None
    }

    fn process_is(pid: u32, recorded: Option<u64>) -> bool {
        if pid == 0 {
            return false;
        }
        match start_time(pid) {
            Some(now) => recorded == Some(now),
            None if cfg!(target_os = "linux") => false,
            None => {
                let Ok(pid) = libc::pid_t::try_from(pid) else {
                    return false;
                };
                // SAFETY: signal 0 only checks that the process exists.
                recorded.is_none() && unsafe { libc::kill(pid, 0) == 0 }
            }
        }
    }

    fn host_file(project: &ProjectRef) -> Option<(HostFile, bool)> {
        let bytes = std::fs::read(project.state.join("host.json")).ok()?;
        let file: HostFile = serde_json::from_slice(&bytes).ok()?;
        let live = process_is(file.pid, file.start_time);
        Some((file, live))
    }

    fn writer_of(file: &HostFile) -> WriterInfo {
        WriterInfo {
            pid: file.pid,
            window: file.mode == "window",
            socket: file.socket.clone(),
        }
    }

    impl Env for RealEnv {
        fn lanes_enabled(&self) -> bool {
            user_settings()["lanes"]["enabled"]
                .as_bool()
                .unwrap_or(false)
        }

        fn projects(&self) -> Result<Vec<ProjectRef>, String> {
            let path = paths::state_dir().join("project-repositories.toml");
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(format!("reading {}: {error}", path.display())),
            };
            let record: Record = toml::from_str(&text)
                .map_err(|error| format!("reading {}: {error}", path.display()))?;
            Ok(record
                .projects
                .into_iter()
                .map(|location| ProjectRef {
                    name: location.folder.file_name().map_or_else(
                        || location.id.clone(),
                        |name| name.to_string_lossy().into_owned(),
                    ),
                    id: location.id,
                    folder: location.folder,
                    state: location.state,
                })
                .collect())
        }

        fn current_dir(&self) -> PathBuf {
            std::env::current_dir().unwrap_or_default()
        }

        fn writer(&self, project: &ProjectRef) -> Option<WriterInfo> {
            let (file, live) = host_file(project)?;
            live.then(|| writer_of(&file))
        }

        fn connect(&self, socket: &Path) -> Result<Box<dyn Session>, RequestError> {
            Ok(Box::new(SocketSession::connect(
                socket,
                &self.bongos_version,
                start_timeout(),
            )?))
        }

        fn start_host(
            &self,
            project: &ProjectRef,
            reason: &str,
            hand_over: bool,
        ) -> Result<WriterInfo, StartFailure> {
            let log =
                paths::logs_dir().join(format!("engine-host-{}.log", project_hex(&project.id)));
            let failure = |message: String| StartFailure {
                message,
                log: log.clone(),
            };
            if !hand_over {
                if let Some(writer) = self.writer(project) {
                    return Ok(writer);
                }
            }
            if let Some(directory) = log.parent() {
                std::fs::create_dir_all(directory).map_err(|error| failure(error.to_string()))?;
            }
            let output = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .map_err(|error| failure(format!("opening the host log: {error}")))?;
            writeln!(&output, "starting the engine host: {reason}").ok();
            let errors = output
                .try_clone()
                .map_err(|error| failure(error.to_string()))?;
            let mut command = std::process::Command::new(&self.program);
            command.arg("--engine-host").arg(&project.id);
            let mut child = util::process::Child::spawn(
                command,
                Stdio::null(),
                Stdio::from(output),
                Stdio::from(errors),
            )
            .map_err(|error| failure(format!("starting the engine host: {error}")))?;
            let timeout = start_timeout();
            let deadline = Instant::now() + timeout;
            loop {
                if let Some((file, true)) = host_file(project) {
                    let done = file.moving.is_none() && (!hand_over || file.mode == "host");
                    if done {
                        return Ok(writer_of(&file));
                    }
                }
                if let Ok(Some(status)) = child.try_status() {
                    // A host refused by the lock exits while the first one holds the writer.
                    if let Some((file, true)) = host_file(project)
                        && file.moving.is_none()
                    {
                        return Ok(writer_of(&file));
                    }
                    return Err(failure(format!("the engine host exited ({status})")));
                }
                if Instant::now() >= deadline {
                    return Err(failure(format!(
                        "the engine host did not start within {}s",
                        timeout.as_secs_f64()
                    )));
                }
                std::thread::sleep(POLL);
            }
        }

        // The command line has nothing else to do while the read mode runs, so it blocks.
        #[allow(clippy::disallowed_methods)]
        fn read_mode(&self, project: &ProjectRef, args: &[String]) -> Result<ReadOutput, String> {
            let output = std::process::Command::new(&self.program)
                .arg("--engine-host")
                .arg(&project.id)
                .arg("--read")
                .args(args)
                .stdin(Stdio::null())
                .output()
                .map_err(|error| format!("running {}: {error}", self.program.display()))?;
            Ok(ReadOutput {
                code: output.status.code().unwrap_or(1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    /// The first 12 hex digits of the SHA-256 of the project id, which name the host's log.
    fn project_hex(project_id: &str) -> String {
        use sha2::{Digest as _, Sha256};
        let digest = Sha256::digest(project_id.as_bytes());
        digest
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// A connection to the project's socket.
    pub struct SocketSession {
        socket: PathBuf,
        version: String,
        moving_timeout: Duration,
        stream: std::sync::Mutex<(BufReader<UnixStream>, UnixStream)>,
    }

    fn open(
        socket: &Path,
        version: &str,
    ) -> Result<(BufReader<UnixStream>, UnixStream), RequestError> {
        let stream = UnixStream::connect(socket).map_err(|error| {
            RequestError::Unreachable(format!("connecting to {}: {error}", socket.display()))
        })?;
        stream.set_read_timeout(Some(ANSWER_TIMEOUT)).ok();
        let mut writer = stream
            .try_clone()
            .map_err(|error| RequestError::Unreachable(error.to_string()))?;
        let hello = json!({
            "message": "hello",
            "protocol": PROTOCOL_VERSION,
            "bongos_version": version,
            "client": "cli",
        });
        writeln!(writer, "{hello}")
            .map_err(|error| RequestError::Unreachable(error.to_string()))?;
        let mut reader = BufReader::new(stream);
        let mut text = String::new();
        reader.read_line(&mut text).map_err(|error| {
            RequestError::Unreachable(format!("the engine host did not answer: {error}"))
        })?;
        let answer: Value = serde_json::from_str(&text).map_err(|error| {
            RequestError::Protocol(format!("reading the engine host's answer: {error}"))
        })?;
        match answer["message"].as_str() {
            Some("welcome") => Ok((reader, writer)),
            Some("refused") => Err(RequestError::Protocol(
                answer["text"]
                    .as_str()
                    .unwrap_or("the engine host refused")
                    .to_string(),
            )),
            _ => Err(RequestError::Protocol(format!(
                "the engine host answered {answer} to hello"
            ))),
        }
    }

    impl SocketSession {
        pub fn connect(
            socket: &Path,
            version: &str,
            moving_timeout: Duration,
        ) -> Result<Self, RequestError> {
            Ok(Self {
                socket: socket.to_path_buf(),
                version: version.to_string(),
                moving_timeout,
                stream: std::sync::Mutex::new(open(socket, version)?),
            })
        }

        fn ask(&self, id: u64, command: &Value) -> Result<Result<Value, String>, RequestError> {
            let mut guard = self
                .stream
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (reader, writer) = &mut *guard;
            let request = json!({ "message": "request", "id": id, "command": command });
            writeln!(writer, "{request}")
                .map_err(|error| RequestError::Unreachable(error.to_string()))?;
            loop {
                let mut text = String::new();
                let read = reader.read_line(&mut text).map_err(|error| {
                    RequestError::Unreachable(format!("the engine host did not answer: {error}"))
                })?;
                if read == 0 {
                    return Err(RequestError::Unreachable(
                        "the engine host closed the connection".to_string(),
                    ));
                }
                let message: Value = serde_json::from_str(&text).map_err(|error| {
                    RequestError::Protocol(format!("reading the engine host's answer: {error}"))
                })?;
                if message["message"] != "reply" || message["id"] != id {
                    continue;
                }
                let result = &message["result"];
                return Ok(if let Some(value) = result.get("ok") {
                    Ok(value.clone())
                } else if let Some(text) = result.get("refused").and_then(Value::as_str) {
                    Err(text.to_string())
                } else {
                    return Err(RequestError::Protocol(format!(
                        "unreadable reply {message}"
                    )));
                });
            }
        }
    }

    impl Session for SocketSession {
        fn request(&self, command: Value) -> Result<Value, RequestError> {
            let deadline = Instant::now() + self.moving_timeout;
            let mut id = 1;
            loop {
                match self.ask(id, &command) {
                    Ok(Ok(value)) => return Ok(value),
                    Ok(Err(text)) if text == MOVING_REFUSAL && Instant::now() < deadline => {}
                    Ok(Err(text)) => return Err(RequestError::Refused(text)),
                    Err(error @ RequestError::Protocol(_)) => return Err(error),
                    Err(error) => {
                        if Instant::now() >= deadline {
                            return Err(error);
                        }
                    }
                }
                std::thread::sleep(MOVING_RETRY);
                // The old writer stops serving and the next binds the same path.
                if let Ok(next) = open(&self.socket, &self.version) {
                    *self
                        .stream
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
                }
                id += 1;
            }
        }
    }
}

#[cfg(test)]
#[path = "lanes_cli_tests.rs"]
mod tests;
