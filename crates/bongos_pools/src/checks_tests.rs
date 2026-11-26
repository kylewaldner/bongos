//! Feature 47's seam over a real broker: what a check run sees when its parts ask for pools.

use std::time::Duration;

use bongos_checks::{CheckPools as _, Holder, PoolLease, PoolRequest, PoolWait};
use bongos_pools_model::{GuardCheck, PoolKind};
use futures::FutureExt as _;
use gpui::TestAppContext;

use crate::PoolsForChecks;
use crate::broker_tests::{GIB, POLL, World, guarded, pool};
use crate::store_test_support::host;

fn request(part: u32, needs: &[toml::Value]) -> PoolRequest {
    PoolRequest {
        project_id: "a".to_string(),
        needs: needs.to_vec(),
        holder: Holder {
            task_id: "T1".to_string(),
            check: "build".to_string(),
            part,
            run: None,
            host_pid: 1,
        },
    }
}

fn name(pool: &str) -> toml::Value {
    toml::Value::String(pool.to_string())
}

fn table(text: &str) -> toml::Value {
    toml::Value::Table(text.parse().unwrap())
}

/// What the wait resolved to, if it has.
fn ready(wait: &mut PoolWait) -> Option<Result<PoolLease, String>> {
    wait.ready().now_or_never()
}

fn granted(wait: &mut PoolWait) -> PoolLease {
    match ready(wait) {
        Some(Ok(lease)) => lease,
        other => panic!("expected a lease, got {other:?}"),
    }
}

fn checks_over(
    world: &World,
    cx: &mut TestAppContext,
    pools: Vec<crate::PoolConfig>,
) -> (PoolsForChecks, crate::broker_tests::Project) {
    let project = world.project(cx, "a", host(1), pools, vec![]);
    let checks = PoolsForChecks::default();
    checks.insert(project.broker.clone());
    checks.set_task_goals(|task| (task == "T1").then(|| "Invoice export".to_string()));
    (checks, project)
}

#[gpui::test]
fn a_slots_pool_of_two_runs_two_checks_and_shows_the_third_waiting(cx: &mut TestAppContext) {
    let world = World::new();
    let (checks, _project) = checks_over(&world, cx, vec![pool("compile", PoolKind::Slots, 2)]);
    let mut first = checks.acquire(request(1, &[name("compile")]));
    let mut second = checks.acquire(request(2, &[name("compile")]));
    let mut third = checks.acquire(request(3, &[name("compile")]));
    cx.run_until_parked();
    let _first = granted(&mut first);
    let _second = granted(&mut second);
    assert!(ready(&mut third).is_none());
    let status = third.status();
    assert_eq!(status.pool, "compile");
    assert_eq!(status.position, 1);
    assert_eq!(
        status.holders,
        vec![
            "check build of Invoice export, part 1".to_string(),
            "check build of Invoice export, part 2".to_string(),
        ]
    );
    assert_eq!(status.blocked.as_deref(), Some("compile is full"));
}

#[gpui::test]
fn a_killed_bound_part_process_lets_the_next_proceed_within_one_poll(cx: &mut TestAppContext) {
    let world = World::new();
    let (checks, _project) = checks_over(&world, cx, vec![pool("compile", PoolKind::Slots, 1)]);
    let mut first = checks.acquire(request(1, &[name("compile")]));
    cx.run_until_parked();
    let first = granted(&mut first);
    first.bind_process(500, 1_500);
    let mut second = checks.acquire(request(2, &[name("compile")]));
    cx.run_until_parked();
    assert!(ready(&mut second).is_none());

    world.live.kill(500);
    world.advance(cx, POLL);
    let _second = granted(&mut second);
    drop(first);
    cx.run_until_parked();
}

#[gpui::test]
fn a_failing_guard_keeps_a_check_waiting_with_figure_and_threshold(cx: &mut TestAppContext) {
    let world = World::new();
    let (checks, project) = checks_over(
        &world,
        cx,
        vec![guarded(
            "ram",
            PoolKind::Guard,
            GuardCheck::FreeMemoryGb(12.0),
        )],
    );
    *project.readers.memory.lock().unwrap() = Some(9 * GIB);
    let mut wait = checks.acquire(request(1, &[name("ram")]));
    cx.run_until_parked();
    assert!(ready(&mut wait).is_none());
    assert_eq!(
        wait.status().blocked.as_deref(),
        Some("guard on ram failed (9.0 GB, needs 12 GB)")
    );

    *project.readers.memory.lock().unwrap() = Some(13 * GIB);
    world.advance(cx, Duration::from_secs(5));
    let _lease = granted(&mut wait);
}

#[gpui::test]
fn an_undeclared_pool_or_a_bad_need_is_no_verdict(cx: &mut TestAppContext) {
    let world = World::new();
    let (checks, _project) = checks_over(&world, cx, vec![pool("db", PoolKind::Mutex, 1)]);
    let mut undeclared = checks.acquire(request(1, &[name("nope")]));
    let mut bad_mode = checks.acquire(request(1, &[table("pool = 'db'\nmode = 'exclusive'")]));
    let mut unreadable = checks.acquire(request(1, &[table("pool = 'db'\nmode = 'sometimes'")]));
    cx.run_until_parked();
    let text = |result: Option<Result<PoolLease, String>>| result.unwrap().unwrap_err();
    assert_eq!(
        text(ready(&mut undeclared)),
        "pool `nope` is not declared in pools/pools.toml"
    );
    assert_eq!(
        text(ready(&mut bad_mode)),
        "pool `db` is not declared in pools/pools.toml"
    );
    assert_eq!(
        text(ready(&mut unreadable)),
        "pool `db` is not declared in pools/pools.toml"
    );
    assert!(
        checks
            .try_acquire_now(request(2, &[name("nope")]))
            .is_none()
    );
}

#[gpui::test]
fn a_parallel_part_never_jumps_a_queued_waiter(cx: &mut TestAppContext) {
    let world = World::new();
    let (checks, _project) = checks_over(&world, cx, vec![pool("compile", PoolKind::Slots, 2)]);
    let mut first = checks.acquire(request(1, &[name("compile")]));
    cx.run_until_parked();
    let first = granted(&mut first);
    let two = table("pool = 'compile'\ncount = 2");
    let mut other_run = checks.acquire(request(4, &[two]));
    cx.run_until_parked();
    assert!(ready(&mut other_run).is_none());
    // One slot is free, but a request waits in line for the pool.
    assert!(
        checks
            .try_acquire_now(request(2, &[name("compile")]))
            .is_none()
    );
    drop(first);
    cx.run_until_parked();
    let other_run = granted(&mut other_run);
    assert!(
        checks
            .try_acquire_now(request(3, &[name("compile")]))
            .is_none(),
        "both slots are held"
    );
    drop(other_run);
    cx.run_until_parked();
    let lease = checks
        .try_acquire_now(request(3, &[name("compile")]))
        .expect("free and nobody waits");
    lease.bind_process(42, 1_042);
}
