//! Tests of subtickets out over a real work store in a temporary directory and a fake Linear: the
//! offer after Kyle's apply, the chip after an agent's, the command, the creation with client ids
//! and its retry, and that nothing reaches Linear before the confirm.

use std::sync::Arc;
use std::time::Duration;

use bongos_linear_model::fake::Call;
use bongos_linear_model::subtickets::{Missing, begin_create};
use bongos_linear_model::sync::is_synced;
use bongos_linear_model::{LinearApi, NewIssue};
use bongos_minibuffer::ReadKeyView;
use bongos_work::{NeedsYouRegistry, RowChipsRegistry};
use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::{Change, Context as WorkContext, Write, default_todos};
use bongos_work_model::rules::Place;
use bongos_work_model::{
    ByKind, Extra, HistoryEntry, Kind, ProposalRecord, ProposalState, TaskFile, layout,
    serialize_task,
};
use editor::Editor;
use gpui::{Entity, TestAppContext, VisualTestContext};
use uuid::Uuid;

use crate::actions_tests::dispatch;
use crate::checklist::Checklist;
use crate::link_tests::{Env, env, wait_until};
use crate::refresh;
use crate::subtickets::{self, offer_question, rows, select};
use crate::sync::{self, Refusal, Scope};

/// The group linked to SEQ-100 in the seeded tree, and its leaf `sub`.
const GROUP: &str = "g2";
const SUB: &str = "l5";
const PARENT_ID: &str = "uuid-SEQ-100";

async fn start(cx: &mut TestAppContext) -> (Env, &mut VisualTestContext) {
    let (env, cx) = env(cx, true, true).await;
    // The test creates the store after the sync: attach the watcher now.
    cx.update(|_, cx| subtickets::connect(cx));
    cx.run_until_parked();
    (env, cx)
}

fn requests(cx: &mut VisualTestContext) -> u64 {
    let linear = cx.update(|_, cx| sync::global(cx).unwrap());
    linear.read_with(cx, |linear, cx| linear.snapshot(cx).requests_allowed)
}

fn creates(env: &Env) -> Vec<Call> {
    env.fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Create { .. }))
        .collect()
}

/// Adds the leaves `sub2` (l6) and `sub3` (l7) under the linked group, on disk, and waits for the
/// store to load them.
async fn add_leaves(env: &Env, cx: &mut VisualTestContext) {
    let dir = env.tree(cx).task_by_id(GROUP).unwrap().dir.clone();
    for (slug, id, goal) in [("sub2", "l6", "Sub two"), ("sub3", "l7", "Sub three")] {
        let mut file = TaskFile::new(goal, Kind::PrShip);
        file.id = id.to_string();
        file.todos = default_todos(Kind::PrShip);
        file.planned_result = Some(format!("{goal} planned"));
        let path = layout::task_file(&layout::child_dir(&dir, slug));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serialize_task(&file).unwrap()).unwrap();
    }
    wait_until(cx, "the new leaves", |cx| {
        let tree = env.tree(cx);
        tree.task_by_id("l6").is_some() && tree.task_by_id("l7").is_some()
    })
    .await;
}

/// Writes an applied `[[proposals]]` record on `root`, as feature 26's apply does.
async fn apply(env: &Env, root: &str, number: u32, by: &str, cx: &mut VisualTestContext) {
    apply_as(env, root, number, by, None, cx).await
}

