//! Tests of the status writes over a real work store in a temporary directory and a fake Linear:
//! started once when a plan begins (never for a stacked leaf, never backwards, a group with its
//! first leaf, retried after a failure), the cancel question after a drop and what each answer
//! writes, and that no close, advance, result or reopen ever reaches `set_state`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bongos_linear_model::api::{
    ApiError, Issue, IssueRef, IssueSummary, LinearApi, NewIssue, StateKind, WorkflowState,
};
use bongos_linear_model::fake::{Call, FakeLinear};
use bongos_linear_model::status::{CancelAnswer, CancelQuestion, CancelTarget};
use bongos_minibuffer::ReadKeyView;
use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::{Context as WorkContext, Outcome, advance, close, default_todos};
use bongos_work_model::commands::{drop_task, reopen, set_result, todo_start};
use bongos_work_model::store::WorkTree;
use bongos_work_model::{
    Kind, LinearCancel, LinearState, ResultKind, ResultState, StackedOn, TaskFile, TaskResult,
    UnblockPoint, layout, serialize_task,
};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::{TestAppContext, VisualTestContext};

use crate::actions_tests::dispatch;
use crate::link_tests::{Env, env, issue, wait_until};
use crate::status::{answer_of, keys, question_text};
use crate::sync;
use crate::token;

/// The states of the team: two started, two canceled (the lowest position is used).
fn team() -> Vec<WorkflowState> {
    let at = |id: &str, kind: StateKind, position: f64| WorkflowState {
        id: id.to_string(),
        name: id.to_string(),
        kind,
        position,
    };
    vec![
        at("backlog", StateKind::Backlog, 1.0),
        at("todo", StateKind::Unstarted, 2.0),
        at("review", StateKind::Started, 5.0),
        at("doing", StateKind::Started, 3.0),
        at("done", StateKind::Completed, 6.0),
        at("later", StateKind::Canceled, 9.0),
        at("canceled", StateKind::Canceled, 8.0),
    ]
}

/// The env of the link tests with a full team and the watcher attached (the test creates the
/// store after the sync). Tickets 5, 100, 101 and 121 exist, and 6, 102 and 103 are added.
pub(crate) async fn start(cx: &mut TestAppContext) -> (Env, &mut VisualTestContext) {
    let (env, cx) = env(cx, true, true).await;
    prepare(&env, cx);
    (env, cx)
}

fn prepare(env: &Env, cx: &mut VisualTestContext) {
    env.fake.add_team("team", "SEQ", team());
    for number in [6, 102, 103] {
        env.fake.add_issue(issue(number));
    }
    cx.update(|_, cx| {
        crate::refresh::connect(cx);
        crate::status::connect(cx);
    });
    cx.run_until_parked();
}

fn linked(file: &mut TaskFile, number: u32) {
    file.linear = Some(format!("SEQ-{number}"));
    file.linear_state = Some(LinearState {
        issue_id: format!("uuid-SEQ-{number}"),
        team_id: "team".to_string(),
        url: format!("https://linear.app/acme/issue/SEQ-{number}"),
        ..LinearState::default()
    });
}

/// Adds a task file on disk under the task `parent` (the project when `None`) and waits for the
/// store to load it. A leaf has the default todos, a group none.
pub(crate) async fn add(
    env: &Env,
    parent: Option<&str>,
    slug: &str,
    id: &str,
    number: Option<u32>,
    leaf: bool,
    stacked_on: Option<&str>,
    cx: &mut VisualTestContext,
) {
    let dir = match parent {
        Some(parent) => env.tree(cx).task_by_id(parent).unwrap().dir.clone(),
        None => layout::project_dir(&env.root, "p"),
    };
    let mut file = TaskFile::new(slug, Kind::PrShip);
    file.id = id.to_string();
    if leaf {
        file.todos = default_todos(Kind::PrShip);
    }
    if let Some(number) = number {
        linked(&mut file, number);
    }
    file.stacked_on = stacked_on.map(|task| StackedOn {
        task: task.to_string(),
        unblock: UnblockPoint::Merged,
        extra: Default::default(),
    });
    let path = layout::task_file(&layout::child_dir(&dir, slug));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serialize_task(&file).unwrap()).unwrap();
    let id = id.to_string();
    wait_until(cx, "the new task", |cx| {
        env.tree(cx).task_by_id(&id).is_some()
    })
    .await;
}

