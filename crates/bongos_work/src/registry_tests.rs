//! The registries of D13 filled with fake sources, and the changed-file counts. Nothing here
//! stores a source's state in the work tree: the roll-ups are computed from what the sources
//! say, and a removed source leaves nothing behind.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bongos_work_model::{Kind, TaskFile, layout, serialize_task};
use git::status::FileStatus;
use gpui::{App, Entity, TestAppContext, VisualTestContext};
use serde_json::json;

use crate::changes::{CHANGES_INTERVAL, ChangedCount, count_porcelain};
use crate::command_tests::{Harness, Input, group_toml, harness, leaf_toml, wait_until};
use crate::registry::{
    ActiveRun, LiveModes, LiveModesRegistry, LiveModesSource, NeedsYou, NeedsYouRegistry,
    NeedsYouSource,
};
use crate::store_tests::write;
use crate::worktree_tests::git;
use crate::{WorkStore, WorkStoreEvent};

/// A needs-you source whose answer the test sets.
#[derive(Default)]
pub(crate) struct FakeGates(pub(crate) RefCell<Vec<NeedsYou>>);

impl NeedsYouSource for FakeGates {
    fn needs_you(&self, _: &App) -> Vec<NeedsYou> {
        self.0.borrow().clone()
    }
}

/// A live-modes source with one run, on one task.
struct FakeRuns {
    task: &'static str,
    run: RefCell<Option<ActiveRun>>,
    chats: usize,
}

impl LiveModesSource for FakeRuns {
    fn live_modes(&self, task_id: &str, _: &App) -> LiveModes {
        if task_id != self.task {
            return LiveModes::default();
        }
        LiveModes {
            open_chats: self.chats,
            run: self.run.borrow().clone(),
        }
    }
}

pub(crate) fn gate(task_id: &str, reason: &str) -> NeedsYou {
    NeedsYou {
        task_id: task_id.to_string(),
        reason: reason.to_string(),
    }
}

fn run(workflow: &str) -> ActiveRun {
    ActiveRun {
        workflow: workflow.to_string(),
        status: "waiting at a gate".to_string(),
        step: "review".to_string(),
        past_runs: 2,
    }
}

/// Adds the group `deep` (`g4`) under `phase`, holding the leaf `gate` (`l4`): from the project
/// (`g1`) that is `phase` (`g2`), `deep` and `gate`, three levels down.
pub(crate) async fn with_deep_leaf(harness: &Harness, cx: &mut VisualTestContext) {
    let deep = harness.task_dir(&["phase", "deep"]);
    write(&layout::task_file(&deep), &group_toml("g4", "Deep"));
    write(
        &layout::task_file(&layout::child_dir(&deep, "gate")),
        &leaf_toml("l4", "Gate", Kind::PrShip),
    );
    wait_until(cx, "the store to see the deep leaf", |cx| {
        harness.tree(cx).task_by_id("l4").is_some()
    })
    .await;
}

fn needs_you_of(store: &Entity<WorkStore>, id: &str, cx: &mut VisualTestContext) -> usize {
    store.read_with(cx, |store, _| store.rollup(id).expect("a task").needs_you)
}

#[gpui::test]
async fn a_gate_three_levels_down_marks_every_ancestor_and_removing_it_clears_them(
    cx: &mut TestAppContext,
) {
    let (harness, cx) = harness(cx, true, true).await;
    with_deep_leaf(&harness, cx).await;
    let store = harness.store.clone();
    let events = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(&store, move |_, event: &WorkStoreEvent, _| {
            events.borrow_mut().push(*event)
        })
    });

    let source = Rc::new(FakeGates::default());
    source
        .0
        .borrow_mut()
        .push(gate("l4", "gate: approve the plan"));
    cx.update(|_, cx| NeedsYouRegistry::add_source("gates", source.clone(), cx));

    // The task itself and each of its ancestors, to the project, count it once.
    for id in ["l4", "g4", "g2", "g1"] {
        assert_eq!(needs_you_of(&store, id, cx), 1, "{id}");
    }
    // Neither the sibling leaves nor the sibling group do.
    for id in ["l1", "l2", "l3"] {
        assert_eq!(needs_you_of(&store, id, cx), 0, "{id}");
    }
    let drill = store.read_with(cx, |store, _| store.rollup("g1").unwrap().first_needs_you);
    assert_eq!(
        drill,
        Some(
            vec!["g1", "g2", "g4", "l4"]
                .into_iter()
                .map(String::from)
                .collect()
        )
    );
    store.read_with(cx, |store, _| {
        assert_eq!(store.needs_you_reasons("l4"), ["gate: approve the plan"]);
    });
    assert_eq!(*events.borrow(), [WorkStoreEvent::NeedsYouChanged]);

    // A change in the source's answer reaches the store when the source says so.
    source.0.borrow_mut().push(gate("l2", "todo: waiting"));
    cx.update(|_, cx| NeedsYouRegistry::changed(cx));
    assert_eq!(needs_you_of(&store, "g2", cx), 2);
    assert_eq!(needs_you_of(&store, "g1", cx), 2);

    // Removing the source clears every badge; nothing is left in the store or on disk.
    cx.update(|_, cx| NeedsYouRegistry::remove_source("gates", cx));
    for id in ["l4", "g4", "g2", "g1", "l2"] {
        assert_eq!(needs_you_of(&store, id, cx), 0, "{id} after removal");
    }
    store.read_with(cx, |store, _| assert!(store.needs_you().is_empty()));
    assert_eq!(harness.writes(cx), 0);
}