/// [`apply`] with the author kind the record stores; `None` is an old record with no kind.
async fn apply_as(
    env: &Env,
    root: &str,
    number: u32,
    by: &str,
    by_kind: Option<ByKind>,
    cx: &mut VisualTestContext,
) {
    let before = env.writes(cx);
    let (root, by) = (root.to_string(), by.to_string());
    env.store.update(cx, |store, cx| {
        store
            .write(
                WorkContext::at_keyboard(PersonAtKeyboard::for_test()),
                move |tree, ctx| {
                    let place = Place::locate(tree, &root)?;
                    let mut file = place.task.file.clone().unwrap();
                    file.proposals.push(ProposalRecord {
                        number,
                        state: ProposalState::Applied,
                        at: ctx.at,
                        by: by.clone(),
                        by_kind,
                        hash: format!("hash-{number}"),
                        extra: Extra::new(),
                    });
                    let entry = HistoryEntry {
                        at: ctx.at,
                        by: ctx.by.clone(),
                        event: format!("proposal {number} applied"),
                        detail: None,
                        extra: Extra::new(),
                    };
                    file.history.push(entry.clone());
                    let dir = place.task.dir.clone();
                    Ok(Change {
                        dir: dir.clone(),
                        writes: vec![Write::Task { dir, file }],
                        entry,
                        moves: Vec::new(),
                    })
                },
                cx,
            )
            .detach()
    });
    wait_until(cx, "the apply", |cx| env.writes(cx) > before).await;
}

/// Writes the pending create of `task_id` with `id`, as the confirmed creation does first.
async fn begin(env: &Env, task_id: &str, id: Uuid, cx: &mut VisualTestContext) {
    let before = env.writes(cx);
    let task_id = task_id.to_string();
    env.store.update(cx, |store, cx| {
        store
            .write(
                WorkContext::at_keyboard(PersonAtKeyboard::for_test()),
                move |tree, ctx| begin_create(tree, ctx, &task_id, id, true),
                cx,
            )
            .detach()
    });
    wait_until(cx, "the pending create", |cx| env.writes(cx) > before).await;
}

fn checklist(env: &Env, cx: &mut VisualTestContext) -> Option<Entity<Checklist>> {
    env.workspace
        .read_with(cx, |workspace, cx| workspace.active_modal::<Checklist>(cx))
}

fn read_key(env: &Env, cx: &mut VisualTestContext) -> bool {
    env.workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<ReadKeyView>(cx).is_some()
    })
}

fn has_modal(env: &Env, cx: &mut VisualTestContext) -> bool {
    env.workspace.update_in(cx, |workspace, window, cx| {
        workspace.has_active_modal(window, cx)
    })
}

/// Waits for the checklist and returns its title and row labels.
async fn wait_checklist(env: &Env, cx: &mut VisualTestContext) -> (String, Vec<String>) {
    wait_until(cx, "the checklist", |cx| checklist(env, cx).is_some()).await;
    let list = checklist(env, cx).unwrap();
    list.read_with(cx, |list, _| {
        (
            list.title().to_string(),
            list.rows().iter().map(|row| row.label.clone()).collect(),
        )
    })
}

/// The title and text of the active editor, when it is the offer's preview.
fn preview(env: &Env, cx: &mut VisualTestContext) -> Option<(String, String)> {
    let editor = env
        .workspace
        .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))?;
    editor.read_with(cx, |editor, cx| {
        assert!(editor.read_only(cx), "the preview is read-only");
        let title = editor.buffer().read(cx).title(cx).to_string();
        title
            .starts_with("New subtickets under ")
            .then(|| (title, editor.text(cx)))
    })
}

/// Confirms the checklist and waits for the yes/no question.
async fn confirm_checklist(env: &Env, cx: &mut VisualTestContext) {
    dispatch("menu::Confirm", cx);
    wait_until(cx, "the question", |cx| read_key(env, cx)).await;
}

async fn settle(cx: &mut VisualTestContext) {
    cx.executor().timer(Duration::from_millis(500)).await;
    cx.run_until_parked();
}

fn offered(env: &Env, cx: &mut VisualTestContext) -> Vec<(String, u32)> {
    env.file("stream", cx)
        .linear_state
        .unwrap()
        .offered
        .iter()
        .map(|offered| (offered.root.clone(), offered.number))
        .collect()
}