/// The group `phase` (g9, SEQ-101) with the leaves `one` (l10, SEQ-102) and `two` (l11, SEQ-103).
async fn add_phase(env: &Env, cx: &mut VisualTestContext) {
    add(env, None, "phase", "g9", Some(101), false, None, cx).await;
    add(env, Some("g9"), "one", "l10", Some(102), true, None, cx).await;
    add(env, Some("g9"), "two", "l11", Some(103), true, None, cx).await;
}

async fn run(
    env: &Env,
    cx: &mut VisualTestContext,
    command: impl Fn(&WorkTree, &WorkContext) -> Outcome + Send + 'static,
) {
    let before = env.writes(cx);
    env.store.update(cx, |store, cx| {
        store
            .write(
                WorkContext::at_keyboard(PersonAtKeyboard::for_test()),
                command,
                cx,
            )
            .detach()
    });
    wait_until(cx, "the write", |cx| env.writes(cx) > before).await;
}

/// Starts the first todo (the plan, for a leaf with no `plans` todo).
pub(crate) async fn begin(env: &Env, id: &str, cx: &mut VisualTestContext) {
    let id = id.to_string();
    run(env, cx, move |tree, ctx| {
        todo_start(tree, ctx, &id, Some(0))
    })
    .await;
}

async fn drop_it(env: &Env, id: &str, confirmed: bool, cx: &mut VisualTestContext) {
    let id = id.to_string();
    run(env, cx, move |tree, ctx| {
        drop_task(tree, ctx, &id, confirmed)
    })
    .await;
}

pub(crate) async fn settle(cx: &mut VisualTestContext) {
    cx.background_executor
        .timer(Duration::from_millis(500))
        .await;
    cx.run_until_parked();
}

/// The `set_state` calls the fake saw: ticket and state.
fn sets(env: &Env) -> Vec<(String, String)> {
    env.fake
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::SetState { issue_id, state_id } => Some((issue_id, state_id)),
            _ => None,
        })
        .collect()
}

fn link_state(env: &Env, slug: &str, cx: &mut VisualTestContext) -> LinearState {
    env.file(slug, cx).linear_state.unwrap()
}

fn started(env: &Env, slug: &str, cx: &mut VisualTestContext) -> bool {
    link_state(env, slug, cx).started_sent.is_some()
}

fn cancel_record(env: &Env, slug: &str, cx: &mut VisualTestContext) -> Option<LinearCancel> {
    link_state(env, slug, cx).cancel
}

fn asking(env: &Env, cx: &mut VisualTestContext) -> bool {
    env.workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<ReadKeyView>(cx).is_some()
    })
}

async fn wait_question(env: &Env, cx: &mut VisualTestContext) {
    wait_until(cx, "the question", |cx| asking(env, cx)).await;
}

fn set_ticket(env: &Env, number: u32, state_id: &str) {
    futures::executor::block_on(env.fake.set_state(&format!("uuid-SEQ-{number}"), state_id))
        .unwrap();
    env.fake.clear_calls();
}

fn detail_of(env: &Env, slug: &str, event: &str, cx: &mut VisualTestContext) -> Option<String> {
    env.file(slug, cx)
        .history
        .iter()
        .rev()
        .find(|entry| entry.event == event)
        .and_then(|entry| entry.detail.clone())
}

// ---------------------------------------------------------------------------------------------
// Texts

