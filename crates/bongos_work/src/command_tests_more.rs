//! The edge, result, structure and `project` commands, driven like the ones in `command_tests`:
//! an action dispatched by name in a workspace, its pickers and prompts answered, and the result
//! read back from the store or the disk.

use bongos_work_model::activity::to_utc;
use bongos_work_model::layout;
use bongos_work_model::slug::oneoff_dir_name;
use bongos_work_model::store::WorkTree;
use bongos_work_model::{ResultKind, ResultState, Status, UnblockPoint};
use gpui::TestAppContext;

use crate::command_tests::{Input, dispatch, harness, wait_until};
use crate::links::blockers_message;

#[gpui::test]
async fn edges_are_added_changed_and_removed(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;

    harness
        .run_and_wait(
            cx,
            "task::AddEdge",
            &[
                Input::Pick("beta"),
                Input::Pick("alpha"),
                Input::Pick("after"),
                Input::Pick("merged"),
            ],
        )
        .await;
    let edge = harness.file(cx, &["phase", "beta"]).after;
    assert_eq!(edge.len(), 1);
    assert_eq!(edge[0].task, "alpha");
    assert_eq!(edge[0].unblock, UnblockPoint::Merged);

    // Beta waits for alpha, and the command that says so names alpha.
    let tree = harness.tree(cx);
    let beta = tree
        .task_by_dir(&harness.task_dir(&["phase", "beta"]))
        .unwrap();
    let message = blockers_message(&tree, beta.id().unwrap());
    assert!(
        message.contains("beta") && message.contains("alpha"),
        "{message}"
    );
    harness.clear_toasts(cx);
    harness
        .run(cx, "task::ShowBlockers", &[Input::Pick("beta")])
        .await;
    assert!(harness.has_toast(cx));

    harness
        .run_and_wait(
            cx,
            "task::SetUnblockPoint",
            &[
                Input::Pick("beta"),
                Input::Pick("after alpha"),
                Input::Pick("implemented"),
            ],
        )
        .await;
    assert_eq!(
        harness.file(cx, &["phase", "beta"]).after[0].unblock,
        UnblockPoint::Implemented
    );

    harness
        .run_and_wait(
            cx,
            "task::RemoveEdge",
            &[Input::Pick("beta"), Input::Pick("after alpha")],
        )
        .await;
    assert!(harness.file(cx, &["phase", "beta"]).after.is_empty());
    let tree = harness.tree(cx);
    let beta = tree
        .task_by_dir(&harness.task_dir(&["phase", "beta"]))
        .unwrap();
    assert!(blockers_message(&tree, beta.id().unwrap()).contains("not blocked"));
}

#[gpui::test]
async fn a_stacked_task_is_retargeted_to_main(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(
            cx,
            "task::AddEdge",
            &[
                Input::Pick("beta"),
                Input::Pick("alpha"),
                Input::Pick("stacked on"),
                Input::Pick("review"),
            ],
        )
        .await;
    let stacked = harness.file(cx, &["phase", "beta"]).stacked_on.unwrap();
    assert_eq!(stacked.task, "alpha");
    assert_eq!(stacked.unblock, UnblockPoint::Review);

    harness
        .run_and_wait(
            cx,
            "task::Retarget",
            &[Input::Pick("beta"), Input::Pick("main")],
        )
        .await;
    assert!(harness.file(cx, &["phase", "beta"]).stacked_on.is_none());
}

