use crate::decl::{GuardCheck, PoolKind};
use crate::grant::{
    Admit, Blocked, GuardReadings, GuardValue, Hold, PoolState, PoolTable, QueueError, Request,
    RequestId, Satisfied, Timestamp, admit_now, check_own_hold, grant, try_now,
};
use crate::holder::{Holder, HolderLabel, HolderPath, HostRef, Level, OwnerKey};
use crate::needs::{Need, NeedMode};
use crate::wait_for::{CycleLink, Refused, judge, would_cycle};

const HOST: HostRef = HostRef { pid: 100, start: 7 };

fn step(key: &str) -> Level {
    Level::Step {
        key: key.to_string(),
    }
}

fn part(key: &str, part: &str) -> Level {
    Level::Part {
        key: key.to_string(),
        part: part.to_string(),
    }
}

fn holder(owner: &str, level: Level) -> Holder {
    Holder {
        project: "p".to_string(),
        label: HolderLabel(format!("{owner} {level:?}")),
        path: HolderPath {
            owner: OwnerKey(owner.to_string()),
            level,
        },
        host: HOST,
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
        mode: Some(mode),
        ..need(pool)
    }
}

fn count(pool: &str, count: u32) -> Need {
    Need {
        count: Some(count),
        ..need(pool)
    }
}

fn request(id: u64, holder: &Holder, needs: Vec<Need>) -> Request {
    Request {
        id: RequestId(id),
        holder: holder.clone(),
        mixed: false,
        needs,
    }
}

fn table(pools: &[(&str, PoolKind, u32)]) -> PoolTable {
    let mut table = PoolTable::default();
    for (name, kind, size) in pools {
        table
            .pools
            .insert(name.to_string(), PoolState::new(*kind, *size));
    }
    table
}

/// Records a hold directly, as a grant would have.
fn hold(table: &mut PoolTable, id: u64, holder: &Holder, need: Need) {
    let ticket = table.issue_ticket();
    let pool = table.pools.get_mut(&need.pool).unwrap();
    pool.holds.push(Hold {
        ticket,
        request: RequestId(id),
        holder: holder.clone(),
        need,
        since: Timestamp(0),
    });
}

/// Queues a request after checking it is neither refused nor a deadlock, as the broker would.
fn queue(table: &mut PoolTable, request: &Request) {
    let satisfied = judge(&[table], request).expect("queued");
    table
        .enqueue(&satisfied.remaining(request), Timestamp(0))
        .unwrap();
}

fn satisfied(pools: &[&str]) -> Satisfied {
    Satisfied {
        pools: pools.iter().map(|pool| pool.to_string()).collect(),
    }
}

fn cycle_paths(refused: Refused) -> Vec<(HolderPath, Option<String>)> {
    match refused {
        Refused::Deadlock { cycle } => cycle
            .into_iter()
            .map(|link: CycleLink| (link.path, link.waits_for))
            .collect(),
        other => panic!("not a deadlock: {other:?}"),
    }
}

fn waits(holder: &Holder, pool: Option<&str>) -> (HolderPath, Option<String>) {
    (holder.path.clone(), pool.map(str::to_string))
}

#[test]
fn a_node_under_a_job_holding_jobs_shared_is_satisfied_and_refused_exclusive() {
    let mut t = table(&[("jobs", PoolKind::Rw, 0)]);
    let job = holder("r1", Level::Job);
    let node = holder("r1", step("build"));
    hold(&mut t, 1, &job, mode("jobs", NeedMode::Shared));
    let writer = holder("r9", Level::Job);
    t.enqueue(
        &request(2, &writer, vec![mode("jobs", NeedMode::Exclusive)]),
        Timestamp(0),
    )
    .unwrap();

    let shared = request(3, &node, vec![need("jobs")]);
    assert_eq!(check_own_hold(&t, &shared), Ok(satisfied(&["jobs"])));
    assert_eq!(judge(&[&t], &shared), Ok(satisfied(&["jobs"])));
    let before = t.next_ticket;
    assert_eq!(
        t.enqueue(&satisfied(&["jobs"]).remaining(&shared), Timestamp(0)),
        Ok(vec![]),
        "no ticket, not behind the waiting writer"
    );
    assert_eq!(t.next_ticket, before);

    let exclusive = request(4, &node, vec![mode("jobs", NeedMode::Exclusive)]);
    let refused = Refused::OwnHold {
        pool: "jobs".to_string(),
    };
    assert_eq!(check_own_hold(&t, &exclusive), Err(refused.clone()));
    assert_eq!(judge(&[&t], &exclusive), Err(refused));
}

