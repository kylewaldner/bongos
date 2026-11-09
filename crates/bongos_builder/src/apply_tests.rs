//! `task: apply proposal`, `task: reject proposal` and `proposal: dump preview` in a real window
//! over a temporary work tree read by the real store (the harness of the view's tests). Every test
//! that must write nothing compares the bytes of every file of the tree.

#![cfg(unix)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bongos_builder_model::apply::{Request, STALE, applied, file_hash};
use bongos_builder_model::{Choices, LiveSet, diff, fingerprint, parse, preview};
use bongos_work::model::commands::Context as WorkContext;
use bongos_work::model::store::WorkTree;
use bongos_work::model::{ProposalState, Status};
use bongos_work::{LiveModes, LiveModesRegistry, LiveModesSource, WorkStore};
use editor::Editor;
use fs::Fs as _;
use gpui::{App, Entity, VisualTestContext};
use language::Point;
use workspace::Workspace;

use crate::actions::{LastToast, OFF_MESSAGE};
use crate::apply::{
    already_applied_message, apply_command, confirm_prompt, conflict_message, errors_message,
    nothing_message, reject_applied_message, reject_command, rejected_message,
};
use crate::view_tests::{
    BROKEN, LAYERS, dispatch, harness, has_modal, open_editor, proposal, set_settings_in, snapshot,
    text, the_view, wait_for_in, write,
};

/// Chats open on the tasks in the set, as the chats' live-modes source would report them.
#[derive(Clone, Default)]
struct FakeChats(Rc<RefCell<BTreeSet<String>>>);

impl LiveModesSource for FakeChats {
    fn live_modes(&self, task_id: &str, _: &App) -> LiveModes {
        LiveModes {
            open_chats: usize::from(self.0.borrow().contains(task_id)),
            run: None,
        }
    }
}

impl FakeChats {
    fn install(cx: &mut VisualTestContext) -> Self {
        let chats = Self::default();
        let source = Rc::new(chats.clone());
        cx.update(|_, cx| LiveModesRegistry::add_source("test chats", source, cx));
        chats
    }

    fn open(&self, task_id: &str, cx: &mut VisualTestContext) {
        self.0.borrow_mut().insert(task_id.to_string());
        cx.update(|_, cx| LiveModesRegistry::changed(cx));
        cx.run_until_parked();
    }
}

/// The work root of the harness: `billing` is `<root>/projects/acme/tasks/billing`.
fn root_of(billing: &Path) -> PathBuf {
    billing.ancestors().nth(4).unwrap().to_path_buf()
}

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

fn last_toast(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| cx.try_global::<LastToast>().map(|toast| toast.0.clone()))
}

fn clear_toast(cx: &mut VisualTestContext) {
    cx.update(|_, cx| cx.set_global(LastToast(String::new())));
}

async fn wait_for_toast(cx: &mut VisualTestContext, expected: &str) {
    wait_for_in(cx, expected, |cx| {
        last_toast(cx).as_deref() == Some(expected)
    })
    .await;
}

async fn wait_for_toast_starting(cx: &mut VisualTestContext, prefix: &str) -> String {
    wait_for_in(cx, prefix, |cx| {
        last_toast(cx).is_some_and(|toast| toast.starts_with(prefix))
    })
    .await;
    last_toast(cx).unwrap()
}

/// Dispatches `task::ApplyProposal` and waits for the confirm, behind the preview.
async fn until_confirm(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    clear_toast(cx);
    dispatch(cx, "task::ApplyProposal");
    let ws = workspace.clone();
    wait_for_in(cx, "the confirm", |cx| has_modal(&ws, cx)).await;
    assert!(
        preview_text(cx).is_some(),
        "the preview opened before the confirm"
    );
}

/// Answers the open picker with the row matching `query`.
fn answer(query: &str, cx: &mut VisualTestContext) {
    cx.simulate_input(query);
    cx.run_until_parked();
    dispatch(cx, "menu::Confirm");
}

fn preview_text(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| crate::preview_view::last_preview(cx))
}

/// The task with `id` in the store's tree now.
fn task_file(cx: &mut VisualTestContext, id: &str) -> bongos_work::model::TaskFile {
    snapshot(cx)
        .task_by_id(id)
        .and_then(|task| task.file.clone())
        .unwrap_or_else(|| panic!("no task {id}"))
}

fn task_by_goal(tree: &WorkTree, goal: &str) -> Option<String> {
    tree.tasks()
        .into_iter()
        .find(|task| task.goal() == goal)
        .and_then(|task| task.id().map(str::to_string))
}

