//! Tests of `task: show linear suggestions` over a real work store in a temporary directory and a
//! fake Linear: a new child of the group's ticket shows once, accepting makes a linked leaf with a
//! plan todo (or links the leaf whose goal matches), dismissing never comes back, and nothing is
//! written to Linear.

use bongos_linear_model::fake::Call;
use bongos_minibuffer::ReadKeyView;
use bongos_work::RowChipsRegistry;
use bongos_work_model::TodoRole;
use gpui::{Entity, TestAppContext, VisualTestContext};

use crate::actions_tests::dispatch;
use crate::link_tests::{Env, env, issue, wait_until};
use crate::refresh;
use crate::suggestions::SuggestionPicker;

/// The group linked to SEQ-100 in the seeded tree, and its leaf `sub` (goal "Sub").
const GROUP: &str = "g2";

/// Tickets 130 "Handle refunds" and 131 "Sub", both under SEQ-100; the leaf `sub` has the goal
/// of the second.
async fn start(cx: &mut TestAppContext) -> (Env, &mut VisualTestContext) {
    let (env, cx) = env(cx, true, true).await;
    for (number, title) in [(130, "Handle refunds"), (131, "Sub")] {
        let mut child = issue(number);
        child.title = title.to_string();
        child.parent = Some(issue(100).summary());
        env.fake.add_issue(child);
    }
    // The test creates the store after the sync: attach the refresh now.
    cx.update(|_, cx| refresh::connect(cx));
    env.focus(GROUP, cx);
    cx.run_until_parked();
    (env, cx)
}

fn chips(cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_, cx| RowChipsRegistry::chips(GROUP, cx))
        .iter()
        .map(|chip| chip.to_string())
        .collect()
}

/// Refreshes the group and waits until its chip says `chip`.
async fn refresh_until(chip: &str, cx: &mut VisualTestContext) {
    dispatch("task::RefreshFromLinear", cx);
    wait_until(cx, chip, |cx| chips(cx) == vec![chip.to_string()]).await;
}

fn picker(env: &Env, cx: &mut VisualTestContext) -> Option<Entity<SuggestionPicker>> {
    env.workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SuggestionPicker>(cx)
    })
}

/// Opens the picker with the command and returns its title and rows.
async fn open(env: &Env, cx: &mut VisualTestContext) -> (String, Vec<String>) {
    dispatch("task::ShowLinearSuggestions", cx);
    wait_until(cx, "the picker", |cx| picker(env, cx).is_some()).await;
    contents(env, cx)
}

fn contents(env: &Env, cx: &mut VisualTestContext) -> (String, Vec<String>) {
    picker(env, cx).unwrap().read_with(cx, |picker, _| {
        (picker.title().to_string(), picker.rows().to_vec())
    })
}

fn writes_to_linear(env: &Env) -> usize {
    env.fake
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::Create { .. } | Call::SetState { .. }))
        .count()
}

fn task_count(env: &Env, cx: &mut VisualTestContext) -> usize {
    env.tree(cx).tasks().len()
}

#[gpui::test]
async fn a_new_child_shows_once_as_a_chip_and_a_row(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    // Nothing is known before the first refresh.
    dispatch("task::ShowLinearSuggestions", cx);
    cx.run_until_parked();
    assert!(picker(&env, cx).is_none());
    assert_eq!(env.last_toast(cx), "No suggestions under SEQ-100");

    let tasks = task_count(&env, cx);
    refresh_until("2 suggestions", cx).await;
    // A second refresh does not list them twice.
    refresh_until("2 suggestions", cx).await;
    let (title, rows) = open(&env, cx).await;
    assert_eq!(title, "2 suggestions under SEQ-100");
    assert_eq!(
        rows,
        vec![
            "SEQ-130: Handle refunds".to_string(),
            "SEQ-131: Sub  (link to Sub)".to_string()
        ]
    );
    // Never a task by themselves, never a write to Linear.
    assert_eq!(task_count(&env, cx), tasks);
    assert_eq!(writes_to_linear(&env), 0);
}

