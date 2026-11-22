//! [`ensure_host`]: makes sure a project has a live writer to attach to, starting an engine host
//! process when it has none. Whichever process `host.json` then names as a live writer is the
//! one to attach to: two callers that start a host at once both attach to the one that took the
//! writer lock, and the other host, refused by the lock, exits.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use bongos_project_repo::Location;
use chrono::Utc;

use crate::host_file::{HostInfo, live_host};
use crate::protocol::HostMode;

/// The hidden flag that runs the app as a project's engine host.
pub const ENGINE_HOST_FLAG: &str = "--engine-host";

/// How often `host.json` is read again while waiting for a writer.
pub const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Starts an engine host process.
pub trait HostSpawner: Send + Sync {
    /// Starts the engine host of `project_id` detached, its output appended to `log`.
    fn spawn(&self, project_id: &str, log: &Path) -> Result<Box<dyn SpawnedHost>>;
}

/// An engine host process this process started.
pub trait SpawnedHost: Send {
    /// How it exited once it did (it is reaped then), `None` while it runs.
    fn try_exit(&mut self) -> Option<String>;
    /// Hands the process to a thread that reaps it whenever it exits, so it never stays a zombie.
    fn release(self: Box<Self>);
    /// Its process id, when it has one.
    fn pid(&self) -> Option<u32> {
        None
    }
}

/// Runs `<program> --engine-host <project id>` in a new session, stdin from `/dev/null`, stdout
/// and stderr appended to the host's log.
pub struct ProgramSpawner {
    pub program: PathBuf,
}

impl ProgramSpawner {
    /// The spawner of this executable.
    pub fn current_exe() -> Result<Self> {
        Ok(Self {
            program: std::env::current_exe().context("finding this executable")?,
        })
    }
}

impl HostSpawner for ProgramSpawner {
    fn spawn(&self, project_id: &str, log: &Path) -> Result<Box<dyn SpawnedHost>> {
        let output = append_to(log)?;
        let errors = output.try_clone()?;
        let mut command = std::process::Command::new(&self.program);
        command.arg(ENGINE_HOST_FLAG).arg(project_id);
        let child = util::process::Child::spawn(
            command,
            Stdio::null(),
            Stdio::from(output),
            Stdio::from(errors),
        )?;
        Ok(Box::new(SpawnedProcess(child)))
    }
}

struct SpawnedProcess(util::process::Child);

impl SpawnedHost for SpawnedProcess {
    fn try_exit(&mut self) -> Option<String> {
        match self.0.try_status() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(error) => Some(format!("unknown ({error})")),
        }
    }

    fn pid(&self) -> Option<u32> {
        Some(self.0.id())
    }

    fn release(self: Box<Self>) {
        let mut child = self.0;
        let reaping = std::thread::Builder::new()
            .name("engine-host-reaper".to_string())
            .spawn(move || {
                futures::executor::block_on(child.status()).ok();
            });
        if let Err(error) = reaping {
            log::error!("starting the engine host's reaper: {error}");
        }
    }
}

fn append_to(log: &Path) -> Result<std::fs::File> {
    if let Some(directory) = log.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))
}

/// How [`ensure_host`] starts a host and how long it waits.
#[derive(Clone)]
pub struct EnsureHost {
    pub spawner: Arc<dyn HostSpawner>,
    /// The engine host's log ([`crate::host_log_path`]).
    pub log: PathBuf,
    /// `lanes.host.start_timeout_seconds`.
    pub start_timeout: Duration,
    pub poll_interval: Duration,
}

/// The live writer to attach to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attach {
    pub writer_pid: u32,
    /// The socket to connect to with [`crate::HostClient`]; `None` when the writer serves none.
    pub socket: Option<PathBuf>,
    pub mode: HostMode,
    /// Whether this call started a host process (which may not be the one that won).
    pub spawned: bool,
}

impl Attach {
    fn of(info: HostInfo, spawned: bool) -> Self {
        Self {
            writer_pid: info.pid,
            socket: info.socket,
            mode: info.mode,
            spawned,
        }
    }
}

/// Makes sure `location`'s project has a live writer and says how to attach to it. A live writer
/// named by `host.json` is attached to at once (when it is moving to another process, after the
/// move ended or the start timeout passed); with none an engine host is started and the call
/// waits up to the start timeout for `host.json` to name a live writer, whichever process that
/// is (a `host.json` naming a dead pid or another start time is never live). `reason` says why a host is
/// needed; it goes to the host's log and to the error. Blocks: call it off the main thread.
pub fn ensure_host(location: &Location, reason: &str, options: &EnsureHost) -> Result<Attach> {
    if let Some(info) = live_host(location) {
        if info.moving.is_none() {
            return Ok(Attach::of(info, false));
        }
        let deadline = Instant::now() + options.start_timeout;
        if let Some(info) = wait_for_writer(location, deadline, options.poll_interval, &mut || {}) {
            return Ok(Attach::of(info, false));
        }
        // The move did not end in time; a writer still live is the one to reach (another host
        // would be refused by its lock).
        if let Some(info) = live_host(location) {
            return Ok(Attach::of(info, false));
        }
    }

    note(&options.log, &format!("starting the engine host: {reason}"));
    let mut spawned = options
        .spawner
        .spawn(&location.id, &options.log)
        .with_context(|| {
            format!(
                "starting the engine host of {} ({reason}); see {}",
                location.id,
                options.log.display()
            )
        })?;
    let deadline = Instant::now() + options.start_timeout;
    let mut exited = None;
    let found = wait_for_writer(location, deadline, options.poll_interval, &mut || {
        if exited.is_none() {
            exited = spawned.try_exit();
        }
    });
    if exited.is_none() {
        spawned.release();
    }
    match found {
        Some(info) => Ok(Attach::of(info, true)),
        None => {
            let exited = exited
                .map(|status| format!("; it exited ({status})"))
                .unwrap_or_default();
            Err(anyhow!(
                "the engine host of {} did not start within {}s ({reason}){exited}; see {}",
                location.id,
                options.start_timeout.as_secs_f64(),
                options.log.display()
            ))
        }
    }
}

/// Reads `host.json` until it names a live writer that is not moving, or `deadline` passes.
/// `each` runs once per look (the caller checks on its child there).
fn wait_for_writer(
    location: &Location,
    deadline: Instant,
    poll: Duration,
    each: &mut dyn FnMut(),
) -> Option<HostInfo> {
    loop {
        each();
        if let Some(info) = live_host(location).filter(|info| info.moving.is_none()) {
            return Some(info);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::sleep(poll.min(deadline - now));
    }
}

/// Appends a line to the host's log, best effort.
fn note(log: &Path, text: &str) {
    if let Ok(mut file) = append_to(log) {
        writeln!(file, "{} {text}", Utc::now().to_rfc3339()).ok();
    }
}
