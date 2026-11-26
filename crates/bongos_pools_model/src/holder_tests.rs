use std::collections::BTreeMap;

use crate::decl::PoolKind;
use crate::grant::{GuardReadings, PoolState, PoolTable, Request, RequestId, Timestamp, grant};
use crate::holder::{
    Holder, HolderLabel, HolderPath, HostRef, Level, OwnerKey, ProcRef, alive, prune,
};
use crate::needs::Need;

const HOST: HostRef = HostRef { pid: 10, start: 1 };
const DEAD_HOST: HostRef = HostRef { pid: 20, start: 2 };

/// A probe over a fixed process list: pid to start time.
fn probe(running: &[(u32, u64)]) -> impl Fn(u32, u64) -> bool {
    let running: BTreeMap<u32, u64> = running.iter().copied().collect();
    move |pid, start| running.get(&pid) == Some(&start)
}

fn request(id: u64, host: HostRef, process: Option<ProcRef>) -> Request {
    Request {
        id: RequestId(id),
        holder: Holder {
            project: "p".to_string(),
            path: HolderPath {
                owner: OwnerKey(format!("run-{id}")),
                level: Level::Step {
                    key: "build".to_string(),
                },
            },
            label: HolderLabel(format!("run {id}, step build")),
            host,
            process,
        },
        mixed: false,
        needs: vec![Need {
            pool: "jobs".to_string(),
            mode: None,
            count: None,
        }],
    }
}

/// A table where request 1 holds the only slot and request 2 waits.
fn held_table(process: Option<ProcRef>, waiter_host: HostRef) -> PoolTable {
    let mut table = PoolTable::default();
    table
        .pools
        .insert("jobs".to_string(), PoolState::new(PoolKind::Slots, 1));
    table
        .enqueue(&request(1, HOST, process), Timestamp(0))
        .unwrap();
    let grants = grant(&table, &GuardReadings::default(), Timestamp(0), &HOST);
    table.apply(&grants);
    table
        .enqueue(&request(2, waiter_host, None), Timestamp(1))
        .unwrap();
    table
}

#[test]
fn a_hold_with_a_process_lives_as_long_as_that_process() {
    let process = ProcRef {
        pid: 4123,
        start: 500,
    };
    let table = held_table(Some(process), HOST);
    let hold = &table.pools["jobs"].holds[0];
    assert!(alive(hold, &probe(&[(10, 1), (4123, 500)])));
    assert!(!alive(hold, &probe(&[(10, 1)])), "the process exited");
    assert!(
        alive(hold, &probe(&[(4123, 500)])),
        "the process outlives the host that asked"
    );
}

#[test]
fn prune_drops_a_hold_whose_process_start_time_differs() {
    let process = ProcRef {
        pid: 4123,
        start: 500,
    };
    let mut table = held_table(Some(process), HOST);
    let pruned = prune(&mut table, &probe(&[(10, 1), (4123, 501)]));
    assert_eq!(
        pruned.holds.len(),
        1,
        "pid 4123 was reused by another process"
    );
    assert_eq!(pruned.holds[0].0, "jobs");
    assert_eq!(pruned.holds[0].1.request, RequestId(1));
    assert!(pruned.waiters.is_empty());
    assert!(table.pools["jobs"].holds.is_empty());
    let grants = grant(&table, &GuardReadings::default(), Timestamp(2), &HOST);
    assert_eq!(grants.len(), 1, "the waiter gets the slot back");
}

#[test]
fn prune_drops_a_dead_hosts_holds_and_waiters() {
    let mut table = held_table(None, DEAD_HOST);
    let pruned = prune(&mut table, &probe(&[(10, 1)]));
    assert!(pruned.holds.is_empty(), "the holding host lives");
    assert_eq!(pruned.waiters.len(), 1);
    assert_eq!(pruned.waiters[0].1.request, RequestId(2));
    assert!(table.pools["jobs"].waiters.is_empty());

    let mut table = held_table(None, HOST);
    let pruned = prune(&mut table, &probe(&[(10, 2)]));
    assert_eq!(
        pruned.holds.len(),
        1,
        "the host restarted with the same pid"
    );
    assert_eq!(pruned.waiters.len(), 1);
    assert!(prune(&mut table, &probe(&[])).is_empty());
}
