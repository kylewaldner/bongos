//! Property tests of the wait-for graph over generated tables, with a seeded SplitMix64 so every
//! run checks the same cases.

use crate::decl::PoolKind;
use crate::grant::{
    GuardReadings, Hold, PoolState, PoolTable, Request, RequestId, Timestamp, check_own_hold,
    grant, try_now,
};
use crate::holder::{Holder, HolderLabel, HolderPath, HostRef, Level, OwnerKey};
use crate::needs::{Need, NeedMode};
use crate::wait_for::{CycleLink, Refused, judge, would_cycle};

const HOST: HostRef = HostRef { pid: 1, start: 1 };
const SEEDS: u64 = 2000;

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn paths() -> Vec<Holder> {
    let mut holders = Vec::new();
    for (project, owner) in [("p", "r1"), ("p", "r2"), ("q", "r1")] {
        for level in [
            Level::Job,
            Level::Step { key: "a".into() },
            Level::Step { key: "b".into() },
            Level::Part {
                key: "a".into(),
                part: "1".into(),
            },
            Level::Part {
                key: "a".into(),
                part: "2".into(),
            },
        ] {
            holders.push(Holder {
                project: project.to_string(),
                label: HolderLabel(format!("{project}/{owner} {level:?}")),
                path: HolderPath {
                    owner: OwnerKey(owner.to_string()),
                    level,
                },
                host: HOST,
                process: None,
            });
        }
    }
    holders
}

fn generate_tables(rng: &mut Rng) -> Vec<PoolTable> {
    let mut tables = Vec::new();
    for table_index in 0..2 {
        let mut table = PoolTable::default();
        for index in 0..(1 + rng.below(2)) {
            let pool = match rng.below(3) {
                0 => PoolState::new(PoolKind::Mutex, 1),
                1 => PoolState::new(PoolKind::Rw, 0),
                _ => PoolState::new(PoolKind::Slots, 1 + rng.below(3) as u32),
            };
            table.pools.insert(format!("t{table_index}p{index}"), pool);
        }
        tables.push(table);
    }
    tables
}

fn generate_request(rng: &mut Rng, tables: &[PoolTable], holders: &[Holder], id: u64) -> Request {
    let names: Vec<(&String, &PoolState)> = tables.iter().flat_map(|t| &t.pools).collect();
    let mut needs: Vec<Need> = Vec::new();
    for _ in 0..(1 + rng.below(2)) {
        let (name, state) = names[rng.below(names.len() as u64) as usize];
        if needs.iter().any(|need| need.pool == *name) {
            continue;
        }
        let (mode, count) = match state.kind {
            PoolKind::Rw => (
                Some(if rng.below(2) == 0 {
                    NeedMode::Exclusive
                } else {
                    NeedMode::Shared
                }),
                None,
            ),
            PoolKind::Slots => (None, Some(1 + rng.below(u64::from(state.size)) as u32)),
            _ => (None, None),
        };
        needs.push(Need {
            pool: name.clone(),
            mode,
            count,
        });
    }
    Request {
        id: RequestId(id),
        holder: holders[rng.below(holders.len() as u64) as usize].clone(),
        mixed: false,
        needs,
    }
}

/// Queues the request's remaining needs, each in the table that has the pool.
fn enqueue(tables: &mut [PoolTable], request: &Request) {
    for table in tables.iter_mut() {
        let part = Request {
            needs: request
                .needs
                .iter()
                .filter(|need| table.pools.contains_key(&need.pool))
                .cloned()
                .collect(),
            ..request.clone()
        };
        if !part.needs.is_empty() {
            let _ = table.enqueue(&part, Timestamp(0));
        }
    }
}

/// Builds tables the way a broker would, with an occasional unchecked request so states the
/// checks would have refused appear too.
fn generate_state(rng: &mut Rng, holders: &[Holder]) -> Vec<PoolTable> {
    let mut tables = generate_tables(rng);
    for id in 0..(2 + rng.below(8)) {
        match rng.below(6) {
            0..=2 => {
                let request = generate_request(rng, &tables, holders, id);
                let views: Vec<&PoolTable> = tables.iter().collect();
                if let Ok(satisfied) = judge(&views, &request) {
                    enqueue(&mut tables, &satisfied.remaining(&request));
                }
            }
            3 => {
                let request = generate_request(rng, &tables, holders, id);
                enqueue(&mut tables, &request);
            }
            4 => {
                for table in tables.iter_mut() {
                    let grants = grant(table, &GuardReadings::default(), Timestamp(id), &HOST);
                    table.apply(&grants);
                }
            }
            _ => {
                for table in tables.iter_mut() {
                    for pool in table.pools.values_mut() {
                        if !pool.holds.is_empty() && rng.below(2) == 0 {
                            pool.holds.remove(0);
                        }
                    }
                }
            }
        }
    }
    tables
}

fn same(link: &CycleLink, holder: &Holder) -> bool {
    link.project == holder.project && link.path == holder.path
}

/// Whether `link` waits in `pool`: a waiter of its path, or the new request itself.
fn waits_in(tables: &[PoolTable], request: &Request, link: &CycleLink, pool: &str) -> bool {
    let new = same(link, &request.holder) && request.needs.iter().any(|need| need.pool == pool);
    new || tables
        .iter()
        .filter_map(|table| table.pools.get(pool))
        .flat_map(|state| &state.waiters)
        .any(|waiter| same(link, &waiter.holder))
}

