//! Property tests of the grant rule over generated tables and request sequences, with a small
//! seeded generator so every run checks the same cases.

use std::collections::BTreeSet;

use crate::decl::{GuardCheck, PoolKind};
use crate::grant::{
    GuardReadings, GuardValue, PoolState, PoolTable, Request, RequestId, Timestamp, grant,
};
use crate::holder::{Holder, HolderLabel, HolderPath, HostRef, Level, OwnerKey};
use crate::needs::{Need, NeedMode};

const HOST: HostRef = HostRef { pid: 1, start: 1 };
const SEEDS: u64 = 300;

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

fn generate_table(rng: &mut Rng) -> PoolTable {
    let mut table = PoolTable::default();
    let count = 2 + rng.below(3);
    for index in 0..count {
        let pool = match rng.below(5) {
            0 => PoolState::new(PoolKind::Mutex, 1),
            1 => PoolState::new(PoolKind::Rw, 0),
            2 => PoolState::new(PoolKind::Guard, 0).with_guard(GuardCheck::FreeMemoryGb(4.0), 2),
            3 => PoolState::new(PoolKind::Slots, 1 + rng.below(3) as u32).with_guard(
                GuardCheck::FreeDiskGb {
                    gb: 1.0,
                    path: None,
                },
                0,
            ),
            _ => PoolState::new(PoolKind::Slots, 1 + rng.below(3) as u32),
        };
        table.pools.insert(format!("p{index}"), pool);
    }
    table
}

fn generate_request(rng: &mut Rng, table: &PoolTable, id: u64) -> Request {
    let names: Vec<&String> = table.pools.keys().collect();
    let wanted = 1 + rng.below(2);
    let mut chosen = BTreeSet::new();
    for _ in 0..wanted {
        chosen.insert(names[rng.below(names.len() as u64) as usize].clone());
    }
    let needs = chosen
        .into_iter()
        .map(|pool| {
            let state = &table.pools[&pool];
            let (mode, count) = match state.kind {
                PoolKind::Rw => (
                    Some(if rng.below(3) == 0 {
                        NeedMode::Exclusive
                    } else {
                        NeedMode::Shared
                    }),
                    None,
                ),
                PoolKind::Slots => (None, Some(1 + rng.below(u64::from(state.size)) as u32)),
                PoolKind::Mutex | PoolKind::Guard => (None, None),
            };
            Need { pool, mode, count }
        })
        .collect();
    Request {
        id: RequestId(id),
        holder: Holder {
            project: "p".to_string(),
            path: HolderPath {
                owner: OwnerKey(format!("run-{id}")),
                level: Level::Run,
            },
            label: HolderLabel::default(),
            host: HOST,
            process: None,
        },
        mixed: false,
        needs,
    }
}

/// Passing readings for every guarded pool, taken at `now`.
fn readings(table: &PoolTable, now: Timestamp) -> GuardReadings {
    let mut guards = GuardReadings::default();
    for (name, pool) in &table.pools {
        if let Some(guard) = &pool.guard {
            let value = match guard.check {
                GuardCheck::FreeMemoryGb(_) => GuardValue::FreeMemoryGb(100.0),
                GuardCheck::FreeDiskGb { .. } => GuardValue::FreeDiskGb(100.0),
                GuardCheck::Script(_) => GuardValue::ScriptExit(0),
            };
            guards = guards.with(name, value, now);
        }
    }
    guards
}

fn assert_size_kept(table: &PoolTable, seed: u64) {
    for (name, pool) in &table.pools {
        let count: u32 = pool.holds.iter().map(|hold| hold.need.count()).sum();
        match pool.kind {
            PoolKind::Slots => assert!(count <= pool.size, "seed {seed}: {name} over size"),
            PoolKind::Mutex => assert!(count <= 1, "seed {seed}: mutex {name} held twice"),
            PoolKind::Rw => {
                let exclusive = pool
                    .holds
                    .iter()
                    .any(|hold| hold.need.mode == Some(NeedMode::Exclusive));
                assert!(
                    !exclusive || pool.holds.len() == 1,
                    "seed {seed}: {name} exclusive beside another hold"
                );
            }
            PoolKind::Guard => assert!(pool.holds.is_empty(), "seed {seed}: guard {name} held"),
        }
    }
}