#[test]
fn sibling_nodes_and_sibling_parts_queue_for_merge_like_strangers() {
    for (first, second) in [
        (holder("r1", step("a")), holder("r1", step("b"))),
        (
            holder("check/t/build", part("", "1")),
            holder("check/t/build", part("", "2")),
        ),
    ] {
        let mut t = table(&[("merge", PoolKind::Mutex, 1)]);
        queue(&mut t, &request(1, &first, vec![need("merge")]));
        let second_request = request(2, &second, vec![need("merge")]);
        assert_eq!(check_own_hold(&t, &second_request), Ok(satisfied(&[])));
        assert_eq!(would_cycle(&[&t], &second_request), Ok(()));
        queue(&mut t, &second_request);
        let grants = grant(&t, &GuardReadings::default(), Timestamp(1), &HOST);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].request, RequestId(1), "the first goes first");
        t.apply(&grants);
        let third = request(3, &second, vec![need("merge")]);
        assert_eq!(
            check_own_hold(&t, &third),
            Ok(satisfied(&[])),
            "a sibling's hold never satisfies"
        );
        assert_eq!(would_cycle(&[&t], &third), Ok(()));
    }
}

#[test]
fn a_parts_request_for_a_pool_its_node_holds_is_satisfied_and_a_siblings_is_not() {
    let mut t = table(&[("merge", PoolKind::Mutex, 1), ("cpu", PoolKind::Slots, 2)]);
    let node = holder("r1", step("build"));
    hold(&mut t, 1, &node, need("merge"));
    let part2 = holder("r1", part("build", "2"));
    assert_eq!(
        check_own_hold(&t, &request(2, &part2, vec![need("merge")])),
        Ok(satisfied(&["merge"]))
    );
    let other_key = holder("r1", part("lint", "2"));
    assert_eq!(
        check_own_hold(&t, &request(3, &other_key, vec![need("merge")])),
        Ok(satisfied(&[])),
        "a part of another node"
    );

    let mut t = table(&[("merge", PoolKind::Mutex, 1)]);
    let part1 = holder("r1", part("build", "1"));
    hold(&mut t, 1, &part1, need("merge"));
    let asks = request(2, &part2, vec![need("merge")]);
    assert_eq!(check_own_hold(&t, &asks), Ok(satisfied(&[])));
    assert_eq!(would_cycle(&[&t], &asks), Ok(()), "an ordinary wait");
}

#[test]
fn a_two_path_cycle_refuses_the_newest_request_naming_both() {
    let mut t = table(&[
        ("compile", PoolKind::Mutex, 1),
        ("merge", PoolKind::Mutex, 1),
    ]);
    let node = holder("R1", step("merge"));
    let part2 = holder("check/t/build", part("", "2"));
    hold(&mut t, 1, &node, need("compile"));
    hold(&mut t, 2, &part2, need("merge"));
    queue(&mut t, &request(3, &node, vec![need("merge")]));
    let newest = request(4, &part2, vec![need("compile")]);
    let refused = would_cycle(&[&t], &newest).unwrap_err();
    let Refused::Deadlock { cycle } = &refused else {
        panic!("not a deadlock: {refused:?}");
    };
    assert_eq!(cycle[0].label, part2.label, "labels come from the holders");
    assert_eq!(
        cycle_paths(refused.clone()),
        vec![waits(&part2, Some("compile")), waits(&node, Some("merge"))]
    );
    assert_eq!(judge(&[&t], &newest), Err(refused.clone()));
    assert_eq!(would_cycle(&[&t], &newest), Err(refused), "deterministic");
}