fn declined(env: &Env, cx: &mut VisualTestContext) -> Vec<String> {
    let mut declined = env.file("stream", cx).linear_state.unwrap().declined;
    declined.sort();
    declined
}

#[test]
fn the_sync_writes_as_the_linear_sync_workflow() {
    let ctx = subtickets::sync_context();
    assert_eq!(ctx.author.kind(), bongos_work_model::AuthorKind::Workflow);
    assert_eq!(ctx.author.name, "linear sync");
    assert_eq!(ctx.author.run, None);
    assert_eq!(ctx.by, "linear sync");
    assert!(!ctx.has_attestation());
}

#[gpui::test]
async fn a_persons_apply_opens_the_offer_and_sends_nothing(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    apply(&env, GROUP, 1, "kyle", cx).await;
    let (title, labels) = wait_checklist(&env, cx).await;
    assert_eq!(title, "1 leaf has no subticket. Create it under SEQ-100?");
    assert_eq!(labels, vec!["Sub"]);
    let (preview_title, text) = preview(&env, cx).expect("the preview");
    assert_eq!(preview_title, "New subtickets under SEQ-100");
    assert!(text.contains("- Sub\n"), "{text}");
    // Every row starts checked.
    let list = checklist(&env, cx).unwrap();
    assert_eq!(
        list.read_with(cx, |list, _| list.checked().to_vec()),
        [true]
    );
    // Space toggles the selected row.
    cx.simulate_keystrokes("space");
    assert_eq!(
        list.read_with(cx, |list, _| list.checked().to_vec()),
        [false]
    );
    cx.simulate_keystrokes("space");
    assert_eq!(
        list.read_with(cx, |list, _| list.checked().to_vec()),
        [true]
    );

    assert!(
        env.fake.calls().is_empty(),
        "nothing is sent before the confirm"
    );
    assert_eq!(requests(cx), 0);
    assert!(
        offered(&env, cx).is_empty(),
        "nothing is recorded before an answer"
    );
}

#[gpui::test]
async fn not_now_records_declined_and_creates_nothing(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    add_leaves(&env, cx).await;
    apply(&env, GROUP, 1, "kyle", cx).await;
    let (title, labels) = wait_checklist(&env, cx).await;
    assert_eq!(
        title,
        "3 leaves have no subticket. Create them under SEQ-100?"
    );
    assert_eq!(labels.len(), 3);
    confirm_checklist(&env, cx).await;
    assert!(env.fake.calls().is_empty());
    cx.simulate_keystrokes("n");
    wait_until(cx, "the record", |cx| !offered(&env, cx).is_empty()).await;
    settle(cx).await;

    assert_eq!(offered(&env, cx), vec![(GROUP.to_string(), 1)]);
    assert_eq!(declined(&env, cx), vec!["l5", "l6", "l7"]);
    assert!(preview(&env, cx).is_none(), "the preview closed");
    assert!(!has_modal(&env, cx), "the offer does not open again");
    assert!(env.fake.calls().is_empty(), "Not now sends nothing");
    assert_eq!(requests(cx), 0);
    for slug in ["sub", "sub2", "sub3"] {
        let file = env.file(slug, cx);
        assert!(
            file.linear.is_none() && file.linear_state.is_none(),
            "{slug}"
        );
    }
}

#[gpui::test]
async fn escape_on_the_checklist_is_not_now(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    apply(&env, GROUP, 1, "kyle", cx).await;
    wait_checklist(&env, cx).await;
    dispatch("menu::Cancel", cx);
    wait_until(cx, "the record", |cx| !offered(&env, cx).is_empty()).await;
    assert_eq!(declined(&env, cx), vec![SUB]);
    assert!(env.fake.calls().is_empty());
    assert_eq!(requests(cx), 0);
}

