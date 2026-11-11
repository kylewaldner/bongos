//! Node processes (feature 20, D12). Each process starts with `util::process::Child::spawn`, which
//! puts it in a new session, so its pid is also its process group. Stop sends SIGTERM to the group,
//! then SIGKILL to the group after [`STOP_GRACE`] (even when the leader already exited, so its
//! children die too), so halt and cancel end every process within 5 s even when it ignores
//! SIGTERM.
//!
//! A started process is recorded with its pid and its start time (field 22 of `/proc/<pid>/stat`,
//! in clock ticks since boot). A crash leaves processes running, since `setsid` detaches them; at
//! the next load [`kill_orphans`] stops a recorded process only when the pid is alive with the same
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

/// Stops every process together: SIGTERM to each group, one shared [`STOP_GRACE`] while any
/// group still has a live member, then SIGKILL to every group. SIGKILL goes to the group even when
/// its leader already exited (on SIGTERM or before), so children left in the group die too; it is
/// never sent to the leader's pid alone. Many processes stop in the time of one.
pub fn stop_all(processes: &mut [NodeProcess]) {
    for process in processes.iter() {
        signal_group(process.record.pid, Signal::Term);
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && any_alive(processes) {
        std::thread::sleep(POLL);
    }
    for process in processes.iter() {
        // A group already gone answers ESRCH, which is ignored. A group cannot get a new
        // process with its id while any member lives, so this reaches only what the node left.
        signal_group(process.record.pid, Signal::Kill);
    }
    let deadline = Instant::now() + KILL_WAIT;
    while Instant::now() < deadline && any_alive(processes) {
        std::thread::sleep(POLL);
    }
}

/// Whether a leader is not yet reaped or a group still has a live member. Polling reaps the
/// leaders that exited.
fn any_alive(processes: &mut [NodeProcess]) -> bool {
    let mut alive = false;
    for process in processes.iter_mut() {
        let leader = !process.has_exited();
        alive |= leader || group_alive(process.record.pid);
    }
    alive
}

/// What [`kill_orphan`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanOutcome {
    /// The recorded process was alive with the recorded start time; it and its group were
    /// stopped and are gone.
    Killed,
    /// The recorded process was alive with the recorded start time and was signalled, but its
    /// group still had a live member after SIGKILL.
    StillRunning,
    /// No process with that pid, or one with another start time (a reused pid): left alone.
    NotRunning,
    /// The record has no pid or no start time, so nothing can be matched.
    Unknown,
}

/// Stops one process left by an earlier bongos; see [`kill_orphans`].
pub fn kill_orphan(record: ProcessRecord) -> OrphanOutcome {
    kill_orphans(&[record])[0]
}

/// Stops the processes left by an earlier bongos (a crash leaves them running, since `setsid`
/// detaches them), all together like [`stop_all`]: SIGTERM to every matched group, one shared
/// [`STOP_GRACE`], SIGKILL to every matched group, then one shared wait.
///
/// A record is matched only when its pid is alive (not a zombie) with the recorded start time; that
/// leader is the group's first process, so its group is the node's. A record whose leader is gone
/// is left alone even when a group with that id still has members: the leader's start time is all
/// the record holds, and members of a group whose id was reused after the node's group died cannot
/// be told apart from children the node left, so signalling them could kill an unrelated program.
/// Such children are logged and live on until they exit.
pub fn kill_orphans(records: &[ProcessRecord]) -> Vec<OrphanOutcome> {
    let mut outcomes: Vec<OrphanOutcome> = records
        .iter()
        .map(|record| {
            if record.pid == 0 || record.start_time == 0 {
                OrphanOutcome::Unknown
            } else if matches_record(*record) {
                OrphanOutcome::Killed
            } else {
                if group_alive(record.pid) {
                    log::warn!(
                        "workflows: process {} is gone but its group still has processes; \
                         they are left alone, since the group id may have been reused",
                        record.pid
                    );
                }
                OrphanOutcome::NotRunning
            }
        })
        .collect();
    let matched: Vec<u32> = records
        .iter()
        .zip(&outcomes)
        .filter(|(_, outcome)| **outcome == OrphanOutcome::Killed)
        .map(|(record, _)| record.pid)
        .collect();
    if matched.is_empty() {
        return outcomes;
    }
    for pid in &matched {
        log::info!("workflows: stopping process {pid} left by an earlier session");
        signal_group(*pid, Signal::Term);
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && matched.iter().any(|pid| group_alive(*pid)) {
        std::thread::sleep(POLL);
    }
    // The leader matched before SIGTERM; a group keeps its id while any member lives, so the
    // members still alive are the node's own.
    for pid in &matched {
        signal_group(*pid, Signal::Kill);
    }
    let deadline = Instant::now() + KILL_WAIT;
    while Instant::now() < deadline && matched.iter().any(|pid| group_alive(*pid)) {
        std::thread::sleep(POLL);
    }
    for (record, outcome) in records.iter().zip(outcomes.iter_mut()) {
        if *outcome == OrphanOutcome::Killed && group_alive(record.pid) {
            log::warn!(
                "workflows: process group {} still runs after SIGKILL",
                record.pid
            );
            *outcome = OrphanOutcome::StillRunning;
        }
    }
    outcomes
}

/// Whether process group `pgid` has a live member (zombies excepted). Without `/proc` it asks
/// `killpg(pgid, 0)`, which counts zombies too.
pub fn group_alive(pgid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return probe_group(pgid);
        };
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
                .and_then(stat)
                .is_some_and(|stat| stat.pgrp == pgid && stat.state != 'Z')
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        probe_group(pgid)
    }
}

fn probe_group(pgid: u32) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the group exists.
    pgid > 1 && unsafe { libc::killpg(pgid, 0) } == 0
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
    pgrp: u32,
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
/// and parentheses, so the fields are counted after the last `)`: state is field 3, process
/// group field 5, start time field 22.
fn parse_stat(text: &str) -> Option<Stat> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let pgrp = fields.nth(1)?.parse().ok()?;
    let start_time = fields.nth(16)?.parse().ok()?;
    Some(Stat {
        state,
        pgrp,
        start_time,
    })
}

enum Signal {
    Term,
    Kill,
}

/// Sends `signal` to the process group `pgid` (a node process leads its own, see
/// [`NodeProcess::spawn`]); never to a single pid. Refuses ids 0 and 1 and this process's own pid
/// and group, so a bad record cannot signal bongos or every process. A group already gone
/// (`ESRCH`) is ignored.
fn signal_group(pgid: u32, signal: Signal) {
    let Ok(pgid) = i32::try_from(pgid) else {
        return;
    };
    // SAFETY: getpid and getpgrp only read this process's ids.
    let (own_pid, own_group) = unsafe { (libc::getpid(), libc::getpgrp()) };
    if pgid <= 1 || pgid == own_pid || pgid == own_group {
        log::warn!("workflows: refused to signal process group {pgid}");
        return;
    }
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: killpg only sends a signal, to a group id above 1 that is not bongos's own.
    unsafe {
        libc::killpg(pgid, signal);
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
        assert_eq!(stat.pgrp, 4242);
        assert_eq!(stat.start_time, 987654);
    }
}