#[test]
fn an_enclosing_holder_whose_node_waits_is_part_of_the_cycle() {
    let mut t = table(&[("a", PoolKind::Mutex, 1), ("b", PoolKind::Mutex, 1)]);
    let job = holder("R1", Level::Job);
    let node = holder("R1", step("s"));
    let stranger = holder("R2", Level::Job);
    hold(&mut t, 1, &job, need("a"));
    hold(&mut t, 2, &stranger, need("b"));
    queue(&mut t, &request(3, &node, vec![need("b")]));
    let newest = request(4, &stranger, vec![need("a")]);
    assert_eq!(
        cycle_paths(would_cycle(&[&t], &newest).unwrap_err()),
        vec![
            waits(&stranger, Some("a")),
            waits(&job, None),
            waits(&node, Some("b")),
        ]
    );

    let mut t = table(&[("a", PoolKind::Mutex, 1), ("b", PoolKind::Mutex, 1)]);
    let other_job = holder("R3", Level::Job);
    hold(&mut t, 1, &other_job, need("a"));
    hold(&mut t, 2, &stranger, need("b"));
    queue(&mut t, &request(3, &node, vec![need("b")]));
    assert_eq!(
        would_cycle(&[&t], &newest),
        Ok(()),
        "the holder of a does not enclose the waiting node"
    );
}

#[test]
fn a_three_path_cycle_is_refused_and_a_chain_is_not() {
    let mut t = table(&[
        ("a", PoolKind::Mutex, 1),
        ("b", PoolKind::Mutex, 1),
        ("c", PoolKind::Mutex, 1),
    ]);
    let x = holder("X", Level::Job);
    let y = holder("Y", Level::Job);
    let z = holder("Z", Level::Job);
    hold(&mut t, 1, &x, need("a"));
    hold(&mut t, 2, &y, need("b"));
    queue(&mut t, &request(3, &x, vec![need("b")]));
    queue(&mut t, &request(4, &y, vec![need("c")]));
    let chain = request(5, &z, vec![need("a")]);
    assert_eq!(would_cycle(&[&t], &chain), Ok(()), "z holds nothing yet");
    hold(&mut t, 6, &z, need("c"));
    assert_eq!(
        cycle_paths(would_cycle(&[&t], &chain).unwrap_err()),
        vec![
            waits(&z, Some("a")),
            waits(&x, Some("b")),
            waits(&y, Some("c")),
        ]
    );
}

#[test]
fn a_cycle_through_the_project_and_machine_tables_is_found() {
    let mut project = table(&[("a", PoolKind::Mutex, 1)]);
    let mut machine = table(&[("gpu", PoolKind::Slots, 1)]);
    let x = holder("X", step("train"));
    let y = holder("Y", step("eval"));
    hold(&mut project, 1, &x, need("a"));
    hold(&mut machine, 2, &y, need("gpu"));
    machine
        .enqueue(&request(3, &x, vec![need("gpu")]), Timestamp(0))
        .unwrap();
    let newest = request(4, &y, vec![need("a")]);
    assert_eq!(
        cycle_paths(would_cycle(&[&project, &machine], &newest).unwrap_err()),
        vec![waits(&y, Some("a")), waits(&x, Some("gpu"))]
    );
    assert_eq!(
        would_cycle(&[&project], &newest),
        Ok(()),
        "the machine table is not visible"
    );
}

#[test]
fn enclosing_slots_satisfy_equal_or_smaller_counts() {
    let mut t = table(&[("jobs", PoolKind::Slots, 4)]);
    let job = holder("r1", Level::Job);
    let node = holder("r1", step("s"));
    hold(&mut t, 1, &job, count("jobs", 2));
    for asked in [1, 2] {
        assert_eq!(
            check_own_hold(&t, &request(2, &node, vec![count("jobs", asked)])),
            Ok(satisfied(&["jobs"]))
        );
    }
    assert_eq!(
        check_own_hold(&t, &request(2, &node, vec![count("jobs", 3)])),
        Err(Refused::OwnHold {
            pool: "jobs".to_string()
        }),
        "2 held + 3 asked is above 4"
    );

    let mut t = table(&[("jobs", PoolKind::Slots, 5)]);
    hold(&mut t, 1, &job, count("jobs", 2));
    let stranger = holder("r2", Level::Job);
    hold(&mut t, 2, &stranger, count("jobs", 2));
    let three = request(3, &node, vec![count("jobs", 3)]);
    assert_eq!(
        check_own_hold(&t, &three),
        Ok(satisfied(&[])),
        "fits beside the job's hold, so it queues"
    );
    t.enqueue(&request(5, &stranger, vec![count("jobs", 1)]), Timestamp(0))
        .unwrap();
    assert_eq!(
        would_cycle(&[&t], &three),
        Ok(()),
        "it waits for the stranger, not for its own job"
    );

    let mut t = table(&[("jobs", PoolKind::Slots, 4)]);
    hold(&mut t, 1, &job, count("jobs", 1));
    hold(&mut t, 2, &holder("r1", Level::Run), count("jobs", 1));
    assert_eq!(
        check_own_hold(&t, &request(3, &node, vec![count("jobs", 2)])),
        Ok(satisfied(&["jobs"])),
        "enclosing holds count together"
    );
}