#[test]
fn the_questions_say_what_they_will_do() {
    let target = |number: u32| CancelTarget {
        task_id: format!("t{number}"),
        identifier: format!("SEQ-{number}"),
        issue_id: format!("uuid-SEQ-{number}"),
    };
    let leaf = CancelQuestion::Leaf(target(121));
    assert_eq!(question_text(&leaf), "Cancel SEQ-121 in Linear?");
    assert_eq!(
        keys(&leaf),
        (vec![('c', "Cancel ticket"), ('l', "Leave it")], 'c')
    );
    let group = CancelQuestion::Group {
        group: target(120),
        leaves: vec![target(121), target(122), target(123)],
    };
    assert_eq!(
        question_text(&group),
        "Cancel SEQ-120 and its 3 subtickets?"
    );
    assert_eq!(
        keys(&group),
        (
            vec![
                ('l', "Leave all"),
                ('o', "Cancel leaves only"),
                ('a', "Cancel all")
            ],
            'l'
        )
    );
    assert_eq!(answer_of('c'), CancelAnswer::Cancel);
    assert_eq!(answer_of('l'), CancelAnswer::Leave);
    assert_eq!(answer_of('o'), CancelAnswer::LeavesOnly);
    assert_eq!(answer_of('a'), CancelAnswer::All);
}

// ---------------------------------------------------------------------------------------------
// D13: started

#[gpui::test]
async fn started_is_written_once_from_unstarted(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    set_ticket(&env, 5, "todo");
    begin(&env, "l2", cx).await;
    wait_until(cx, "started_sent", |cx| started(&env, "beta", cx)).await;
    assert_eq!(
        sets(&env),
        vec![("uuid-SEQ-5".to_string(), "doing".to_string())],
        "the started state with the lowest position"
    );
    assert_eq!(env.fake.issue("SEQ-5").unwrap().state.id, "doing");
    assert_eq!(detail_of(&env, "beta", "linear started", cx), None);

    // More changes and refreshes write nothing more.
    dispatch("task::RefreshFromLinear", cx);
    settle(cx).await;
    run(&env, cx, |tree, ctx| advance(tree, ctx, "l2", false)).await;
    dispatch("task::RefreshFromLinear", cx);
    settle(cx).await;
    assert_eq!(sets(&env).len(), 1);
}

async fn started_stays_put(ticket_state: &str, cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    set_ticket(&env, 5, ticket_state);
    begin(&env, "l2", cx).await;
    wait_until(cx, "started_sent", |cx| started(&env, "beta", cx)).await;
    assert!(sets(&env).is_empty(), "{ticket_state}");
    assert_eq!(
        detail_of(&env, "beta", "linear started", cx),
        Some(format!("ticket is {ticket_state}; not moved"))
    );
    assert_eq!(env.fake.issue("SEQ-5").unwrap().state.id, ticket_state);
}

#[gpui::test]
async fn nothing_is_written_from_started(cx: &mut TestAppContext) {
    started_stays_put("review", cx).await;
}

#[gpui::test]
async fn nothing_is_written_from_completed(cx: &mut TestAppContext) {
    started_stays_put("done", cx).await;
}

#[gpui::test]
async fn nothing_is_written_for_a_stacked_leaf_or_an_unlinked_task(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    add(&env, None, "stk", "l12", Some(6), true, Some("beta"), cx).await;
    begin(&env, "l12", cx).await;
    wait_until(cx, "started_sent", |cx| started(&env, "stk", cx)).await;
    assert_eq!(
        detail_of(&env, "stk", "linear started", cx).as_deref(),
        Some("no Linear status for a stacked leaf")
    );
    // Not even a read: the decision needs no ticket.
    assert!(env.fake.calls().is_empty(), "{:?}", env.fake.calls());
    // An unlinked task and a hand-set link never reach Linear.
    begin(&env, "l1", cx).await;
    begin(&env, "l4", cx).await;
    settle(cx).await;
    assert!(env.fake.calls().is_empty(), "{:?}", env.fake.calls());
}