#[gpui::test]
async fn an_active_run_stops_a_second_run_on_the_leaf(cx: &mut TestAppContext) {
    let (_harness, cx) = harness(cx, true, true).await;
    let can_start = |cx: &mut VisualTestContext, id: &str| {
        cx.update(|_, cx| LiveModesRegistry::can_start_run(id, cx))
    };
    // With no source, any leaf may start a run.
    assert!(can_start(cx, "l1").allowed);

    let runs = Rc::new(FakeRuns {
        task: "l1",
        run: RefCell::new(Some(run("ship"))),
        chats: 1,
    });
    cx.update(|_, cx| LiveModesRegistry::add_source("runs", runs.clone(), cx));
    let blocked = can_start(cx, "l1");
    assert!(!blocked.allowed);
    assert_eq!(
        blocked.chip.as_deref(),
        Some("Run in progress: ship, waiting at a gate, step review")
    );
    // One run per leaf: another leaf is not held back.
    assert!(can_start(cx, "l2").allowed);

    // Chats add up across sources; the run comes from the first source that has one.
    let chats = Rc::new(FakeRuns {
        task: "l1",
        run: RefCell::new(None),
        chats: 2,
    });
    cx.update(|_, cx| LiveModesRegistry::add_source("chats", chats, cx));
    let live = cx.update(|_, cx| LiveModesRegistry::live_modes("l1", cx));
    assert_eq!(live.open_chats, 3);
    assert_eq!(live.run.map(|run| run.past_runs), Some(2));

    // The run ends: the leaf may start another.
    runs.run.borrow_mut().take();
    assert!(can_start(cx, "l1").allowed);
    cx.update(|_, cx| LiveModesRegistry::remove_source("runs", cx));
    cx.update(|_, cx| LiveModesRegistry::remove_source("chats", cx));
    assert_eq!(
        cx.update(|_, cx| LiveModesRegistry::live_modes("l1", cx)),
        LiveModes::default()
    );
}

/// The commands that change the todos of a task with a run refuse, with the chip of the run, and
/// write nothing; every other task, and every task once the run ends, is not held back.
#[gpui::test]
async fn advance_skip_and_remove_are_refused_while_the_task_has_a_run(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let todos =
        |harness: &Harness, cx: &mut VisualTestContext| harness.file(cx, &["backfill"]).todos;
    let before = todos(&harness, cx);
    let runs = Rc::new(FakeRuns {
        task: "l1",
        run: RefCell::new(Some(run("ship"))),
        chats: 0,
    });
    cx.update(|_, cx| LiveModesRegistry::add_source("runs", runs.clone(), cx));

    let writes = harness.writes(cx);
    for (name, inputs) in [
        ("task::Advance", vec![Input::Pick("backfill")]),
        (
            "todo::Skip",
            vec![Input::Pick("backfill"), Input::Pick("qa")],
        ),
        (
            "todo::Remove",
            vec![Input::Pick("backfill"), Input::Pick("qa")],
        ),
    ] {
        harness.clear_toasts(cx);
        harness.run(cx, name, &inputs).await;
        cx.run_until_parked();
        assert!(harness.has_toast(cx), "{name} says why");
        assert_eq!(harness.writes(cx), writes, "{name} wrote");
        assert_eq!(todos(&harness, cx), before, "{name} changed the todos");
    }
    assert_eq!(
        crate::commands::run_refusal("Run in progress: ship, waiting at a gate, step review"),
        "Run in progress: ship, waiting at a gate, step review; cancel the run first \
         (workflow: cancel)"
    );

    // Another task has no run, so it advances.
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("alpha")])
        .await;

    // The run ends: the task advances again.
    runs.run.borrow_mut().take();
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("backfill")])
        .await;
    cx.update(|_, cx| LiveModesRegistry::remove_source("runs", cx));
}

