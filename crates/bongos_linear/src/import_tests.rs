//! Tests of `task: new from linear` through the pickers, the preview buffer and the confirm, over
//! a real work store in a temporary directory and a fake Linear: that nothing is written before
//! the confirm, what one import writes, and that a ticket already imported is focused instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bongos_linear_model::fake::{Call, FakeLinear};
use bongos_linear_model::{ApiError, LinearApi};
use bongos_work::Focus;
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::{Kind, TodoRole};
use editor::Editor;
use gpui::{Entity, TestAppContext, VisualTestContext};
use workspace::Workspace;

use crate::actions_tests::dispatch;
use crate::import::{ReadError, placements, read_levels};
use crate::link::TYPED_ROW;
use crate::link_tests::{Env, env, issue, wait_until};

/// Every file under `root` with its bytes.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, found: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else {
                found.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(root, &mut found);
    found
}

fn has_modal(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.has_active_modal(window, cx)
    })
}

/// Waits for a modal (a picker or a prompt), types `text` into it and confirms.
async fn answer(env: &Env, text: &str, cx: &mut VisualTestContext) {
    wait_until(cx, "a modal", |cx| has_modal(&env.workspace, cx)).await;
    if !text.is_empty() {
        cx.simulate_input(text);
        cx.run_until_parked();
    }
    dispatch("menu::Confirm", cx);
}

/// The ticket `number` with `children` subtickets numbered from `first`, the second blocking the
/// first.
fn add_parent(env: &Env, number: u32, first: u32, children: u32) {
    let parent = issue(number);
    env.fake.add_issue(parent.clone());
    for child in first..first + children {
        let mut ticket = issue(child);
        ticket.parent = Some(parent.summary());
        if child == first + 1 {
            ticket.blocks = vec![format!("uuid-SEQ-{first}")];
        }
        env.fake.add_issue(ticket);
    }
}

/// Runs `task: new from linear` for the typed `ticket` into the first placement row, up to the
/// confirm behind the preview, and returns the text of the preview.
async fn until_confirm(env: &Env, ticket: &str, cx: &mut VisualTestContext) -> String {
    env.clear_toast(cx);
    dispatch("task::NewFromLinear", cx);
    answer(env, TYPED_ROW, cx).await;
    answer(env, ticket, cx).await;
    answer(env, "", cx).await;
    wait_until(cx, "the preview", |cx| {
        has_modal(&env.workspace, cx) && preview(env, cx).is_some()
    })
    .await;
    preview(env, cx).unwrap().1
}

/// The title and text of the active editor, when it is a preview.
fn preview(env: &Env, cx: &mut VisualTestContext) -> Option<(String, String)> {
    let editor = env
        .workspace
        .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))?;
    editor.read_with(cx, |editor, cx| {
        assert!(editor.read_only(cx), "the preview is read-only");
        let title = editor.buffer().read(cx).title(cx).to_string();
        title
            .starts_with("Import ")
            .then(|| (title, editor.text(cx)))
    })
}

#[gpui::test]
async fn cancel_writes_nothing(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    add_parent(&env, 200, 201, 4);
    let before = files(&env.root);
    let writes = env.writes(cx);

    let text = until_confirm(&env, "SEQ-200", cx).await;
    assert_eq!(preview(&env, cx).unwrap().0, "Import SEQ-200");
    assert!(
        text.starts_with("Import SEQ-200 \"Ticket 200\" into P\n"),
        "{text}"
    );
    // Nothing exists behind the preview and the confirm.
    assert_eq!(files(&env.root), before, "the preview wrote something");
    assert_eq!(env.writes(cx), writes);

    answer(&env, "Cancel", cx).await;
    cx.executor()
        .timer(std::time::Duration::from_millis(500))
        .await;
    cx.run_until_parked();
    assert_eq!(files(&env.root), before, "Cancel wrote something");
    assert_eq!(env.writes(cx), writes, "no journal entry");
    assert!(preview(&env, cx).is_none(), "the preview closed");
    assert_eq!(env.last_toast(cx), "");
    assert!(
        env.fake
            .calls()
            .iter()
            .all(|call| matches!(call, Call::Issues(_) | Call::Recent(_))),
        "only reads"
    );
}

