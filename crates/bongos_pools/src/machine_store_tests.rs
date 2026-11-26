use std::fs::OpenOptions;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bongos_pools_model::{PoolKind, PoolTable, ProcRef};

use crate::store_test_support::{
    Liveness, NOW, add_pool, enqueue, holder, holds, host, machine, patient_machine, request,
    waiters,
};
use crate::{
    DeclDefaults, LOCK_FILE, LockBusy, LockWait, MACHINE_FILE, MachineStore, PoolConfig,
    PoolStore as _, ProcessProbe, ProjectRef, StoreContext, this_host,
};

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn an_absent_machine_file_reads_as_an_empty_table_in_a_private_directory() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("state").join("pools");
    let live = Liveness::new();
    let store = machine(&dir, host(1), &live);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(store.read().unwrap(), PoolTable::default());
    assert_eq!(store.generation(), 0);
    assert!(!dir.join(MACHINE_FILE).exists(), "reading writes nothing");

    // A directory that exists with a wider mode is made private.
    let shared = root.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    machine(&shared, host(1), &live);
    assert_eq!(mode(&shared), 0o700);

    // The first transaction writes generation 1.
    let done = store
        .try_transact_full(|table| {
            add_pool(table, "gpu", PoolKind::Slots, 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(done.generation, 1);
    assert_eq!(store.generation(), 1);
    let text = std::fs::read_to_string(dir.join(MACHINE_FILE)).unwrap();
    let parsed: toml::Table = text.parse().unwrap();
    assert_eq!(parsed["schema"].as_integer(), Some(1));
    assert_eq!(parsed["generation"].as_integer(), Some(1));
}

#[test]
fn two_handles_in_two_threads_lose_no_update_over_1000_transactions() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let setup = machine(&dir, host(1), &live);
    setup
        .transact(|table| add_pool(table, "jobs", PoolKind::Slots, 2))
        .unwrap();
    let threads: Vec<_> = (0..2u64)
        .map(|thread| {
            let store = patient_machine(&dir, host(1), &live);
            std::thread::spawn(move || {
                let mut tickets = Vec::new();
                let mut held: Option<u64> = None;
                for round in 0..500u64 {
                    let id = thread * 1_000_000 + round;
                    let done = store
                        .try_transact_full(|table| {
                            // Each thread holds at most one slot of two: release the last one,
                            // then ask again; the grant converts it at once.
                            if let Some(ticket) = held {
                                table
                                    .release("jobs", ticket)
                                    .expect("my hold is still there");
                            }
                            let owner = format!("thread-{thread}");
                            let tickets = table.enqueue(
                                &request(id, holder("p", &owner, host(1)), false, &[("jobs", 1)]),
                                NOW,
                            );
                            Ok(tickets.unwrap()[0].1)
                        })
                        .unwrap();
                    assert_eq!(done.grants.len(), 1, "a free slot is granted at once");
                    held = Some(done.value);
                    tickets.push(done.value);
                }
                (tickets, held.unwrap())
            })
        })
        .collect();
    let results: Vec<(Vec<u64>, u64)> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let table = setup.read().unwrap();
    assert_eq!(setup.generation(), 1001);
    assert_eq!(table.next_ticket, 1000);
    let mut all: Vec<u64> = results
        .iter()
        .flat_map(|(tickets, _)| tickets.clone())
        .collect();
    all.sort_unstable();
    assert_eq!(
        all,
        (0..1000).collect::<Vec<u64>>(),
        "every ticket issued once"
    );
    let mut last_holds: Vec<u64> = table.pools["jobs"]
        .holds
        .iter()
        .map(|hold| hold.ticket)
        .collect();
    last_holds.sort_unstable();
    let mut expected: Vec<u64> = results.iter().map(|(_, last)| *last).collect();
    expected.sort_unstable();
    assert_eq!(
        last_holds, expected,
        "exactly each thread's last hold remains"
    );
    assert!(table.pools["jobs"].waiters.is_empty());
}

#[test]
fn a_reader_never_sees_a_torn_file() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let writer = patient_machine(&dir, host(1), &live);
    writer
        .transact(|table| add_pool(table, "jobs", PoolKind::Slots, 1000))
        .unwrap();
    let reader = machine(&dir, host(2), &live);
    let done = Arc::new(AtomicBool::new(false));
    let reading = {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut reads = 0u64;
            let mut last = 0;
            // At least one read, however soon the writer finishes.
            loop {
                let file = reader.read_file().expect("every read parses");
                assert!(file.generation >= last, "generations never go back");
                last = file.generation;
                reads += 1;
                if done.load(Ordering::SeqCst) {
                    break;
                }
            }
            reads
        })
    };
    for round in 0..300u64 {
        writer
            .transact(|table| {
                enqueue(
                    table,
                    &request(round, holder("p", "job", host(1)), false, &[("jobs", 1)]),
                );
            })
            .unwrap();
    }
    done.store(true, Ordering::SeqCst);
    assert!(reading.join().unwrap() > 0);
    assert_eq!(writer.read().unwrap().pools["jobs"].holds.len(), 300);
}

