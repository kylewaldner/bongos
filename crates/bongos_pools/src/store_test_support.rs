//! Shared inputs of the store tests: hosts whose liveness the test decides, holders, requests,
//! project locations in temporary directories.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bongos_pools_model::{
    Holder, HolderLabel, HolderPath, HostRef, Level, Need, OwnerKey, PoolKind, PoolState,
    PoolTable, Request, RequestId, Timestamp,
};
use bongos_project_repo::{Location, StateApplyHooks, StateStore, StateWriter};

use crate::{LockWait, MachineStore, StoreContext};

pub(crate) const NOW: Timestamp = Timestamp(1_700_000_000_000);

pub(crate) fn host(pid: u32) -> HostRef {
    HostRef {
        pid,
        start: 1_000 + u64::from(pid),
    }
}

/// Which pids are dead; every other pid lives.
#[derive(Default)]
pub(crate) struct Liveness {
    dead: Mutex<BTreeSet<u32>>,
}

impl Liveness {
    pub(crate) fn new() -> Arc<Liveness> {
        Arc::new(Liveness::default())
    }

    pub(crate) fn kill(&self, pid: u32) {
        self.dead.lock().unwrap().insert(pid);
    }

    fn alive(&self, pid: u32, start: u64) -> bool {
        start == 1_000 + u64::from(pid) && !self.dead.lock().unwrap().contains(&pid)
    }
}

pub(crate) fn context(host: HostRef, live: &Arc<Liveness>) -> StoreContext {
    let live = live.clone();
    StoreContext::new(host, move |pid, start| live.alive(pid, start), || NOW)
}

/// A machine store in `dir` for `host` that tries the lock once.
pub(crate) fn machine(dir: &Path, host: HostRef, live: &Arc<Liveness>) -> MachineStore {
    MachineStore::open(dir.to_path_buf(), context(host, live), LockWait::once()).unwrap()
}

/// A machine store in `dir` that waits for the lock as long as it takes, yielding between tries.
pub(crate) fn patient_machine(dir: &Path, host: HostRef, live: &Arc<Liveness>) -> MachineStore {
    MachineStore::open(
        dir.to_path_buf(),
        context(host, live),
        LockWait::with_pause(u64::MAX, std::thread::yield_now),
    )
    .unwrap()
}

pub(crate) fn holder(project: &str, owner: &str, host: HostRef) -> Holder {
    Holder {
        project: project.to_string(),
        path: HolderPath {
            owner: OwnerKey(owner.to_string()),
            level: Level::Job,
        },
        label: HolderLabel(format!("job {owner}")),
        host,
        process: None,
    }
}

pub(crate) fn need(pool: &str, count: u32) -> Need {
    Need {
        pool: pool.to_string(),
        mode: None,
        count: Some(count),
    }
}

pub(crate) fn request(id: u64, holder: Holder, mixed: bool, needs: &[(&str, u32)]) -> Request {
    Request {
        id: RequestId(id),
        holder,
        mixed,
        needs: needs
            .iter()
            .map(|(pool, count)| need(pool, *count))
            .collect(),
    }
}

/// Adds an empty pool to `table`.
pub(crate) fn add_pool(table: &mut PoolTable, name: &str, kind: PoolKind, size: u32) {
    table
        .pools
        .insert(name.to_string(), PoolState::new(kind, size));
}

/// Queues `request` in `table`.
pub(crate) fn enqueue(table: &mut PoolTable, request: &Request) {
    table.enqueue(request, NOW).unwrap();
}

/// The holds of `pool`, by request id.
pub(crate) fn holds(table: &PoolTable, pool: &str) -> Vec<u64> {
    table.pools[pool]
        .holds
        .iter()
        .map(|hold| hold.request.0)
        .collect()
}

/// The waiters of `pool`, by request id.
pub(crate) fn waiters(table: &PoolTable, pool: &str) -> Vec<u64> {
    table.pools[pool]
        .waiters
        .iter()
        .map(|waiter| waiter.request.0)
        .collect()
}

/// A project location whose parts are all inside `root`, with an empty state directory.
pub(crate) fn location(root: &Path) -> Location {
    let folder = root.join("project");
    let state = folder.join("state");
    std::fs::create_dir_all(&state).unwrap();
    Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state,
        folder,
        remote: None,
        converted_from: None,
    }
}

/// The project's writer, with no state apply hook.
pub(crate) fn writer(location: &Location) -> Arc<StateWriter> {
    Arc::new(StateStore::open_writer_with_hooks(location, &StateApplyHooks::new()).unwrap())
}
