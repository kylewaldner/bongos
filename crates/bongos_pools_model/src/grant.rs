//! The grant rule: which waiters of a table of pools become holds, decided by one pure function.
//!
//! Every request becomes one waiter per pool it names, each with a ticket from the table's single
//! counter. Per pool, waiters are considered strictly in ticket order and the scan stops at the
//! first waiter that cannot be granted, so nobody is granted ahead of an older waiter. A request
//! naming several pools is granted all or none: when it cannot be granted in one of its pools, the
//! scan of each of its other pools stops at it too.
//!
//! A host converts only its own waiters: [`grant`] takes the calling host and returns that host's
//! grants, while every other host's waiters still count against the size in ticket order. A
//! `mixed` waiter (the machine half of a request that also names a project pool) is judged only by
//! its own host; for every other host the scan of its pool stops at it.
//!
//! Before queuing, a need is judged against the holds of the requester's enclosing holders
//! ([`check_own_hold`]): covered needs are satisfied with no ticket, and a need such a hold makes
//! impossible is refused as [`Refused::OwnHold`]. The broker queues only the remaining needs
//! ([`Satisfied::remaining`]); [`PoolTable::enqueue`] itself is unchanged.
//!
//! Time and guard readings are inputs; nothing here reads a clock or the system.

use std::collections::{BTreeMap, BTreeSet};

use crate::decl::{GuardCheck, PoolKind};
use crate::holder::{Holder, HostRef};
use crate::needs::{Need, NeedMode};
use crate::wait_for::Refused;

/// Milliseconds since the Unix epoch, as given by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn plus_seconds(self, seconds: u32) -> Timestamp {
        Timestamp(self.0.saturating_add(u64::from(seconds) * 1000))
    }
}

/// The caller's id for one request; all its waiters and holds carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// What a request asks for: one need per pool, at most once per pool. A request naming no pool
/// needs no ticket and is admitted at once.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: RequestId,
    pub holder: Holder,
    /// Set on the machine rows of a request that also names a project pool.
    pub mixed: bool,
    pub needs: Vec<Need>,
}

/// One store's pools and its ticket counter.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PoolTable {
    pub pools: BTreeMap<String, PoolState>,
    pub next_ticket: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolState {
    pub kind: PoolKind,
    /// The number of slots of a `slots` pool; 1 for a mutex; unused by `rw` and `guard`.
    pub size: u32,
    pub guard: Option<GuardState>,
    pub holds: Vec<Hold>,
    pub waiters: Vec<Waiter>,
}

impl PoolState {
    /// An empty pool. A mutex always has size 1.
    pub fn new(kind: PoolKind, size: u32) -> PoolState {
        PoolState {
            kind,
            size: if kind == PoolKind::Mutex { 1 } else { size },
            guard: None,
            holds: Vec::new(),
            waiters: Vec::new(),
        }
    }

    pub fn with_guard(mut self, check: GuardCheck, settle_seconds: u32) -> PoolState {
        self.guard = Some(GuardState {
            check,
            settle_seconds,
            reading: None,
            spent: None,
        });
        self
    }
}

/// A pool's guard: its check, the time a reading must wait after the one that admitted the last
/// grant, the last reading recorded and the time of the reading that admitted the last grant.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardState {
    pub check: GuardCheck,
    pub settle_seconds: u32,
    pub reading: Option<GuardReading>,
    pub spent: Option<Timestamp>,
}

/// One reading of a guard and when it was taken.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardReading {
    pub value: GuardValue,
    pub at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GuardValue {
    FreeMemoryGb(f64),
    FreeDiskGb(f64),
    ScriptExit(i32),
}

