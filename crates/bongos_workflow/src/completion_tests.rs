//! Tests of the completion hooks: a successful run that completed its todo calls every hook with
//! its task, run and worktree, and ends only once each hook resolved; a hook that fails leaves the
//! run to end as it would; a completion that did not happen calls no hook. A child of the
//! lifecycle tests, sharing the task link tests' engine with task writes in memory.

use std::time::Duration;

use futures::channel::oneshot;
use gpui::AppContext as _;

use super::task_link_tests::{auto_runner, linked};
use super::*;
use crate::RunId;
use crate::completion::{CompletionHook, CompletionHooks};
use crate::task_link::{MERGE_GATE, TODO_GATE};

/// One call of a hook: the task, the run, the worktree, whether the worktree existed, the run's
/// todo as the hook saw it and the run's status then.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    task_id: String,
    run: String,
    worktree: PathBuf,
    worktree_exists: bool,
    todo: TodoState,
    status: RunStatus,
}

/// A hook that records its calls and resolves when the test releases it (at once when it is
/// not held).
struct RecordingHook {
    engine: WorkflowEngine,
    writes: Arc<task_link_tests::MemoryWrites>,
    calls: RefCell<Vec<Call>>,
    hold: bool,
    release: RefCell<Vec<oneshot::Sender<()>>>,
}

impl RecordingHook {
    fn new(linked: &task_link_tests::Linked, hold: bool) -> Rc<Self> {
        Rc::new(Self {
            engine: linked.engine.clone(),
            writes: linked.writes.clone(),
            calls: RefCell::default(),
            hold,
            release: RefCell::default(),
        })
    }

    fn release(&self) {
        for sender in self.release.borrow_mut().drain(..) {
            sender.send(()).ok();
        }
    }
}

impl CompletionHook for RecordingHook {
    fn after_complete(
        &self,
        task_id: &str,
        run_id: &RunId,
        worktree: &Path,
        cx: &mut gpui::App,
    ) -> gpui::Task<()> {
        self.calls.borrow_mut().push(Call {
            task_id: task_id.to_string(),
            run: run_id.clone(),
            worktree: worktree.to_path_buf(),
            worktree_exists: worktree.is_dir(),
            todo: self.writes.todo_states()[1].1,
            status: self.engine.lifecycle().status(run_id).unwrap(),
        });
        if !self.hold {
            return gpui::Task::ready(());
        }
        let (sender, receiver) = oneshot::channel();
        self.release.borrow_mut().push(sender);
        cx.background_spawn(async move {
            receiver.await.ok();
        })
    }
}

/// A hook that could not decide: it logs and resolves without doing anything.
struct FailingHook {
    called: RefCell<u32>,
}

impl CompletionHook for FailingHook {
    fn after_complete(
        &self,
        task_id: &str,
        _: &RunId,
        _: &Path,
        _: &mut gpui::App,
    ) -> gpui::Task<()> {
        *self.called.borrow_mut() += 1;
        log::error!("the hook could not decide {task_id}");
        gpui::Task::ready(())
    }
}

#[gpui::test]
fn completion_hook_runs_after_the_todo_and_before_the_run_ends(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = linked(&world, auto_runner(), cx);
    let hook = RecordingHook::new(&linked, true);
    cx.update(|cx| CompletionHooks::register(hook.clone(), cx));
    let run = linked.start(StartRequest::new(LEAF), cx);
    let (_, worktree) = linked.engine.active_run_worktree(LEAF).expect("a worktree");

    linked.drive(cx);
    assert!(hook.calls.borrow().is_empty(), "no hook before the todo");
    linked.approve(&run, MERGE_GATE, cx);

    // The todo is completed, the hook runs on the run's worktree, and the run has not ended.
    let calls = hook.calls.borrow().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].task_id, LEAF);
    assert_eq!(calls[0].run, run);
    assert_eq!(calls[0].worktree, worktree);
    assert!(calls[0].worktree_exists);
    assert_eq!(calls[0].todo, TodoState::Done);
    assert_ne!(calls[0].status, RunStatus::Succeeded);
    assert_ne!(
        linked.status(&run),
        RunStatus::Succeeded,
        "Ended waits for the hook"
    );
    assert_eq!(
        linked.engine.active_run_worktree(LEAF),
        Some((run.clone(), worktree))
    );

    hook.release();
    linked.drive(cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(hook.calls.borrow().len(), 1, "called once");
}

#[gpui::test]
fn completion_hook_failure_skips_no_hook_and_still_ends_the_run(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = linked(&world, auto_runner(), cx);
    let failing = Rc::new(FailingHook {
        called: RefCell::new(0),
    });
    let hook = RecordingHook::new(&linked, true);
    cx.update(|cx| {
        CompletionHooks::register(failing.clone(), cx);
        CompletionHooks::register(hook.clone(), cx);
    });
    let run = linked.start(StartRequest::new(LEAF), cx);
    linked.drive(cx);
    linked.approve(&run, MERGE_GATE, cx);

    assert_eq!(*failing.called.borrow(), 1);
    assert_eq!(hook.calls.borrow().len(), 1, "the failure skipped no hook");
    assert_ne!(
        linked.status(&run),
        RunStatus::Succeeded,
        "Ended waits for every hook"
    );
    hook.release();
    linked.drive(cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(linked.writes.todo_states()[1].1, TodoState::Done);
}

#[gpui::test]
fn completion_hook_is_not_called_when_the_todo_was_not_completed(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = linked(&world, auto_runner(), cx);
    let hook = RecordingHook::new(&linked, false);
    cx.update(|cx| CompletionHooks::register(hook.clone(), cx));
    let run = linked.start(StartRequest::new(LEAF), cx);
    linked
        .writes
        .edit_leaf(|file| file.todos[1].state = TodoState::Done);

    linked.drive(cx);
    linked.approve(&run, MERGE_GATE, cx);
    linked.approve(&run, TODO_GATE, cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert!(hook.calls.borrow().is_empty());
}

#[gpui::test]
fn completion_with_no_hook_ends_as_before(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = linked(&world, auto_runner(), cx);
    let run = linked.start(StartRequest::new(LEAF), cx);
    linked.drive(cx);
    linked.approve(&run, MERGE_GATE, cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(linked.writes.todo_states()[1].1, TodoState::Done);
    assert_eq!(linked.writes.changes.lock().unwrap().len(), 2);
}

#[gpui::test]
fn completion_hook_that_never_resolves_is_given_up_after_the_timeout(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = linked(&world, auto_runner(), cx);
    // Held and never released, as a hook whose decision can no longer end.
    let hook = RecordingHook::new(&linked, true);
    cx.update(|cx| CompletionHooks::register(hook.clone(), cx));
    let run = linked.start(StartRequest::new(LEAF), cx);
    linked.drive(cx);
    linked.approve(&run, MERGE_GATE, cx);
    assert_eq!(hook.calls.borrow().len(), 1);

    cx.executor()
        .advance_clock(crate::engine::COMPLETION_HOOKS_TIMEOUT - Duration::from_millis(1));
    linked.drive(cx);
    assert_ne!(
        linked.status(&run),
        RunStatus::Succeeded,
        "the run waits until the timeout"
    );

    cx.executor().advance_clock(Duration::from_millis(1));
    linked.drive(cx);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(linked.writes.todo_states()[1].1, TodoState::Done);
    assert_eq!(hook.calls.borrow().len(), 1, "called once");
}
