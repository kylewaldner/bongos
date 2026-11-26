//! A request that names both a machine pool and a project pool, judged only by its own host with
//! both stores held: the machine store's `flock` first, then the project's state transaction
//! inside it. Both halves are converted together or not at all; the project transaction commits,
//! then `machine.toml` is written and the lock released. When the project transaction fails,
//! `machine.toml` is never written.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use bongos_pools_model::{Grant, PoolTable, Pruned, RequestId};

use crate::machine_store::MachineStore;
use crate::project_store::ProjectStore;
use crate::store::{StoreContext, mixed_requests};

/// What a transaction over both stores did, per store.
#[derive(Debug, Clone, PartialEq)]
pub struct MixedTransacted<T> {
    pub value: T,
    pub machine_pruned: Pruned,
    pub project_pruned: Pruned,
    pub machine_grants: Vec<Grant>,
    pub project_grants: Vec<Grant>,
    pub machine_generation: u64,
    pub project_generation: u64,
}

/// One transaction over both stores of this host: each is pruned, then `f` changes both tables
/// (the machine table first), then both are granted together, so a request with `mixed` waiters
/// in both tables is converted in both or in neither. An error from `f` or from the project
/// transaction writes nothing in either store.
pub fn transact_mixed<T>(
    machine: &MachineStore,
    project: &ProjectStore,
    f: impl FnOnce(&mut PoolTable, &mut PoolTable) -> Result<T>,
) -> Result<MixedTransacted<T>> {
    let context = machine.context();
    if context.host != project.context().host {
        bail!("a mixed transaction needs both stores of one host");
    }
    let lock = machine.lock()?;
    let mut file = machine.read_file()?;
    let machine_pruned = context.prune(&mut file.table);
    let mut machine_grants = Vec::new();
    let (done, _) = project.writer().transact(|tx| {
        ProjectStore::transact_in(tx, |project_table| {
            let project_pruned = context.prune(project_table);
            let value = f(&mut file.table, project_table)?;
            let (machine, project) = joint_grant(context, &mut file.table, project_table);
            machine_grants = machine;
            Ok((value, project_pruned, project))
        })
    })?;
    project.note_generation(done.generation);
    file.generation += 1;
    machine.write_file(&lock, &file)?;
    drop(lock);
    machine.note_generation(file.generation);
    Ok(MixedTransacted {
        value: done.value,
        machine_pruned,
        project_pruned: done.pruned,
        machine_grants,
        project_grants: done.grants,
        machine_generation: file.generation,
        project_generation: done.generation,
    })
}

/// Grants both tables for this host. A request with `mixed` waiters in both tables that one table
/// would convert and the other would not is held back in both (its pools stop their scan at it),
/// and the tables are judged again, until both agree. A `mixed` request with waiters in only one
/// table has nothing to wait for in the other and is judged there alone.
fn joint_grant(
    context: &StoreContext,
    machine: &mut PoolTable,
    project: &mut PoolTable,
) -> (Vec<Grant>, Vec<Grant>) {
    let host = context.host;
    let in_machine = mixed_requests(machine, &host);
    let in_project = mixed_requests(project, &host);
    let joint: BTreeSet<RequestId> = in_machine.intersection(&in_project).copied().collect();
    let mut held: BTreeSet<RequestId> = BTreeSet::new();
    let now = context.now();
    loop {
        let is_held =
            |waiter: &bongos_pools_model::Waiter| waiter.mixed && held.contains(&waiter.request);
        let machine_grants = crate::store::grant_holding_back(machine, now, &host, is_held);
        let project_grants = crate::store::grant_holding_back(project, now, &host, is_held);
        let granted = |grants: &[Grant]| -> BTreeSet<RequestId> {
            grants.iter().map(|grant| grant.request).collect()
        };
        let (in_machine_grants, in_project_grants) =
            (granted(&machine_grants), granted(&project_grants));
        let split: Vec<RequestId> = joint
            .iter()
            .filter(|request| {
                !held.contains(request)
                    && in_machine_grants.contains(request) != in_project_grants.contains(request)
            })
            .copied()
            .collect();
        if split.is_empty() {
            machine.apply(&machine_grants);
            project.apply(&project_grants);
            return (machine_grants, project_grants);
        }
        held.extend(split);
    }
}
