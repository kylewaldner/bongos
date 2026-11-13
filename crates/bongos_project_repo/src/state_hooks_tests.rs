use std::sync::Arc;

use anyhow::{Result, anyhow};
use bongos_project_repo_model::{Author, TaskStateRecord};
use bongos_work_model::{Status, TodoState};
use futures::StreamExt as _;
use gpui::TestAppContext;
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::test_support::{location, task_with_todos, todo, writer};
use crate::{
    LocalSink, StateApplyChange, StateApplyHook, StateApplyHooks, StateEvent, StateOp, StateSink,
    StateStore, StateTx,
};

/// What a hook saw of one touched task.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    id: String,
    before: Option<TaskStateRecord>,
    after: Option<TaskStateRecord>,
}

#[derive(Clone, Copy)]
enum Behavior {
    /// Writes `done:<id>` in namespace `checks` for each touched task.
    WriteKv,
    Fail,
    WriteTask,
}

struct FakeHook {
    name: &'static str,
    behavior: Behavior,
    calls: Mutex<Vec<Vec<Seen>>>,
}

impl FakeHook {
    fn new(name: &'static str, behavior: Behavior) -> Arc<Self> {
        Arc::new(Self {
            name,
            behavior,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Vec<Seen>> {
        self.calls.lock().clone()
    }
}

impl StateApplyHook for FakeHook {
    fn name(&self) -> &str {
        self.name
    }

    fn apply(&self, tx: &mut StateTx, change: &StateApplyChange) -> Result<()> {
        self.calls.lock().push(
            change
                .touched
                .iter()
                .map(|touch| Seen {
                    id: touch.id.clone(),
                    before: touch.before.clone(),
                    after: touch.after.clone(),
                })
                .collect(),
        );
        match self.behavior {
            Behavior::WriteKv => {
                for touch in change.touched {
                    let status = touch.after.as_ref().map(|after| after.status);
                    tx.kv("checks").put(
                        &format!("done:{}", touch.id),
                        toml::Value::Boolean(status == Some(Status::Done)),
                    )?;
                }
                Ok(())
            }
            Behavior::Fail => Err(anyhow!("no evidence")),
            Behavior::WriteTask => tx.set_task("other", TaskStateRecord::default()),
        }
    }
}

fn hooks_with(hooks: &[Arc<FakeHook>]) -> StateApplyHooks {
    let registry = StateApplyHooks::new();
    for hook in hooks {
        registry.register(hook.clone());
    }
    registry
}

fn done_set_op(task: &str) -> StateOp {
    StateOp::Set {
        task: task.to_string(),
        field: "status".to_string(),
        expected: Some(toml::Value::String("open".to_string())),
        value: Some(toml::Value::String("done".to_string())),
    }
}

#[gpui::test]
async fn a_hook_kv_write_commits_with_a_local_sink_write(cx: &mut TestAppContext) {
    let root = TempDir::new().unwrap();
    let hook = FakeHook::new("checks", Behavior::WriteKv);
    let writer = writer(&location(&root), &hooks_with(&[hook.clone()]));
    writer
        .transact(|tx| tx.set_task("a", TaskStateRecord::default()))
        .unwrap();
    hook.calls.lock().clear();

    let sink = LocalSink::new(writer.clone(), cx.executor());
    sink.apply(vec![done_set_op("a")], Author::agent("runner"))
        .await
        .unwrap();

    let calls = hook.calls();
    assert_eq!(calls.len(), 1);
    let seen = &calls[0];
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, "a");
    assert_eq!(seen[0].before.as_ref().unwrap().status, Status::Open);
    assert_eq!(seen[0].after.as_ref().unwrap().status, Status::Done);
    writer
        .read(|read| {
            assert_eq!(read.task("a")?.unwrap().status, Status::Done);
            assert_eq!(
                read.kv("checks").get("done:a")?,
                Some(toml::Value::Boolean(true))
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_todos_only_write_gives_a_touch_whose_todo_states_differ() {
    let root = TempDir::new().unwrap();
    let hook = FakeHook::new("checks", Behavior::WriteKv);
    let writer = writer(&location(&root), &hooks_with(&[hook.clone()]));
    writer
        .transact(|tx| {
            tx.set_task(
                "a",
                task_with_todos(&[("t1", TodoState::Done), ("t2", TodoState::Open)]),
            )
        })
        .unwrap();
    hook.calls.lock().clear();

    // `todo: skip` on the last open todo: a write to the todos table only.
    writer
        .transact(|tx| tx.set_todo("a", todo("t2", TodoState::Skipped)))
        .unwrap();

    let calls = hook.calls();
    assert_eq!(calls.len(), 1);
    let seen = &calls[0][0];
    let states = |record: &Option<TaskStateRecord>| -> Vec<TodoState> {
        record
            .as_ref()
            .unwrap()
            .todos
            .iter()
            .map(|todo| todo.state)
            .collect()
    };
    assert_eq!(states(&seen.before), [TodoState::Done, TodoState::Open]);
    assert_eq!(states(&seen.after), [TodoState::Done, TodoState::Skipped]);
    assert_eq!(seen.before.as_ref().unwrap().status, Status::Open);
}

#[test]
fn a_hook_namespace_is_in_the_one_event_with_one_generation_bump() {
    let root = TempDir::new().unwrap();
    let hook = FakeHook::new("checks", Behavior::WriteKv);
    let writer = writer(&location(&root), &hooks_with(&[hook]));
    let mut events = writer.subscribe();

    writer
        .transact(|tx| {
            tx.set_task("a", TaskStateRecord::default())?;
            tx.kv("notes").put("k", toml::Value::Integer(1))
        })
        .unwrap();

    assert_eq!(writer.generation().unwrap(), 1);
    let event = futures::executor::block_on(events.next()).unwrap();
    assert_eq!(
        event,
        StateEvent::Changed {
            generation: 1,
            task_ids: vec!["a".into()],
            namespaces: vec!["checks".into(), "notes".into()],
        }
    );
    assert!(events.try_recv().is_err(), "one event per transaction");
}

#[test]
#[should_panic(expected = "registered after a project was opened")]
fn register_after_the_first_writer_is_refused() {
    let root = TempDir::new().unwrap();
    let hooks = StateApplyHooks::new();
    let _writer = writer(&location(&root), &hooks);
    hooks.register(FakeHook::new("late", Behavior::WriteKv));
}

#[test]
fn a_failing_hook_changes_nothing_and_is_named() {
    let root = TempDir::new().unwrap();
    let first = FakeHook::new("checks", Behavior::WriteKv);
    let failing = FakeHook::new("evidence", Behavior::Fail);
    let writer = writer(&location(&root), &hooks_with(&[first, failing]));

    let error = writer
        .transact(|tx| {
            tx.set_task("a", TaskStateRecord::default())?;
            tx.kv("notes").put("k", toml::Value::Integer(1))
        })
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "state hook evidence failed: no evidence"
    );
    writer
        .read(|read| {
            assert_eq!(read.task("a")?, None);
            assert!(read.kv("notes").scan("")?.is_empty());
            assert!(read.kv("checks").scan("")?.is_empty());
            assert_eq!(read.generation()?, 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_hook_that_writes_a_task_row_fails() {
    let root = TempDir::new().unwrap();
    let hook = FakeHook::new("rogue", Behavior::WriteTask);
    let writer = writer(&location(&root), &hooks_with(&[hook]));

    let error = writer
        .transact(|tx| tx.set_task("a", TaskStateRecord::default()))
        .unwrap_err()
        .to_string();

    assert!(error.starts_with("state hook rogue failed: "), "{error}");
    assert!(error.contains("only kv"), "{error}");
    let ids = writer.read(|read| read.task_ids()).unwrap();
    assert!(ids.is_empty(), "{ids:?}");
}

#[test]
fn no_hook_runs_when_no_task_row_is_touched() {
    let root = TempDir::new().unwrap();
    let hook = FakeHook::new("checks", Behavior::WriteKv);
    let writer = writer(&location(&root), &hooks_with(&[hook.clone()]));

    writer
        .transact(|tx| tx.kv("notes").put("k", toml::Value::Integer(1)))
        .unwrap();
    writer.transact(|tx| tx.task("a").map(drop)).unwrap();

    assert!(hook.calls().is_empty());
}

#[test]
fn hooks_run_in_registration_order() {
    struct Ordered(&'static str, Arc<Mutex<Vec<&'static str>>>);
    impl StateApplyHook for Ordered {
        fn name(&self) -> &str {
            self.0
        }
        fn apply(&self, _tx: &mut StateTx, _change: &StateApplyChange) -> Result<()> {
            self.1.lock().push(self.0);
            Ok(())
        }
    }
    let order = Arc::new(Mutex::new(Vec::new()));
    let hooks = StateApplyHooks::new();
    for name in ["first", "second", "third"] {
        hooks.register(Arc::new(Ordered(name, order.clone())));
    }
    let root = TempDir::new().unwrap();
    let writer = StateStore::open_writer_with_hooks(&location(&root), &hooks).unwrap();
    writer
        .transact(|tx| tx.set_task("a", TaskStateRecord::default()))
        .unwrap();
    assert_eq!(*order.lock(), ["first", "second", "third"]);
}