#[gpui::test]
async fn confirming_two_of_three_creates_two_under_the_parent_and_links_them(
    cx: &mut TestAppContext,
) {
    let (env, cx) = start(cx).await;
    add_leaves(&env, cx).await;
    apply(&env, GROUP, 1, "kyle", cx).await;
    let (_, labels) = wait_checklist(&env, cx).await;
    // Uncheck the second row.
    dispatch("menu::SelectNext", cx);
    cx.simulate_keystrokes("space");
    let unchecked = labels[1].clone();
    confirm_checklist(&env, cx).await;
    assert!(
        env.fake.calls().is_empty(),
        "nothing is sent before the yes"
    );
    assert_eq!(requests(cx), 0);
    env.clear_toast(cx);
    cx.simulate_keystrokes("y");
    wait_until(cx, "the creation", |cx| {
        env.last_toast(cx).starts_with("Created")
    })
    .await;
    settle(cx).await;
    assert_eq!(env.last_toast(cx), "Created SEQ-122, SEQ-123 under SEQ-100");

    let made = creates(&env);
    assert_eq!(made.len(), 2, "{made:?}");
    for call in &made {
        let Call::Create {
            team_id, parent_id, ..
        } = call
        else {
            unreachable!()
        };
        assert_eq!(team_id, "team");
        assert_eq!(parent_id.as_deref(), Some(PARENT_ID));
    }
    let mut linked = 0;
    for slug in ["sub", "sub2", "sub3"] {
        let file = env.file(slug, cx);
        if file.goal == unchecked {
            assert!(
                file.linear.is_none() && file.linear_state.is_none(),
                "{slug}"
            );
            continue;
        }
        linked += 1;
        let identifier = file.linear.clone().expect("linked");
        let state = file.linear_state.clone().unwrap();
        assert!(
            state.pending_create.is_none(),
            "the pending create is cleared"
        );
        let ticket = env.fake.issue(&identifier).unwrap();
        assert_eq!(state.issue_id, ticket.id);
        assert_eq!(ticket.parent.unwrap().id, PARENT_ID);
        assert_eq!(ticket.title, file.goal);
        assert_eq!(state.description, ticket.description);
        assert!(made.iter().any(|call| matches!(
            call,
            Call::Create { id, .. } if id.hyphenated().to_string() == ticket.id
        )));
        assert_eq!(file.goal, ticket.title, "the goal stays");
    }
    assert_eq!(linked, 2);
    let unchecked_id = ["l5", "l6", "l7"]
        .into_iter()
        .find(|id| env.tree(cx).task_by_id(id).unwrap().goal() == unchecked)
        .unwrap();
    assert_eq!(declined(&env, cx), vec![unchecked_id]);
    assert_eq!(offered(&env, cx), vec![(GROUP.to_string(), 1)]);
}

#[gpui::test]
async fn the_offer_follows_the_stored_author_kind_not_the_name(cx: &mut TestAppContext) {
    // A person under any name opens the offer at once.
    let (env, cx) = start(cx).await;
    apply_as(&env, GROUP, 1, "alice", Some(ByKind::Person), cx).await;
    let (_, labels) = wait_checklist(&env, cx).await;
    assert_eq!(labels, vec!["Sub"]);
    assert!(preview(&env, cx).is_some());
}

#[gpui::test]
async fn an_agent_named_kyle_gets_the_chip_and_no_modal(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    apply_as(&env, GROUP, 1, "kyle", Some(ByKind::Agent), cx).await;
    wait_until(cx, "the chip", |cx| {
        cx.update(|_, cx| RowChipsRegistry::chips(GROUP, cx))
            .iter()
            .any(|chip| chip.as_ref() == "1 needs a ticket")
    })
    .await;
    settle(cx).await;
    assert!(!has_modal(&env, cx), "no modal for an agent");
}

