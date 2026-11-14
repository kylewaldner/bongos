use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::anyhow;
use bongos_project_repo_model::TaskStateRecord;
use bongos_work_model::{HistoryEntry, Status, TodoState};
use futures::StreamExt as _;
use tempfile::TempDir;

use crate::test_support::{location, task_with_todos, writer};
use crate::{Location, OpenError, StateApplyHooks, StateEvent, StateStore};

fn branch(record: &Option<TaskStateRecord>) -> Option<String> {
    record.as_ref().and_then(|record| record.branch.clone())
}

fn with_branch(name: &str) -> TaskStateRecord {
    TaskStateRecord {
        branch: Some(name.to_string()),
        ..TaskStateRecord::default()
    }
}

fn history(event: &str) -> HistoryEntry {
    HistoryEntry {
        at: "2026-11-12T10:00:00Z".parse().unwrap(),
        by: "agent".to_string(),
        event: event.to_string(),
        detail: None,
        extra: Default::default(),
    }
}

#[test]
fn a_task_round_trips_with_its_todos_and_history() {
    let root = TempDir::new().unwrap();
    let writer = writer(&location(&root), &StateApplyHooks::new());
    let mut record = task_with_todos(&[("t1", TodoState::Done), ("t2", TodoState::Open)]);
    record.status = Status::Done;
    record.branch = Some("work/a".to_string());
    record.todos[0].started = Some("2026-11-12T09:00:00Z".parse().unwrap());
    record.history = vec![history("created")];

    writer
        .transact(|tx| {
            tx.set_task("a", record.clone())?;
            tx.append_history("a", history("claimed"))
        })
        .unwrap();

    let read = writer.read(|read| read.task("a")).unwrap().unwrap();
    let mut expected = record;
    expected.history.push(history("claimed"));
    assert_eq!(read, expected);
    assert_eq!(writer.read(|read| read.task("missing")).unwrap(), None);
}

#[test]
fn orphans_are_state_rows_of_unknown_tasks() {
    let root = TempDir::new().unwrap();
    let writer = writer(&location(&root), &StateApplyHooks::new());
    writer
        .transact(|tx| {
            tx.set_task("kept", TaskStateRecord::default())?;
            tx.set_task("gone", TaskStateRecord::default())?;
            tx.append_history("history-only", history("created"))
        })
        .unwrap();
    let known = BTreeSet::from(["kept".to_string()]);
    let orphans = writer.read(|read| read.orphans(&known)).unwrap();
    assert_eq!(
        orphans,
        vec!["gone".to_string(), "history-only".to_string()]
    );
}