#[gpui::test]
async fn four_subtickets_make_one_group_and_four_leaves_with_a_plan_todo(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    add_parent(&env, 200, 201, 4);
    let writes = env.writes(cx);

    let text = until_confirm(&env, "SEQ-200", cx).await;
    assert!(
        text.contains("create 1 group and 4 leaves, each leaf with one todo: plan\n"),
        "{text}"
    );
    // Hints are shown and never added as edges.
    assert!(
        text.contains("Hints (not added): SEQ-202 blocks SEQ-201\n"),
        "{text}"
    );
    answer(&env, "Import", cx).await;
    wait_until(cx, "the import", |cx| env.last_toast(cx) != "").await;
    assert_eq!(env.last_toast(cx), "Imported SEQ-200");
    assert_eq!(env.writes(cx), writes + 1, "one write");

    let tree = env.tree(cx);
    let find = |identifier: &str| {
        tree.tasks()
            .into_iter()
            .find(|task| {
                task.file
                    .as_ref()
                    .is_some_and(|file| file.linear.as_deref() == Some(identifier))
            })
            .unwrap_or_else(|| panic!("no task for {identifier}"))
    };
    let group = find("SEQ-200");
    assert_eq!(derive::shape(group, &tree), Shape::Group);
    assert_eq!(group.children.len(), 4);
    let file = group.file.as_ref().unwrap();
    assert_eq!(file.goal, "Ticket 200");
    assert!(file.todos.is_empty());
    assert_eq!(file.history.last().unwrap().event, "imported from SEQ-200");
    for number in 201..=204 {
        let identifier = format!("SEQ-{number}");
        let leaf = find(&identifier);
        let file = leaf.file.as_ref().unwrap();
        assert_eq!(derive::shape(leaf, &tree), Shape::Leaf);
        assert_eq!(file.goal, format!("Ticket {number}"));
        assert_eq!(file.kind, Kind::PrShip);
        assert_eq!(file.todos.len(), 1, "{identifier}");
        assert_eq!(file.todos[0].name, "plan");
        assert_eq!(file.todos[0].role, Some(TodoRole::Plans));
        assert!(
            file.after.is_empty() && file.stacked_on.is_none(),
            "no edges"
        );
        let state = file.linear_state.as_ref().unwrap();
        assert_eq!(state.issue_id, format!("uuid-SEQ-{number}"));
        assert!(state.offered.is_empty());
        assert_eq!(file.history.last().unwrap().event, "imported from SEQ-200");
        assert!(group.children.iter().any(|child| child.dir == leaf.dir));
    }
    // The rest of the tree is as it was.
    assert!(env.file("alpha", cx).history.is_empty());
}

#[gpui::test]
async fn no_subtickets_make_one_leaf(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    let text = until_confirm(&env, "SEQ-121", cx).await;
    assert!(
        text.contains("create 1 leaf, each leaf with one todo: plan\n"),
        "{text}"
    );
    assert!(!text.contains("Hints"), "{text}");
    answer(&env, "Import", cx).await;
    wait_until(cx, "the import", |cx| env.last_toast(cx) != "").await;

    let file = env.file("ticket-121", cx);
    assert_eq!(file.linear.as_deref(), Some("SEQ-121"));
    assert_eq!(file.todos.len(), 1);
    assert_eq!(file.todos[0].name, "plan");
    assert_eq!(file.todos[0].role, Some(TodoRole::Plans));
    assert_eq!(file.history.last().unwrap().event, "imported from SEQ-121");
}

#[gpui::test]
async fn more_than_200_subtickets_are_not_imported(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    add_parent(&env, 300, 1000, 201);
    let before = files(&env.root);

    let text = until_confirm(&env, "SEQ-300", cx).await;
    assert!(
        text.contains("SEQ-300 not imported: more than 200 subtickets\n"),
        "{text}"
    );
    assert!(text.contains("create 1 leaf"), "{text}");
    answer(&env, "Cancel", cx).await;
    cx.run_until_parked();
    assert_eq!(files(&env.root), before);
}