/// Grants and applies; asserts that no grant passes an older waiter left waiting in its pool.
fn grant_step(table: &mut PoolTable, now: Timestamp, seed: u64) -> Vec<RequestId> {
    let grants = grant(table, &readings(table, now), now, &HOST);
    let granted: BTreeSet<(String, u64)> = grants
        .iter()
        .flat_map(|grant| grant.pools.iter().map(|p| (p.pool.clone(), p.ticket)))
        .collect();
    for (pool, ticket) in &granted {
        for waiter in &table.pools[pool].waiters {
            assert!(
                waiter.ticket > *ticket || granted.contains(&(pool.clone(), waiter.ticket)),
                "seed {seed}: ticket {ticket} granted in {pool} ahead of waiting ticket {}",
                waiter.ticket
            );
        }
    }
    let all_or_none: usize = table
        .pools
        .values()
        .flat_map(|pool| &pool.waiters)
        .filter(|waiter| grants.iter().any(|grant| grant.request == waiter.request))
        .count();
    assert_eq!(
        all_or_none,
        granted.len(),
        "seed {seed}: a request granted in part"
    );
    table.apply(&grants);
    assert_size_kept(table, seed);
    grants.iter().map(|grant| grant.request).collect()
}

fn release_random_hold(rng: &mut Rng, table: &mut PoolTable) -> bool {
    let holds: Vec<(String, u64)> = table
        .pools
        .iter()
        .flat_map(|(name, pool)| pool.holds.iter().map(|hold| (name.clone(), hold.ticket)))
        .collect();
    if holds.is_empty() {
        return false;
    }
    let (pool, ticket) = &holds[rng.below(holds.len() as u64) as usize];
    table.release(pool, *ticket).is_some()
}

#[test]
fn size_is_never_exceeded_and_nobody_passes_an_older_waiter() {
    for seed in 0..SEEDS {
        let mut rng = Rng(seed);
        let mut table = generate_table(&mut rng);
        let mut now = Timestamp(0);
        for id in 0..80 {
            now = Timestamp(now.0 + 500 * rng.below(5));
            match rng.below(3) {
                0 => {
                    let request = generate_request(&mut rng, &table, id);
                    table.enqueue(&request, now).unwrap();
                }
                1 => {
                    release_random_hold(&mut rng, &mut table);
                }
                _ => {
                    grant_step(&mut table, now, seed);
                }
            }
        }
    }
}

#[test]
fn every_waiter_is_granted_when_holders_release_in_any_order() {
    for seed in 0..SEEDS {
        let mut rng = Rng(seed.wrapping_mul(31).wrapping_add(7));
        let mut table = generate_table(&mut rng);
        let requests = 3 + rng.below(10);
        let mut waiting = BTreeSet::new();
        for id in 0..requests {
            let request = generate_request(&mut rng, &table, id);
            table.enqueue(&request, Timestamp(0)).unwrap();
            waiting.insert(RequestId(id));
        }
        let mut now = Timestamp(0);
        let mut idle = 0;
        while !waiting.is_empty() {
            // Each round takes a fresh reading after every guard's settle time.
            now = Timestamp(now.0 + 2000);
            let granted = grant_step(&mut table, now, seed);
            for request in &granted {
                waiting.remove(request);
            }
            if granted.is_empty() {
                if !release_random_hold(&mut rng, &mut table) {
                    idle += 1;
                    assert!(
                        idle < 3,
                        "seed {seed}: waiters left with no hold to wait for"
                    );
                }
            } else {
                idle = 0;
                if rng.below(2) == 0 {
                    release_random_hold(&mut rng, &mut table);
                }
            }
        }
    }
}
