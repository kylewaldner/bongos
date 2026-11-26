use crate::decl::{GuardCheck, PoolKind};
use crate::grant::{
    Blocked, Grant, GuardReadings, GuardValue, PoolState, PoolTable, PoolWait, Request, RequestId,
    Timestamp, WaitStatus, admit_now, grant, try_now, wait_status,
};
use crate::holder::{Holder, HolderLabel, HolderPath, HostRef, Level, OwnerKey};
use crate::needs::{Need, NeedMode};

const HOST: HostRef = HostRef { pid: 100, start: 7 };
const OTHER_HOST: HostRef = HostRef { pid: 200, start: 9 };

fn holder(owner: &str, host: HostRef) -> Holder {
    Holder {
        project: "p".to_string(),
        path: HolderPath {
            owner: OwnerKey(owner.to_string()),
            level: Level::Job,
        },
        label: HolderLabel(owner.to_string()),
        host,
        process: None,
    }
}

fn need(pool: &str) -> Need {
    Need {
        pool: pool.to_string(),
        mode: None,
        count: None,
    }
}

fn mode(pool: &str, mode: NeedMode) -> Need {
    Need {
        pool: pool.to_string(),
        mode: Some(mode),
        count: None,
    }
}

fn request(id: u64, needs: Vec<Need>) -> Request {
    request_on(id, HOST, needs)
}

fn request_on(id: u64, host: HostRef, needs: Vec<Need>) -> Request {
    Request {
        id: RequestId(id),
        holder: holder(&format!("r{id}"), host),
        mixed: false,
        needs,
    }
}

fn table(pools: &[(&str, PoolState)]) -> PoolTable {
    let mut table = PoolTable::default();
    for (name, pool) in pools {
        table.pools.insert(name.to_string(), pool.clone());
    }
    table
}

fn at(ms: u64) -> Timestamp {
    Timestamp(ms)
}

fn ids(grants: &[Grant]) -> Vec<u64> {
    grants.iter().map(|grant| grant.request.0).collect()
}

/// Grants for HOST with no readings, applies them, and returns the granted request ids.
fn step(table: &mut PoolTable) -> Vec<u64> {
    let grants = grant(table, &GuardReadings::default(), at(0), &HOST);
    table.apply(&grants);
    ids(&grants)
}

fn release_request(table: &mut PoolTable, id: u64) {
    for pool in table.pools.values_mut() {
        pool.holds.retain(|hold| hold.request != RequestId(id));
    }
}

fn blocked_of(table: &PoolTable, id: u64) -> Vec<Option<Blocked>> {
    match wait_status(table, RequestId(id)) {
        WaitStatus::NotWaiting => Vec::new(),
        WaitStatus::Waiting(waits) => waits
            .into_iter()
            .map(|wait: PoolWait| wait.blocked)
            .collect(),
    }
}

#[test]
fn a_waiting_writer_blocks_later_readers_until_it_ran() {
    let mut t = table(&[("db", PoolState::new(PoolKind::Rw, 0))]);
    t.enqueue(&request(1, vec![need("db")]), at(0)).unwrap();
    t.enqueue(&request(2, vec![mode("db", NeedMode::Shared)]), at(0))
        .unwrap();
    assert_eq!(
        step(&mut t),
        vec![1, 2],
        "consecutive shared waiters at the head"
    );
    t.enqueue(&request(3, vec![mode("db", NeedMode::Exclusive)]), at(1))
        .unwrap();
    t.enqueue(&request(4, vec![need("db")]), at(2)).unwrap();
    assert_eq!(step(&mut t), Vec::<u64>::new());
    assert_eq!(
        blocked_of(&t, 4),
        vec![Some(Blocked::Behind {
            pool: "db".to_string(),
            ticket: 2
        })]
    );
    release_request(&mut t, 1);
    assert_eq!(step(&mut t), Vec::<u64>::new(), "one shared holder remains");
    release_request(&mut t, 2);
    assert_eq!(
        step(&mut t),
        vec![3],
        "the writer runs once the holders drain"
    );
    assert_eq!(
        step(&mut t),
        Vec::<u64>::new(),
        "the reader waits while the writer runs"
    );
    release_request(&mut t, 3);
    assert_eq!(step(&mut t), vec![4]);
}

#[test]
fn slots_of_two_grant_two_of_three() {
    let mut t = table(&[("jobs", PoolState::new(PoolKind::Slots, 2))]);
    for id in 1..=3 {
        t.enqueue(&request(id, vec![need("jobs")]), at(id)).unwrap();
    }
    assert_eq!(step(&mut t), vec![1, 2]);
    assert_eq!(
        blocked_of(&t, 3),
        vec![Some(Blocked::Busy {
            pool: "jobs".to_string()
        })]
    );
    release_request(&mut t, 2);
    assert_eq!(step(&mut t), vec![3]);
    assert_eq!(wait_status(&t, RequestId(3)), WaitStatus::NotWaiting);
}