#[test]
fn a_busy_lock_is_retried_then_refused_naming_the_holder() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let holding = machine(&dir, host(1), &live);
    let pauses = Arc::new(AtomicU64::new(0));
    let waiting = MachineStore::open(
        dir.clone(),
        crate::store_test_support::context(host(2), &live),
        LockWait::with_pause(4, {
            let pauses = pauses.clone();
            move || {
                pauses.fetch_add(1, Ordering::SeqCst);
            }
        }),
    )
    .unwrap();

    let lock = holding.lock().unwrap();
    let error = waiting.transact(|_| ()).unwrap_err();
    let busy = error.downcast_ref::<LockBusy>().expect("a busy lock");
    assert_eq!(busy.pid, Some(std::process::id()));
    assert!(
        error
            .to_string()
            .contains(&format!("process {}", std::process::id())),
        "{error}"
    );
    assert_eq!(
        pauses.load(Ordering::SeqCst),
        3,
        "three pauses between four tries"
    );
    assert!(!dir.join(MACHINE_FILE).exists(), "nothing written");

    drop(lock);
    waiting.transact(|_| ()).unwrap();
    assert_eq!(waiting.generation(), 1);
}

#[test]
fn the_settings_wait_tries_every_50_ms_up_to_the_timeout() {
    let wait = LockWait::every(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_secs(10),
    );
    assert_eq!(wait.attempts(), 201);
    assert_eq!(LockWait::once().attempts(), 1);
}