#[gpui::test]
async fn a_group_moves_with_its_first_leaf(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    add_phase(&env, cx).await;
    begin(&env, "l10", cx).await;
    wait_until(cx, "the group", |cx| started(&env, "phase", cx)).await;
    assert!(started(&env, "one", cx));
    assert_eq!(
        sets(&env),
        vec![
            ("uuid-SEQ-102".to_string(), "doing".to_string()),
            ("uuid-SEQ-101".to_string(), "doing".to_string()),
        ]
    );
    // The second leaf moves its own ticket; the group has moved already.
    begin(&env, "l11", cx).await;
    wait_until(cx, "the second leaf", |cx| started(&env, "two", cx)).await;
    settle(cx).await;
    assert_eq!(sets(&env).len(), 3);
    assert_eq!(
        sets(&env)
            .iter()
            .filter(|(issue, _)| issue == "uuid-SEQ-101")
            .count(),
        1
    );
}

#[gpui::test]
async fn nothing_is_sent_while_off_or_without_a_token(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, false, true).await;
    prepare(&env, cx);
    begin(&env, "l2", cx).await;
    drop_it(&env, "l2", false, cx).await;
    settle(cx).await;
    assert!(env.fake.calls().is_empty());
    assert!(!started(&env, "beta", cx));
    assert!(!asking(&env, cx));
}

#[gpui::test]
async fn nothing_is_sent_without_a_token(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, false).await;
    prepare(&env, cx);
    begin(&env, "l2", cx).await;
    settle(cx).await;
    assert!(env.fake.calls().is_empty());
    assert!(!started(&env, "beta", cx));
}

/// A client that fails its first `set_state` and counts the attempts.
struct Flaky {
    inner: FakeLinear,
    fail_next: AtomicUsize,
    attempts: Arc<AtomicUsize>,
}

impl LinearApi for Flaky {
    fn issues(&self, ids: Vec<IssueRef>) -> BoxFuture<'static, Result<Vec<Issue>, ApiError>> {
        self.inner.issues(ids)
    }

    fn recent(&self, limit: usize) -> BoxFuture<'static, Result<Vec<IssueSummary>, ApiError>> {
        self.inner.recent(limit)
    }

    fn team_states(
        &self,
        team_id: &str,
    ) -> BoxFuture<'static, Result<Vec<WorkflowState>, ApiError>> {
        self.inner.team_states(team_id)
    }

    fn set_state(
        &self,
        issue_id: &str,
        state_id: &str,
    ) -> BoxFuture<'static, Result<(), ApiError>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .fail_next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return async { Err(ApiError::Network("down".to_string())) }.boxed();
        }
        self.inner.set_state(issue_id, state_id)
    }

    fn create(&self, input: NewIssue) -> BoxFuture<'static, Result<IssueSummary, ApiError>> {
        self.inner.create(input)
    }

    fn viewer(&self) -> BoxFuture<'static, Result<String, ApiError>> {
        self.inner.viewer()
    }
}

#[gpui::test]
async fn a_failed_write_records_nothing_and_is_retried_by_the_next_refresh(
    cx: &mut TestAppContext,
) {
    let (env, cx) = start(cx).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let flaky = Arc::new(Flaky {
        inner: env.fake.clone(),
        fail_next: AtomicUsize::new(1),
        attempts: attempts.clone(),
    });
    cx.update(|_, cx| {
        // A refresh reads the token again and builds the client with the factory.
        let client = flaky.clone();
        token::install_client_factory(Arc::new(move |_| client.clone() as Arc<dyn LinearApi>), cx);
        let linear = sync::global(cx).unwrap();
        linear.update(cx, |linear, _| linear.set_token(true, Some(flaky)));
    });
    begin(&env, "l2", cx).await;
    wait_until(cx, "the failed attempt", |_| {
        attempts.load(Ordering::SeqCst) == 1
    })
    .await;
    settle(cx).await;
    assert!(!started(&env, "beta", cx), "nothing is recorded");
    assert_eq!(env.fake.issue("SEQ-5").unwrap().state.id, "backlog");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "a change does not retry"
    );

    dispatch("task::RefreshFromLinear", cx);
    wait_until(cx, "started_sent", |cx| started(&env, "beta", cx)).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(env.fake.issue("SEQ-5").unwrap().state.id, "doing");
    assert_eq!(
        sets(&env).len(),
        1,
        "the fake saw only the write that worked"
    );
}