impl GuardValue {
    /// Whether this value passes the check: free memory or disk at or above the figure, or a
    /// script that exited 0. A value of another kind than the check fails.
    pub fn passes(self, check: &GuardCheck) -> bool {
        match (self, check) {
            (GuardValue::FreeMemoryGb(value), GuardCheck::FreeMemoryGb(needed)) => value >= *needed,
            (GuardValue::FreeDiskGb(value), GuardCheck::FreeDiskGb { gb, .. }) => value >= *gb,
            (GuardValue::ScriptExit(code), GuardCheck::Script(_)) => code == 0,
            _ => false,
        }
    }
}

/// Fresh guard readings by pool name. A pool uses the newer of this reading and the one recorded
/// in its [`GuardState`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GuardReadings {
    pub readings: BTreeMap<String, GuardReading>,
}

impl GuardReadings {
    pub fn with(mut self, pool: &str, value: GuardValue, at: Timestamp) -> GuardReadings {
        self.readings
            .insert(pool.to_string(), GuardReading { value, at });
        self
    }
}

/// A granted need. Its ticket is its id, which a release names.
#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub ticket: u64,
    pub request: RequestId,
    pub holder: Holder,
    pub need: Need,
    pub since: Timestamp,
}

/// A need waiting for its turn in one pool.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiter {
    pub ticket: u64,
    pub request: RequestId,
    pub holder: Holder,
    pub mixed: bool,
    pub need: Need,
    pub since: Timestamp,
}

/// A request to convert from waiters to holds, in every pool it named.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub request: RequestId,
    pub host: HostRef,
    pub at: Timestamp,
    pub pools: Vec<GrantedPool>,
    /// Pools covered by an enclosing holder's hold: no ticket, no hold, no reading spent. Only
    /// [`admit_now`] fills it.
    pub satisfied: Vec<String>,
}

/// How [`try_now`] admits one need.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Admit {
    /// Granted, with the guard reading that admits it, if the pool has a guard.
    Granted(Option<GuardReading>),
    /// Covered by an enclosing holder's hold: no ticket, no hold, no reading.
    Satisfied,
}

/// The pools of a request covered by an enclosing holder's hold, in need order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Satisfied {
    pub pools: Vec<String>,
}

impl Satisfied {
    pub fn contains(&self, pool: &str) -> bool {
        self.pools.iter().any(|name| name == pool)
    }