#[test]
fn a_failed_transaction_changes_nothing() {
    let root = TempDir::new().unwrap();
    let writer = writer(&location(&root), &StateApplyHooks::new());
    let error = writer
        .transact(|tx| {
            tx.set_task("a", with_branch("work/a"))?;
            tx.kv("notes").put("k", toml::Value::Integer(1))?;
            Err::<(), _>(anyhow!("refused"))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "refused");
    writer
        .read(|read| {
            assert_eq!(read.task("a")?, None);
            assert_eq!(read.kv("notes").get("k")?, None);
            assert_eq!(read.generation()?, 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_second_writer_gets_held_by_with_the_pid() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    let _first = writer(&location, &StateApplyHooks::new());
    match StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()) {
        Err(OpenError::HeldBy { pid }) => assert_eq!(pid, Some(std::process::id())),
        Err(error) => panic!("expected HeldBy, got {error}"),
        Ok(_) => panic!("a second writer opened"),
    }
}

#[test]
fn dropping_the_writer_releases_the_lock() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    drop(writer(&location, &StateApplyHooks::new()));
    writer(&location, &StateApplyHooks::new());
}

const HOLDER_STATE_DIR: &str = "BONGOS_STATE_TEST_HOLDER_STATE_DIR";
/// Printed by the holder once it holds the writer; libtest prints it after the test name.
const HOLDER_MARKER: &str = "state-writer-held-by-child";
const HOLDER_TEST: &str = "state_tests::hold_the_writer_for_another_test";

/// Not a test of its own: `a_killed_holder_releases_the_lock` runs the test binary again with
/// only this test and the state directory in the child's environment, and this holds the writer
/// until the child is killed.
#[test]
fn hold_the_writer_for_another_test() {
    let Some(state) = std::env::var_os(HOLDER_STATE_DIR) else {
        return;
    };
    let state = PathBuf::from(state);
    let location = Location {
        id: "project-1".to_string(),
        folder: state.clone(),
        repository: state.clone(),
        working_copies: state.clone(),
        files: state.clone(),
        state,
        remote: None,
        converted_from: None,
    };
    let _writer = StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap();
    println!("{HOLDER_MARKER}");
    loop {
        std::thread::park();
    }
}

#[test]
#[allow(clippy::disallowed_methods)]
fn a_killed_holder_releases_the_lock() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([HOLDER_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(HOLDER_STATE_DIR, &location.state)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let holding = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains(HOLDER_MARKER));
    assert!(holding, "the holder exited before it held the writer");

    match StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()) {
        Err(OpenError::HeldBy { pid }) => assert_eq!(pid, Some(child.id())),
        Err(error) => panic!("expected HeldBy, got {error}"),
        Ok(_) => panic!("opened while the holder held the writer"),
    }

    child.kill().unwrap();
    child.wait().unwrap();
    writer(&location, &StateApplyHooks::new());
}

#[test]
fn a_reader_during_a_write_sees_the_old_state() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    let writer = writer(&location, &StateApplyHooks::new());
    writer
        .transact(|tx| {
            tx.set_task("a", with_branch("old"))?;
            tx.set_task("b", with_branch("old"))
        })
        .unwrap();
    let reader = StateStore::open_reader(&location).unwrap();

    writer
        .transact(|tx| {
            tx.set_task("a", with_branch("new"))?;
            let (a, b) = reader.read(|read| Ok((read.task("a")?, read.task("b")?)))?;
            assert_eq!(
                (branch(&a), branch(&b)),
                (Some("old".into()), Some("old".into()))
            );
            tx.set_task("b", with_branch("new"))
        })
        .unwrap();

    let (a, b) = reader
        .read(|read| Ok((read.task("a")?, read.task("b")?)))
        .unwrap();
    assert_eq!(
        (branch(&a), branch(&b)),
        (Some("new".into()), Some("new".into()))
    );
}

#[test]
fn concurrent_reads_never_see_half_a_transaction() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    let writer = writer(&location, &StateApplyHooks::new());
    writer
        .transact(|tx| {
            tx.set_task("a", with_branch("0"))?;
            tx.set_task("b", with_branch("0"))
        })
        .unwrap();
    let reader = StateStore::open_reader(&location).unwrap();

    let reading = std::thread::spawn(move || {
        for _ in 0..200 {
            let (a, b) = reader
                .read(|read| Ok((read.task("a")?, read.task("b")?)))
                .unwrap();
            assert_eq!(branch(&a), branch(&b));
        }
    });
    for n in 1..=200 {
        writer
            .transact(|tx| {
                tx.set_task("a", with_branch(&n.to_string()))?;
                tx.set_task("b", with_branch(&n.to_string()))
            })
            .unwrap();
    }
    reading.join().unwrap();
}

#[test]
fn a_reader_beside_a_writer_sees_committed_state_and_cannot_write() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    let writer = writer(&location, &StateApplyHooks::new());
    writer
        .transact(|tx| tx.kv("notes").put("k", toml::Value::String("v".into())))
        .unwrap();

    let reader = StateStore::open_reader(&location).unwrap();
    let value = reader.read(|read| read.kv("notes").get("k")).unwrap();
    assert_eq!(value, Some(toml::Value::String("v".into())));
    assert!(
        reader
            .raw_exec("INSERT INTO kv (namespace, key, value_toml) VALUES ('n', 'k', 'value = 1')")
            .is_err()
    );
    assert!(reader.raw_exec("DELETE FROM kv").is_err());

    writer
        .transact(|tx| tx.kv("notes").put("k", toml::Value::String("w".into())))
        .unwrap();
    let value = reader.read(|read| read.kv("notes").get("k")).unwrap();
    assert_eq!(value, Some(toml::Value::String("w".into())));
}