#[test]
fn rw_and_mutex_enclosing_holds_cover_by_mode() {
    let job = holder("r1", Level::Job);
    let node = holder("r1", step("s"));
    let mut t = table(&[("db", PoolKind::Rw, 0), ("merge", PoolKind::Mutex, 1)]);
    hold(&mut t, 1, &job, mode("db", NeedMode::Exclusive));
    hold(&mut t, 2, &job, need("merge"));
    let both = request(
        3,
        &node,
        vec![mode("db", NeedMode::Exclusive), need("merge")],
    );
    assert_eq!(check_own_hold(&t, &both), Ok(satisfied(&["db", "merge"])));
    assert_eq!(
        check_own_hold(&t, &request(4, &node, vec![mode("db", NeedMode::Shared)])),
        Ok(satisfied(&["db"]))
    );
    let mut t = table(&[("db", PoolKind::Rw, 0)]);
    hold(&mut t, 1, &job, mode("db", NeedMode::Shared));
    hold(
        &mut t,
        2,
        &holder("r1", Level::Run),
        mode("db", NeedMode::Exclusive),
    );
    assert_eq!(
        check_own_hold(
            &t,
            &request(3, &node, vec![mode("db", NeedMode::Exclusive)])
        ),
        Ok(satisfied(&["db"])),
        "an exclusive among the enclosing holds covers"
    );
}

#[test]
fn try_now_and_admit_now_judge_enclosing_holds_first() {
    let mut t = table(&[("jobs", PoolKind::Rw, 0), ("cpu", PoolKind::Slots, 2)]);
    let job = holder("r1", Level::Job);
    let node = holder("r1", step("s"));
    hold(&mut t, 1, &job, need("jobs"));
    let writer = holder("r9", Level::Job);
    t.enqueue(
        &request(2, &writer, vec![mode("jobs", NeedMode::Exclusive)]),
        Timestamp(0),
    )
    .unwrap();
    let r = request(3, &node, vec![need("jobs"), need("cpu")]);
    assert_eq!(
        try_now(&t, &r, &GuardReadings::default()),
        Ok(vec![
            ("jobs".to_string(), Admit::Satisfied),
            ("cpu".to_string(), Admit::Granted(None)),
        ]),
        "satisfied although a writer is queued"
    );
    let jobs_before = t.pools["jobs"].clone();
    let grant = admit_now(&mut t, &r, &GuardReadings::default(), Timestamp(5)).unwrap();
    assert_eq!(grant.satisfied, vec!["jobs".to_string()]);
    assert_eq!(grant.pools.len(), 1);
    assert_eq!(grant.pools[0].pool, "cpu");
    assert_eq!(t.pools["jobs"], jobs_before, "no hold in a satisfied pool");
    assert_eq!(t.pools["cpu"].holds.len(), 1);

    let exclusive = request(4, &node, vec![mode("jobs", NeedMode::Exclusive)]);
    let before = t.clone();
    let blocked = Blocked::OwnHold {
        pool: "jobs".to_string(),
    };
    assert_eq!(
        try_now(&t, &exclusive, &GuardReadings::default()),
        Err(blocked.clone())
    );
    assert_eq!(
        admit_now(&mut t, &exclusive, &GuardReadings::default(), Timestamp(6)),
        Err(blocked)
    );
    assert_eq!(t, before);
}

