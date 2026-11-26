use bongos_pools_model::{
    GuardCheck, GuardReading, GuardValue, PoolKind, PoolScope, PoolTable, Timestamp, parse_pools,
};
use bongos_project_repo::{StateExporters, StateStore};
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::SettingsStore;

use crate::store_test_support::{
    Liveness, add_pool, context, enqueue, holder, holds, host, location, request, waiters, writer,
};
use crate::{
    DeclDefaults, POOLS_NAMESPACE, PoolConfig, PoolStore as _, ProjectPoolsReader, ProjectStore,
    read_project_table,
};

#[test]
fn an_empty_namespace_reads_as_an_empty_table() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let writer = writer(&location);
    assert_eq!(
        writer.read(read_project_table).unwrap(),
        (PoolTable::default(), 0)
    );
    let live = Liveness::new();
    let (store, pruned) = ProjectStore::open(writer, context(host(1), &live)).unwrap();
    assert!(pruned.is_empty());
    assert_eq!(store.read().unwrap(), PoolTable::default());
    assert_eq!(store.generation(), 1, "opening is one transaction");
    let reader = ProjectPoolsReader::open(&location).unwrap();
    assert_eq!(reader.read().unwrap(), (PoolTable::default(), 1));
}

#[test]
fn a_transaction_prunes_applies_grants_and_bumps_the_generation() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let live = Liveness::new();
    let (store, _) = ProjectStore::open(writer(&location), context(host(1), &live)).unwrap();
    let done = store
        .try_transact_full(|table| {
            add_pool(table, "compile", PoolKind::Slots, 1);
            enqueue(
                table,
                &request(1, holder("p", "a", host(1)), false, &[("compile", 1)]),
            );
            enqueue(
                table,
                &request(2, holder("p", "b", host(1)), false, &[("compile", 1)]),
            );
            Ok("changed")
        })
        .unwrap();
    assert_eq!(done.value, "changed");
    assert_eq!(done.generation, 2);
    assert_eq!(done.grants.len(), 1);
    let reader = ProjectPoolsReader::open(&location).unwrap();
    let (table, generation) = reader.read().unwrap();
    assert_eq!(generation, 2);
    assert_eq!(holds(&table, "compile"), vec![1]);
    assert_eq!(waiters(&table, "compile"), vec![2]);
    assert_eq!(table.next_ticket, 2);
    assert_eq!(table, store.read().unwrap());

    // The hold's holder dies: the next transaction prunes it before its change and grants the
    // waiter behind it.
    live.kill(1);
    let done = store
        .try_transact_full(|table| {
            assert!(table.pools["compile"].holds.is_empty());
            Ok(())
        })
        .unwrap();
    assert_eq!(done.pruned.holds.len(), 1);
    assert_eq!(done.pruned.waiters.len(), 1);
    assert!(done.grants.is_empty(), "the waiter's host is dead too");
}

#[test]
fn an_error_in_the_change_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let live = Liveness::new();
    let (store, _) = ProjectStore::open(writer(&location), context(host(1), &live)).unwrap();
    let error = store
        .try_transact_full::<()>(|table| {
            add_pool(table, "compile", PoolKind::Slots, 1);
            anyhow::bail!("refused")
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "refused");
    assert_eq!(store.read().unwrap(), PoolTable::default());
    assert_eq!(store.generation(), 1);
}

#[test]
fn a_reopened_store_has_no_hold_of_its_gone_writer() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let live = Liveness::new();
    {
        let (store, _) = ProjectStore::open(writer(&location), context(host(1), &live)).unwrap();
        store
            .transact(|table| {
                add_pool(table, "compile", PoolKind::Slots, 1);
                enqueue(
                    table,
                    &request(1, holder("p", "a", host(1)), false, &[("compile", 1)]),
                );
                enqueue(
                    table,
                    &request(2, holder("p", "b", host(1)), false, &[("compile", 1)]),
                );
            })
            .unwrap();
        assert_eq!(holds(&store.read().unwrap(), "compile"), vec![1]);
    }
    // The first writer's process is gone; a new writer opens the project.
    live.kill(1);
    let (store, pruned) = ProjectStore::open(writer(&location), context(host(2), &live)).unwrap();
    assert_eq!(pruned.holds.len(), 1);
    assert_eq!(pruned.waiters.len(), 1);
    let table = store.read().unwrap();
    assert!(holds(&table, "compile").is_empty());
    assert!(waiters(&table, "compile").is_empty());
    let (table, _) = ProjectPoolsReader::open(&location).unwrap().read().unwrap();
    assert!(holds(&table, "compile").is_empty());
}