#[gpui::test]
async fn refused_edges_reach_the_toast(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let add = |task: &'static str, target: &'static str| {
        [
            Input::Pick(task),
            Input::Pick(target),
            Input::Pick("after"),
            Input::Pick("merged"),
        ]
    };
    harness
        .run_and_wait(cx, "task::AddEdge", &add("beta", "alpha"))
        .await;

    // The same edge again.
    harness.clear_toasts(cx);
    let writes = harness.writes(cx);
    harness
        .run(cx, "task::AddEdge", &add("beta", "alpha"))
        .await;
    wait_until(cx, "the duplicate edge toast", |cx| harness.has_toast(cx)).await;

    // An edge that closes a cycle.
    harness.clear_toasts(cx);
    harness
        .run(cx, "task::AddEdge", &add("alpha", "beta"))
        .await;
    wait_until(cx, "the cycle toast", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);
    assert!(harness.file(cx, &["phase", "alpha"]).after.is_empty());

    // Only the task with an edge is offered to `remove edge`.
    dispatch(cx, "task::RemoveEdge");
    let rows = harness.choices(cx);
    assert_eq!(rows.len(), 1, "only beta has an edge: {rows:?}");
}

#[gpui::test]
async fn merge_ship_and_results_are_set_and_cleared(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(
            cx,
            "task::SetMergeShip",
            &[Input::Pick("backfill"), Input::Pick("off")],
        )
        .await;
    assert_eq!(harness.file(cx, &["backfill"]).merge_is_ship, Some(false));
    harness
        .run_and_wait(
            cx,
            "task::SetMergeShip",
            &[Input::Pick("backfill"), Input::Pick("repository")],
        )
        .await;
    assert_eq!(harness.file(cx, &["backfill"]).merge_is_ship, None);

    harness
        .run_and_wait(
            cx,
            "task::SetResult",
            &[
                Input::Pick("backfill"),
                Input::Pick("pr"),
                Input::Text("https://example.com/pull/1"),
                Input::Pick("open"),
            ],
        )
        .await;
    let result = harness.file(cx, &["backfill"]).result.unwrap();
    assert_eq!(result.kind, ResultKind::Pr);
    assert_eq!(result.url.as_deref(), Some("https://example.com/pull/1"));
    assert_eq!(result.state, Some(ResultState::Open));

    // A different result is refused until the first is cleared.
    harness.clear_toasts(cx);
    harness
        .run(
            cx,
            "task::SetResult",
            &[
                Input::Pick("backfill"),
                Input::Pick("link"),
                Input::Text("https://example.com/other"),
                Input::Pick("no state"),
            ],
        )
        .await;
    wait_until(cx, "the refusal toast", |cx| harness.has_toast(cx)).await;
    assert_eq!(
        harness.file(cx, &["backfill"]).result.unwrap().kind,
        ResultKind::Pr
    );

    harness
        .run_and_wait(cx, "task::ClearResult", &[Input::Pick("backfill")])
        .await;
    assert!(harness.file(cx, &["backfill"]).result.is_none());
}

#[gpui::test]
async fn split_asks_first_and_is_refused_on_a_group_or_a_stacked_task(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;

    // A group.
    harness.clear_toasts(cx);
    harness
        .run(cx, "task::Split", &[Input::Pick("phase")])
        .await;
    wait_until(cx, "the group toast", |cx| harness.has_toast(cx)).await;
    assert!(harness.prompt(cx).is_none() && harness.picker(cx).is_none());

    // A stacked task.
    harness
        .run_and_wait(
            cx,
            "task::AddEdge",
            &[
                Input::Pick("beta"),
                Input::Pick("alpha"),
                Input::Pick("stacked on"),
                Input::Pick("review"),
            ],
        )
        .await;
    harness.clear_toasts(cx);
    harness.run(cx, "task::Split", &[Input::Pick("beta")]).await;
    wait_until(cx, "the stacked toast", |cx| harness.has_toast(cx)).await;
    assert!(harness.prompt(cx).is_none() && harness.picker(cx).is_none());

    // A plain leaf asks, and the default answer changes nothing.
    let before = harness.writes(cx);
    harness
        .run(
            cx,
            "task::Split",
            &[Input::Pick("backfill"), Input::Text("Implementation")],
        )
        .await;
    let rows = harness.choices(cx);
    assert!(rows[0].starts_with("Split: "), "{rows:?}");
    assert_eq!(rows[1], "Cancel");
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(harness.writes(cx), before);
    assert!(!harness.task_dir(&["backfill", "implementation"]).exists());

    harness
        .run_and_wait(
            cx,
            "task::Split",
            &[
                Input::Pick("backfill"),
                Input::Text("Implementation"),
                Input::Pick("Split"),
            ],
        )
        .await;
    let parent = harness.file(cx, &["backfill"]);
    let child = harness.file(cx, &["backfill", "implementation"]);
    assert!(parent.todos.is_empty());
    assert_eq!(child.todos.len(), 3);
    assert_eq!(parent.history.last().unwrap().event, "split");
}