#[test]
fn a_dead_process_lock_is_dropped_and_its_holds_pruned_before_the_first_grant() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let dying = machine(&dir, host(7), &live);
    dying
        .transact(|table| {
            add_pool(table, "gpu", PoolKind::Slots, 1);
            enqueue(
                table,
                &request(1, holder("a", "job-a", host(7)), false, &[("gpu", 1)]),
            );
        })
        .unwrap();
    assert_eq!(holds(&dying.read().unwrap(), "gpu"), vec![1]);

    // The dying process holds the lock when it dies: the kernel drops it with its last file.
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .unwrap();
    // SAFETY: flock only takes a lock on this open file.
    assert_eq!(
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    live.kill(7);
    drop(lock_file);

    let next = machine(&dir, host(8), &live);
    let done = next
        .try_transact_full(|table| {
            assert!(
                table.pools["gpu"].holds.is_empty(),
                "pruned before the change"
            );
            enqueue(
                table,
                &request(2, holder("b", "job-b", host(8)), false, &[("gpu", 1)]),
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(done.pruned.holds.len(), 1);
    assert_eq!(done.pruned.holds[0].1.request.0, 1);
    assert_eq!(
        done.grants.len(),
        1,
        "the freed slot goes to the next waiter at once"
    );
    assert_eq!(holds(&next.read().unwrap(), "gpu"), vec![2]);
}

#[test]
fn a_host_converts_only_its_own_waiters() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let one = machine(&dir, host(1), &live);
    let two = machine(&dir, host(2), &live);
    let three = machine(&dir, host(3), &live);
    three
        .transact(|table| {
            add_pool(table, "gpu", PoolKind::Slots, 1);
            enqueue(
                table,
                &request(3, holder("c", "c", host(3)), false, &[("gpu", 1)]),
            );
        })
        .unwrap();
    one.transact(|table| {
        enqueue(
            table,
            &request(1, holder("a", "a", host(1)), false, &[("gpu", 1)]),
        );
    })
    .unwrap();
    // Host 2 releases host 3's hold by hand: host 1's waiter is first and would fit, so host 2
    // converts nothing, its own waiter included, which is behind it.
    let done = two
        .try_transact_full(|table| {
            let released = table.pools["gpu"].holds[0].ticket;
            table.release("gpu", released).unwrap();
            enqueue(
                table,
                &request(2, holder("b", "b", host(2)), false, &[("gpu", 1)]),
            );
            Ok(())
        })
        .unwrap();
    assert!(done.grants.is_empty());
    let table = one.read().unwrap();
    assert!(holds(&table, "gpu").is_empty());
    assert_eq!(waiters(&table, "gpu"), vec![1, 2]);
    let done = one.try_transact_full(|_| Ok(())).unwrap();
    assert_eq!(done.grants.len(), 1);
    let table = one.read().unwrap();
    assert_eq!(holds(&table, "gpu"), vec![1]);
    assert_eq!(waiters(&table, "gpu"), vec![2]);
}

#[test]
fn the_process_probe_keeps_live_holders_and_drops_a_reused_pid() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let me = this_host().expect("this process has a start time");
    let store = MachineStore::open(
        dir,
        StoreContext::new(me, ProcessProbe, || NOW),
        LockWait::once(),
    )
    .unwrap();
    let mut alive = holder("a", "alive", me);
    alive.process = Some(ProcRef {
        pid: me.pid,
        start: me.start,
    });
    let mut reused = holder("a", "reused", me);
    reused.process = Some(ProcRef {
        pid: me.pid,
        start: me.start + 1,
    });
    store
        .transact(|table| {
            add_pool(table, "gpu", PoolKind::Slots, 2);
            enqueue(table, &request(1, alive, false, &[("gpu", 1)]));
            enqueue(table, &request(2, reused, false, &[("gpu", 1)]));
        })
        .unwrap();
    assert_eq!(holds(&store.read().unwrap(), "gpu"), vec![1, 2]);
    // The holds bound to a process live as long as it: the one whose start time differs is a
    // reused pid, so it is pruned at the next transaction.
    let done = store.try_transact_full(|_| Ok(())).unwrap();
    assert_eq!(done.pruned.holds.len(), 1);
    assert_eq!(holds(&store.read().unwrap(), "gpu"), vec![1]);
}

fn project(id: &str) -> ProjectRef {
    ProjectRef {
        id: id.to_string(),
        name: format!("Project {id}"),
    }
}

fn config(name: &str, kind: PoolKind, size: u32) -> PoolConfig {
    PoolConfig {
        name: name.to_string(),
        kind,
        size,
        guard: None,
        settle_seconds: DeclDefaults { settle_seconds: 5 }.settle_seconds,
        hold_while_waiting: false,
    }
}

#[test]
fn the_first_size_stands_another_warns_and_another_kind_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let store = machine(&dir, host(1), &live);

    let first = store
        .declare(&project("a"), &[config("gpu", PoolKind::Slots, 1)])
        .unwrap();
    assert!(first.value.warnings.is_empty() && first.value.refused.is_empty());

    let second = store
        .declare(&project("b"), &[config("gpu", PoolKind::Slots, 2)])
        .unwrap()
        .value;
    assert!(second.refused.is_empty());
    assert_eq!(second.warnings.len(), 1);
    assert_eq!(second.warnings[0].declared, 2);
    assert_eq!(second.warnings[0].stands, 1);
    assert_eq!(
        second.warnings[0].to_string(),
        "machine pool gpu: this project declares 2, project Project a set 1"
    );

    let third = store
        .declare(&project("c"), &[config("gpu", PoolKind::Rw, 1)])
        .unwrap()
        .value;
    assert!(third.warnings.is_empty());
    assert_eq!(third.refused.len(), 1);
    assert_eq!(third.refused[0].stands, PoolKind::Slots);
    assert_eq!(third.refused[0].set_by, Some(project("a")));

    let file = store.read_file().unwrap();
    assert_eq!(
        file.table.pools["gpu"].size, 1,
        "the first project's size stands"
    );
    let meta = &file.meta["gpu"];
    assert_eq!(meta.set_by, Some(project("a")));
    let declared: Vec<(&str, u32)> = meta
        .declared
        .iter()
        .map(|entry| (entry.project.id.as_str(), entry.size))
        .collect();
    assert_eq!(
        declared,
        vec![("a", 1), ("b", 2)],
        "the refused kind is not recorded"
    );

    // On disk: set_by and every declaration, with kind and size.
    let text = std::fs::read_to_string(dir.join(MACHINE_FILE)).unwrap();
    let parsed: toml::Table = text.parse().unwrap();
    let gpu = parsed["pools"]["gpu"].as_table().unwrap();
    assert_eq!(gpu["kind"].as_str(), Some("slots"));
    assert_eq!(gpu["set_by"]["id"].as_str(), Some("a"));
    assert_eq!(gpu["declared"].as_array().unwrap().len(), 2);
    assert_eq!(gpu["declared"][1]["size"].as_integer(), Some(2));

    // When the setting project stops declaring it, the next declaring project sets it.
    store.declare(&project("a"), &[]).unwrap();
    let file = store.read_file().unwrap();
    assert_eq!(file.meta["gpu"].set_by, Some(project("b")));
    assert_eq!(file.table.pools["gpu"].size, 2);
}