// ---------------------------------------------------------------------------------------------
// D14: cancel

#[gpui::test]
async fn a_dropped_leaf_asks_and_leave_writes_nothing(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    drop_it(&env, "l2", false, cx).await;
    wait_question(&env, cx).await;
    cx.simulate_keystrokes("l");
    wait_until(cx, "the answer", |cx| {
        cancel_record(&env, "beta", cx).is_some()
    })
    .await;
    assert_eq!(
        cancel_record(&env, "beta", cx),
        Some(LinearCancel::Declined)
    );
    settle(cx).await;
    assert!(sets(&env).is_empty());
    assert!(!asking(&env, cx), "asked once");
    assert_eq!(env.fake.issue("SEQ-5").unwrap().state.id, "backlog");
}

#[gpui::test]
async fn cancel_writes_once_and_is_not_asked_again(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    drop_it(&env, "l2", false, cx).await;
    wait_question(&env, cx).await;
    cx.simulate_keystrokes("c");
    wait_until(cx, "the answer", |cx| {
        cancel_record(&env, "beta", cx).is_some()
    })
    .await;
    assert_eq!(
        cancel_record(&env, "beta", cx),
        Some(LinearCancel::Cancelled)
    );
    assert_eq!(
        sets(&env),
        vec![("uuid-SEQ-5".to_string(), "canceled".to_string())],
        "the canceled state with the lowest position"
    );
    dispatch("task::RefreshFromLinear", cx);
    settle(cx).await;
    assert!(!asking(&env, cx));
    assert_eq!(sets(&env).len(), 1);
}

#[gpui::test]
async fn reopen_clears_the_answer_and_writes_nothing(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    drop_it(&env, "l2", false, cx).await;
    wait_question(&env, cx).await;
    cx.simulate_keystrokes("l");
    wait_until(cx, "the answer", |cx| {
        cancel_record(&env, "beta", cx).is_some()
    })
    .await;
    run(&env, cx, |tree, ctx| reopen(tree, ctx, "l2")).await;
    wait_until(cx, "the cleared record", |cx| {
        cancel_record(&env, "beta", cx).is_none()
    })
    .await;
    settle(cx).await;
    assert!(sets(&env).is_empty(), "no Linear reopen");
    assert!(!asking(&env, cx));
    // Dropped again, it is asked again.
    drop_it(&env, "l2", false, cx).await;
    wait_question(&env, cx).await;
    cx.simulate_keystrokes("l");
    wait_until(cx, "the second answer", |cx| {
        cancel_record(&env, "beta", cx).is_some()
    })
    .await;
    assert!(sets(&env).is_empty());
}

#[gpui::test]
async fn a_dropped_stacked_leaf_is_not_asked_and_says_so(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    add(&env, None, "stk", "l12", Some(6), true, Some("beta"), cx).await;
    drop_it(&env, "l12", false, cx).await;
    wait_until(cx, "the toast", |cx| env.last_toast(cx).contains("stacked")).await;
    assert!(
        env.last_toast(cx).contains("SEQ-6"),
        "{}",
        env.last_toast(cx)
    );
    assert!(!asking(&env, cx));
    assert!(cancel_record(&env, "stk", cx).is_none());
    assert!(env.fake.calls().is_empty());
}

