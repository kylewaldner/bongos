//! Done holds: the store fills each task's hold from the registered sources, `task: advance` on
//! the last open todo asks the sources first and writes nothing when one would hold the task,
//! and a write to `checks.live` alone does not read the Work tree again.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use bongos_project_repo::LoadedProject;
use bongos_work_model::TodoState;
use bongos_work_model::commands;
use bongos_work_model::derive::{self, State};
use futures::FutureExt as _;
use futures::channel::oneshot;
use gpui::{App, TestAppContext, VisualTestContext};
use tempfile::TempDir;

use crate::command_tests::{Harness, Input, dispatch, harness, wait_until};
use crate::commands::{LastWorkToast, keyboard_context};
use crate::store_tests::canonical;
use crate::{DoneHoldSource, DoneHolds};

const HOLD: &str = "not done: 1 check has no pass on the current tree (tests)";

/// A source that holds the tasks in `holds`, and answers `would_hold` with `would`, or, while
/// `wait` is set, only once the test answers through `waiting`.
#[derive(Default)]
struct FakeHolds {
    holds: RefCell<BTreeMap<String, String>>,
    would: RefCell<Option<String>>,
    asked: Cell<usize>,
    wait: Cell<bool>,
    waiting: RefCell<Vec<oneshot::Sender<Option<String>>>>,
}

impl DoneHoldSource for FakeHolds {
    fn hold(&self, task_id: &str, _cx: &App) -> Option<String> {
        self.holds.borrow().get(task_id).cloned()
    }

    fn would_hold(&self, _task_id: &str, cx: &mut App) -> gpui::Task<Option<String>> {
        self.asked.set(self.asked.get() + 1);
        if self.wait.get() {
            let (sender, receiver) = oneshot::channel();
            self.waiting.borrow_mut().push(sender);
            return cx
                .background_executor()
                .spawn(async move { receiver.await.ok().flatten() });
        }
        gpui::Task::ready(self.would.borrow().clone())
    }
}

fn register(cx: &mut VisualTestContext) -> Rc<FakeHolds> {
    let source = Rc::new(FakeHolds::default());
    let added = source.clone();
    cx.update(|_, cx| DoneHolds::add_source("fake", added, cx));
    source
}

fn last_toast(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| {
        cx.try_global::<LastWorkToast>()
            .map(|toast| toast.0.clone())
    })
}

fn todo_states(harness: &Harness, cx: &mut VisualTestContext) -> Vec<TodoState> {
    harness
        .file(cx, &["backfill"])
        .todos
        .iter()
        .map(|todo| todo.state)
        .collect()
}

fn backfill_state(harness: &Harness, cx: &mut VisualTestContext) -> (Option<String>, State) {
    let tree = harness.tree(cx);
    let task = tree.task_by_id("l1").unwrap();
    (task.done_hold.clone(), derive::state(task, &tree))
}

/// Advances `backfill` (three todos) until only its last todo is open.
async fn to_last_todo(harness: &Harness, cx: &mut VisualTestContext) {
    for _ in 0..2 {
        harness
            .run_and_wait(cx, "task::Advance", &[Input::Pick("backfill")])
            .await;
    }
    assert_eq!(
        todo_states(harness, cx),
        [TodoState::Done, TodoState::Done, TodoState::Open]
    );
}

#[gpui::test]
async fn with_no_source_nothing_is_held(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    cx.update(|_, cx| {
        assert!(!DoneHolds::has_sources(cx));
        assert_eq!(DoneHolds::hold("l1", cx), None);
        assert_eq!(DoneHolds::would_hold("l1", cx).now_or_never(), Some(None));
    });
    to_last_todo(&harness, cx).await;
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("backfill")])
        .await;
    let tree = harness.tree(cx);
    assert!(tree.tasks().iter().all(|task| task.done_hold.is_none()));
    assert_eq!(backfill_state(&harness, cx), (None, State::Done));
}

