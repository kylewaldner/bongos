//! The two pool stores and what they share. A store keeps one [`PoolTable`]; every transaction
//! prunes the holds and waiters of dead processes, applies the caller's change, runs `grant` for
//! this process's own waiters, applies the grants and bumps the store's generation, all at once:
//! no other transaction of the same store falls between those steps.
//!
//! - [`ProjectStore`](crate::ProjectStore): rows in the project's state store (`kv` namespace
//!   `pools`), written only by the project's writer process.
//! - [`MachineStore`](crate::MachineStore): `machine.toml` in a directory shared by every bongos
//!   process of the user, serialized by an `flock` on `machine.lock`.
//!
//! A request that names a machine pool and a project pool is queued in both stores with `mixed`
//! set on every one of its waiters. Plain transactions of either store never convert such a
//! waiter (its pools stop their scan at it); only [`transact_mixed`](crate::transact_mixed),
//! which holds both stores at once, converts both halves together.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use bongos_pools_model::{
    Grant, GuardReadings, HostRef, PoolTable, Probe, Pruned, RequestId, Timestamp, Waiter, grant,
    prune,
};
use bongos_workflow::process::{self, ProcessRecord};

/// What one transaction did besides the caller's change.
#[derive(Debug, Clone, PartialEq)]
pub struct Transacted<T> {
    /// What the caller's change returned.
    pub value: T,
    /// The holds and waiters of dead processes removed before the change, for the log.
    pub pruned: Pruned,
    /// The requests of this process converted from waiters to holds after the change.
    pub grants: Vec<Grant>,
    /// The store's generation after the transaction.
    pub generation: u64,
}

/// One store of pools. Every transaction prunes, applies `f`, grants and bumps the generation in
/// one step; an error from `f` writes nothing.
pub trait PoolStore {
    /// Runs one transaction and reports what it pruned and granted.
    fn try_transact_full<T>(
        &self,
        f: impl FnOnce(&mut PoolTable) -> Result<T>,
    ) -> Result<Transacted<T>>;

    /// Runs one transaction whose change cannot fail.
    fn transact<T>(&self, f: impl FnOnce(&mut PoolTable) -> T) -> Result<T> {
        self.try_transact_full(|table| Ok(f(table)))
            .map(|done| done.value)
    }

    /// The table as last written, without taking part in any transaction.
    fn read(&self) -> Result<PoolTable>;

    /// The generation this handle last wrote or read.
    fn generation(&self) -> u64;
}

/// Who this process is to the stores, how it tells a live process from a dead one, and its
/// clock.
#[derive(Clone)]
pub struct StoreContext {
    pub host: HostRef,
    probe: Arc<dyn Probe + Send + Sync>,
    clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
}

impl StoreContext {
    pub fn new(
        host: HostRef,
        probe: impl Probe + Send + Sync + 'static,
        clock: impl Fn() -> Timestamp + Send + Sync + 'static,
    ) -> StoreContext {
        StoreContext {
            host,
            probe: Arc::new(probe),
            clock: Arc::new(clock),
        }
    }

    /// This process as host, [`ProcessProbe`] and the system clock. `None` when this process's
    /// start time cannot be read.
    pub fn this_process() -> Option<StoreContext> {
        Some(StoreContext::new(this_host()?, ProcessProbe, system_now))
    }

    pub fn now(&self) -> Timestamp {
        (self.clock)()
    }

    pub fn alive(&self, pid: u32, start: u64) -> bool {
        self.probe.alive(pid, start)
    }

    /// Prunes the dead from `table`, applies `f`, then converts this host's grantable waiters,
    /// leaving every waiter that `held` names in place and stopping its pool's scan at it.
    pub(crate) fn step<T>(
        &self,
        table: &mut PoolTable,
        held: impl Fn(&Waiter) -> bool,
        f: impl FnOnce(&mut PoolTable) -> Result<T>,
    ) -> Result<(T, Pruned, Vec<Grant>)> {
        let pruned = self.prune(table);
        let value = f(table)?;
        let grants = self.grant(table, held);
        Ok((value, pruned, grants))
    }

    /// Removes the holds and waiters of dead processes from `table` and returns them.
    pub(crate) fn prune(&self, table: &mut PoolTable) -> Pruned {
        prune(table, &|pid, start| self.alive(pid, start))
    }

    /// Converts this host's grantable waiters except those `held` names, and returns the grants.
    pub(crate) fn grant(
        &self,
        table: &mut PoolTable,
        held: impl Fn(&Waiter) -> bool,
    ) -> Vec<Grant> {
        let grants = grant_holding_back(table, self.now(), &self.host, held);
        table.apply(&grants);
        grants
    }
}

/// The probe of the real system: a process lives while [`process::record_alive`] says so (on
/// Linux a zombie is dead; a pid reused by a later process never matches).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessProbe;

impl Probe for ProcessProbe {
    fn alive(&self, pid: u32, start: u64) -> bool {
        process::record_alive(ProcessRecord {
            pid,
            start_time: start,
        })
    }
}

/// This process as a host: its pid and its start time as [`process::start_time`] reads it.
pub fn this_host() -> Option<HostRef> {
    let pid = std::process::id();
    Some(HostRef {
        pid,
        start: process::start_time(pid)?,
    })
}

/// Milliseconds since the epoch by the system clock.
pub fn system_now() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    Timestamp(u64::try_from(millis).unwrap_or(u64::MAX))
}

/// The host no process is: a waiter moved to it is judged as another host's `mixed` waiter, so
/// its pool's scan stops at it and it is never converted.
const HELD_BACK: HostRef = HostRef { pid: 0, start: 0 };

/// `grant` for `host`, except that `host`'s waiters for which `held` is true are not converted
/// and block their pools as a `mixed` waiter of another host would. Guards are judged on the
/// readings recorded in the table: the caller records fresh ones in its change.
pub(crate) fn grant_holding_back(
    table: &PoolTable,
    now: Timestamp,
    host: &HostRef,
    held: impl Fn(&Waiter) -> bool,
) -> Vec<Grant> {
    let is_held = |waiter: &Waiter| waiter.holder.host == *host && held(waiter);
    let any_held = table
        .pools
        .values()
        .flat_map(|pool| &pool.waiters)
        .any(is_held);
    if !any_held {
        return grant(table, &GuardReadings::default(), now, host);
    }
    let mut judged = table.clone();
    for pool in judged.pools.values_mut() {
        for waiter in &mut pool.waiters {
            if is_held(waiter) {
                waiter.holder.host = HELD_BACK;
                waiter.mixed = true;
            }
        }
    }
    grant(&judged, &GuardReadings::default(), now, host)
}

/// The requests of `host` with a `mixed` waiter in `table`.
pub(crate) fn mixed_requests(table: &PoolTable, host: &HostRef) -> BTreeSet<RequestId> {
    table
        .pools
        .values()
        .flat_map(|pool| &pool.waiters)
        .filter(|waiter| waiter.mixed && waiter.holder.host == *host)
        .map(|waiter| waiter.request)
        .collect()
}

/// A plain transaction converts no `mixed` waiter.
pub(crate) fn is_mixed(waiter: &Waiter) -> bool {
    waiter.mixed
}