/// A proposal that drops the leaf `rates` and mentions nothing else under `billing`.
const DROP_RATES: &str = "\
proposal 1
for b
by kyle
why drop rates

task billing: Billing | id b
  task engine: Engine | id e
    task rates: Rates | id r | drop
";

/// A proposal that splits the leaf `schema` into two new children, marking no keeper.
const SPLIT_SCHEMA: &str = "\
proposal 1
for b
by kyle
why split schema

task billing: Billing | id b
  task schema: Schema | id s
    task cols: Columns
      result a PR adding columns
      todo implement | role produces-branch
      todo ship | role ships
    task docs: Column docs
      result a PR with the docs
      todo implement | role produces-branch
      todo ship | role ships
";

#[gpui::test]
async fn cancel_writes_nothing(cx: &mut gpui::TestAppContext) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let before = files(&root_of(&h.billing));
    until_confirm(&workspace, cx).await;
    let shown = preview_text(cx).unwrap();
    assert!(shown.starts_with("Proposal 1 for \"Billing\"\n"), "{shown}");
    assert!(shown.contains("\nCreate\n  api"), "{shown}");
    // The preview is a read-only buffer titled after the proposal.
    let editor = workspace.read_with(cx, |workspace, cx| {
        workspace.active_item_as::<Editor>(cx).unwrap()
    });
    editor.read_with(cx, |editor, cx| {
        assert!(editor.read_only(cx));
        assert_eq!(editor.buffer().read(cx).title(cx), "Proposal 1 preview");
    });

    answer("Cancel", cx);
    assert!(!has_modal(&workspace, cx));
    cx.executor()
        .timer(std::time::Duration::from_millis(300))
        .await;
    cx.run_until_parked();
    assert_eq!(
        files(&root_of(&h.billing)),
        before,
        "Cancel wrote something"
    );
    assert_eq!(last_toast(cx).as_deref(), Some(""));
}

#[gpui::test]
async fn apply_writes_exactly_the_previewed_operations_and_a_second_apply_writes_nothing(
    cx: &mut gpui::TestAppContext,
) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let root = root_of(&h.billing);
    let before = files(&root);
    let tree = snapshot(cx);
    let parsed = parse(LAYERS);
    let expected = diff(&parsed, &tree, &LiveSet::default(), &Choices::default());
    let change = applied(
        &parsed,
        &tree,
        &expected,
        &WorkContext::kyle(),
        &file_hash(LAYERS.as_bytes()),
    )
    .unwrap();

    until_confirm(&workspace, cx).await;
    assert_eq!(
        preview_text(cx).unwrap(),
        preview(&parsed, &tree, &expected)
    );
    answer("Apply", cx);
    let toast = wait_for_toast_starting(cx, "proposal 1 applied: ").await;
    assert_eq!(toast, change.summary);

    // The task files written are exactly the change's, and nothing else changed.
    let after = files(&root);
    let written: BTreeSet<PathBuf> = change.change.writes.iter().map(|w| w.path()).collect();
    let changed: BTreeSet<PathBuf> = after
        .iter()
        .filter(|(path, bytes)| before.get(*path) != Some(*bytes))
        .map(|(path, _)| path.clone())
        .filter(|path| path.file_name().is_some_and(|name| name == "task.toml"))
        .collect();
    assert_eq!(changed, written);
    for (path, bytes) in &before {
        if !written.contains(path) {
            assert_eq!(after.get(path), Some(bytes), "{} changed", path.display());
        }
    }
    let now = snapshot(cx);
    let api = task_by_goal(&now, "Api endpoint").expect("api created");
    assert_eq!(
        now.task_by_id(&api).unwrap().dir.parent().unwrap(),
        h.billing.join("tasks")
    );
    let record = &task_file(cx, "b").proposals[0];
    assert_eq!(record.state, ProposalState::Applied);
    assert_eq!(record.hash, file_hash(LAYERS.as_bytes()));

    // Applying the same file again creates nothing a second time.
    let once = files(&root);
    clear_toast(cx);
    dispatch(cx, "task::ApplyProposal");
    wait_for_toast(cx, &already_applied_message(1)).await;
    assert!(!has_modal(&workspace, cx));
    assert_eq!(files(&root), once);
}