#[gpui::test]
async fn advance_on_the_last_todo_of_a_held_task_is_refused_and_writes_nothing(
    cx: &mut TestAppContext,
) {
    let (harness, cx) = harness(cx, true, true).await;
    to_last_todo(&harness, cx).await;
    let source = register(cx);
    *source.would.borrow_mut() = Some(HOLD.to_string());
    let writes = harness.writes(cx);
    let before =
        std::fs::read_to_string(harness.task_dir(&["backfill"]).join("task.toml")).unwrap();

    harness
        .run(cx, "task::Advance", &[Input::Pick("backfill")])
        .await;
    wait_until(cx, "the refusal", |cx| {
        last_toast(cx).as_deref() == Some(HOLD)
    })
    .await;
    assert_eq!(source.asked.get(), 1);
    assert_eq!(harness.writes(cx), writes);
    let after = std::fs::read_to_string(harness.task_dir(&["backfill"]).join("task.toml")).unwrap();
    assert_eq!(after, before);
    assert_eq!(todo_states(&harness, cx)[2], TodoState::Open);

    // An advance that does not finish the task asks nothing.
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("alpha")])
        .await;
    assert_eq!(source.asked.get(), 1);

    // When the source would not hold it, the advance is written; the hold the source then
    // reports keeps the task from Done, and the hold is not in its file.
    *source.would.borrow_mut() = None;
    source
        .holds
        .borrow_mut()
        .insert("l1".to_string(), HOLD.to_string());
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("backfill")])
        .await;
    assert_eq!(source.asked.get(), 2);
    assert_eq!(todo_states(&harness, cx)[2], TodoState::Done);
    let (hold, state) = backfill_state(&harness, cx);
    assert_eq!(hold.as_deref(), Some(HOLD));
    assert_ne!(state, State::Done);
    let file = std::fs::read_to_string(harness.task_dir(&["backfill"]).join("task.toml")).unwrap();
    assert!(!file.contains("not done"), "{file}");

    // Once the source stops holding it and says so, the task is Done.
    source.holds.borrow_mut().clear();
    cx.update(|_, cx| DoneHolds::changed(cx));
    assert_eq!(backfill_state(&harness, cx), (None, State::Done));
}

#[gpui::test]
async fn two_advances_each_wait_for_the_source_before_writing(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    to_last_todo(&harness, cx).await;
    let source = register(cx);
    source.wait.set(true);
    let writes = harness.writes(cx);
    for _ in 0..2 {
        harness
            .run(cx, "task::Advance", &[Input::Pick("backfill")])
            .await;
    }
    cx.run_until_parked();
    assert_eq!(source.asked.get(), 2);
    assert_eq!(harness.writes(cx), writes);
    assert_eq!(todo_states(&harness, cx)[2], TodoState::Open);

    // The first answer lets its advance through; the second advance then finds no open todo.
    for sender in source.waiting.borrow_mut().drain(..) {
        sender.send(None).ok();
    }
    wait_until(cx, "the advance", |cx| harness.writes(cx) > writes).await;
    assert_eq!(todo_states(&harness, cx)[2], TodoState::Done);
    assert_eq!(harness.writes(cx), writes + 1);
}

#[gpui::test]
async fn an_advance_that_finishes_the_task_only_on_the_writers_tree_is_refused(
    cx: &mut TestAppContext,
) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("backfill")])
        .await;
    assert_eq!(
        todo_states(&harness, cx),
        [TodoState::Done, TodoState::Open, TodoState::Open]
    );
    let source = register(cx);
    *source.would.borrow_mut() = Some(HOLD.to_string());

    // A second press picks the task while the snapshot still shows two open todos.
    dispatch(cx, "task::Advance");
    wait_until(cx, "a picker", |cx| harness.picker(cx).is_some()).await;
    let picker = harness.picker(cx).unwrap();
    picker.update_in(cx, |modal, window, cx| {
        modal
            .picker
            .update(cx, |picker, cx| picker.set_query("backfill", window, cx))
    });
    cx.run_until_parked();

    // An earlier press is still being written when this one is confirmed: it closes the
    // second todo, so the confirmed advance would close the last one.
    let store = harness.store.clone();
    let earlier = cx.update(|window, cx| {
        let ctx = keyboard_context(window, cx);
        let earlier = store.update(cx, |store, cx| {
            store.write(
                ctx,
                |tree, ctx| commands::advance(tree, ctx, "l1", false),
                cx,
            )
        });
        window.dispatch_action(Box::new(menu::Confirm), cx);
        earlier
    });
    earlier.await.unwrap();
    wait_until(cx, "the refusal", |cx| {
        last_toast(cx).is_some_and(|toast| toast.contains("run task: advance again"))
    })
    .await;
    assert_eq!(
        todo_states(&harness, cx),
        [TodoState::Done, TodoState::Done, TodoState::Open]
    );
    assert_eq!(source.asked.get(), 0);
    assert_ne!(backfill_state(&harness, cx).1, State::Done);

    // Pressed again, the advance now asks the source, which holds it.
    harness
        .run(cx, "task::Advance", &[Input::Pick("backfill")])
        .await;
    wait_until(cx, "the hold", |cx| last_toast(cx).as_deref() == Some(HOLD)).await;
    assert_eq!(source.asked.get(), 1);
    assert_eq!(todo_states(&harness, cx)[2], TodoState::Open);
}