#[gpui::test]
async fn move_asks_first_and_a_project_top_cannot_move(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;

    harness.clear_toasts(cx);
    harness
        .run(cx, "task::Move", &[Input::Pick("invoicing")])
        .await;
    wait_until(cx, "the project toast", |cx| harness.has_toast(cx)).await;
    assert!(harness.picker(cx).is_none());

    let before = harness.writes(cx);
    harness
        .run(
            cx,
            "task::Move",
            &[Input::Pick("beta"), Input::Pick("backfill")],
        )
        .await;
    let rows = harness.choices(cx);
    assert!(rows[0].starts_with("Move: "), "{rows:?}");
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(harness.writes(cx), before);
    assert!(harness.task_dir(&["phase", "beta"]).exists());

    harness
        .run_and_wait(
            cx,
            "task::Move",
            &[
                Input::Pick("beta"),
                Input::Pick("backfill"),
                Input::Pick("Move"),
            ],
        )
        .await;
    assert!(!harness.task_dir(&["phase", "beta"]).exists());
    assert_eq!(harness.file(cx, &["backfill", "beta"]).goal, "Beta");
    // The plain leaf that gained a child gave up its todos.
    assert!(harness.file(cx, &["backfill"]).todos.is_empty());
}

#[gpui::test]
async fn a_one_off_goes_under_its_activity_with_a_dated_directory(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;

    harness
        .run_and_wait(
            cx,
            "task::NewOneOff",
            &[Input::Text("Research"), Input::Text("Look at proration")],
        )
        .await;
    let tree = harness.tree(cx);
    let created = tree
        .activities
        .iter()
        .find(|activity| activity.slug == "research")
        .expect("the activity directory");
    assert_eq!(created.oneoffs.len(), 1);
    let file = created.oneoffs[0].file.as_ref().unwrap();
    let date = to_utc(&file.created).unwrap().date_naive();
    let name = oneoff_dir_name(date, "look-at-proration");
    assert_eq!(
        created.oneoffs[0].dir,
        layout::oneoff_dir(&harness.root, "research", &name)
    );
    assert_eq!(file.activity.as_deref(), Some("Research"));

    // Making a project task a one-off asks for the activity, then confirms.
    let before = harness.writes(cx);
    harness
        .run(
            cx,
            "task::MakeOneOff",
            &[Input::Pick("backfill"), Input::Text("Spikes")],
        )
        .await;
    let rows = harness.choices(cx);
    assert!(rows[0].starts_with("Make one-off: "), "{rows:?}");
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(harness.writes(cx), before);

    harness
        .run_and_wait(
            cx,
            "task::MakeOneOff",
            &[
                Input::Pick("backfill"),
                Input::Text("Spikes"),
                Input::Pick("Make"),
            ],
        )
        .await;
    let tree = harness.tree(cx);
    let spikes = tree
        .activities
        .iter()
        .find(|activity| activity.slug == "spikes")
        .expect("the activity directory");
    let moved = &spikes.oneoffs[0];
    let date = to_utc(&moved.file.as_ref().unwrap().created)
        .unwrap()
        .date_naive();
    assert_eq!(
        moved.dir,
        layout::oneoff_dir(&harness.root, "spikes", &oneoff_dir_name(date, "backfill"))
    );
    assert!(!harness.task_dir(&["backfill"]).exists());
}

