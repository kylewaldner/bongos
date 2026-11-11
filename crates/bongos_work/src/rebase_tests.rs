//! "Rebase needed" and "retarget needed" (D16) against real git repositories in temporary
//! directories, with the real git binary. The 30 s timer and the focus hook call the same
//! `refresh_rebase` these tests call.

use bongos_work_model::{StackedOn, Status, UnblockPoint, layout, serialize_task};
use gpui::{TestAppContext, VisualTestContext};
use std::path::PathBuf;
use std::time::Duration;

use crate::command_tests::{Input, dispatch, wait_until};
use crate::rebase::RebaseState;
use crate::store_tests::write;
use crate::worktree_tests::{Setup, git, setup};

const BETA: &str = "l3";

/// Rechecks until `check` holds on beta's state.
async fn until(
    setup: &Setup,
    cx: &mut VisualTestContext,
    what: &str,
    check: impl Fn(Option<&RebaseState>) -> bool,
) {
    for _ in 0..200 {
        setup
            .harness
            .store
            .update(cx, |store, cx| store.refresh_rebase(cx));
        cx.run_until_parked();
        if setup
            .harness
            .store
            .read_with(cx, |store, _| check(store.rebase_state(BETA)))
        {
            return;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn state(setup: &Setup, cx: &mut VisualTestContext) -> Option<RebaseState> {
    setup
        .harness
        .store
        .read_with(cx, |store, _| store.rebase_state(BETA).cloned())
}

/// Beta stacked on alpha, both started: alpha has one commit and is implemented, beta was cut
/// from it. Returns alpha's head then.
async fn stacked(setup: &Setup, cx: &mut VisualTestContext) -> String {
    let harness = &setup.harness;
    let mut beta = harness.file(cx, &["phase", "beta"]);
    beta.stacked_on = Some(StackedOn {
        task: "alpha".to_string(),
        unblock: UnblockPoint::Implemented,
        extra: Default::default(),
    });
    write(
        &layout::task_file(&harness.task_dir(&["phase", "beta"])),
        &serialize_task(&beta).unwrap(),
    );
    wait_until(cx, "the edge to load", |cx| {
        harness.file(cx, &["phase", "beta"]).stacked_on.is_some()
    })
    .await;
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("alpha")])
        .await;
    commit_alpha(setup, cx, "alpha work");
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("alpha")])
        .await;
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("beta")])
        .await;
    let head = git(&setup.real, &["rev-parse", "alpha"]);
    assert_eq!(
        harness.file(cx, &["phase", "beta"]).base_sha.as_deref(),
        Some(head.as_str())
    );
    head
}

/// The worktree the task at `path` records.
fn worktree_of(setup: &Setup, cx: &mut VisualTestContext, path: &[&str]) -> PathBuf {
    PathBuf::from(
        setup
            .harness
            .file(cx, path)
            .worktree
            .expect("a recorded worktree"),
    )
}

fn commit_alpha(setup: &Setup, cx: &mut VisualTestContext, message: &str) -> String {
    let worktree = worktree_of(setup, cx, &["phase", "alpha"]);
    git(&worktree, &["commit", "-q", "--allow-empty", "-m", message]);
    git(&setup.real, &["rev-parse", "alpha"])
}

#[gpui::test]
async fn moving_the_parent_flags_the_child_and_rebasing_clears_it(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    stacked(&setup, cx).await;
    until(&setup, cx, "no flag at first", |s| s.is_none()).await;

    let moved = commit_alpha(&setup, cx, "alpha moves");
    until(&setup, cx, "the flag", |s| s.is_some()).await;
    assert_eq!(
        state(&setup, cx),
        Some(RebaseState::RebaseNeeded { parent_head: moved })
    );

    let beta_worktree = worktree_of(&setup, cx, &["phase", "beta"]);
    git(&beta_worktree, &["rebase", "-q", "alpha"]);
    until(&setup, cx, "the flag to clear", |s| s.is_none()).await;
}

#[gpui::test]
async fn clear_rebase_holds_until_the_parent_moves_again(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    stacked(&setup, cx).await;

    // Nothing to clear yet: a toast says so and nothing is written.
    let writes = harness.writes(cx);
    harness.clear_toasts(cx);
    dispatch(cx, "task::ClearRebase");
    assert!(harness.has_toast(cx));
    assert_eq!(harness.writes(cx), writes);

    let moved = commit_alpha(&setup, cx, "alpha moves");
    until(&setup, cx, "the flag", |s| s.is_some()).await;
    harness
        .run_and_wait(cx, "task::ClearRebase", &[Input::Pick("beta")])
        .await;
    assert_eq!(
        harness.file(cx, &["phase", "beta"]).rebase_ack_sha,
        Some(moved)
    );
    until(&setup, cx, "the flag to clear", |s| s.is_none()).await;

    // The parent moves again: flagged again.
    let moved_again = commit_alpha(&setup, cx, "alpha moves again");
    until(&setup, cx, "the flag again", |s| s.is_some()).await;
    assert_eq!(
        state(&setup, cx),
        Some(RebaseState::RebaseNeeded {
            parent_head: moved_again
        })
    );
}

#[gpui::test]
async fn a_cancelled_parent_shows_retarget_needed(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    stacked(&setup, cx).await;
    commit_alpha(&setup, cx, "alpha moves");
    until(&setup, cx, "the flag", |s| s.is_some()).await;

    let mut alpha = harness.file(cx, &["phase", "alpha"]);
    alpha.status = Status::Cancelled;
    write(
        &layout::task_file(&harness.task_dir(&["phase", "alpha"])),
        &serialize_task(&alpha).unwrap(),
    );
    until(&setup, cx, "retarget needed", |s| {
        s == Some(&RebaseState::RetargetNeeded)
    })
    .await;

    // A retarget removes the edge, and the state with it.
    harness
        .run_and_wait(
            cx,
            "task::Retarget",
            &[Input::Pick("beta"), Input::Pick("main")],
        )
        .await;
    until(&setup, cx, "the state to clear", |s| s.is_none()).await;
}