    /// The request with only the needs still to queue.
    pub fn remaining(&self, request: &Request) -> Request {
        Request {
            needs: request
                .needs
                .iter()
                .filter(|need| !self.contains(&need.pool))
                .cloned()
                .collect(),
            ..request.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GrantedPool {
    pub pool: String,
    pub ticket: u64,
    /// The guard reading that admitted this grant, if the pool has a guard.
    pub reading: Option<GuardReading>,
}

/// Why a request is not granted (now) in a pool.
#[derive(Debug, Clone, PartialEq)]
pub enum Blocked {
    /// The table has no pool of this name.
    UnknownPool { pool: String },
    /// The holds (and the older waiters granted with it) leave no room.
    Busy { pool: String },
    /// `try_now` only: waiters are queued in the pool.
    Queued { pool: String, waiters: usize },
    /// An older waiter of this pool cannot be granted yet.
    Behind { pool: String, ticket: u64 },
    /// The guard's last reading fails.
    Guard {
        pool: String,
        reading: GuardValue,
        threshold: GuardCheck,
    },
    /// The guard has no reading yet.
    GuardUnread { pool: String },
    /// The last reading admitted a grant; the next one is taken at or after `next`.
    GuardSettling { pool: String, next: Timestamp },
    /// Another host's mixed waiter is ahead; only its own host judges it.
    Mixed { pool: String, ticket: u64 },
    /// Grantable here, but the request cannot be granted in `other`, and it is all or none.
    OtherPool { pool: String, other: String },
    /// `try_now` only: the request names this pool in more than one need.
    NamedTwice { pool: String },
    /// `try_now` only: an enclosing holder holds this pool incompatibly (see [`Refused::OwnHold`]).
    OwnHold { pool: String },
}

impl Blocked {
    pub fn pool(&self) -> &str {
        match self {
            Blocked::UnknownPool { pool }
            | Blocked::Busy { pool }
            | Blocked::Queued { pool, .. }
            | Blocked::Behind { pool, .. }
            | Blocked::Guard { pool, .. }
            | Blocked::GuardUnread { pool }
            | Blocked::GuardSettling { pool, .. }
            | Blocked::Mixed { pool, .. }
            | Blocked::OtherPool { pool, .. }
            | Blocked::NamedTwice { pool }
            | Blocked::OwnHold { pool } => pool,
        }
    }

    fn is_guard(&self) -> bool {
        matches!(
            self,
            Blocked::Guard { .. } | Blocked::GuardUnread { .. } | Blocked::GuardSettling { .. }
        )
    }
}

/// Where a request waits.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitStatus {
    /// The request has no waiter in the table: it was granted, never queued, or pruned.
    NotWaiting,
    /// One entry per pool it still waits in, in pool name order.
    Waiting(Vec<PoolWait>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolWait {
    pub pool: String,
    pub ticket: u64,
    /// The number of older waiters in this pool.
    pub ahead: usize,
    /// `None` when its host grants it at its next grant.
    pub blocked: Option<Blocked>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    UnknownPool(String),
    NamedTwice(String),
    AboveSize { pool: String, count: u32, size: u32 },
}

impl PoolTable {
    /// Issues the next ticket. Every ticket of the table, in every pool, comes from this counter.
    pub fn issue_ticket(&mut self) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        ticket
    }

    /// Queues one waiter per pool the request names, each with its own ticket, and returns the
    /// tickets in the order of the needs. Nothing is queued when a need is refused.
    pub fn enqueue(
        &mut self,
        request: &Request,
        now: Timestamp,
    ) -> Result<Vec<(String, u64)>, QueueError> {
        let mut named = BTreeSet::new();
        for need in &request.needs {
            let Some(pool) = self.pools.get(&need.pool) else {
                return Err(QueueError::UnknownPool(need.pool.clone()));
            };
            if !named.insert(need.pool.as_str()) {
                return Err(QueueError::NamedTwice(need.pool.clone()));
            }
            let capacity = match pool.kind {
                PoolKind::Slots => Some(pool.size),
                PoolKind::Mutex => Some(1),
                PoolKind::Rw | PoolKind::Guard => None,
            };
            if let Some(size) = capacity {
                if need.count() > size {
                    return Err(QueueError::AboveSize {
                        pool: need.pool.clone(),
                        count: need.count(),
                        size,
                    });
                }
            }
        }
        let mut tickets = Vec::new();
        for need in &request.needs {
            let ticket = self.issue_ticket();
            let waiter = Waiter {
                ticket,
                request: request.id,
                holder: request.holder.clone(),
                mixed: request.mixed,
                need: need.clone(),
                since: now,
            };
            if let Some(pool) = self.pools.get_mut(&need.pool) {
                pool.waiters.push(waiter);
            }
            tickets.push((need.pool.clone(), ticket));
        }
        Ok(tickets)
    }

    /// Converts the grants' waiters to holds. A `guard` pool records no hold; its guard records the
    /// reading that admitted the grant, so the next grant waits for a later reading.
    pub fn apply(&mut self, grants: &[Grant]) {
        for grant in grants {
            for granted in &grant.pools {
                let Some(pool) = self.pools.get_mut(&granted.pool) else {
                    continue;
                };
                let Some(index) = pool
                    .waiters
                    .iter()
                    .position(|waiter| waiter.ticket == granted.ticket)
                else {
                    continue;
                };
                let waiter = pool.waiters.remove(index);
                if let (Some(guard), Some(reading)) = (pool.guard.as_mut(), granted.reading) {
                    guard.reading = Some(reading);
                    guard.spent = Some(reading.at);
                }
                if pool.kind != PoolKind::Guard {
                    pool.holds.push(Hold {
                        ticket: waiter.ticket,
                        request: waiter.request,
                        holder: waiter.holder,
                        need: waiter.need,
                        since: grant.at,
                    });
                }
            }
        }
    }

    /// Removes the hold with this ticket and returns it; `None` when it was already released.
    pub fn release(&mut self, pool: &str, ticket: u64) -> Option<Hold> {
        let pool = self.pools.get_mut(pool)?;
        let index = pool.holds.iter().position(|hold| hold.ticket == ticket)?;
        Some(pool.holds.remove(index))
    }

    /// Records fresh readings as each pool's last reading, where they are newer.
    pub fn record_readings(&mut self, guards: &GuardReadings) {
        for (name, reading) in &guards.readings {
            let Some(guard) = self
                .pools
                .get_mut(name)
                .and_then(|pool| pool.guard.as_mut())
            else {
                continue;
            };
            if guard.reading.map_or(true, |last| last.at < reading.at) {
                guard.reading = Some(*reading);
            }
        }
    }
}

/// The requests of `host` to convert now. Requests of other hosts are judged in ticket order and
/// count against the size, but are left for their own host to convert.
///
/// A reading recorded in the table has no age limit: the broker must pass fresh readings in
/// `guards`, or a stale passing reading keeps admitting once per `settle_seconds`.
pub fn grant(
    table: &PoolTable,
    guards: &GuardReadings,
    now: Timestamp,
    host: &HostRef,
) -> Vec<Grant> {
    let scan = scan(table, guards, host);
    let mut grants: BTreeMap<RequestId, Grant> = BTreeMap::new();
    for (name, pool) in &table.pools {
        for waiter in &pool.waiters {
            if waiter.holder.host != *host || !scan.granted.contains(&waiter.request) {
                continue;
            }
            grants
                .entry(waiter.request)
                .or_insert_with(|| Grant {
                    request: waiter.request,
                    host: waiter.holder.host,
                    at: now,
                    pools: Vec::new(),
                    satisfied: Vec::new(),
                })
                .pools
                .push(GrantedPool {
                    pool: name.clone(),
                    ticket: waiter.ticket,
                    reading: scan.readings.get(name).copied(),
                });
        }
    }
    let mut grants: Vec<Grant> = grants.into_values().collect();
    grants.sort_by_key(|grant| grant.pools.iter().map(|p| p.ticket).min());
    grants
}

/// Whether the request could be admitted at once without queuing. Each pool is first judged
/// against the enclosing holders' holds: a covered pool is [`Admit::Satisfied`] whatever waits
/// there, and an incompatible one is `Blocked::OwnHold`. Every other pool must have no waiter at
/// all and the grant rule must admit it now. Nothing is queued either way.
///
/// On success it returns, per need in order, the pool name and how it is admitted. It does not
/// spend a guard reading: use [`admit_now`] to admit and spend it, or a second caller on the same
/// reading would be admitted too.
pub fn try_now(
    table: &PoolTable,
    request: &Request,
    guards: &GuardReadings,
) -> Result<Vec<(String, Admit)>, Blocked> {
    let mut admitted = Vec::new();
    let mut named = BTreeSet::new();
    for need in &request.needs {
        if !named.insert(need.pool.as_str()) {
            return Err(Blocked::NamedTwice {
                pool: need.pool.clone(),
            });
        }
        let Some(pool) = table.pools.get(&need.pool) else {
            return Err(Blocked::UnknownPool {
                pool: need.pool.clone(),
            });
        };
        match cover(pool, need, &request.holder) {
            Cover::Satisfied => {
                admitted.push((need.pool.clone(), Admit::Satisfied));
                continue;
            }
            Cover::OwnHold => {
                return Err(Blocked::OwnHold {
                    pool: need.pool.clone(),
                });
            }
            Cover::Open => {}
        }
        if !pool.waiters.is_empty() {
            return Err(Blocked::Queued {
                pool: need.pool.clone(),
                waiters: pool.waiters.len(),
            });
        }
        let reading = match gate(&need.pool, pool, guards) {
            Gate::Closed(blocked) => return Err(blocked),
            Gate::Open(reading) => reading,
        };
        if !Usage::of(pool).fits(pool, need) {
            return Err(Blocked::Busy {
                pool: need.pool.clone(),
            });
        }
        admitted.push((need.pool.clone(), Admit::Granted(reading)));
    }
    Ok(admitted)
}

/// Admits the request at once when [`try_now`] would, taking its holds (none in a `guard` pool)
/// and spending each guard's reading, so a second admission on the same reading is refused as
/// settling. A satisfied pool takes no ticket, hold or reading and is listed in
/// [`Grant::satisfied`]. Nothing changes when the request is blocked.
pub fn admit_now(
    table: &mut PoolTable,
    request: &Request,
    guards: &GuardReadings,
    now: Timestamp,
) -> Result<Grant, Blocked> {
    let admitted = try_now(table, request, guards)?;
    let mut pools = Vec::new();
    let mut satisfied = Vec::new();
    for (need, (name, admit)) in request.needs.iter().zip(admitted) {
        let reading = match admit {
            Admit::Satisfied => {
                satisfied.push(name);
                continue;
            }
            Admit::Granted(reading) => reading,
        };
        let ticket = table.issue_ticket();
        if let Some(pool) = table.pools.get_mut(&name) {
            if let (Some(guard), Some(reading)) = (pool.guard.as_mut(), reading) {
                guard.reading = Some(reading);
                guard.spent = Some(reading.at);
            }
            if pool.kind != PoolKind::Guard {
                pool.holds.push(Hold {
                    ticket,
                    request: request.id,
                    holder: request.holder.clone(),
                    need: need.clone(),
                    since: now,
                });
            }
        }
        pools.push(GrantedPool {
            pool: name,
            ticket,
            reading,
        });
    }
    Ok(Grant {
        request: request.id,
        host: request.holder.host,
        at: now,
        pools,
        satisfied,
    })
}

/// Judges the request's needs in this table against the holds of its enclosing holders (paths
/// that strictly enclose the requester's, in the same project). A need is satisfied when those
/// holds cover it: a held `exclusive` covers any mode, a held `shared` covers `shared`, and held
/// slots (summed) cover an equal or smaller count. It is refused as [`Refused::OwnHold`] when the
/// need cannot fit beside those holds (`exclusive` over a held `shared`, more slots than the pool
/// has beside them), since they are not released while the requester waits. The first refused
/// need in need order is reported.
///
/// Pools this table does not have are left alone (another table, or `enqueue` refuses them), as
/// is a pool the request names twice, so `enqueue` still refuses it. A `guard` pool records no
/// hold, so it is never satisfied or refused here.
pub fn check_own_hold(table: &PoolTable, request: &Request) -> Result<Satisfied, Refused> {
    own_holds(|name| table.pools.get(name), request)
}

pub(crate) fn own_holds<'a>(
    lookup: impl Fn(&str) -> Option<&'a PoolState>,
    request: &Request,
) -> Result<Satisfied, Refused> {
    let mut satisfied = Satisfied::default();
    for need in &request.needs {
        let named = request
            .needs
            .iter()
            .filter(|other| other.pool == need.pool)
            .count();
        let Some(pool) = lookup(&need.pool).filter(|_| named == 1) else {
            continue;
        };
        match cover(pool, need, &request.holder) {
            Cover::Satisfied => satisfied.pools.push(need.pool.clone()),
            Cover::OwnHold => {
                return Err(Refused::OwnHold {
                    pool: need.pool.clone(),
                });
            }
            Cover::Open => {}
        }
    }
    Ok(satisfied)
}

/// How the requester's enclosing holders' holds in one pool stand toward one need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cover {
    /// No enclosing hold covers it or rules it out: it queues as usual.
    Open,
    Satisfied,
    OwnHold,
}

pub(crate) fn cover(pool: &PoolState, need: &Need, requester: &Holder) -> Cover {
    let enclosing: Vec<&Hold> = pool
        .holds
        .iter()
        .filter(|hold| hold.holder.encloses(requester))
        .collect();
    if enclosing.is_empty() || pool.kind == PoolKind::Guard {
        return Cover::Open;
    }
    let usage = Usage::of_holds(pool, enclosing);
    let covered = match pool.kind {
        PoolKind::Rw => usage.exclusive || mode(need) == NeedMode::Shared,
        PoolKind::Slots | PoolKind::Mutex => usage.count >= u64::from(need.count()),
        PoolKind::Guard => false,
    };
    if covered {
        Cover::Satisfied
    } else if !usage.fits(pool, need) {
        Cover::OwnHold
    } else {
        Cover::Open
    }
}

/// The holds a waiter with this need waits for, by the grant rule's own fit test: none when it
/// fits beside every hold. Otherwise, when it would fit beside the holds of its own path and its
/// enclosing paths (which are not released while it waits), only the other holds that conflict
/// with it; when it would not, every conflicting hold, its own included. A hold conflicts unless
/// the need is `shared` on an `rw` pool and the hold is `shared` too. On a `slots` pool every
/// other hold counts, since any of them may be the one that must release.
pub(crate) fn blocking_holds<'a>(
    pool: &'a PoolState,
    need: &Need,
    requester: &Holder,
) -> Vec<&'a Hold> {
    if Usage::of(pool).fits(pool, need) {
        return Vec::new();
    }
    let own = |hold: &Hold| hold.holder.is_or_encloses(requester);
    let own_fits =
        Usage::of_holds(pool, pool.holds.iter().filter(|hold| own(hold))).fits(pool, need);
    pool.holds
        .iter()
        .filter(|hold| !own_fits || !own(hold))
        .filter(|hold| {
            !(pool.kind == PoolKind::Rw
                && mode(need) == NeedMode::Shared
                && mode(&hold.need) == NeedMode::Shared)
        })
        .collect()
}