#[gpui::test]
async fn a_task_moves_to_another_project_and_back_with_its_files_and_history(
    cx: &mut TestAppContext,
) {
    let (harness, cx) = harness(cx, true, true).await;

    // `project: new` makes the project and runs git init in its directory.
    harness
        .run_and_wait(cx, "project::New", &[Input::Text("Billing")])
        .await;
    let billing = layout::project_dir(&harness.root, "billing");
    wait_until(cx, "git init", |_| billing.join(".git").exists()).await;
    assert!(layout::project_file(&billing).exists());

    let backfill = harness.task_dir(&["backfill"]);
    std::fs::write(backfill.join("notes.md"), "keep me").unwrap();
    let before = harness.file(cx, &["backfill"]);

    harness
        .run_and_wait(
            cx,
            "task::MoveToProject",
            &[
                Input::Pick("backfill"),
                Input::Pick("billing"),
                Input::Pick("Move to project"),
            ],
        )
        .await;
    let there = layout::child_dir(&billing, "backfill");
    assert!(!backfill.exists());
    assert_eq!(
        std::fs::read_to_string(there.join("notes.md")).unwrap(),
        "keep me"
    );
    let tree = harness.tree(cx);
    let moved = tree.task_by_dir(&there).unwrap().file.clone().unwrap();
    assert_eq!(moved.id, before.id);
    assert_eq!(moved.history.last().unwrap().event, "moved to project");

    harness
        .run_and_wait(
            cx,
            "task::Move",
            &[
                Input::Pick("backfill"),
                Input::Pick("invoicing"),
                Input::Pick("Move"),
            ],
        )
        .await;
    assert!(!there.exists());
    assert_eq!(
        std::fs::read_to_string(backfill.join("notes.md")).unwrap(),
        "keep me"
    );
    let back = harness.file(cx, &["backfill"]);
    assert_eq!(back.id, before.id);
    assert_eq!(back.todos, before.todos);
    let events: Vec<&str> = back
        .history
        .iter()
        .map(|entry| entry.event.as_str())
        .collect();
    assert_eq!(
        events[before.history.len()..],
        ["moved to project", "moved"],
        "{events:?}"
    );
}

#[gpui::test]
async fn projects_are_archived_unarchived_and_opened_as_folders(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let archived = |harness: &crate::command_tests::Harness, cx: &mut gpui::VisualTestContext| {
        harness.tree(cx).projects[0].file.as_ref().unwrap().archived
    };

    harness
        .run_and_wait(cx, "project::Archive", &[Input::Pick("invoicing")])
        .await;
    assert!(archived(&harness, cx));
    // Nothing is left to archive.
    harness.clear_toasts(cx);
    dispatch(cx, "project::Archive");
    assert!(harness.has_toast(cx) && harness.picker(cx).is_none());

    harness
        .run_and_wait(cx, "project::Unarchive", &[Input::Pick("invoicing")])
        .await;
    assert!(!archived(&harness, cx));

    let dir = layout::project_dir(&harness.root, "invoicing");
    let fs = harness
        .project
        .read_with(cx, |project, _| project.fs().clone());
    fs.create_dir(&dir).await.unwrap();
    harness
        .run(cx, "project::OpenFolder", &[Input::Pick("invoicing")])
        .await;
    wait_until(cx, "the folder", |cx| {
        harness.project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == dir.as_path())
        })
    })
    .await;
    assert_eq!(harness.file(cx, &[]).status, Status::Open);
}