fn holds_or_waits_in(tables: &[PoolTable], link: &CycleLink, pool: &str) -> bool {
    tables
        .iter()
        .filter_map(|table| table.pools.get(pool))
        .any(|state| {
            state
                .holds
                .iter()
                .any(|hold: &Hold| same(link, &hold.holder))
                || state
                    .waiters
                    .iter()
                    .any(|waiter| same(link, &waiter.holder))
        })
}

/// Whether this holder's path, or a path inside it, waits anywhere (the new request included).
/// A holder that does not can release while anybody waits.
fn busy(tables: &[PoolTable], request: &Request, holder: &Holder) -> bool {
    holder.is_or_encloses(&request.holder)
        || tables
            .iter()
            .flat_map(|table| table.pools.values())
            .flat_map(|state| &state.waiters)
            .any(|waiter| holder.is_or_encloses(&waiter.holder))
}

/// On a `slots` pool where `next` only holds, the link must be a real wait: the free slots and
/// those of every idle holder must not cover the largest need the link's path waits with there.
fn assert_slots_wait_is_real(
    seed: u64,
    tables: &[PoolTable],
    request: &Request,
    link: &CycleLink,
    next: &CycleLink,
    pool: &str,
) {
    let Some(state) = tables.iter().find_map(|table| table.pools.get(pool)) else {
        return;
    };
    if state.kind != PoolKind::Slots
        || state
            .waiters
            .iter()
            .any(|waiter| same(next, &waiter.holder))
    {
        return;
    }
    let new = same(link, &request.holder)
        .then(|| request.needs.iter().find(|need| need.pool == pool))
        .flatten();
    let need = state
        .waiters
        .iter()
        .filter(|waiter| same(link, &waiter.holder))
        .map(|waiter| &waiter.need)
        .chain(new)
        .map(|need| u64::from(need.count()))
        .max()
        .unwrap_or(0);
    let held: u64 = state
        .holds
        .iter()
        .map(|hold| u64::from(hold.need.count()))
        .sum();
    let idle: u64 = state
        .holds
        .iter()
        .filter(|hold| !busy(tables, request, &hold.holder))
        .map(|hold| u64::from(hold.need.count()))
        .sum();
    let free = u64::from(state.size).saturating_sub(held);
    assert!(
        free + idle < need,
        "seed {seed}: {link:?} waits in {pool} though {free} free and {idle} idle slots cover {need}"
    );
}

#[test]
fn would_cycle_refuses_only_real_cycles_and_never_a_free_request() {
    let holders = paths();
    let mut deadlocks = 0;
    for seed in 0..SEEDS {
        let mut rng = Rng(seed);
        let tables = generate_state(&mut rng, &holders);
        let views: Vec<&PoolTable> = tables.iter().collect();
        let request = generate_request(&mut rng, &tables, &holders, 1000);
        let result = would_cycle(&views, &request);
        assert_eq!(result, would_cycle(&views, &request), "seed {seed}");

        let free = tables.iter().all(|table| {
            let satisfied = check_own_hold(table, &request).unwrap_or_default();
            let part = Request {
                needs: satisfied
                    .remaining(&request)
                    .needs
                    .into_iter()
                    .filter(|need| table.pools.contains_key(&need.pool))
                    .collect(),
                ..request.clone()
            };
            try_now(table, &part, &GuardReadings::default()).is_ok()
        });
        if free {
            assert_eq!(result, Ok(()), "seed {seed}: grantable now");
        }
        let untouched = request.needs.iter().all(|need| {
            tables
                .iter()
                .filter_map(|table| table.pools.get(&need.pool))
                .all(|state| state.holds.is_empty() && state.waiters.is_empty())
        });
        if untouched {
            assert_eq!(result, Ok(()), "seed {seed}: no holders or waiters");
        }

        let Err(refused) = result else {
            continue;
        };
        let Refused::Deadlock { cycle } = refused else {
            panic!("seed {seed}: would_cycle refuses only deadlocks: {refused:?}");
        };
        deadlocks += 1;
        assert!(
            same(&cycle[0], &request.holder),
            "seed {seed}: starts at the requester"
        );
        for (index, link) in cycle.iter().enumerate() {
            let next = &cycle[(index + 1) % cycle.len()];
            match &link.waits_for {
                Some(pool) => {
                    assert!(
                        waits_in(&tables, &request, link, pool),
                        "seed {seed}: {link:?} waits in {pool}"
                    );
                    assert!(
                        holds_or_waits_in(&tables, next, pool),
                        "seed {seed}: {next:?} holds or waits in {pool}"
                    );
                    assert_slots_wait_is_real(seed, &tables, &request, link, next, pool);
                }
                None => assert!(
                    link.project == next.project && link.path.encloses(&next.path),
                    "seed {seed}: {link:?} encloses {next:?}"
                ),
            }
        }
        let refused_by_judge = judge(&views, &request);
        assert!(refused_by_judge.is_err(), "seed {seed}: judge refuses too");
    }
    assert!(deadlocks > 20, "the generator finds cycles: {deadlocks}");
}