#[test]
fn a_satisfied_pool_spends_no_guard_reading() {
    let pool = PoolState::new(PoolKind::Slots, 2).with_guard(GuardCheck::FreeMemoryGb(4.0), 5);
    let mut t = PoolTable::default();
    t.pools.insert("build".to_string(), pool);
    let job = holder("r1", Level::Job);
    hold(&mut t, 1, &job, need("build"));
    let guards =
        GuardReadings::default().with("build", GuardValue::FreeMemoryGb(1.0), Timestamp(9));
    let node = request(2, &holder("r1", step("s")), vec![need("build")]);
    let grant = admit_now(&mut t, &node, &guards, Timestamp(9)).unwrap();
    assert_eq!(grant.satisfied, vec!["build".to_string()]);
    assert!(grant.pools.is_empty());
    assert_eq!(t.pools["build"].guard.as_ref().unwrap().spent, None);
}

#[test]
fn a_guard_pool_is_never_satisfied_by_an_enclosing_holder() {
    let pool = PoolState::new(PoolKind::Guard, 0).with_guard(GuardCheck::FreeMemoryGb(4.0), 5);
    let mut t = PoolTable::default();
    t.pools.insert("mem".to_string(), pool);
    let job = holder("r1", Level::Job);
    let guards = GuardReadings::default().with("mem", GuardValue::FreeMemoryGb(8.0), Timestamp(1));
    admit_now(
        &mut t,
        &request(1, &job, vec![need("mem")]),
        &guards,
        Timestamp(1),
    )
    .unwrap();
    let node = request(2, &holder("r1", step("s")), vec![need("mem")]);
    assert_eq!(check_own_hold(&t, &node), Ok(satisfied(&[])));
    assert_eq!(
        try_now(&t, &node, &guards),
        Err(Blocked::GuardSettling {
            pool: "mem".to_string(),
            next: Timestamp(5001)
        }),
        "one grant per reading still holds"
    );
}

#[test]
fn a_multi_pool_request_queues_only_its_unsatisfied_pools_all_or_none() {
    let mut t = table(&[("jobs", PoolKind::Rw, 0), ("merge", PoolKind::Mutex, 1)]);
    let job = holder("r1", Level::Job);
    hold(&mut t, 1, &job, need("jobs"));
    let sibling = holder("r1", step("a"));
    hold(&mut t, 2, &sibling, need("merge"));
    let node = holder("r1", step("b"));
    let r = request(3, &node, vec![need("jobs"), need("merge")]);
    let found = judge(&[&t], &r).unwrap();
    assert_eq!(found, satisfied(&["jobs"]));
    let tickets = t.enqueue(&found.remaining(&r), Timestamp(0)).unwrap();
    assert_eq!(tickets, vec![("merge".to_string(), 2)]);
    assert!(grant(&t, &GuardReadings::default(), Timestamp(1), &HOST).is_empty());
    t.pools.get_mut("merge").unwrap().holds.clear();
    let grants = grant(&t, &GuardReadings::default(), Timestamp(2), &HOST);
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].pools.len(), 1, "only merge was queued");
}

#[test]
fn a_pool_named_twice_is_left_for_enqueue_to_refuse() {
    let mut t = table(&[("merge", PoolKind::Mutex, 1)]);
    hold(&mut t, 1, &holder("r1", Level::Job), need("merge"));
    let twice = request(
        2,
        &holder("r1", step("s")),
        vec![need("merge"), need("merge")],
    );
    let found = check_own_hold(&t, &twice).unwrap();
    assert_eq!(found, satisfied(&[]));
    assert_eq!(
        t.enqueue(&found.remaining(&twice), Timestamp(0)),
        Err(QueueError::NamedTwice("merge".to_string()))
    );
}