/// Where the request waits and why, judged as its own host would judge it, with the readings
/// recorded in the table.
pub fn wait_status(table: &PoolTable, request: RequestId) -> WaitStatus {
    let Some(host) = table
        .pools
        .values()
        .flat_map(|pool| &pool.waiters)
        .find(|waiter| waiter.request == request)
        .map(|waiter| waiter.holder.host)
    else {
        return WaitStatus::NotWaiting;
    };
    let scan = scan(table, &GuardReadings::default(), &host);
    let mut waits = Vec::new();
    for (name, pool) in &table.pools {
        for waiter in pool.waiters.iter().filter(|w| w.request == request) {
            waits.push(PoolWait {
                pool: name.clone(),
                ticket: waiter.ticket,
                ahead: pool
                    .waiters
                    .iter()
                    .filter(|other| other.ticket < waiter.ticket)
                    .count(),
                blocked: scan.blocked.get(&(name.clone(), waiter.ticket)).cloned(),
            });
        }
    }
    WaitStatus::Waiting(waits)
}

struct Scan {
    granted: BTreeSet<RequestId>,
    blocked: BTreeMap<(String, u64), Blocked>,
    readings: BTreeMap<String, GuardReading>,
}

enum Gate {
    Open(Option<GuardReading>),
    Closed(Blocked),
}

