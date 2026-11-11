//! Node processes (feature 20, D12). Each process starts with `util::process::Child::spawn`, which
//! puts it in a new session, so its pid is also its process group. Stop sends SIGTERM to the group,
//! then SIGKILL ([`util::process::Child::kill`]) after [`STOP_GRACE`], so halt and cancel end every
//! process within 5 s even when it ignores SIGTERM.
//!
//! A started process is recorded with its pid and its start time (field 22 of `/proc/<pid>/stat`,
//! in clock ticks since boot). A crash leaves processes running, since `setsid` detaches them; at
//! the next load [`kill_orphan`] stops a recorded process only when the pid is alive with the same
//! start time, so a reused pid is never killed.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use util::process::Child;

/// How long a stopped process has between SIGTERM and SIGKILL.
pub const STOP_GRACE: Duration = Duration::from_secs(2);

/// How long to wait for a process to go after SIGKILL.
const KILL_WAIT: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(20);

/// A process as the run record keeps it (`NodeStarted { pid, pid_start_time }`). A start time of 0
/// means unknown; such a record is never killed at load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessRecord {
    pub pid: u32,
    pub start_time: u64,
}

/// A node process this bongos started.
pub struct NodeProcess {
    child: Child,
    record: ProcessRecord,
}

impl NodeProcess {
    /// Starts `command` in its own session and records its pid and start time.
    pub fn spawn(command: Command, stdin: Stdio, stdout: Stdio, stderr: Stdio) -> Result<Self> {
        let child = Child::spawn(command, stdin, stdout, stderr)?;
        let pid = child.id();
        let record = ProcessRecord {
            pid,
            start_time: start_time(pid).unwrap_or(0),
        };
        Ok(Self { child, record })
    }

    pub fn record(&self) -> ProcessRecord {
        self.record
    }

    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Whether the process has exited (and was reaped).
    pub fn has_exited(&mut self) -> bool {
        !matches!(self.child.try_status(), Ok(None))
    }

    /// Stops the process group: SIGTERM, then SIGKILL after [`STOP_GRACE`]. Returns within
    /// [`STOP_GRACE`] plus about 2 s.
    pub fn stop(&mut self) {
        stop_all(std::slice::from_mut(self));
    }
}

/// Stops every process together: SIGTERM to each group, one shared [`STOP_GRACE`], then SIGKILL to
/// the groups still running. Many processes stop in the time of one.
pub fn stop_all(processes: &mut [NodeProcess]) {
    for process in processes.iter_mut() {
        if !process.has_exited() {
            signal_group(process.record.pid, Signal::Term);
        }
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && processes.iter_mut().any(|process| !process.has_exited()) {
        std::thread::sleep(POLL);
    }
    for process in processes.iter_mut() {
        if !process.has_exited()
            && let Err(error) = process.child.kill()
        {
            log::warn!(
                "workflows: could not kill process {}: {error}",
                process.record.pid
            );
        }
    }
    let deadline = Instant::now() + KILL_WAIT;
    while Instant::now() < deadline && processes.iter_mut().any(|process| !process.has_exited()) {
        std::thread::sleep(POLL);
    }
}

/// What [`kill_orphan`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanOutcome {
    /// The recorded process was alive with the recorded start time and was stopped.
    Killed,
    /// No process with that pid, or one with another start time (a reused pid): left alone.
    NotRunning,
    /// The record has no pid or no start time, so nothing can be matched.
    Unknown,
}

/// Stops a process left by an earlier bongos (a crash leaves them running) when `record` matches
/// a live process: same pid and same start time. A process with another start time is never
/// touched. Stop is SIGTERM to the group, then SIGKILL after [`STOP_GRACE`].
pub fn kill_orphan(record: ProcessRecord) -> OrphanOutcome {
    if record.pid == 0 || record.start_time == 0 {
        return OrphanOutcome::Unknown;
    }
    if !matches_record(record) {
        return OrphanOutcome::NotRunning;
    }
    log::info!(
        "workflows: stopping process {} left by an earlier session",
        record.pid
    );
    signal_group(record.pid, Signal::Term);
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && matches_record(record) {
        std::thread::sleep(POLL);
    }
    if matches_record(record) {
        signal_group(record.pid, Signal::Kill);
        let deadline = Instant::now() + KILL_WAIT;
        while Instant::now() < deadline && matches_record(record) {
            std::thread::sleep(POLL);
        }
    }
    OrphanOutcome::Killed
}

/// Whether `record.pid` is a live (not zombie) process with `record.start_time`.
fn matches_record(record: ProcessRecord) -> bool {
    match stat(record.pid) {
        Some(stat) => stat.start_time == record.start_time && stat.state != 'Z',
        None => false,
    }
}

/// The start time of process `pid`, in clock ticks since boot; `None` when it does not exist or
/// the platform has no `/proc`.
pub fn start_time(pid: u32) -> Option<u64> {
    stat(pid).map(|stat| stat.start_time)
}

struct Stat {
    state: char,
    start_time: u64,
}

#[cfg(target_os = "linux")]
fn stat(pid: u32) -> Option<Stat> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&text)
}

#[cfg(not(target_os = "linux"))]
fn stat(_pid: u32) -> Option<Stat> {
    None
}

/// Parses `/proc/<pid>/stat`. The command name (field 2) is in parentheses and may hold spaces
/// and parentheses, so the fields are counted after the last `)`: state is field 3, start time
/// field 22.
fn parse_stat(text: &str) -> Option<Stat> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start_time = fields.nth(18)?.parse().ok()?;
    Some(Stat { state, start_time })
}

enum Signal {
    Term,
    Kill,
}

/// Sends `signal` to the process group `pid` leads, or to the process alone when it leads none.
fn signal_group(pid: u32, signal: Signal) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    if pid <= 0 {
        return;
    }
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: killpg and kill only send a signal; the pid is positive, so neither targets every
    // process.
    unsafe {
        if libc::killpg(pid, signal) != 0 {
            libc::kill(pid, signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_stat;

    #[test]
    fn stat_fields_count_after_the_command_name() {
        let text = "4242 (a (weird) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 \
                    987654 1000 10 18446744073709551615";
        let stat = parse_stat(text).unwrap();
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.start_time, 987654);
    }
}