#[test]
fn porcelain_output_counts_one_file_per_line() {
    assert_eq!(count_porcelain(""), 0);
    assert_eq!(count_porcelain(" M src/a.rs\n?? notes.md\nA  b.rs\n"), 3);
}

#[test]
fn a_count_is_due_once_the_interval_passed_and_no_check_runs() {
    let at = Instant::now();
    let entry = |checking| ChangedCount::new(at, Some(1), checking);
    assert!(!entry(false).due(at + CHANGES_INTERVAL - Duration::from_millis(1)));
    assert!(entry(false).due(at + CHANGES_INTERVAL));
    assert!(!entry(true).due(at + CHANGES_INTERVAL * 5));
}

/// Rewrites the leaf `l1` (`backfill`) with a worktree and waits for the store to see it.
async fn give_backfill_a_worktree(harness: &Harness, worktree: &Path, cx: &mut VisualTestContext) {
    let mut file = TaskFile::new("Backfill", Kind::PrShip);
    file.id = "l1".to_string();
    file.worktree = Some(worktree.to_string_lossy().into_owned());
    write(
        &layout::task_file(&harness.task_dir(&["backfill"])),
        &serialize_task(&file).unwrap(),
    );
    wait_until(cx, "the worktree to load", |cx| {
        harness
            .tree(cx)
            .task_by_id("l1")
            .and_then(|task| task.file.as_ref())
            .is_some_and(|file| file.worktree.is_some())
    })
    .await;
}

#[gpui::test]
async fn changed_files_are_counted_by_git_status_at_a_limited_rate(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let repository = tempfile::TempDir::new().unwrap();
    let worktree = std::fs::canonicalize(repository.path()).unwrap();
    git(&worktree, &["init", "-q", "-b", "main"]);
    write(&worktree.join("a.rs"), "a\n");
    write(&worktree.join("dir/b.rs"), "b\n");
    give_backfill_a_worktree(&harness, &worktree, cx).await;
    let store = harness.store.clone();
    assert_eq!(
        store.read_with(cx, |store, _| store.changed_files("l1")),
        None
    );

    store.update(cx, |store, cx| store.refresh_changed_files("l1", cx));
    wait_until(cx, "the count", |cx| {
        store.read_with(cx, |store, _| store.changed_files("l1")) == Some(2)
    })
    .await;

    // A third file, asked for again inside the interval: the old count is served, git not run.
    write(&worktree.join("c.rs"), "c\n");
    store.update(cx, |store, cx| store.refresh_changed_files("l1", cx));
    cx.run_until_parked();
    store.read_with(cx, |store, _| {
        assert_eq!(store.changed_files("l1"), Some(2));
        assert_eq!(store.status_runs(), 1);
    });
}

#[gpui::test]
async fn an_open_worktree_is_counted_through_the_git_store(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    give_backfill_a_worktree(&harness, Path::new("/wt"), cx).await;
    let fs = harness
        .workspace
        .read_with(cx, |workspace, _| workspace.app_state().fs.as_fake());
    fs.insert_tree(
        "/wt",
        json!({ ".git": {}, "a.rs": "", "b.rs": "", "c.rs": "" }),
    )
    .await;
    fs.set_status_for_repo(
        Path::new("/wt/.git"),
        &[
            ("a.rs", FileStatus::Untracked),
            ("b.rs", FileStatus::Untracked),
        ],
    );
    harness
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/wt", true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let store = harness.store.clone();
    store.update(cx, |store, cx| store.refresh_changed_files("l1", cx));
    store.read_with(cx, |store, _| {
        assert_eq!(store.changed_files("l1"), Some(2));
        // The worktree is open: no git process ran.
        assert_eq!(store.status_runs(), 0);
    });
}