/// A tree of one-offs `hand` (hand-set `linear`), `synced` (`linear` and `linear_state`) and
/// `plain` (no link), loaded as the store loads it.
fn linked_tree() -> (tempfile::TempDir, WorkTree) {
    use bongos_work_model::{Kind, LinearState, TaskFile, serialize_task};
    let temp = tempfile::TempDir::new().unwrap();
    let root = crate::store_tests::canonical(&temp);
    for (id, goal, linear, synced) in [
        ("hand", "Hand linked", Some("SEQ-7"), false),
        ("synced", "Synced task", Some("SEQ-121"), true),
        ("plain", "Plain task", None, false),
    ] {
        let mut file = TaskFile::new(goal, Kind::Other);
        file.id = id.to_string();
        file.linear = linear.map(str::to_string);
        file.linear_state = synced.then(LinearState::default);
        crate::store_tests::write(
            &root.join(format!("oneoffs/misc/2020-01-02-{id}/task.toml")),
            &serialize_task(&file).unwrap(),
        );
    }
    let tree = bongos_work_model::store::load(&root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    (temp, tree)
}

fn choice_labels(tree: &WorkTree) -> Vec<String> {
    let mut labels: Vec<String> = crate::commands::task_choices(tree, |_, _| true)
        .into_iter()
        .map(|(_, label)| label)
        .collect();
    labels.sort();
    labels
}

#[test]
fn task_pickers_name_a_linked_task_with_its_identifier() {
    let (_temp, tree) = linked_tree();
    let labels = choice_labels(&tree);
    // A hand-set `linear` without `linear_state` shows its identifier, as the row does.
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("Hand linked (SEQ-7)  ["))
    );
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("Synced task (SEQ-121)  ["))
    );
}

#[test]
fn task_pickers_leave_an_unlinked_task_alone() {
    let (_temp, tree) = linked_tree();
    let plain: Vec<String> = choice_labels(&tree)
        .into_iter()
        .filter(|label| label.starts_with("Plain task"))
        .collect();
    assert_eq!(plain.len(), 1);
    assert!(plain[0].starts_with("Plain task  ["), "{plain:?}");
}

#[gpui::test]
async fn edit_skips_the_goal_prompt_when_the_goal_is_locked(cx: &mut TestAppContext) {
    use bongos_work_model::{Kind, LinearState, TaskFile, serialize_task};
    let (harness, cx) = harness(cx, true, true).await;
    let mut file = TaskFile::new("From Linear", Kind::PrShip);
    file.id = "l9".to_string();
    file.linear = Some("SEQ-121".to_string());
    file.linear_state = Some(LinearState::default());
    crate::store_tests::write(
        &layout::task_file(&harness.task_dir(&["backfill"])),
        &serialize_task(&file).unwrap(),
    );
    wait_until(cx, "the synced link", |cx| {
        harness
            .tree(cx)
            .task_by_id("l9")
            .is_some_and(|task| task.file.as_ref().is_some_and(|f| f.linear_state.is_some()))
    })
    .await;

    // Picking the task opens the kind picker at once; no goal prompt comes first.
    harness
        .run(cx, "task::Edit", &[Input::Pick("From Linear")])
        .await;
    assert!(harness.prompt(cx).is_none());
    assert!(
        harness
            .choices(cx)
            .iter()
            .any(|row| row.starts_with("pr-ship (current)"))
    );
    harness.answer(cx, &Input::Pick("document")).await;
    wait_until(cx, "the kind to be written", |cx| {
        harness.file(cx, &["backfill"]).kind == Kind::Document
    })
    .await;
    let file = harness.file(cx, &["backfill"]);
    assert_eq!(file.goal, "From Linear");
    assert!(!harness.has_toast(cx));
}

#[gpui::test]
async fn edit_still_asks_for_the_goal_of_a_hand_set_link(cx: &mut TestAppContext) {
    use bongos_work_model::{Kind, TaskFile, serialize_task};
    let (harness, cx) = harness(cx, true, true).await;
    let mut file = TaskFile::new("Hand set", Kind::PrShip);
    file.id = "l9".to_string();
    file.linear = Some("SEQ-7".to_string());
    crate::store_tests::write(
        &layout::task_file(&harness.task_dir(&["backfill"])),
        &serialize_task(&file).unwrap(),
    );
    wait_until(cx, "the hand-set link", |cx| {
        harness.tree(cx).task_by_id("l9").is_some()
    })
    .await;
    harness
        .run(cx, "task::Edit", &[Input::Pick("Hand set")])
        .await;
    assert!(harness.prompt(cx).is_some());
}