fn reading(name: &str, pool: &PoolState, guards: &GuardReadings) -> Option<GuardReading> {
    let recorded = pool.guard.as_ref().and_then(|guard| guard.reading);
    match (recorded, guards.readings.get(name).copied()) {
        (Some(recorded), Some(fresh)) if recorded.at > fresh.at => Some(recorded),
        (_, Some(fresh)) => Some(fresh),
        (recorded, None) => recorded,
    }
}

fn gate(name: &str, pool: &PoolState, guards: &GuardReadings) -> Gate {
    let Some(guard) = &pool.guard else {
        return Gate::Open(None);
    };
    let Some(reading) = reading(name, pool, guards) else {
        return Gate::Closed(Blocked::GuardUnread {
            pool: name.to_string(),
        });
    };
    if !reading.value.passes(&guard.check) {
        return Gate::Closed(Blocked::Guard {
            pool: name.to_string(),
            reading: reading.value,
            threshold: guard.check.clone(),
        });
    }
    if let Some(spent) = guard.spent {
        let next = next_reading(spent, guard.settle_seconds);
        if reading.at < next {
            return Gate::Closed(Blocked::GuardSettling {
                pool: name.to_string(),
                next,
            });
        }
    }
    Gate::Open(Some(reading))
}

/// The earliest time of a reading that may admit a grant after the reading taken at `spent`
/// admitted one: `settle_seconds` later, and always strictly later.
fn next_reading(spent: Timestamp, settle_seconds: u32) -> Timestamp {
    Timestamp(
        spent
            .plus_seconds(settle_seconds)
            .0
            .max(spent.0.saturating_add(1)),
    )
}