#[test]
fn a_reader_needs_an_existing_store() {
    let root = TempDir::new().unwrap();
    assert!(StateStore::open_reader(&location(&root)).is_err());
}

#[test]
fn generation_bumps_once_per_transaction() {
    let root = TempDir::new().unwrap();
    let writer = writer(&location(&root), &StateApplyHooks::new());
    let mut events = writer.subscribe();
    assert_eq!(writer.generation().unwrap(), 0);

    writer
        .transact(|tx| {
            tx.set_task("a", TaskStateRecord::default())?;
            tx.set_task("b", TaskStateRecord::default())?;
            tx.kv("one").put("x", toml::Value::Integer(1))?;
            tx.kv("two").put("y", toml::Value::Integer(2))
        })
        .unwrap();
    assert_eq!(writer.generation().unwrap(), 1);
    writer
        .transact(|tx| tx.append_history("a", history("claimed")))
        .unwrap();
    assert_eq!(writer.generation().unwrap(), 2);
    // A transaction that writes nothing commits nothing new.
    writer.transact(|tx| tx.task("a").map(drop)).unwrap();
    assert_eq!(writer.generation().unwrap(), 2);

    let first = futures::executor::block_on(events.next()).unwrap();
    assert_eq!(
        first,
        StateEvent::Changed {
            generation: 1,
            task_ids: vec!["a".into(), "b".into()],
            namespaces: vec!["one".into(), "two".into()],
        }
    );
    let second = futures::executor::block_on(events.next()).unwrap();
    assert_eq!(
        second,
        StateEvent::Changed {
            generation: 2,
            task_ids: vec!["a".into()],
            namespaces: vec![],
        }
    );
    assert!(
        events.try_recv().is_err(),
        "no event for an empty transaction"
    );
}

#[test]
fn generation_survives_reopening() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    let first = writer(&location, &StateApplyHooks::new());
    first
        .transact(|tx| tx.kv("n").put("k", toml::Value::Boolean(true)))
        .unwrap();
    drop(first);
    let second = writer(&location, &StateApplyHooks::new());
    assert_eq!(second.generation().unwrap(), 1);
}