#[test]
fn a_path_waiting_for_its_own_hold_is_a_one_path_cycle() {
    let mut t = table(&[("merge", PoolKind::Mutex, 1), ("db", PoolKind::Rw, 0)]);
    let node = holder("r1", step("s"));
    hold(&mut t, 1, &node, need("merge"));
    assert_eq!(
        cycle_paths(would_cycle(&[&t], &request(2, &node, vec![need("merge")])).unwrap_err()),
        vec![waits(&node, Some("merge"))]
    );
    hold(&mut t, 3, &node, need("db"));
    t.enqueue(&request(4, &node, vec![need("db")]), Timestamp(0))
        .unwrap();
    assert_eq!(
        would_cycle(&[&t], &request(5, &node, vec![need("db")])),
        Ok(()),
        "shared beside its own shared hold and its own older shared waiter"
    );
}

#[test]
fn a_full_slots_pool_waits_only_for_enough_holders_to_release() {
    let mut t = table(&[("cores", PoolKind::Slots, 3), ("merge", PoolKind::Mutex, 1)]);
    let a = holder("A", Level::Job);
    let b = holder("B", Level::Job);
    let c = holder("C", Level::Job);
    let d = holder("D", Level::Job);
    hold(&mut t, 1, &a, count("cores", 1));
    hold(&mut t, 2, &b, count("cores", 1));
    hold(&mut t, 3, &c, count("cores", 1));
    hold(&mut t, 4, &d, need("merge"));
    queue(&mut t, &request(5, &a, vec![need("merge")]));
    assert_eq!(
        would_cycle(&[&t], &request(6, &d, vec![count("cores", 1)])),
        Ok(()),
        "B or C releases a slot while A waits on D"
    );
    assert_eq!(
        cycle_paths(would_cycle(&[&t], &request(7, &d, vec![count("cores", 3)])).unwrap_err()),
        vec![waits(&d, Some("cores")), waits(&a, Some("merge"))],
        "all three slots need A's too"
    );
}

#[test]
fn a_slots_deadlock_is_refused_when_only_a_waiting_holder_could_free_a_slot() {
    let mut t = table(&[("cores", PoolKind::Slots, 2), ("merge", PoolKind::Mutex, 1)]);
    let a = holder("A", Level::Job);
    let b = holder("B", Level::Job);
    hold(&mut t, 1, &a, count("cores", 1));
    hold(&mut t, 2, &b, count("cores", 1));
    hold(&mut t, 3, &b, need("merge"));
    queue(&mut t, &request(4, &a, vec![need("merge")]));
    assert_eq!(
        cycle_paths(would_cycle(&[&t], &request(5, &b, vec![count("cores", 1)])).unwrap_err()),
        vec![waits(&b, Some("cores")), waits(&a, Some("merge"))],
        "B's own slot stays held and A waits on B"
    );

    let mut t = table(&[("cores", PoolKind::Slots, 3), ("merge", PoolKind::Mutex, 1)]);
    let c = holder("C", Level::Job);
    hold(&mut t, 1, &a, count("cores", 1));
    hold(&mut t, 2, &b, count("cores", 1));
    hold(&mut t, 3, &c, count("cores", 1));
    hold(&mut t, 4, &b, need("merge"));
    queue(&mut t, &request(5, &a, vec![need("merge")]));
    queue(&mut t, &request(6, &c, vec![need("merge")]));
    assert!(
        would_cycle(&[&t], &request(7, &b, vec![count("cores", 1)])).is_err(),
        "A and C both wait on B, so no slot frees"
    );
}

#[test]
fn try_now_reports_a_pool_named_twice_before_an_own_hold() {
    let mut t = table(&[("db", PoolKind::Rw, 0), ("merge", PoolKind::Mutex, 1)]);
    let job = holder("r1", Level::Job);
    hold(&mut t, 1, &job, mode("db", NeedMode::Shared));
    let r = request(
        2,
        &holder("r1", step("s")),
        vec![
            mode("db", NeedMode::Exclusive),
            need("merge"),
            need("merge"),
        ],
    );
    assert_eq!(
        try_now(&t, &r, &GuardReadings::default()),
        Err(Blocked::NamedTwice {
            pool: "merge".to_string()
        })
    );
}

#[test]
fn an_empty_table_queues_as_before() {
    let t = table(&[("merge", PoolKind::Mutex, 1)]);
    let r = request(1, &holder("r1", Level::Job), vec![need("merge")]);
    assert_eq!(judge(&[&t], &r), Ok(satisfied(&[])));
    assert_eq!(would_cycle(&[], &r), Ok(()));
}
