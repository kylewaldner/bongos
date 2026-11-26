use bongos_pools_model::PoolKind;
use bongos_project_repo::RELEASED_MESSAGE;

use crate::store_test_support::{
    Liveness, add_pool, context, enqueue, holder, holds, host, location, machine, request, waiters,
    writer,
};
use crate::{MACHINE_FILE, MachineStore, PoolStore as _, ProjectStore, transact_mixed};

struct Stores {
    _root: tempfile::TempDir,
    machine: MachineStore,
    project: ProjectStore,
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
        machine,
        project,
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
