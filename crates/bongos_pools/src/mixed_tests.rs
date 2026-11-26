use std::sync::Arc;
use std::sync::atomic::Ordering;

use bongos_pools_model::PoolKind;
use bongos_project_repo::RELEASED_MESSAGE;
use gpui::TestAppContext;

use crate::store_test_support::{
    Liveness, add_pool, context, enqueue, holder, holds, host, location, machine, request, waiters,
    writer,
};
use crate::{
    DeclDefaults, MACHINE_FILE, MachineHalfUnwritten, MachineStore, PoolConfig, PoolStore as _,
    ProjectRef, ProjectStore, transact_mixed, transact_mixed_in_background,
};

struct Stores {
    _root: tempfile::TempDir,
    machine: Arc<MachineStore>,
    project: Arc<ProjectStore>,
}

/// A machine store with `gpu` (slots 1) and a project store with `db` (mutex), both of host 1.
fn stores() -> Stores {
    let root = tempfile::tempdir().unwrap();
    let live = Liveness::new();
    let machine = machine(&root.path().join("pools"), host(1), &live);
    let location = location(root.path());
    let (project, _) = ProjectStore::open(writer(&location), context(host(1), &live)).unwrap();
    machine
        .transact(|table| add_pool(table, "gpu", PoolKind::Slots, 1))
        .unwrap();
    project
        .transact(|table| add_pool(table, "db", PoolKind::Mutex, 1))
        .unwrap();
    Stores {
        _root: root,
        machine: Arc::new(machine),
        project: Arc::new(project),
    }
}

fn machine_bytes(stores: &Stores) -> Vec<u8> {
    std::fs::read(stores.machine.dir().join(MACHINE_FILE)).unwrap()
}

#[test]
fn a_mixed_request_converts_both_halves_together() {
    let stores = stores();
    let job = holder("p", "job", host(1));
    let done = transact_mixed(&stores.machine, &stores.project, |machine, project| {
        enqueue(machine, &request(1, job.clone(), true, &[("gpu", 1)]));
        enqueue(project, &request(1, job, true, &[("db", 1)]));
        Ok(())
    })
    .unwrap();
    assert_eq!(done.machine_grants.len(), 1);
    assert_eq!(done.project_grants.len(), 1);
    assert_eq!(done.machine_generation, 2);
    assert_eq!(done.project_generation, 3);
    assert_eq!(holds(&stores.machine.read().unwrap(), "gpu"), vec![1]);
    assert_eq!(holds(&stores.project.read().unwrap(), "db"), vec![1]);
}

#[test]
fn plain_transactions_never_convert_a_mixed_waiter() {
    let stores = stores();
    // Queued, but granted by nobody until a mixed transaction judges both halves.
    stores
        .machine
        .transact(|table| {
            enqueue(
                table,
                &request(1, holder("p", "job", host(1)), true, &[("gpu", 1)]),
            );
            enqueue(
                table,
                &request(2, holder("p", "late", host(1)), false, &[("gpu", 1)]),
            );
        })
        .unwrap();
    stores
        .project
        .transact(|table| {
            enqueue(
                table,
                &request(1, holder("p", "job", host(1)), true, &[("db", 1)]),
            );
        })
        .unwrap();
    let machine = stores.machine.read().unwrap();
    assert!(holds(&machine, "gpu").is_empty());
    assert_eq!(
        waiters(&machine, "gpu"),
        vec![1, 2],
        "the later waiter does not pass it"
    );
    assert!(holds(&stores.project.read().unwrap(), "db").is_empty());

    let done = transact_mixed(&stores.machine, &stores.project, |_, _| Ok(())).unwrap();
    assert_eq!(done.machine_grants.len(), 1);
    assert_eq!(done.project_grants.len(), 1);
    let machine = stores.machine.read().unwrap();
    assert_eq!(holds(&machine, "gpu"), vec![1]);
    assert_eq!(waiters(&machine, "gpu"), vec![2]);
}