#[gpui::test]
async fn levels_are_read_one_at_a_time_down_to_depth_four(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    // SEQ-400 with a chain of subtickets six levels deep.
    env.fake.add_issue(issue(400));
    for number in 401..=406 {
        let mut ticket = issue(number);
        ticket.parent = Some(issue(number - 1).summary());
        env.fake.add_issue(ticket);
    }
    env.fake.clear_calls();

    let text = until_confirm(&env, "SEQ-400", cx).await;
    assert!(
        text.contains("create 4 groups and 1 leaf, each leaf with one todo: plan\n"),
        "{text}"
    );
    assert!(
        text.contains("SEQ-405 not imported: deeper than 4 levels\n"),
        "{text}"
    );
    assert!(!text.contains("SEQ-406"), "{text}");
    let reads = env
        .fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Issues(_)))
        .count();
    assert_eq!(reads, 5, "the ticket, then one read per level");
    answer(&env, "Cancel", cx).await;
}

#[gpui::test]
async fn a_second_import_focuses_the_task_and_creates_nothing(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    add_parent(&env, 200, 201, 2);
    until_confirm(&env, "SEQ-200", cx).await;
    answer(&env, "Import", cx).await;
    wait_until(cx, "the import", |cx| env.last_toast(cx) != "").await;
    let tree = env.tree(cx);
    let group_id = tree
        .tasks()
        .into_iter()
        .find(|task| {
            task.file
                .as_ref()
                .is_some_and(|file| file.linear.as_deref() == Some("SEQ-200"))
        })
        .and_then(|task| task.id().map(str::to_string))
        .unwrap();
    let before = files(&env.root);
    let writes = env.writes(cx);

    env.clear_toast(cx);
    dispatch("task::NewFromLinear", cx);
    answer(&env, TYPED_ROW, cx).await;
    answer(&env, "SEQ-200", cx).await;
    wait_until(cx, "the toast", |cx| env.last_toast(cx) != "").await;
    let toast = env.last_toast(cx);
    assert!(
        toast.starts_with("SEQ-200 is already imported as ") && toast.contains("ticket-200"),
        "{toast}"
    );
    assert_eq!(env.writes(cx), writes);
    assert_eq!(files(&env.root), before);
    assert!(!has_modal(&env.workspace, cx), "no placement is asked");
    wait_until(cx, "the focus", |cx| {
        cx.update(|_, cx| Focus::global(cx).unwrap().read(cx).task_id() == Some(group_id.as_str()))
    })
    .await;
}

#[gpui::test]
async fn placements_list_the_group_at_point_then_the_focused_project(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    let tree = env.tree(cx);
    let rows = |at_point, focused| placements(&tree, at_point, focused);
    assert_eq!(
        rows(None, None),
        vec![("top".to_string(), "P (project)".to_string())]
    );
    assert_eq!(
        rows(None, Some("l1")),
        vec![(
            "top".to_string(),
            "P (top of the focused project)".to_string()
        )]
    );
    // A leaf at point offers its parent group.
    assert_eq!(
        rows(Some("l3"), Some("l3")),
        vec![
            ("g1".to_string(), "Phase (group at point)".to_string()),
            (
                "top".to_string(),
                "P (top of the focused project)".to_string()
            ),
        ]
    );
    // A group at point is offered itself, and a project top at point is not listed twice.
    assert_eq!(rows(Some("g2"), None)[0].0, "g2");
    assert_eq!(rows(Some("top"), None).len(), 1);
}

#[test]
fn read_levels_stops_on_a_failed_read() {
    let fake = FakeLinear::new("acme");
    let mut root = issue(200);
    fake.add_issue(root.clone());
    let mut child = issue(201);
    child.parent = Some(root.summary());
    fake.add_issue(child.clone());
    root.children = vec![child];
    fake.fail_next(ApiError::Network("down".into()));
    let result = futures::executor::block_on(read_levels(root, |refs| {
        let client: &dyn LinearApi = &fake;
        Ok(client.issues(refs))
    }));
    assert!(matches!(result, Err(ReadError::Api(ApiError::Network(_)))));
    assert_eq!(fake.calls().len(), 1, "the read stopped at the failure");
}

#[test]
fn the_import_confirm_names_the_automatic_started_write() {
    let prompt = crate::import::confirm_prompt("SEQ-200");
    assert!(prompt.starts_with("Import SEQ-200?"));
    assert!(
        prompt.contains("move each ticket to started when its plan begins"),
        "{prompt}"
    );
}