#[gpui::test]
async fn two_queued_applies_of_one_preview_write_once(cx: &mut gpui::TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let tree = snapshot(cx);
    let parsed = parse(LAYERS);
    let hash = file_hash(LAYERS.as_bytes());
    let previewed = diff(&parsed, &tree, &LiveSet::default(), &Choices::default());
    let request = Request {
        parsed,
        choices: Choices::default(),
        fingerprint: fingerprint(&previewed, &Choices::default(), &hash),
    };
    let store = cx.update(|_, cx| WorkStore::global(cx).unwrap());
    let queue = |cx: &mut VisualTestContext| {
        let request = request.clone();
        store.update(cx, |store, cx| {
            store.write(
                WorkContext::kyle(),
                move |tree, ctx| {
                    apply_command(
                        &request,
                        "b",
                        1,
                        tree,
                        ctx,
                        &LiveSet::default(),
                        LAYERS.as_bytes(),
                    )
                },
                cx,
            )
        })
    };
    let first = queue(cx);
    let second = queue(cx);
    let first = first.await;
    let second = second.await;
    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_err(), "the second apply wrote: {second:?}");
    cx.run_until_parked();
    let created = snapshot(cx)
        .tasks()
        .into_iter()
        .filter(|task| task.goal() == "Api endpoint")
        .count();
    assert_eq!(created, 1);
    drop(h);
}

#[gpui::test]
async fn a_tree_change_between_preview_and_confirm_refuses_and_writes_nothing(
    cx: &mut gpui::TestAppContext,
) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    until_confirm(&workspace, cx).await;

    // Someone renames schema on disk while the confirm is open.
    let schema = h.billing.join("tasks/schema/task.toml");
    let text = std::fs::read_to_string(&schema).unwrap();
    write(
        &schema,
        &text.replace("goal = \"Schema\"", "goal = \"Schema v2\""),
    );
    wait_for_in(cx, "the rename", |cx| {
        task_file(cx, "s").goal == "Schema v2"
    })
    .await;
    let before = files(&root_of(&h.billing));

    answer("Apply", cx);
    wait_for_toast(cx, STALE).await;
    assert_eq!(files(&root_of(&h.billing)), before);
}

#[gpui::test]
async fn a_chat_started_between_preview_and_confirm_refuses_and_writes_nothing(
    cx: &mut gpui::TestAppContext,
) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, DROP_RATES, false)]).await;
    let chats = FakeChats::install(cx);
    until_confirm(&workspace, cx).await;
    assert!(preview_text(cx).unwrap().contains("\nCancel\n  rates\n"));
    let before = files(&root_of(&h.billing));

    chats.open("r", cx);
    answer("Apply", cx);
    wait_for_toast(cx, STALE).await;
    assert_eq!(files(&root_of(&h.billing)), before);
    assert_eq!(task_file(cx, "r").status, Status::Open);
}

#[gpui::test]
async fn a_live_chat_keeps_running_after_an_apply_that_tried_to_drop_its_task(
    cx: &mut gpui::TestAppContext,
) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, DROP_RATES, false)]).await;
    let chats = FakeChats::install(cx);
    chats.open("r", cx);
    let rates = h.billing.join("tasks/engine/tasks/rates/task.toml");
    let rates_before = std::fs::read(&rates).unwrap();

    until_confirm(&workspace, cx).await;
    let shown = preview_text(cx).unwrap();
    assert!(shown.contains("rates: kept"), "{shown}");
    assert!(shown.contains("has a running chat"), "{shown}");
    answer("Apply", cx);
    wait_for_toast_starting(cx, "proposal 1 applied").await;

    assert_eq!(std::fs::read(&rates).unwrap(), rates_before);
    assert_eq!(task_file(cx, "r").status, Status::Open);
    // The chat is still open on the task: the apply closed nothing in the chats' registry.
    assert!(chats.0.borrow().contains("r"), "the chat was closed");
    let modes = cx.update(|_, cx| LiveModesRegistry::live_modes("r", cx));
    assert_eq!(modes.open_chats, 1, "the chat no longer counts as open");
}

#[gpui::test]
async fn a_conflicted_buffer_refuses_and_a_dirty_one_is_saved_first(cx: &mut gpui::TestAppContext) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let before = files(&root_of(&h.billing));
    dispatch(cx, "task::EditProposal");
    let editor = open_editor(&workspace, cx).await;
    let buffer = editor.read_with(cx, |editor, cx| {
        editor.buffer().read(cx).as_singleton().unwrap()
    });

    // A dirty buffer is saved before anything else.
    editor.update_in(cx, |editor, window, cx| {
        editor.go_to_singleton_buffer_point(Point::new(0, 0), window, cx);
        editor.insert("# saved first\n", window, cx);
    });
    assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    until_confirm(&workspace, cx).await;
    assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    let saved = h.fs.load(&h.file(1)).await.unwrap();
    assert!(saved.starts_with("# saved first\n"), "{saved}");
    answer("Cancel", cx);

    // A buffer in conflict with the disk refuses before any question.
    buffer.update(cx, |buffer, _| buffer.set_conflict());
    clear_toast(cx);
    dispatch(cx, "task::ApplyProposal");
    wait_for_toast(cx, &conflict_message(1)).await;
    assert!(!has_modal(&workspace, cx));
    assert_eq!(files(&root_of(&h.billing)), before);
}