#[test]
fn a_half_blocked_in_one_store_holds_back_the_other() {
    let stores = stores();
    // `db` is held by another job of the project.
    stores
        .project
        .transact(|table| {
            enqueue(
                table,
                &request(9, holder("p", "other", host(1)), false, &[("db", 1)]),
            );
        })
        .unwrap();
    let done = transact_mixed(&stores.machine, &stores.project, |machine, project| {
        enqueue(
            machine,
            &request(1, holder("p", "job", host(1)), true, &[("gpu", 1)]),
        );
        enqueue(
            project,
            &request(1, holder("p", "job", host(1)), true, &[("db", 1)]),
        );
        enqueue(
            machine,
            &request(2, holder("p", "late", host(1)), false, &[("gpu", 1)]),
        );
        Ok(())
    })
    .unwrap();
    assert!(
        done.machine_grants.is_empty(),
        "the free gpu is not taken alone"
    );
    assert!(done.project_grants.is_empty());
    let machine = stores.machine.read().unwrap();
    assert!(holds(&machine, "gpu").is_empty());
    assert_eq!(
        waiters(&machine, "gpu"),
        vec![1, 2],
        "nobody passes the mixed waiter"
    );

    // The other job lets `db` go: both halves are converted in one mixed transaction.
    let done = transact_mixed(&stores.machine, &stores.project, |_, project| {
        let ticket = project.pools["db"].holds[0].ticket;
        project.release("db", ticket).unwrap();
        Ok(())
    })
    .unwrap();
    assert_eq!(done.machine_grants.len(), 1);
    assert_eq!(done.project_grants.len(), 1);
    assert_eq!(holds(&stores.machine.read().unwrap(), "gpu"), vec![1]);
    assert_eq!(holds(&stores.project.read().unwrap(), "db"), vec![1]);
}