#[test]
fn scan_returns_keys_in_order_and_only_under_the_prefix() {
    let root = TempDir::new().unwrap();
    let writer = writer(&location(&root), &StateApplyHooks::new());
    writer
        .transact(|tx| {
            let mut queue = tx.kv("queues");
            for key in ["b/2", "a/9", "b/10", "b/1", "c/1", "b"] {
                queue.put(key, toml::Value::String(key.to_string()))?;
            }
            tx.kv("other").put("b/3", toml::Value::Integer(3))?;
            let keys: Vec<String> = tx
                .kv("queues")
                .scan("b/")?
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            assert_eq!(keys, ["b/1", "b/10", "b/2"]);
            Ok(())
        })
        .unwrap();

    let scanned = writer.read(|read| read.kv("queues").scan("b/")).unwrap();
    assert_eq!(
        scanned,
        vec![
            ("b/1".to_string(), toml::Value::String("b/1".into())),
            ("b/10".to_string(), toml::Value::String("b/10".into())),
            ("b/2".to_string(), toml::Value::String("b/2".into())),
        ]
    );
    let all = writer.read(|read| read.kv("queues").scan("")).unwrap();
    assert_eq!(all.len(), 6);
    assert_eq!(all.first().unwrap().0, "a/9");
    // A `%` or `_` in the prefix is not a pattern.
    assert!(
        writer
            .read(|read| read.kv("queues").scan("b_"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn kv_delete_removes_one_key() {
    let root = TempDir::new().unwrap();
    let writer = writer(&location(&root), &StateApplyHooks::new());
    writer
        .transact(|tx| {
            let mut kv = tx.kv("n");
            kv.put("a", toml::Value::Integer(1))?;
            kv.put("b", toml::Value::Integer(2))?;
            kv.delete("a")
        })
        .unwrap();
    let all = writer.read(|read| read.kv("n").scan("")).unwrap();
    assert_eq!(all, vec![("b".to_string(), toml::Value::Integer(2))]);
}

#[test]
fn state_event_round_trips_through_json() {
    let event = StateEvent::Changed {
        generation: 7,
        task_ids: vec!["a".into()],
        namespaces: vec!["checks".into()],
    };
    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(serde_json::from_str::<StateEvent>(&json).unwrap(), event);
}

struct PanicHook;

impl crate::StateApplyHook for PanicHook {
    fn name(&self) -> &str {
        "panics"
    }

    fn apply(
        &self,
        _tx: &mut crate::StateTx,
        _change: &crate::StateApplyChange,
    ) -> anyhow::Result<()> {
        panic!("the hook panicked")
    }
}

#[test]
fn a_panic_in_the_closure_or_a_hook_rolls_the_transaction_back() {
    let root = TempDir::new().unwrap();
    let location = location(&root);
    let writer = writer(&location, &StateApplyHooks::new());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        writer.transact(|tx| -> anyhow::Result<()> {
            tx.set_task("a", with_branch("work/a"))?;
            panic!("the closure panicked")
        })
    }));
    assert!(result.is_err());
    assert_eq!(writer.read(|read| read.task("a")).unwrap(), None);
    // The connection is not left inside a transaction.
    writer
        .transact(|tx| tx.set_task("b", with_branch("work/b")))
        .unwrap();
    assert_eq!(writer.generation().unwrap(), 1);
    drop(writer);

    let hooks = StateApplyHooks::new();
    hooks.register(std::sync::Arc::new(PanicHook));
    let writer = crate::test_support::writer(&location, &hooks);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        writer.transact(|tx| tx.set_task("c", with_branch("work/c")))
    }));
    assert!(result.is_err());
    assert_eq!(writer.read(|read| read.task("c")).unwrap(), None);
    assert_eq!(writer.generation().unwrap(), 1);
}

#[test]
fn events_arrive_in_commit_order_from_many_threads() {
    let root = TempDir::new().unwrap();
    let writer = writer(&location(&root), &StateApplyHooks::new());
    let mut events = writer.subscribe();
    std::thread::scope(|scope| {
        for thread in 0..4 {
            let writer = &writer;
            scope.spawn(move || {
                for index in 0..25 {
                    let id = format!("t{thread}-{index}");
                    writer
                        .transact(|tx| tx.set_task(&id, with_branch("b")))
                        .unwrap();
                }
            });
        }
    });
    let mut generations = Vec::new();
    while let Ok(Some(StateEvent::Changed { generation, .. })) = events.try_next() {
        generations.push(generation);
    }
    assert_eq!(generations, (1..=100).collect::<Vec<u64>>());
}

#[cfg(unix)]
#[test]
fn held_by_never_reports_pid_zero() {
    use std::os::fd::AsRawFd as _;

    for contents in ["", "0\n", "not a pid\n"] {
        let root = TempDir::new().unwrap();
        let location = location(&root);
        let path = location
            .state
            .join(bongos_project_repo_model::layout::HOST_LOCK_FILE);
        std::fs::write(&path, contents).unwrap();
        let holder = std::fs::File::open(&path).unwrap();
        // SAFETY: the descriptor is open for the duration of the call.
        let locked = unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(locked, 0);
        match StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()) {
            Err(OpenError::HeldBy { pid }) => assert_eq!(pid, None, "{contents:?}"),
            Err(error) => panic!("expected HeldBy, got {error}"),
            Ok(_) => panic!("the lock is held"),
        }
        let error = OpenError::HeldBy { pid: None };
        assert_eq!(
            error.to_string(),
            "this project is open in another bongos process (pid unknown)"
        );
    }
}