#[gpui::test]
fn the_pools_namespace_is_not_in_a_snapshot(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        crate::init(cx);
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.pools.get_or_insert_default().enabled = Some(true);
            });
        });
    });
    cx.update(|cx| assert!(crate::is_registered(cx)));
    // A snapshot holds the tasks, the runs and one entry per registered exporter.
    assert!(
        StateExporters::global()
            .all()
            .iter()
            .all(|exporter| exporter.namespace() != POOLS_NAMESPACE)
    );
    // And the rows are where a reader of the store finds them, outside any exported namespace.
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let live = Liveness::new();
    let (store, _) = ProjectStore::open(writer(&location), context(host(1), &live)).unwrap();
    store
        .transact(|table| add_pool(table, "compile", PoolKind::Slots, 1))
        .unwrap();
    let rows = StateStore::open_reader(&location)
        .unwrap()
        .read(|read| read.kv(POOLS_NAMESPACE).scan(""))
        .unwrap();
    let keys: Vec<&str> = rows.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, vec!["pool/compile", "table"]);
}

const POOLS: &str = r#"schema = 1

[[pool]]
name = "compile"
kind = "slots"
size = 2
guard = { free_memory_gb = 4 }

[[pool]]
name = "jobs"
kind = "rw"

[[pool]]
name = "gpu"
kind = "slots"
size = 1
scope = "machine"
settle_seconds = 9
"#;

#[test]
fn a_declaration_becomes_a_pool_with_layered_size_and_settle_time() {
    let decls = parse_pools(POOLS).unwrap().decls;
    let defaults = DeclDefaults { settle_seconds: 5 };
    let project = PoolConfig::of_scope(&decls, PoolScope::Project, |_| None, defaults);
    assert_eq!(
        project
            .iter()
            .map(|config| config.name.as_str())
            .collect::<Vec<_>>(),
        vec!["compile", "jobs"]
    );
    let compile = &project[0];
    assert_eq!(compile.size, 2, "the declaration's size");
    assert_eq!(
        compile.settle_seconds, 5,
        "the setting, since the declaration has none"
    );
    assert_eq!(compile.guard, Some(GuardCheck::FreeMemoryGb(4.0)));
    let machine = PoolConfig::of_scope(&decls, PoolScope::Machine, |_| Some(3), defaults);
    assert_eq!(machine[0].size, 3, "the profile's size wins");
    assert_eq!(
        machine[0].settle_seconds, 9,
        "the declaration's settle time"
    );

    let state = compile.state();
    let guard = state.guard.as_ref().unwrap();
    assert_eq!(guard.settle_seconds, 5);
    assert_eq!(guard.reading, None);
}

#[test]
fn declaring_again_keeps_holds_and_the_guard_reading_and_refuses_a_kind_change_in_use() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let live = Liveness::new();
    let (store, _) = ProjectStore::open(writer(&location), context(host(1), &live)).unwrap();
    let decls = parse_pools(POOLS).unwrap().decls;
    let configs = PoolConfig::of_scope(
        &decls,
        PoolScope::Project,
        |_| None,
        DeclDefaults { settle_seconds: 5 },
    );
    store.declare(&configs).unwrap();
    let reading = GuardReading {
        value: GuardValue::FreeMemoryGb(16.0),
        at: Timestamp(42),
    };
    store
        .transact(|table| {
            table
                .pools
                .get_mut("compile")
                .unwrap()
                .guard
                .as_mut()
                .unwrap()
                .reading = Some(reading);
            enqueue(
                table,
                &request(1, holder("p", "a", host(1)), false, &[("compile", 1)]),
            );
            enqueue(
                table,
                &request(2, holder("p", "b", host(1)), false, &[("jobs", 1)]),
            );
        })
        .unwrap();

    let mut changed = configs;
    changed[0].size = 4;
    changed[1].kind = PoolKind::Mutex;
    let outcome = store.declare(&changed).unwrap().value;
    assert_eq!(outcome.refused.len(), 1);
    assert_eq!(outcome.refused[0].pool, "jobs");
    let table = store.read().unwrap();
    let compile = &table.pools["compile"];
    assert_eq!(compile.size, 4);
    assert_eq!(compile.holds.len(), 1);
    assert_eq!(compile.guard.as_ref().unwrap().reading, Some(reading));
    assert_eq!(table.pools["jobs"].kind, PoolKind::Rw);

    // Stored and read back exactly, guard reading included.
    let (stored, _) = ProjectPoolsReader::open(&location).unwrap().read().unwrap();
    assert_eq!(stored, table);
}
