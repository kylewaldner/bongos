//! Who holds or waits for a pool, and whether that holder is still alive.
//!
//! A holder is named by a path (its owner and the level inside that owner), a label for the views,
//! the host process that asked, and, once it exists, the holder's own process. Processes are named
//! by pid and start time, so a reused pid never keeps a hold. Whether a process lives is an input
//! (a [`Probe`]); nothing here reads the system.

use serde::{Deserialize, Serialize};

use crate::grant::{Hold, PoolTable, Waiter};

/// The key every level of one owner shares: the run id for anything inside a workflow run,
/// `check/<task id>/<check>` for a check started by hand or by landing, and `tree/<lane>` for a
/// hook outside any job.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OwnerKey(pub String);

/// Where inside its owner a holder sits. `Job` and `Run` enclose every other level of the same
/// owner and `Step { key }` encloses `Part { key, .. }` (see [`HolderPath::encloses`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    Job,
    Run,
    Step { key: String },
    Part { key: String, part: String },
    Hook { point: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HolderPath {
    pub owner: OwnerKey,
    pub level: Level,
}

impl HolderPath {
    /// Whether this path strictly encloses `other`: both have the same owner, the paths differ,
    /// and either this is a `Job` or `Run` level (each encloses every other level of its owner,
    /// so they enclose each other), or this is `Step { key }` and `other` is a `Part` of that key.
    /// Siblings (two steps, two parts, two hooks) never enclose each other.
    pub fn encloses(&self, other: &HolderPath) -> bool {
        if self.owner != other.owner || self == other {
            return false;
        }
        match (&self.level, &other.level) {
            (Level::Job | Level::Run, _) => true,
            (Level::Step { key }, Level::Part { key: part_key, .. }) => key == part_key,
            _ => false,
        }
    }
}

/// The text the views show for a holder ("lane build-2, job on Invoice export").
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HolderLabel(pub String);

/// The writer process that asked for a pool. `start` is the process start time as the platform
/// reports it (the `/proc` start time on Linux, seconds since the epoch elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HostRef {
    pub pid: u32,
    pub start: u64,
}

/// The holder's own process, once it exists, named like [`HostRef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProcRef {
    pub pid: u32,
    pub start: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// The id of the project the holder belongs to.
    pub project: String,
    pub path: HolderPath,
    pub label: HolderLabel,
    pub host: HostRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcRef>,
}

impl Holder {
    /// Whether this holder's path strictly encloses `other`'s within the same project.
    pub fn encloses(&self, other: &Holder) -> bool {
        self.project == other.project && self.path.encloses(&other.path)
    }

    /// Whether this holder's path is `other`'s or strictly encloses it, within the same project:
    /// such a holder cannot release while `other` waits.
    pub fn is_or_encloses(&self, other: &Holder) -> bool {
        self.project == other.project
            && (self.path == other.path || self.path.encloses(&other.path))
    }
}

/// Answers whether the process with this pid and start time still runs. A pid whose process
/// started at another time is a different process, and the answer is no.
pub trait Probe {
    fn alive(&self, pid: u32, start: u64) -> bool;
}

impl<F: Fn(u32, u64) -> bool> Probe for F {
    fn alive(&self, pid: u32, start: u64) -> bool {
        self(pid, start)
    }
}

/// A hold with a process lives exactly as long as that process; one without lives as long as its
/// host.
pub fn alive(hold: &Hold, probe: &impl Probe) -> bool {
    match hold.holder.process {
        Some(process) => probe.alive(process.pid, process.start),
        None => probe.alive(hold.holder.host.pid, hold.holder.host.start),
    }
}

/// A waiter lives as long as its host.
pub fn waiter_alive(waiter: &Waiter, probe: &impl Probe) -> bool {
    probe.alive(waiter.holder.host.pid, waiter.holder.host.start)
}

/// What [`prune`] removed, each with the name of its pool, for the log.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pruned {
    pub holds: Vec<(String, Hold)>,
    pub waiters: Vec<(String, Waiter)>,
}

impl Pruned {
    pub fn is_empty(&self) -> bool {
        self.holds.is_empty() && self.waiters.is_empty()
    }
}

/// Removes every dead hold and waiter from the table and returns them.
pub fn prune(table: &mut PoolTable, probe: &impl Probe) -> Pruned {
    let mut pruned = Pruned::default();
    for (name, pool) in &mut table.pools {
        let (live, dead): (Vec<Hold>, Vec<Hold>) = std::mem::take(&mut pool.holds)
            .into_iter()
            .partition(|hold| alive(hold, probe));
        pool.holds = live;
        pruned
            .holds
            .extend(dead.into_iter().map(|hold| (name.clone(), hold)));
        let (live, dead): (Vec<Waiter>, Vec<Waiter>) = std::mem::take(&mut pool.waiters)
            .into_iter()
            .partition(|waiter| waiter_alive(waiter, probe));
        pool.waiters = live;
        pruned
            .waiters
            .extend(dead.into_iter().map(|waiter| (name.clone(), waiter)));
    }
    pruned
}