#[gpui::test]
async fn an_agents_apply_shows_the_chip_and_no_modal(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    add_leaves(&env, cx).await;
    apply(&env, GROUP, 1, "agent", cx).await;
    wait_until(cx, "the chip", |cx| {
        cx.update(|_, cx| RowChipsRegistry::chips(GROUP, cx))
            .iter()
            .any(|chip| chip.as_ref() == "3 need tickets")
    })
    .await;
    settle(cx).await;
    assert!(!has_modal(&env, cx), "no modal for an agent's apply");
    assert!(preview(&env, cx).is_none());
    let reasons = cx.update(|_, cx| NeedsYouRegistry::collect(cx));
    assert_eq!(
        reasons.get(GROUP).cloned(),
        Some(vec!["3 need tickets".to_string()])
    );
    assert!(env.fake.calls().is_empty());
    assert_eq!(requests(cx), 0);
    assert!(
        offered(&env, cx).is_empty(),
        "the chip stays until the command"
    );

    // The command lists them; its answer clears the chip and the reason.
    env.focus(GROUP, cx);
    dispatch("task::CreateMissingSubtickets", cx);
    let (_, labels) = wait_checklist(&env, cx).await;
    assert_eq!(labels.len(), 3);
    dispatch("menu::Cancel", cx);
    wait_until(cx, "the chip to go", |cx| {
        cx.update(|_, cx| RowChipsRegistry::chips(GROUP, cx))
            .is_empty()
    })
    .await;
    assert_eq!(offered(&env, cx), vec![(GROUP.to_string(), 1)]);
    let reasons = cx.update(|_, cx| NeedsYouRegistry::collect(cx));
    assert!(!reasons.contains_key(GROUP));
    assert!(creates(&env).is_empty());
}

#[gpui::test]
async fn a_crash_between_create_and_write_ends_with_one_ticket(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    // The first attempt: the pending create is written and the ticket is created, then bongos
    // stops before the link is written.
    let id = Uuid::new_v4();
    begin(&env, SUB, id, cx).await;
    env.fake
        .create(NewIssue {
            id,
            team_id: "team".to_string(),
            parent_id: Some(PARENT_ID.to_string()),
            title: "Sub".to_string(),
            description: String::new(),
        })
        .await
        .unwrap();
    env.fake.clear_calls();
    let children = || env.fake.children_page(PARENT_ID, 50, None).children.len();
    assert_eq!(children(), 1);

    // The retry: the command finds the leaf with its pending create and reuses the UUID.
    env.focus(SUB, cx);
    dispatch("task::CreateMissingSubtickets", cx);
    let (_, labels) = wait_checklist(&env, cx).await;
    assert_eq!(labels, vec!["Sub"]);
    confirm_checklist(&env, cx).await;
    assert!(env.fake.calls().is_empty());
    env.clear_toast(cx);
    cx.simulate_keystrokes("y");
    wait_until(cx, "the retry", |cx| {
        env.last_toast(cx).starts_with("Created")
    })
    .await;

    assert_eq!(children(), 1, "no second ticket");
    let hyphenated = id.hyphenated().to_string();
    let calls = env.fake.calls();
    assert!(
        matches!(&calls[0], Call::Create { id: sent, .. } if *sent == id),
        "{calls:?}"
    );
    assert_eq!(calls[1], Call::Issues(vec![hyphenated.clone()]));
    assert_eq!(calls.len(), 2);
    let file = env.file("sub", cx);
    let state = file.linear_state.unwrap();
    assert_eq!(state.issue_id, hyphenated);
    assert!(state.pending_create.is_none());
    assert_eq!(
        file.linear,
        Some(env.fake.issue(&hyphenated).unwrap().identifier)
    );
}