#[test]
fn a_failing_project_transaction_inside_the_lock_leaves_machine_toml_unwritten() {
    let stores = stores();
    let before = machine_bytes(&stores);
    let project_before = stores.project.read().unwrap();

    // The change fails after both tables were changed in memory.
    let error = transact_mixed::<()>(&stores.machine, &stores.project, |machine, project| {
        enqueue(
            machine,
            &request(1, holder("p", "job", host(1)), true, &[("gpu", 1)]),
        );
        enqueue(
            project,
            &request(1, holder("p", "job", host(1)), true, &[("db", 1)]),
        );
        anyhow::bail!("the broker refused")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "the broker refused");
    assert_eq!(machine_bytes(&stores), before);
    assert_eq!(stores.project.read().unwrap(), project_before);

    // The project's state transaction itself fails: this process gave its writer away.
    stores.project.writer().release();
    let error = transact_mixed(&stores.machine, &stores.project, |machine, _| {
        enqueue(
            machine,
            &request(1, holder("p", "job", host(1)), true, &[("gpu", 1)]),
        );
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains(RELEASED_MESSAGE), "{error}");
    assert_eq!(machine_bytes(&stores), before);

    // The lock was let go: the next machine transaction takes it at once.
    stores.machine.transact(|_| ()).unwrap();
    assert_eq!(stores.machine.generation(), 2);
}

/// Queues the mixed request 1 of `job`: `gpu` in the machine store, `db` in the project store,
/// each in a plain transaction of its own.
fn queue_both_halves(stores: &Stores) {
    stores
        .machine
        .transact(|table| {
            enqueue(
                table,
                &request(1, holder("p", "job", host(1)), true, &[("gpu", 1)]),
            );
        })
        .unwrap();
    stores
        .project
        .transact(|table| {
            enqueue(
                table,
                &request(1, holder("p", "job", host(1)), true, &[("db", 1)]),
            );
        })
        .unwrap();
}

#[test]
fn a_half_whose_other_half_is_not_queued_is_held_back() {
    let stores = stores();
    // The machine half is queued first; the project half is not queued yet.
    stores
        .machine
        .transact(|table| {
            enqueue(
                table,
                &request(1, holder("p", "job", host(1)), true, &[("gpu", 1)]),
            );
        })
        .unwrap();
    let done = transact_mixed(&stores.machine, &stores.project, |_, _| Ok(())).unwrap();
    assert!(done.machine_grants.is_empty(), "the gpu is not taken alone");
    assert!(holds(&stores.machine.read().unwrap(), "gpu").is_empty());
    assert_eq!(waiters(&stores.machine.read().unwrap(), "gpu"), vec![1]);

    // Once the project half is queued, both are converted together.
    stores
        .project
        .transact(|table| {
            enqueue(
                table,
                &request(1, holder("p", "job", host(1)), true, &[("db", 1)]),
            );
        })
        .unwrap();
    let done = transact_mixed(&stores.machine, &stores.project, |_, _| Ok(())).unwrap();
    assert_eq!(done.machine_grants.len(), 1);
    assert_eq!(done.project_grants.len(), 1);
}

#[test]
fn a_half_whose_other_half_was_removed_is_never_granted() {
    let stores = stores();
    queue_both_halves(&stores);
    // The project half is removed by hand.
    stores
        .project
        .transact(|table| table.pools.get_mut("db").unwrap().waiters.clear())
        .unwrap();
    let done = transact_mixed(&stores.machine, &stores.project, |_, _| Ok(())).unwrap();
    assert!(done.machine_grants.is_empty());
    assert!(done.project_grants.is_empty());
    let machine = stores.machine.read().unwrap();
    assert!(holds(&machine, "gpu").is_empty());
    assert_eq!(waiters(&machine, "gpu"), vec![1]);
}

#[test]
fn a_failed_machine_write_reports_the_project_grants_and_the_next_transaction_finishes() {
    let stores = stores();
    queue_both_halves(&stores);
    let before = machine_bytes(&stores);

    stores.machine.fail_next_write.store(true, Ordering::SeqCst);
    let error = transact_mixed(&stores.machine, &stores.project, |_, _| Ok(())).unwrap_err();
    let unwritten = error
        .downcast_ref::<MachineHalfUnwritten>()
        .expect("a typed error");
    let granted: Vec<u64> = unwritten
        .project_grants
        .iter()
        .map(|grant| grant.request.0)
        .collect();
    assert_eq!(granted, vec![1]);
    assert_eq!(unwritten.project_generation, stores.project.generation());
    assert!(error.to_string().contains("injected failure"), "{error}");
    assert_eq!(machine_bytes(&stores), before, "machine.toml is unchanged");
    assert_eq!(holds(&stores.project.read().unwrap(), "db"), vec![1]);

    // The lock was let go, and the project table holds the request: the next mixed transaction
    // converts the machine half alone.
    let done = transact_mixed(&stores.machine, &stores.project, |_, _| Ok(())).unwrap();
    let granted: Vec<u64> = done
        .machine_grants
        .iter()
        .map(|grant| grant.request.0)
        .collect();
    assert_eq!(granted, vec![1]);
    assert!(done.project_grants.is_empty());
    assert_eq!(holds(&stores.machine.read().unwrap(), "gpu"), vec![1]);
}

#[gpui::test]
async fn mixed_transactions_and_declarations_run_on_the_background_executor(
    cx: &mut TestAppContext,
) {
    let stores = stores();
    let executor = cx.executor();
    let job = holder("p", "job", host(1));
    let done = transact_mixed_in_background(
        &stores.machine,
        &stores.project,
        &executor,
        move |machine, project| {
            enqueue(machine, &request(1, job.clone(), true, &[("gpu", 1)]));
            enqueue(project, &request(1, job, true, &[("db", 1)]));
            Ok(7)
        },
    )
    .await
    .unwrap();
    assert_eq!(done.value, 7);
    assert_eq!(done.machine_grants.len(), 1);
    assert_eq!(done.project_grants.len(), 1);

    let project = ProjectRef {
        id: "a".to_string(),
        name: "Project a".to_string(),
    };
    let config = PoolConfig {
        name: "render".to_string(),
        kind: PoolKind::Slots,
        size: 2,
        guard: None,
        settle_seconds: DeclDefaults { settle_seconds: 5 }.settle_seconds,
    };
    let declared = stores
        .machine
        .declare_in_background(&executor, project.clone(), vec![config])
        .await
        .unwrap();
    assert!(declared.value.refused.is_empty());
    let file = stores.machine.read_file().unwrap();
    assert_eq!(file.table.pools["render"].size, 2);
    assert_eq!(file.meta["render"].set_by, Some(project));
}