#[gpui::test]
async fn a_proposal_with_an_error_opens_the_view_on_its_first_problem_and_applies_nothing(
    cx: &mut gpui::TestAppContext,
) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(2, BROKEN, false)]).await;
    let before = files(&root_of(&h.billing));
    clear_toast(cx);
    dispatch(cx, "task::ApplyProposal");
    wait_for_toast(cx, &errors_message(2)).await;
    let ws = workspace.clone();
    wait_for_in(cx, "the view on its first problem", |cx| {
        the_view(&ws, cx).is_some_and(|view| text(&view, cx).contains(">  line 10: error: cycle"))
    })
    .await;
    assert!(!has_modal(&workspace, cx));
    assert!(preview_text(cx).is_none());
    assert_eq!(files(&root_of(&h.billing)), before);
}

/// Gives the leaf `schema` the branch `schema-cols`, so a split moves work into a keeper.
async fn give_schema_a_branch(billing: &Path, cx: &mut VisualTestContext) {
    let schema = billing.join("tasks/schema/task.toml");
    let text = std::fs::read_to_string(&schema).unwrap();
    write(
        &schema,
        &text.replacen(
            "status = \"open\"\n",
            "status = \"open\"\nbranch = \"schema-cols\"\n",
            1,
        ),
    );
    wait_for_in(cx, "the branch", |cx| {
        task_file(cx, "s").branch.as_deref() == Some("schema-cols")
    })
    .await;
}

#[gpui::test]
async fn the_keeper_picked_reaches_the_change(cx: &mut gpui::TestAppContext) {
    let (h, workspace, cx) =
        harness(cx, true, Some("b"), &[proposal(1, SPLIT_SCHEMA, false)]).await;
    give_schema_a_branch(&h.billing, cx).await;

    clear_toast(cx);
    dispatch(cx, "task::ApplyProposal");
    let ws = workspace.clone();
    wait_for_in(cx, "the keeper picker", |cx| has_modal(&ws, cx)).await;
    // The keeper is asked before any preview; the default is `cols`, `docs` is picked.
    assert!(preview_text(cx).is_none());
    answer("docs", cx);
    wait_for_in(cx, "the confirm", |cx| {
        has_modal(&ws, cx) && preview_text(cx).is_some()
    })
    .await;
    let shown = preview_text(cx).unwrap();
    assert!(
        shown.contains("into \"docs\" (keeps work, chosen)"),
        "{shown}"
    );
    answer("Apply", cx);
    wait_for_toast_starting(cx, "proposal 1 applied").await;

    let now = snapshot(cx);
    let docs = task_by_goal(&now, "Column docs").expect("docs created");
    let cols = task_by_goal(&now, "Columns").expect("cols created");
    assert_eq!(
        now.task_by_id(&docs)
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .branch
            .as_deref(),
        Some("schema-cols")
    );
    assert_eq!(
        now.task_by_id(&cols).unwrap().file.as_ref().unwrap().branch,
        None
    );
    assert_eq!(task_file(cx, "s").branch, None);
}

#[gpui::test]
async fn a_split_of_a_leaf_with_a_running_chat_shows_kept_in_the_view(
    cx: &mut gpui::TestAppContext,
) {
    let (_h, workspace, cx) =
        harness(cx, true, Some("b"), &[proposal(1, SPLIT_SCHEMA, false)]).await;
    let chats = FakeChats::install(cx);
    let view = crate::view_tests::show(&workspace, cx).await;
    assert!(
        crate::view_tests::row(&view, "schema: Schema", cx).contains("[split"),
        "{}",
        text(&view, cx)
    );
    chats.open("s", cx);
    let shown = crate::view_tests::row(&view, "schema: Schema", cx);
    assert!(shown.contains("[kept"), "{shown}");
}