/// Drops the group `phase` with its leaves, answers `key` and checks which tickets were moved
/// and how each task's answer was recorded (group, then leaf).
async fn group_answer(
    key: &str,
    expected: Vec<&str>,
    records: (LinearCancel, LinearCancel),
    cx: &mut TestAppContext,
) {
    let (env, cx) = start(cx).await;
    add_phase(&env, cx).await;
    drop_it(&env, "g9", true, cx).await;
    wait_question(&env, cx).await;
    cx.simulate_keystrokes(key);
    wait_until(cx, "the answers", |cx| {
        ["phase", "one", "two"]
            .iter()
            .all(|slug| cancel_record(&env, slug, cx).is_some())
    })
    .await;
    settle(cx).await;
    let written: Vec<String> = sets(&env).into_iter().map(|(issue, _)| issue).collect();
    assert_eq!(written, expected, "answer {key}");
    assert!(
        !asking(&env, cx),
        "one question for the group and its leaves"
    );
    assert_eq!(cancel_record(&env, "phase", cx), Some(records.0));
    assert_eq!(cancel_record(&env, "one", cx), Some(records.1));
    assert_eq!(cancel_record(&env, "two", cx), Some(records.1));
}

#[gpui::test]
async fn a_dropped_group_asks_once_and_leave_all_writes_nothing(cx: &mut TestAppContext) {
    let both = (LinearCancel::Declined, LinearCancel::Declined);
    group_answer("l", vec![], both, cx).await;
}

#[gpui::test]
async fn a_dropped_group_can_cancel_its_leaves_only(cx: &mut TestAppContext) {
    let records = (LinearCancel::Declined, LinearCancel::Cancelled);
    group_answer("o", vec!["uuid-SEQ-102", "uuid-SEQ-103"], records, cx).await;
}

#[gpui::test]
async fn a_dropped_group_can_cancel_all(cx: &mut TestAppContext) {
    let both = (LinearCancel::Cancelled, LinearCancel::Cancelled);
    let all = vec!["uuid-SEQ-102", "uuid-SEQ-103", "uuid-SEQ-101"];
    group_answer("a", all, both, cx).await;
}

#[gpui::test]
async fn a_group_with_an_open_leaf_is_never_cancelled(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    add_phase(&env, cx).await;
    drop_it(&env, "g9", true, cx).await;
    wait_question(&env, cx).await;
    // A leaf is open again while the question is up.
    run(&env, cx, |tree, ctx| reopen(tree, ctx, "l11")).await;
    cx.run_until_parked();
    cx.simulate_keystrokes("a");
    wait_until(cx, "the leaf's answer", |cx| {
        cancel_record(&env, "one", cx).is_some()
    })
    .await;
    settle(cx).await;
    assert_eq!(
        sets(&env),
        vec![("uuid-SEQ-102".to_string(), "canceled".to_string())],
        "only the cancelled leaf, whatever the answer"
    );
    assert_eq!(env.fake.issue("SEQ-101").unwrap().state.id, "backlog");
    assert_eq!(env.fake.issue("SEQ-103").unwrap().state.id, "backlog");
    assert!(cancel_record(&env, "two", cx).is_none());
    assert!(
        cancel_record(&env, "phase", cx).is_none(),
        "not answered while a leaf is open"
    );
    assert!(!asking(&env, cx));
}

// ---------------------------------------------------------------------------------------------
// The promise

#[gpui::test]
async fn nothing_else_reaches_set_state(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    let pr = |state| TaskResult {
        kind: ResultKind::Pr,
        url: Some("https://github.com/acme/app/pull/7".to_string()),
        state: Some(state),
        urls: Vec::new(),
        extra: Default::default(),
    };
    run(&env, cx, move |tree, ctx| {
        set_result(tree, ctx, "l2", pr(ResultState::Open))
    })
    .await;
    run(&env, cx, move |tree, ctx| {
        set_result(tree, ctx, "l2", pr(ResultState::Merged))
    })
    .await;
    run(&env, cx, |tree, ctx| advance(tree, ctx, "l2", false)).await;
    run(&env, cx, |tree, ctx| advance(tree, ctx, "l2", false)).await;
    dispatch("task::RefreshFromLinear", cx);
    settle(cx).await;
    run(&env, cx, |tree, ctx| close(tree, ctx, "l2")).await;
    dispatch("task::RefreshFromLinear", cx);
    settle(cx).await;
    assert!(sets(&env).is_empty(), "{:?}", env.fake.calls());
    assert!(!started(&env, "beta", cx));
    assert!(!asking(&env, cx));
}