/// What the holds of a pool (and the waiters accepted so far in a scan) take.
struct Usage {
    count: u64,
    holds: usize,
    exclusive: bool,
}

fn mode(need: &Need) -> NeedMode {
    need.mode.unwrap_or(NeedMode::Shared)
}

impl Usage {
    fn of(pool: &PoolState) -> Usage {
        Usage::of_holds(pool, &pool.holds)
    }

    fn of_holds<'a>(pool: &PoolState, holds: impl IntoIterator<Item = &'a Hold>) -> Usage {
        let mut usage = Usage {
            count: 0,
            holds: 0,
            exclusive: false,
        };
        for hold in holds {
            usage.add(pool, &hold.need);
        }
        usage
    }

    fn fits(&self, pool: &PoolState, need: &Need) -> bool {
        match pool.kind {
            PoolKind::Slots => self.count + u64::from(need.count()) <= u64::from(pool.size),
            PoolKind::Mutex => self.count + u64::from(need.count()) <= 1,
            PoolKind::Rw => match mode(need) {
                NeedMode::Shared => !self.exclusive,
                NeedMode::Exclusive => self.holds == 0,
            },
            PoolKind::Guard => true,
        }
    }

    fn add(&mut self, pool: &PoolState, need: &Need) {
        self.count += u64::from(need.count());
        self.holds += 1;
        if pool.kind == PoolKind::Rw && mode(need) == NeedMode::Exclusive {
            self.exclusive = true;
        }
    }
}