#[gpui::test]
async fn reject_keeps_the_file_and_the_tree(cx: &mut gpui::TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let root = root_of(&h.billing);
    let before = files(&root);
    let billing_before = task_file(cx, "b");
    clear_toast(cx);
    dispatch(cx, "task::RejectProposal");
    wait_for_toast(cx, &rejected_message(1)).await;

    let after = files(&root);
    let billing_file = h.billing.join("task.toml");
    for (path, bytes) in &before {
        if *path != billing_file {
            assert_eq!(after.get(path), Some(bytes), "{} changed", path.display());
        }
    }
    assert_eq!(after.len(), before.len());
    assert_eq!(std::fs::read_to_string(h.file(1)).unwrap(), LAYERS);
    let billing = task_file(cx, "b");
    assert_eq!(billing.proposals.len(), 1);
    assert_eq!(billing.proposals[0].state, ProposalState::Rejected);
    assert_eq!(billing.proposals[0].hash, file_hash(LAYERS.as_bytes()));
    // Only the record: no history, no other field.
    assert_eq!(billing.history, billing_before.history);
    assert_eq!(
        bongos_work::model::TaskFile {
            proposals: Vec::new(),
            ..billing
        },
        billing_before
    );
}

#[gpui::test]
async fn a_reject_of_an_applied_proposal_is_refused_even_after_its_file_was_edited(
    cx: &mut gpui::TestAppContext,
) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, true)]).await;
    let edited = LAYERS.replace("why ", "why edited after the apply: ");
    assert_ne!(edited, LAYERS);
    h.put(1, &edited).await;

    // The writer's command refuses on the tree as it is, whatever the file's bytes.
    let tree = snapshot(cx);
    let refused = reject_command("b", 1, &tree, &WorkContext::kyle(), edited.as_bytes());
    assert_eq!(refused.unwrap_err(), reject_applied_message(1));

    // And so does the command, which leaves the applied record in place.
    let before = files(&root_of(&h.billing));
    clear_toast(cx);
    dispatch(cx, "task::RejectProposal");
    wait_for_toast(cx, &reject_applied_message(1)).await;
    assert_eq!(files(&root_of(&h.billing)), before);
    let billing = task_file(cx, "b");
    assert_eq!(billing.proposals.len(), 1);
    assert_eq!(billing.proposals[0].state, ProposalState::Applied);
    assert_eq!(billing.proposals[0].hash, file_hash(LAYERS.as_bytes()));
}

#[gpui::test]
async fn a_tree_that_already_matches_says_so_and_asks_nothing(cx: &mut gpui::TestAppContext) {
    let unchanged = "\
proposal 1
for b
by kyle
why nothing

task billing: Billing | id b
";
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, unchanged, false)]).await;
    let before = files(&root_of(&h.billing));
    clear_toast(cx);
    dispatch(cx, "task::ApplyProposal");
    wait_for_toast(cx, &nothing_message(1)).await;
    assert!(!has_modal(&workspace, cx));
    assert!(preview_text(cx).unwrap().contains(&nothing_message(1)));
    assert_eq!(files(&root_of(&h.billing)), before);
}

#[gpui::test]
async fn the_actions_resolve_by_name_and_say_so_while_off(cx: &mut gpui::TestAppContext) {
    let (h, workspace, cx) = harness(cx, false, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let before = files(&root_of(&h.billing));
    let mut names = vec!["task::ApplyProposal", "task::RejectProposal"];
    #[cfg(debug_assertions)]
    names.push("proposal::DumpPreview");
    for name in &names {
        clear_toast(cx);
        dispatch(cx, name);
        assert_eq!(last_toast(cx).as_deref(), Some(OFF_MESSAGE), "{name}");
    }
    assert!(!has_modal(&workspace, cx));
    assert_eq!(files(&root_of(&h.billing)), before);

    set_settings_in(cx, true);
    #[cfg(debug_assertions)]
    {
        clear_toast(cx);
        dispatch(cx, "proposal::DumpPreview");
        assert_eq!(
            last_toast(cx).as_deref(),
            Some("No proposal preview to dump")
        );
    }
    until_confirm(&workspace, cx).await;
    answer("Cancel", cx);
    assert_eq!(confirm_prompt(1), "Apply proposal 1?");
}

#[cfg(debug_assertions)]
#[gpui::test]
async fn the_preview_dump_writes_the_last_preview(cx: &mut gpui::TestAppContext) {
    let (_h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    until_confirm(&workspace, cx).await;
    answer("Cancel", cx);
    let dir = tempfile::TempDir::new().unwrap();
    workspace.update(cx, |workspace, cx| {
        crate::dump::dump_preview_into(workspace, dir.path(), cx)
    });
    let written = std::fs::read_to_string(dir.path().join(crate::dump::PREVIEW_DUMP_FILE)).unwrap();
    assert_eq!(Some(written), preview_text(cx));
}