#[test]
fn a_slot_count_waits_for_room_without_barging() {
    let mut t = table(&[("jobs", PoolState::new(PoolKind::Slots, 3))]);
    t.enqueue(&request(1, vec![need("jobs")]), at(0)).unwrap();
    assert_eq!(step(&mut t), vec![1]);
    let big = Need {
        count: Some(3),
        ..need("jobs")
    };
    t.enqueue(&request(2, vec![big]), at(1)).unwrap();
    t.enqueue(&request(3, vec![need("jobs")]), at(2)).unwrap();
    assert_eq!(
        step(&mut t),
        Vec::<u64>::new(),
        "the small one does not pass the big one"
    );
    release_request(&mut t, 1);
    assert_eq!(step(&mut t), vec![2]);
}

#[test]
fn a_mutex_grants_one_at_a_time() {
    let mut t = table(&[("merge", PoolState::new(PoolKind::Mutex, 5))]);
    assert_eq!(t.pools["merge"].size, 1);
    t.enqueue(&request(1, vec![need("merge")]), at(0)).unwrap();
    t.enqueue(&request(2, vec![need("merge")]), at(0)).unwrap();
    assert_eq!(step(&mut t), vec![1]);
    assert_eq!(step(&mut t), Vec::<u64>::new());
    release_request(&mut t, 1);
    assert_eq!(step(&mut t), vec![2]);
}

#[test]
fn an_empty_pool_grants_at_once_and_no_pool_needs_no_ticket() {
    let mut t = table(&[("jobs", PoolState::new(PoolKind::Slots, 1))]);
    assert_eq!(t.enqueue(&request(1, vec![]), at(0)).unwrap(), vec![]);
    assert_eq!(t.next_ticket, 0);
    assert_eq!(
        try_now(&t, &request(1, vec![]), &GuardReadings::default()),
        Ok(vec![])
    );
    t.enqueue(&request(2, vec![need("jobs")]), at(0)).unwrap();
    assert_eq!(step(&mut t), vec![2]);
}

#[test]
fn a_failing_guard_grants_nobody_and_reports_figure_and_threshold() {
    let pool = PoolState::new(PoolKind::Guard, 0).with_guard(GuardCheck::FreeMemoryGb(12.0), 5);
    let mut t = table(&[("mem", pool)]);
    t.enqueue(&request(1, vec![need("mem")]), at(0)).unwrap();
    t.enqueue(&request(2, vec![need("mem")]), at(0)).unwrap();
    let guards = GuardReadings::default().with("mem", GuardValue::FreeMemoryGb(9.0), at(1000));
    assert!(grant(&t, &guards, at(1000), &HOST).is_empty());
    t.record_readings(&guards);
    let expected = Blocked::Guard {
        pool: "mem".to_string(),
        reading: GuardValue::FreeMemoryGb(9.0),
        threshold: GuardCheck::FreeMemoryGb(12.0),
    };
    assert_eq!(blocked_of(&t, 1), vec![Some(expected.clone())]);
    assert_eq!(blocked_of(&t, 2), vec![Some(expected)]);
}

#[test]
fn a_guard_grants_one_per_reading_after_settling() {
    let pool = PoolState::new(PoolKind::Guard, 0).with_guard(GuardCheck::FreeMemoryGb(12.0), 5);
    let mut t = table(&[("mem", pool)]);
    for id in 1..=3 {
        t.enqueue(&request(id, vec![need("mem")]), at(0)).unwrap();
    }
    let first = GuardReadings::default().with("mem", GuardValue::FreeMemoryGb(20.0), at(1000));
    let grants = grant(&t, &first, at(1000), &HOST);
    assert_eq!(ids(&grants), vec![1], "one grant per reading");
    t.apply(&grants);
    assert!(
        t.pools["mem"].holds.is_empty(),
        "a guard pool records no hold"
    );
    assert!(
        grant(&t, &first, at(2000), &HOST).is_empty(),
        "the reading is spent"
    );
    let early = GuardReadings::default().with("mem", GuardValue::FreeMemoryGb(20.0), at(3000));
    assert!(
        grant(&t, &early, at(3000), &HOST).is_empty(),
        "within settle_seconds"
    );
    assert_eq!(
        blocked_of(&t, 2),
        vec![Some(Blocked::GuardSettling {
            pool: "mem".to_string(),
            next: at(6000)
        })]
    );
    let later = GuardReadings::default().with("mem", GuardValue::FreeMemoryGb(20.0), at(6000));
    let grants = grant(&t, &later, at(6000), &HOST);
    assert_eq!(ids(&grants), vec![2]);
}