#[test]
fn the_holder_pid_is_cleared_when_the_lock_is_released() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let store = machine(&dir, host(1), &live);
    let lock = store.lock().unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap(),
        std::process::id().to_string()
    );
    drop(lock);
    assert_eq!(std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap(), "");
}

#[test]
fn the_setting_project_may_change_its_pools_kind_while_nobody_uses_it() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let store = machine(&dir, host(1), &live);
    store
        .declare(&project("a"), &[config("gpu", PoolKind::Slots, 1)])
        .unwrap();
    store
        .declare(&project("b"), &[config("gpu", PoolKind::Slots, 2)])
        .unwrap();

    // In use, its kind stands, even for the project that set it.
    store
        .transact(|table| {
            enqueue(
                table,
                &request(1, holder("a", "job", host(1)), false, &[("gpu", 1)]),
            );
        })
        .unwrap();
    let refused = store
        .declare(&project("a"), &[config("gpu", PoolKind::Rw, 1)])
        .unwrap()
        .value;
    assert_eq!(refused.refused.len(), 1);
    assert_eq!(refused.refused[0].set_by, None);
    let file = store.read_file().unwrap();
    assert_eq!(file.table.pools["gpu"].kind, PoolKind::Slots);
    assert_eq!(file.meta["gpu"].set_by, Some(project("a")));

    // Once nobody holds it, the setting project changes its kind.
    store
        .transact(|table| {
            let ticket = table.pools["gpu"].holds[0].ticket;
            table.release("gpu", ticket).unwrap();
        })
        .unwrap();
    let changed = store
        .declare(&project("a"), &[config("gpu", PoolKind::Rw, 1)])
        .unwrap()
        .value;
    assert!(changed.refused.is_empty() && changed.warnings.is_empty());
    let file = store.read_file().unwrap();
    assert_eq!(file.table.pools["gpu"].kind, PoolKind::Rw);
    assert_eq!(file.meta["gpu"].set_by, Some(project("a")));
    let declared: Vec<&str> = file.meta["gpu"]
        .declared
        .iter()
        .map(|entry| entry.project.id.as_str())
        .collect();
    assert_eq!(
        declared,
        vec!["a"],
        "the declarations of the old kind are dropped"
    );
}

#[test]
fn a_pool_nobody_declares_is_set_afresh_by_the_next_project() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pools");
    let live = Liveness::new();
    let store = machine(&dir, host(1), &live);
    store
        .declare(&project("a"), &[config("gpu", PoolKind::Slots, 1)])
        .unwrap();
    store.declare(&project("a"), &[]).unwrap();
    assert_eq!(store.read_file().unwrap().meta["gpu"].set_by, None);

    let fresh = store
        .declare(&project("c"), &[config("gpu", PoolKind::Rw, 1)])
        .unwrap()
        .value;
    assert!(fresh.refused.is_empty(), "not locked to its old kind");
    let file = store.read_file().unwrap();
    assert_eq!(file.table.pools["gpu"].kind, PoolKind::Rw);
    assert_eq!(file.meta["gpu"].set_by, Some(project("c")));
}