#[gpui::test]
async fn the_command_lists_declined_leaves_again(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    apply(&env, GROUP, 1, "kyle", cx).await;
    wait_checklist(&env, cx).await;
    dispatch("menu::Cancel", cx);
    wait_until(cx, "the record", |cx| !declined(&env, cx).is_empty()).await;
    settle(cx).await;

    // A later apply finds only the declined leaf: no offer, the record is only noted.
    apply(&env, GROUP, 2, "kyle", cx).await;
    wait_until(cx, "the second record", |cx| offered(&env, cx).len() == 2).await;
    settle(cx).await;
    assert!(!has_modal(&env, cx));

    env.focus(GROUP, cx);
    dispatch("task::CreateMissingSubtickets", cx);
    let (_, labels) = wait_checklist(&env, cx).await;
    assert_eq!(labels, vec!["Sub"], "the declined leaf is listed");
    dispatch("menu::Cancel", cx);
    settle(cx).await;
    assert!(env.fake.calls().is_empty());
    assert_eq!(requests(cx), 0);
}

#[gpui::test]
async fn the_pending_create_state_is_not_a_link(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    begin(&env, SUB, Uuid::new_v4(), cx).await;
    let file = env.file("sub", cx);
    assert!(file.linear.is_none());
    assert!(file.linear_state.as_ref().unwrap().pending_create.is_some());
    assert!(!is_synced(&file));
    let tree = env.tree(cx);
    assert!(!refresh::linked_ids(&tree).contains(&SUB.to_string()));
    cx.update(|_, cx| refresh::sync_linked(cx));
    let linear = cx.update(|_, cx| sync::global(cx).unwrap());
    // A token and a client, so that only the link decides.
    let client: Arc<dyn LinearApi> = Arc::new(env.fake.clone());
    linear.update(cx, |linear, _| linear.set_token(true, Some(client)));
    let refused = cx.update(|_, cx| {
        linear.update(cx, |linear, cx| {
            linear
                .call(Scope::LinkedTask(SUB), cx, |client| client.viewer())
                .err()
        })
    });
    assert_eq!(refused, Some(Refusal::NotLinked));
    assert!(
        cx.update(|_, cx| RowChipsRegistry::chips(SUB, cx))
            .is_empty()
    );
    assert!(env.fake.calls().is_empty());
}

#[gpui::test]
async fn the_command_needs_a_linked_group(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    env.focus("l1", cx);
    dispatch("task::CreateMissingSubtickets", cx);
    assert_eq!(env.last_toast(cx), subtickets::NO_GROUP);
    assert!(!has_modal(&env, cx));
    assert!(env.fake.calls().is_empty());
}

fn item(id: &str, parent: &str, is_group: bool) -> Missing {
    Missing {
        task_id: id.to_string(),
        title: id.to_uppercase(),
        description: String::new(),
        parent_task_id: parent.to_string(),
        is_group,
        pending_id: None,
    }
}

#[test]
fn the_selection_follows_the_checks_and_the_nesting() {
    let items = vec![
        item("a", "g", false),
        item("sub", "g", true),
        item("b", "sub", false),
        item("c", "sub", false),
    ];
    let ids = |chosen: &[Missing]| -> Vec<String> {
        chosen.iter().map(|item| item.task_id.clone()).collect()
    };
    let (chosen, declined) = select(&items, &[true, true, true, true]);
    assert_eq!(ids(&chosen), ["a", "sub", "b", "c"]);
    assert!(declined.is_empty());
    // An unchecked group takes its tasks with it; only leaves are declined.
    let (chosen, declined) = select(&items, &[true, false, true, true]);
    assert_eq!(ids(&chosen), ["a"]);
    assert_eq!(declined, ["b", "c"]);
    // A group with nothing checked under it gets no ticket.
    let (chosen, declined) = select(&items, &[false, true, false, false]);
    assert!(chosen.is_empty());
    assert_eq!(declined, ["a", "b", "c"]);

    let depths: Vec<usize> = rows(&items).iter().map(|row| row.depth).collect();
    assert_eq!(depths, [0, 0, 1, 1]);
    assert_eq!(rows(&items)[1].label, "SUB (group)");
    assert_eq!(
        offer_question(&items, "SEQ-1"),
        "4 tasks have no subticket. Create them under SEQ-1?"
    );
}