#[test]
fn a_slots_pool_with_a_guard_needs_room_and_a_passing_reading() {
    let pool = PoolState::new(PoolKind::Slots, 2).with_guard(GuardCheck::Script("ok".into()), 0);
    let mut t = table(&[("build", pool)]);
    t.enqueue(&request(1, vec![need("build")]), at(0)).unwrap();
    assert!(grant(&t, &GuardReadings::default(), at(0), &HOST).is_empty());
    assert_eq!(
        blocked_of(&t, 1),
        vec![Some(Blocked::GuardUnread {
            pool: "build".to_string()
        })]
    );
    let pass = GuardReadings::default().with("build", GuardValue::ScriptExit(0), at(10));
    let grants = grant(&t, &pass, at(10), &HOST);
    assert_eq!(ids(&grants), vec![1]);
    t.apply(&grants);
    assert_eq!(t.pools["build"].holds.len(), 1);
}

#[test]
fn try_now_refuses_while_any_waiter_is_queued() {
    let mut t = table(&[("jobs", PoolState::new(PoolKind::Slots, 2))]);
    let probe = request(9, vec![need("jobs")]);
    assert_eq!(
        try_now(&t, &probe, &GuardReadings::default()),
        Ok(vec![("jobs".to_string(), None)])
    );
    let big = Need {
        count: Some(2),
        ..need("jobs")
    };
    t.enqueue(&request(1, vec![need("jobs")]), at(0)).unwrap();
    assert_eq!(step(&mut t), vec![1]);
    t.enqueue(&request(2, vec![big]), at(0)).unwrap();
    assert_eq!(step(&mut t), Vec::<u64>::new());
    assert_eq!(
        try_now(&t, &probe, &GuardReadings::default()),
        Err(Blocked::Queued {
            pool: "jobs".to_string(),
            waiters: 1
        }),
        "a slot is free, but a waiter is queued"
    );
    assert_eq!(
        t.pools["jobs"].waiters.len(),
        1,
        "nothing is queued by try_now"
    );
    assert_eq!(
        try_now(
            &t,
            &request(9, vec![need("nope")]),
            &GuardReadings::default()
        ),
        Err(Blocked::UnknownPool {
            pool: "nope".to_string()
        })
    );
}

#[test]
fn an_immediate_admission_spends_the_guard_reading() {
    let pool = PoolState::new(PoolKind::Guard, 0).with_guard(GuardCheck::FreeMemoryGb(12.0), 5);
    let mut t = table(&[("mem", pool)]);
    let first = GuardReadings::default().with("mem", GuardValue::FreeMemoryGb(20.0), at(1000));
    let a = request(1, vec![need("mem")]);
    let b = request(2, vec![need("mem")]);
    let grant_a = admit_now(&mut t, &a, &first, at(1000)).unwrap();
    assert_eq!(grant_a.pools.len(), 1);
    assert!(
        t.pools["mem"].holds.is_empty(),
        "a guard pool records no hold"
    );
    assert_eq!(
        admit_now(&mut t, &b, &first, at(1000)),
        Err(Blocked::GuardSettling {
            pool: "mem".to_string(),
            next: at(6000)
        }),
        "a second waiter is not admitted on the same reading"
    );
    assert!(try_now(&t, &b, &first).is_err(), "try_now agrees");
    let later = GuardReadings::default().with("mem", GuardValue::FreeMemoryGb(20.0), at(6000));
    assert!(admit_now(&mut t, &b, &later, at(6000)).is_ok());
}

#[test]
fn try_now_refuses_a_full_pool() {
    let mut t = table(&[("merge", PoolState::new(PoolKind::Mutex, 1))]);
    t.enqueue(&request(1, vec![need("merge")]), at(0)).unwrap();
    step(&mut t);
    assert_eq!(
        try_now(
            &t,
            &request(2, vec![need("merge")]),
            &GuardReadings::default()
        ),
        Err(Blocked::Busy {
            pool: "merge".to_string()
        })
    );
}