#[gpui::test]
async fn a_checks_live_write_alone_does_not_read_the_tree_again(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    let store = crate::seam_tests::new_store(cx, &root);
    let repo = crate::seam_tests::project_repo(&canonical(&temp).join("project"));
    let loaded = LoadedProject::new(repo.clone());
    let _registration = cx.update(|cx| crate::register_project_repo(loaded, cx));
    crate::seam_tests::wait_until(cx, &store, "the repository's project", |tree| {
        tree.task_by_id("r1").is_some()
    })
    .await;
    let writer = repo
        .state()
        .and_then(|state| state.writer())
        .unwrap()
        .clone();
    let put = |namespace: &str| {
        writer
            .transact(|tx| tx.kv(namespace).put("r1", "running".into()))
            .unwrap()
    };
    let refreshes =
        |cx: &mut TestAppContext| store.read_with(cx, |store, _| store.stats().source_refreshes);
    // Let whatever the registration started settle.
    cx.executor().timer(Duration::from_millis(500)).await;
    cx.run_until_parked();
    let settled = refreshes(cx);

    put("checks.live");
    cx.executor().timer(Duration::from_millis(500)).await;
    cx.run_until_parked();
    assert_eq!(refreshes(cx), settled);

    for (count, namespace) in [(1, "checks.owed"), (2, "checks.done")] {
        put(namespace);
        for _ in 0..200 {
            if refreshes(cx) >= settled + count {
                break;
            }
            cx.executor().timer(Duration::from_millis(50)).await;
        }
        assert_eq!(refreshes(cx), settled + count, "{namespace}");
    }
}

#[test]
fn only_a_write_outside_the_tree_reads_nothing_again() {
    let names = |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
    let refreshes = crate::store::state_change_refreshes_tree;
    assert!(!refreshes(&[], &names(&["checks.live"])));
    assert!(refreshes(&[], &names(&["checks.owed"])));
    assert!(refreshes(&[], &names(&["checks.done"])));
    assert!(refreshes(&[], &names(&["checks.live", "checks.owed"])));
    assert!(refreshes(&names(&["r1"]), &names(&["checks.live"])));
    assert!(refreshes(&names(&["r1"]), &[]));
}

/// Holds what a refresher last copied in: a source over run state that another thread updates.
#[derive(Default)]
struct Snapshot {
    /// What the next refresh copies in.
    pending: parking_lot::Mutex<BTreeMap<String, String>>,
    copied: parking_lot::Mutex<BTreeMap<String, String>>,
    refreshes: std::sync::atomic::AtomicUsize,
}

impl crate::DoneHoldRefresher for Snapshot {
    fn refresh(&self) {
        self.refreshes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.copied.lock() = self.pending.lock().clone();
    }
}

struct SnapshotSource(std::sync::Arc<Snapshot>);

impl DoneHoldSource for SnapshotSource {
    fn hold(&self, task_id: &str, _cx: &App) -> Option<String> {
        self.0.copied.lock().get(task_id).cloned()
    }

    fn would_hold(&self, _task_id: &str, _cx: &mut App) -> gpui::Task<Option<String>> {
        gpui::Task::ready(None)
    }
}

#[gpui::test]
async fn a_refresher_runs_on_every_tree_read_before_the_sources_are_asked(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    to_last_todo(&harness, cx).await;
    let snapshot = std::sync::Arc::new(Snapshot::default());
    cx.update(|_, cx| {
        DoneHolds::add_refresher("snapshot", snapshot.clone(), cx);
        DoneHolds::add_source("snapshot", Rc::new(SnapshotSource(snapshot.clone())), cx);
    });
    // What the write that finishes the task commits with it, the refresher reads with the tree
    // that write publishes: the first snapshot showing the task finished already holds it.
    snapshot
        .pending
        .lock()
        .insert("l1".to_string(), HOLD.to_string());
    let refreshes = snapshot.refreshes.load(std::sync::atomic::Ordering::SeqCst);
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("backfill")])
        .await;
    assert!(snapshot.refreshes.load(std::sync::atomic::Ordering::SeqCst) > refreshes);
    assert_eq!(todo_states(&harness, cx)[2], TodoState::Done);
    let (hold, state) = backfill_state(&harness, cx);
    assert_eq!(hold.as_deref(), Some(HOLD));
    assert_ne!(state, State::Done);

    // Removed, it runs no more.
    cx.update(|_, cx| DoneHolds::remove_refresher("snapshot", cx));
    let refreshes = snapshot.refreshes.load(std::sync::atomic::Ordering::SeqCst);
    cx.update(|_, cx| {
        if let Some(store) = crate::WorkStore::global(cx) {
            store.update(cx, |store, cx| store.refresh_sources(cx));
        }
    });
    cx.run_until_parked();
    assert_eq!(
        snapshot.refreshes.load(std::sync::atomic::Ordering::SeqCst),
        refreshes
    );
}