struct PoolScan {
    /// Tickets accepted in this pool, by request.
    accepted: Vec<RequestId>,
    blocked: Vec<(u64, Blocked)>,
}

fn scan_pool(
    name: &str,
    pool: &PoolState,
    guards: &GuardReadings,
    viewer: &HostRef,
    refused: &BTreeMap<RequestId, String>,
) -> PoolScan {
    let mut order: Vec<&Waiter> = pool.waiters.iter().collect();
    order.sort_by_key(|waiter| waiter.ticket);
    let gate = gate(name, pool, guards);
    let mut usage = Usage::of(pool);
    let mut accepted = Vec::new();
    let mut blocked = Vec::new();
    let mut stop: Option<(u64, Blocked)> = None;
    for waiter in order {
        if let Some((first, reason)) = &stop {
            // Behind a failing or spent guard every waiter shows the guard; behind anything else
            // it waits for the first blocked waiter.
            let reason = if reason.is_guard() {
                reason.clone()
            } else {
                Blocked::Behind {
                    pool: name.to_string(),
                    ticket: *first,
                }
            };
            blocked.push((waiter.ticket, reason));
            continue;
        }
        let reason = match &gate {
            Gate::Closed(reason) => Some(reason.clone()),
            Gate::Open(Some(reading)) if !accepted.is_empty() => Some(Blocked::GuardSettling {
                pool: name.to_string(),
                next: next_reading(
                    reading.at,
                    pool.guard.as_ref().map_or(0, |guard| guard.settle_seconds),
                ),
            }),
            Gate::Open(_) => None,
        }
        .or_else(|| {
            (waiter.mixed && waiter.holder.host != *viewer).then(|| Blocked::Mixed {
                pool: name.to_string(),
                ticket: waiter.ticket,
            })
        })
        .or_else(|| {
            // In the pool that blocks it, the request is judged as before and stays blocked
            // there, because later rounds only stop each scan earlier.
            refused
                .get(&waiter.request)
                .filter(|other| other.as_str() != name)
                .map(|other| Blocked::OtherPool {
                    pool: name.to_string(),
                    other: other.clone(),
                })
        })
        .or_else(|| {
            (!usage.fits(pool, &waiter.need)).then(|| Blocked::Busy {
                pool: name.to_string(),
            })
        });
        match reason {
            Some(reason) => {
                blocked.push((waiter.ticket, reason.clone()));
                stop = Some((waiter.ticket, reason));
            }
            None => {
                usage.add(pool, &waiter.need);
                accepted.push(waiter.request);
            }
        }
    }
    PoolScan { accepted, blocked }
}