#[test]
fn a_multi_pool_request_is_all_or_none() {
    let mut t = table(&[
        ("a", PoolState::new(PoolKind::Mutex, 1)),
        ("b", PoolState::new(PoolKind::Mutex, 1)),
    ]);
    t.enqueue(&request(1, vec![need("b")]), at(0)).unwrap();
    assert_eq!(step(&mut t), vec![1]);
    let tickets = t
        .enqueue(&request(2, vec![need("a"), need("b")]), at(1))
        .unwrap();
    assert_eq!(tickets, vec![("a".to_string(), 1), ("b".to_string(), 2)]);
    t.enqueue(&request(3, vec![need("a")]), at(2)).unwrap();
    assert_eq!(
        step(&mut t),
        Vec::<u64>::new(),
        "2 waits in b, so 3 waits behind it in a"
    );
    assert!(
        t.pools["a"].holds.is_empty(),
        "2 holds nothing while it waits"
    );
    assert_eq!(
        blocked_of(&t, 2),
        vec![
            Some(Blocked::OtherPool {
                pool: "a".to_string(),
                other: "b".to_string()
            }),
            Some(Blocked::Busy {
                pool: "b".to_string()
            }),
        ]
    );
    assert_eq!(
        blocked_of(&t, 3),
        vec![Some(Blocked::Behind {
            pool: "a".to_string(),
            ticket: 1
        })]
    );
    release_request(&mut t, 1);
    assert_eq!(step(&mut t), vec![2]);
    assert_eq!(t.pools["a"].holds.len(), 1);
    assert_eq!(t.pools["b"].holds.len(), 1);
}

#[test]
fn a_mixed_waiter_blocks_a_machine_pool_for_every_host_but_its_own() {
    let mut t = table(&[("gpu", PoolState::new(PoolKind::Slots, 2))]);
    let mut mixed = request_on(1, OTHER_HOST, vec![need("gpu")]);
    mixed.mixed = true;
    t.enqueue(&mixed, at(0)).unwrap();
    t.enqueue(&request_on(2, HOST, vec![need("gpu")]), at(1))
        .unwrap();
    assert!(
        grant(&t, &GuardReadings::default(), at(2), &HOST).is_empty(),
        "another host's mixed waiter stops the scan"
    );
    assert_eq!(
        blocked_of(&t, 2),
        vec![Some(Blocked::Behind {
            pool: "gpu".to_string(),
            ticket: 0
        })]
    );
    let own = grant(&t, &GuardReadings::default(), at(2), &OTHER_HOST);
    assert_eq!(ids(&own), vec![1], "its own host judges it");
    t.apply(&own);
    assert_eq!(step(&mut t), vec![2]);
}

#[test]
fn a_host_converts_only_its_own_waiters() {
    let mut t = table(&[("jobs", PoolState::new(PoolKind::Slots, 1))]);
    t.enqueue(&request_on(1, OTHER_HOST, vec![need("jobs")]), at(0))
        .unwrap();
    t.enqueue(&request_on(2, HOST, vec![need("jobs")]), at(1))
        .unwrap();
    assert!(
        grant(&t, &GuardReadings::default(), at(2), &HOST).is_empty(),
        "the other host's older waiter takes the slot"
    );
    let other = grant(&t, &GuardReadings::default(), at(2), &OTHER_HOST);
    assert_eq!(ids(&other), vec![1]);
    assert_eq!(other[0].host, OTHER_HOST);
    t.pools.get_mut("jobs").unwrap().size = 2;
    assert_eq!(
        ids(&grant(&t, &GuardReadings::default(), at(2), &HOST)),
        vec![2],
        "with room for both, each host converts its own"
    );
}

#[test]
fn tickets_come_from_one_counter_and_enqueue_checks_every_need_first() {
    let mut t = table(&[
        ("a", PoolState::new(PoolKind::Slots, 1)),
        ("b", PoolState::new(PoolKind::Rw, 0)),
    ]);
    assert!(
        t.enqueue(&request(1, vec![need("a"), need("zz")]), at(0))
            .is_err()
    );
    assert!(
        t.enqueue(&request(1, vec![need("a"), need("a")]), at(0))
            .is_err()
    );
    let big = Need {
        count: Some(2),
        ..need("a")
    };
    assert!(t.enqueue(&request(1, vec![big]), at(0)).is_err());
    assert_eq!(t.next_ticket, 0);
    assert!(t.pools["a"].waiters.is_empty());
    t.enqueue(&request(1, vec![need("a")]), at(0)).unwrap();
    let tickets = t
        .enqueue(&request(2, vec![need("b"), need("a")]), at(0))
        .unwrap();
    assert_eq!(tickets, vec![("b".to_string(), 1), ("a".to_string(), 2)]);
}

#[test]
fn release_names_the_hold_and_never_releases_twice() {
    let mut t = table(&[("jobs", PoolState::new(PoolKind::Slots, 1))]);
    t.enqueue(&request(1, vec![need("jobs")]), at(0)).unwrap();
    step(&mut t);
    assert_eq!(
        t.release("jobs", 0).map(|hold| hold.request),
        Some(RequestId(1))
    );
    assert_eq!(t.release("jobs", 0), None);
}