#[gpui::test]
async fn accepting_makes_a_linked_leaf_with_a_plan_todo_in_one_write(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    refresh_until("2 suggestions", cx).await;
    open(&env, cx).await;
    let writes = env.writes(cx);
    dispatch("menu::Confirm", cx);
    wait_until(cx, "the new leaf", |cx| {
        env.tree(cx)
            .tasks()
            .iter()
            .any(|task| task.slug == "handle-refunds")
    })
    .await;
    assert_eq!(env.writes(cx), writes + 1, "one write");

    let file = env.file("handle-refunds", cx);
    assert_eq!(file.goal, "Handle refunds");
    assert_eq!(file.linear.as_deref(), Some("SEQ-130"));
    let state = file.linear_state.expect("the link record");
    assert_eq!(state.issue_id, "uuid-SEQ-130");
    assert_eq!(state.team_id, "team");
    assert_eq!(state.description, "Description 130");
    assert_eq!(file.todos.len(), 1);
    assert_eq!(file.todos[0].name, "plan");
    assert_eq!(file.todos[0].role, Some(TodoRole::Plans));
    // Under the group, and the leaf `sub` is untouched.
    let tree = env.tree(cx);
    let group = tree.task_by_id(GROUP).unwrap();
    assert!(group.children.iter().any(|child| child.slug == "handle-refunds"));
    assert!(env.file("sub", cx).linear.is_none());
    assert_eq!(writes_to_linear(&env), 0, "nothing is written to Linear");

    // The suggestion is gone, the chip shrinks and the picker lists what is left.
    wait_until(cx, "the shorter picker", |cx| {
        picker(&env, cx).is_some_and(|picker| picker.read_with(cx, |p, _| p.rows().len() == 1))
    })
    .await;
    assert_eq!(chips(cx), vec!["1 suggestion".to_string()]);
    refresh_until("1 suggestion", cx).await;
}

#[gpui::test]
async fn a_matching_goal_offers_a_link_and_accept_links_that_leaf(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    refresh_until("2 suggestions", cx).await;
    let (_, rows) = open(&env, cx).await;
    assert_eq!(rows[1], "SEQ-131: Sub  (link to Sub)");
    let tasks = task_count(&env, cx);
    dispatch("menu::SelectNext", cx);
    dispatch("menu::Confirm", cx);
    wait_until(cx, "the link", |cx| {
        env.file("sub", cx).linear.as_deref() == Some("SEQ-131")
    })
    .await;
    let file = env.file("sub", cx);
    assert_eq!(file.goal, "Sub");
    assert_eq!(file.linear_state.unwrap().issue_id, "uuid-SEQ-131");
    assert_eq!(task_count(&env, cx), tasks, "no new leaf");
    assert_eq!(writes_to_linear(&env), 0);
}

fn read_key(env: &Env, cx: &mut VisualTestContext) -> bool {
    env.workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<ReadKeyView>(cx).is_some()
    })
}

#[gpui::test]
async fn a_dismissed_suggestion_never_returns(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    refresh_until("2 suggestions", cx).await;
    let tasks = task_count(&env, cx);
    open(&env, cx).await;
    let writes = env.writes(cx);
    // Dismissing asks first: "n" changes nothing.
    cx.simulate_keystrokes("d");
    wait_until(cx, "the question", |cx| read_key(&env, cx)).await;
    cx.simulate_keystrokes("n");
    cx.run_until_parked();
    assert_eq!(env.writes(cx), writes);
    assert!(
        env.file("stream", cx)
            .linear_state
            .unwrap()
            .dismissed
            .is_empty()
    );
    assert_eq!(chips(cx), vec!["2 suggestions".to_string()]);

    // Open again, dismiss the first row and answer yes.
    let (_, _) = open(&env, cx).await;
    cx.simulate_keystrokes("d");
    wait_until(cx, "the question", |cx| read_key(&env, cx)).await;
    cx.simulate_keystrokes("y");
    wait_until(cx, "the record", |cx| {
        !env.file("stream", cx)
            .linear_state
            .unwrap()
            .dismissed
            .is_empty()
    })
    .await;
    assert_eq!(env.writes(cx), writes + 1, "one write");
    let dismissed = env.file("stream", cx).linear_state.unwrap().dismissed;
    assert_eq!(dismissed, vec!["SEQ-130".to_string()]);
    assert_eq!(chips(cx), vec!["1 suggestion".to_string()]);

    // A refresh does not bring it back, and it is never a task.
    refresh_until("1 suggestion", cx).await;
    dispatch("menu::Cancel", cx);
    cx.run_until_parked();
    let (title, rows) = open(&env, cx).await;
    assert_eq!(title, "1 suggestion under SEQ-100");
    assert_eq!(rows, vec!["SEQ-131: Sub  (link to Sub)".to_string()]);
    assert_eq!(task_count(&env, cx), tasks);
    assert_eq!(writes_to_linear(&env), 0);
}