fn scan(table: &PoolTable, guards: &GuardReadings, viewer: &HostRef) -> Scan {
    let mut pools_of: BTreeMap<RequestId, usize> = BTreeMap::new();
    for pool in table.pools.values() {
        for waiter in &pool.waiters {
            *pools_of.entry(waiter.request).or_default() += 1;
        }
    }
    let mut refused: BTreeMap<RequestId, String> = BTreeMap::new();
    loop {
        let scans: BTreeMap<&String, PoolScan> = table
            .pools
            .iter()
            .map(|(name, pool)| (name, scan_pool(name, pool, guards, viewer, &refused)))
            .collect();
        let mut accepted_in: BTreeMap<RequestId, usize> = BTreeMap::new();
        for pool_scan in scans.values() {
            for request in &pool_scan.accepted {
                *accepted_in.entry(*request).or_default() += 1;
            }
        }
        let mut changed = false;
        for (request, accepted) in &accepted_in {
            // The set of refused requests only grows, so the loop ends.
            if *accepted == pools_of[request] || refused.contains_key(request) {
                continue;
            }
            // Accepted in some pools but not all: find a pool where it is blocked.
            let other = table
                .pools
                .iter()
                .find(|(name, pool)| {
                    pool.waiters.iter().any(|waiter| {
                        waiter.request == *request
                            && scans[name]
                                .blocked
                                .iter()
                                .any(|(ticket, _)| *ticket == waiter.ticket)
                    })
                })
                .map(|(name, _)| name.clone())
                .unwrap_or_default();
            refused.insert(*request, other);
            changed = true;
        }
        if changed {
            continue;
        }
        let mut blocked = BTreeMap::new();
        let mut readings = BTreeMap::new();
        for (name, pool_scan) in scans {
            for (ticket, reason) in pool_scan.blocked {
                blocked.insert((name.clone(), ticket), reason);
            }
            if !pool_scan.accepted.is_empty() {
                if let Gate::Open(Some(reading)) = gate(name, &table.pools[name], guards) {
                    readings.insert(name.clone(), reading);
                }
            }
        }
        return Scan {
            granted: accepted_in
                .into_iter()
                .filter(|(request, accepted)| *accepted == pools_of[request])
                .map(|(request, _)| request)
                .collect(),
            blocked,
            readings,
        };
    }
}
