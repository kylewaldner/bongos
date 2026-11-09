//! The proposal view, `task: show proposal`, `task: edit proposal` and `proposal: dump view` in a
//! real `MultiWorkspace` window. The work tree is a temporary directory read by the real store; the
//! window's project lives on a fake file system that mirrors the proposal files, so a buffer opens
//! and an unsaved edit can be told from the file.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bongos_builder_model::apply::file_hash;
use bongos_work::model::store::WorkTree;
use bongos_work::{WorkDirs, WorkStore};
use editor::Editor;
use fs::{FakeFs, Fs as _, RealFs};
use gpui::{Entity, Focusable as _, TestAppContext, UpdateGlobal as _, VisualTestContext};
use language::Point;
use project::Project;
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::choose::{Candidate, Choice, choose};
use crate::{BuilderDirs, BuilderToast, ProposalView};

const STARTUP: Duration = Duration::from_secs(10);

pub(crate) fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

pub(crate) fn task_toml(
    id: &str,
    goal: &str,
    todos: &[(&str, Option<&str>)],
    extra: &str,
) -> String {
    let mut text = format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"pr-ship\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n"
    );
    for (name, role) in todos {
        text.push_str(&format!(
            "\n[[todos]]\nname = \"{name}\"\nstate = \"open\"\n"
        ));
        if let Some(role) = role {
            text.push_str(&format!("role = \"{role}\"\n"));
        }
    }
    text.push_str(extra);
    text
}

/// A proposal file of the test task and whether it was applied as it is.
pub(crate) struct Proposal {
    pub(crate) number: u32,
    pub(crate) text: String,
    pub(crate) applied: bool,
}

pub(crate) fn proposal(number: u32, text: &str, applied: bool) -> Proposal {
    Proposal {
        number,
        text: text.to_string(),
        applied,
    }
}

/// A valid proposal for the group `billing`: a renamed leaf, a group with two leaves under it,
/// and a new leaf.
pub(crate) const LAYERS: &str = "\
proposal 1
for b
by kyle
why more layers

task billing: Billing | id b
  task schema: Schema columns | id s
    result a PR adding columns
    todo implement | role produces-branch
    todo ship | role ships
  task engine: Engine | id e
    after schema
    task rates: Rates | id r
      result rates in code
    task proration: Proration | id p
      result proration in code
  task api: Api endpoint
    after engine
    result a PR with the endpoint
";

/// A proposal with a cycle on line 10 and an edge across levels on line 13.
pub(crate) const BROKEN: &str = "\
proposal 2
for b
by kyle

task billing: Billing | id b
  task schema: Schema | id s
    after engine
    result a PR
  task engine: Engine | id e
    after schema
    result a PR
  task api: Api
    after engine/rates
    result a PR
";

/// `LAYERS` as proposal `number`.
pub(crate) fn layers(number: u32) -> String {
    LAYERS.replace("proposal 1", &format!("proposal {number}"))
}

/// A project `acme` with the group `billing` (`b`) holding the leaf `schema` (`s`) and the group
/// `engine` (`e`, with the leaves `rates` and `proration`), and the proposal files of `billing`.
/// Returns the directory of `billing`.
fn seed_tree(root: &Path, proposals: &[Proposal]) -> PathBuf {
    let project = root.join("projects/acme");
    write(
        &project.join("project.toml"),
        "schema = 1\ntitle = \"Acme\"\n",
    );
    write(
        &project.join("task.toml"),
        &task_toml("top", "Acme", &[], ""),
    );
    let billing = project.join("tasks/billing");
    let mut records = String::new();
    for proposal in proposals {
        write(
            &billing.join(format!("proposals/{}.proposal.txt", proposal.number)),
            &proposal.text,
        );
        if proposal.applied {
            records.push_str(&format!(
                "\n[[proposals]]\nnumber = {}\nstate = \"applied\"\nat = 2026-11-09T12:00:00Z\n\
                 by = \"kyle\"\nhash = \"{}\"\n",
                proposal.number,
                file_hash(proposal.text.as_bytes())
            ));
        }
    }
    write(
        &billing.join("task.toml"),
        &task_toml("b", "Billing", &[], &records),
    );
    write(
        &billing.join("tasks/schema/task.toml"),
        &task_toml(
            "s",
            "Schema",
            &[
                ("implement", Some("produces-branch")),
                ("ship", Some("ships")),
            ],
            "",
        ),
    );
    let engine = billing.join("tasks/engine");
    write(
        &engine.join("task.toml"),
        &task_toml("e", "Engine", &[], ""),
    );
    for (slug, id, goal) in [("rates", "r", "Rates"), ("proration", "p", "Proration")] {
        write(
            &engine.join(format!("tasks/{slug}/task.toml")),
            &task_toml(
                id,
                goal,
                &[
                    ("implement", Some("produces-branch")),
                    ("ship", Some("ships")),
                ],
                "",
            ),
        );
    }
    billing
}

pub(crate) struct Harness {
    _temp: TempDir,
    pub(crate) fs: Arc<FakeFs>,
    /// The directory of the group `billing`.
    pub(crate) billing: PathBuf,
}

impl Harness {
    pub(crate) fn file(&self, number: u32) -> PathBuf {
        self.billing
            .join(format!("proposals/{number}.proposal.txt"))
    }

    /// Writes the proposal file on disk and in the window's file system.
    pub(crate) async fn put(&self, number: u32, text: &str) {
        write(&self.file(number), text);
        self.fs
            .create_dir(self.file(number).parent().unwrap())
            .await
            .unwrap();
        self.fs
            .insert_file(self.file(number), text.as_bytes().to_vec())
            .await;
    }
}

fn set_settings(cx: &mut TestAppContext, builder: bool) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(true);
                content.task_builder.get_or_insert_default().enabled = Some(builder);
            });
        });
    });
}

/// The app's wiring over a temporary work tree with the task `billing` focused, and a window
/// whose project holds `/code` only, so a proposal file opens in an invisible worktree.
pub(crate) async fn harness<'a>(
    cx: &'a mut TestAppContext,
    builder: bool,
    focused: Option<&str>,
    proposals: &[Proposal],
) -> (Harness, Entity<Workspace>, &'a mut VisualTestContext) {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("work");
    let billing = seed_tree(&root, proposals);
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        cx.set_global(db::AppDatabase::test_new());
        editor::init(cx);
        bongos_work::WorkSettings::register(cx);
        app_state
    });
    set_settings(cx, builder);
    if let Some(focused) = focused {
        let db = cx.update(|cx| db::kvp::KeyValueStore::global(cx));
        db.scoped(bongos_work::KVP_NAMESPACE)
            .write(bongos_work::FOCUS_KEY.to_string(), format!("\"{focused}\""))
            .await
            .unwrap();
    }
    let real = Arc::new(RealFs::new(None, cx.executor()));
    cx.update(|cx| {
        bongos_work::init(real.clone(), WorkDirs { root }, cx);
        crate::init(real.clone(), BuilderDirs::new(&home), cx);
    });
    let store = cx.update(|cx| WorkStore::global(cx).unwrap());
    wait_for(cx, "the first load", |cx| {
        store.read_with(cx, |store, _| store.snapshot().is_some())
    })
    .await;

    let fake = app_state.fs.as_fake();
    fake.insert_tree("/code", serde_json::json!({ "main.rs": "" }))
        .await;
    for proposal in proposals {
        fake.create_dir(&billing.join("proposals")).await.unwrap();
        fake.insert_file(
            billing.join(format!("proposals/{}.proposal.txt", proposal.number)),
            proposal.text.as_bytes().to_vec(),
        )
        .await;
    }
    let project = Project::test(app_state.fs.clone(), [Path::new("/code")], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(move |window, cx| {
        let multi_workspace = MultiWorkspace::test_new(project, window, cx);
        multi_workspace.workspace().update(cx, |workspace, _| {
            bongos_work::register(workspace);
            crate::register(workspace);
        });
        multi_workspace
    });
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx)
    });
    cx.run_until_parked();
    (
        Harness {
            _temp: temp,
            fs: fake,
            billing,
        },
        workspace,
        cx,
    )
}

async fn wait_for(
    cx: &mut TestAppContext,
    what: &str,
    check: impl Fn(&mut TestAppContext) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if check(cx) {
            return;
        }
        assert!(
            start.elapsed() <= STARTUP,
            "timed out after {STARTUP:?} waiting for {what}"
        );
        cx.executor().timer(Duration::from_millis(10)).await;
    }
}

pub(crate) async fn wait_for_in(
    cx: &mut VisualTestContext,
    what: &str,
    check: impl Fn(&mut VisualTestContext) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if check(cx) {
            return;
        }
        assert!(
            start.elapsed() <= STARTUP,
            "timed out after {STARTUP:?} waiting for {what}"
        );
        cx.executor().timer(Duration::from_millis(10)).await;
    }
}

pub(crate) fn dispatch(cx: &mut VisualTestContext, name: &str) {
    let action = cx.update(|_, cx| {
        cx.build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"))
    });
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

pub(crate) fn the_view(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<ProposalView>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_item_as::<ProposalView>(cx)
    })
}

/// Dispatches `task::ShowProposal` and waits for the view to read its file.
pub(crate) async fn show(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<ProposalView> {
    dispatch(cx, "task::ShowProposal");
    let ws = workspace.clone();
    wait_for_in(cx, "the proposal view", |cx| {
        the_view(&ws, cx).is_some_and(|view| view.read_with(cx, |view, _| !view.lines().is_empty()))
    })
    .await;
    the_view(workspace, cx).unwrap()
}

pub(crate) fn lines(view: &Entity<ProposalView>, cx: &mut VisualTestContext) -> Vec<String> {
    view.read_with(cx, |view, _| view.lines())
}

pub(crate) fn text(view: &Entity<ProposalView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, _| view.view_text())
}

/// The first line of the view that has `needle`.
pub(crate) fn row(view: &Entity<ProposalView>, needle: &str, cx: &mut VisualTestContext) -> String {
    lines(view, cx)
        .into_iter()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line with `{needle}` in\n{}", text(view, cx)))
}

pub(crate) fn snapshot(cx: &mut VisualTestContext) -> Arc<WorkTree> {
    cx.update(|_, cx| WorkStore::global(cx).unwrap().read(cx).snapshot().unwrap())
}

pub(crate) fn has_modal(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.has_active_modal(window, cx)
    })
}

pub(crate) fn toast_shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<BuilderToast>())
    })
}

#[gpui::test]
async fn only_one_level_is_rendered(cx: &mut TestAppContext) {
    let (_h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    assert!(!snapshot(cx).tasks().is_empty());
    let view = show(&workspace, cx).await;

    assert_eq!(view.read_with(cx, |view, _| view.breadcrumb()), "billing");
    let top = text(&view, cx);
    assert!(top.contains("problems: none"), "{top}");
    for label in ["schema", "engine", "api"] {
        row(&view, &format!("{label}: "), cx);
    }
    // Their children are one level down.
    assert!(!top.contains("rates"), "{top}");
    assert!(!top.contains("proration"), "{top}");

    // The marks, the rollup, the edges and the result and todos of a leaf.
    assert!(row(&view, "schema: Schema columns", cx).contains("[changed, pr-ship]"));
    assert!(row(&view, "engine: Engine", cx).contains("2 tasks, 0 todos"));
    assert!(row(&view, "api: Api endpoint", cx).contains("[new, pr-ship]"));
    let all = lines(&view, cx);
    assert!(all.contains(&"    after schema".to_string()), "{all:?}");
    assert!(all.contains(&"    after engine".to_string()), "{all:?}");
    assert!(
        all.contains(&"    result a PR adding columns".to_string()),
        "{all:?}"
    );
    assert!(
        all.contains(&"    todo implement (produces-branch)".to_string()),
        "{all:?}"
    );
    assert!(
        all.contains(&"    todo ship (ships)".to_string()),
        "{all:?}"
    );
}

#[gpui::test]
async fn ret_descends_and_up_ascends(cx: &mut TestAppContext) {
    let (_h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let view = show(&workspace, cx).await;
    assert_eq!(
        view.read_with(cx, |view, _| view.selected_task().map(str::to_string)),
        Some("schema".to_string())
    );

    // Down to `engine`, then RET.
    dispatch(cx, "menu::SelectNext");
    assert_eq!(
        view.read_with(cx, |view, _| view.selected_task().map(str::to_string)),
        Some("engine".to_string())
    );
    dispatch(cx, "proposal::Descend");
    assert_eq!(
        view.read_with(cx, |view, _| view.breadcrumb()),
        "billing > engine"
    );
    let level = text(&view, cx);
    assert!(level.contains("rates: Rates"), "{level}");
    assert!(level.contains("proration: Proration"), "{level}");
    assert!(!level.contains("schema"), "one level only\n{level}");
    assert!(!level.contains("api"), "one level only\n{level}");
    // A leaf has nothing below it: RET on its row stays where it is.
    dispatch(cx, "proposal::Descend");
    assert_eq!(
        view.read_with(cx, |view, _| view.breadcrumb()),
        "billing > engine"
    );

    // `^` goes back up, with the task that was left selected.
    dispatch(cx, "proposal::Up");
    assert_eq!(view.read_with(cx, |view, _| view.breadcrumb()), "billing");
    assert_eq!(
        view.read_with(cx, |view, _| view.selected_task().map(str::to_string)),
        Some("engine".to_string())
    );
    dispatch(cx, "proposal::Up");
    assert_eq!(view.read_with(cx, |view, _| view.breadcrumb()), "billing");
}

#[gpui::test]
async fn the_problems_line_lists_a_cycle_and_a_cross_level_edge_and_ret_opens_the_file(
    cx: &mut TestAppContext,
) {
    let (_h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(2, BROKEN, false)]).await;
    let view = show(&workspace, cx).await;

    let all = text(&view, cx);
    assert!(all.contains("problems: 2 errors"), "{all}");
    let cycle = row(&view, "cycle", cx);
    assert!(
        cycle.trim_start().starts_with("line 10: error: cycle"),
        "{cycle}"
    );
    let cross = row(&view, "cross-level", cx);
    assert!(
        cross
            .trim_start()
            .starts_with("line 13: error: cross-level"),
        "{cross}"
    );
    // The rows are still there, with the problems counted for the task they are on.
    assert!(row(&view, "api: Api", cx).contains("1 problem"), "{all}");

    // From the first task row up to the last problem (the cross-level edge), then RET.
    dispatch(cx, "menu::SelectPrevious");
    dispatch(cx, "proposal::Descend");
    let editor = open_editor(&workspace, cx).await;
    assert_eq!(editor.read_with(cx, |editor, cx| editor.text(cx)), BROKEN);
    assert_eq!(head(&editor, cx), Point::new(12, 0));

    // And the cycle, one problem up.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.activate_item(&view, true, true, window, cx)
    });
    cx.run_until_parked();
    dispatch(cx, "menu::SelectPrevious");
    dispatch(cx, "proposal::Descend");
    wait_for_in(cx, "the cursor on the cycle", |cx| {
        head(&editor, cx) == Point::new(9, 0)
    })
    .await;
}

/// Waits for an editor to be the active item and returns it.
pub(crate) async fn open_editor(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let ws = workspace.clone();
    wait_for_in(cx, "an editor", |cx| {
        ws.read_with(cx, |workspace, cx| {
            workspace.active_item_as::<Editor>(cx).is_some()
        })
    })
    .await;
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_item_as::<Editor>(cx).unwrap()
    })
}

fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        editor.selections.newest::<Point>(&snapshot).head()
    })
}

#[gpui::test]
async fn an_unsaved_edit_in_the_buffer_shows_in_the_view(cx: &mut TestAppContext) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let view = show(&workspace, cx).await;
    assert!(!text(&view, cx).contains("extra"));

    // `task: edit proposal` opens the same file as a buffer.
    dispatch(cx, "task::EditProposal");
    let editor = open_editor(&workspace, cx).await;
    editor.update_in(cx, |editor, window, cx| {
        editor.go_to_singleton_buffer_point(Point::new(16, 0), window, cx);
        editor.insert("  task extra: An extra task\n", window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        editor.read_with(cx, |editor, cx| editor
            .buffer()
            .read(cx)
            .as_singleton()
            .unwrap()
            .read(cx)
            .is_dirty()),
        true
    );
    let shown = text(&view, cx);
    assert!(shown.contains("extra: An extra task"), "{shown}");
    assert!(row(&view, "extra:", cx).contains("[new, pr-ship"));
    // Nothing was saved.
    let on_disk = std::fs::read_to_string(h.file(1)).unwrap();
    assert!(!on_disk.contains("extra"));
    let in_window = h.fs.load(&h.file(1)).await.unwrap();
    assert!(!in_window.contains("extra"));

    // An edit that breaks the file shows its problem at once.
    editor.update_in(cx, |editor, window, cx| {
        editor.go_to_singleton_buffer_point(Point::new(16, 0), window, cx);
        editor.insert("  task Bad Label: Broken\n", window, cx);
    });
    cx.run_until_parked();
    let broken = text(&view, cx);
    assert!(!broken.contains("problems: none"), "{broken}");
}

#[gpui::test]
async fn without_a_buffer_the_view_reads_the_file_when_the_store_changes(cx: &mut TestAppContext) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let view = show(&workspace, cx).await;
    assert!(!text(&view, cx).contains("late"));

    let changed = LAYERS.replace(
        "  task api: Api endpoint",
        "  task late: Late addition\n  task api: Api endpoint",
    );
    h.put(1, &changed).await;
    wait_for_in(cx, "the new text", |cx| {
        text(&view, cx).contains("late: Late addition")
    })
    .await;
}

#[test]
fn the_newest_pending_proposal_is_chosen() {
    let candidate = |number, pending| Candidate {
        number,
        path: PathBuf::from(format!("/p/{number}.proposal.txt")),
        pending,
    };
    assert_eq!(choose(Vec::new()), Choice::Nothing);
    // One pending: that one, even when a newer one was applied.
    assert_eq!(
        choose(vec![candidate(1, true), candidate(2, false)]),
        Choice::One(candidate(1, true))
    );
    // None pending: the newest.
    assert_eq!(
        choose(vec![
            candidate(2, false),
            candidate(1, false),
            candidate(3, false)
        ]),
        Choice::One(candidate(3, false))
    );
    // Several pending: a picker, newest first, without the applied ones.
    assert_eq!(
        choose(vec![
            candidate(1, true),
            candidate(2, false),
            candidate(3, true)
        ]),
        Choice::Pick(vec![candidate(3, true), candidate(1, true)])
    );
}

#[gpui::test]
async fn the_view_shows_the_newest_pending_proposal_and_a_picker_asks_among_two(
    cx: &mut TestAppContext,
) {
    // Proposal 2 was applied, 3 is pending: 3 opens without asking.
    let (_h, workspace, cx) = harness(
        cx,
        true,
        Some("b"),
        &[
            proposal(2, &layers(2), true),
            proposal(3, &layers(3), false),
        ],
    )
    .await;
    let view = show(&workspace, cx).await;
    assert_eq!(view.read_with(cx, |view, _| view.number()), 3);
    assert!(!has_modal(&workspace, cx));
    assert!(text(&view, cx).starts_with("proposal 3\n"));
}

#[gpui::test]
async fn with_none_pending_the_newest_is_shown(cx: &mut TestAppContext) {
    let (_h, workspace, cx) = harness(
        cx,
        true,
        Some("b"),
        &[proposal(1, &layers(1), true), proposal(2, &layers(2), true)],
    )
    .await;
    let view = show(&workspace, cx).await;
    assert_eq!(view.read_with(cx, |view, _| view.number()), 2);
    assert!(!has_modal(&workspace, cx));
}

#[gpui::test]
async fn a_picker_appears_for_two_pending_proposals(cx: &mut TestAppContext) {
    let (_h, workspace, cx) = harness(
        cx,
        true,
        Some("b"),
        &[
            proposal(1, &layers(1), false),
            proposal(2, &layers(2), false),
        ],
    )
    .await;
    dispatch(cx, "task::ShowProposal");
    let ws = workspace.clone();
    wait_for_in(cx, "the picker", |cx| has_modal(&ws, cx)).await;
    assert!(the_view(&workspace, cx).is_none());
    cx.simulate_input("proposal 1");
    cx.run_until_parked();
    dispatch(cx, "menu::Confirm");
    let ws = workspace.clone();
    wait_for_in(cx, "the view", |cx| the_view(&ws, cx).is_some()).await;
    let view = the_view(&workspace, cx).unwrap();
    assert_eq!(view.read_with(cx, |view, _| view.number()), 1);
    assert!(!has_modal(&workspace, cx));
}

#[gpui::test]
async fn without_a_focused_task_a_picker_lists_the_tasks_with_proposals(cx: &mut TestAppContext) {
    let (_h, workspace, cx) = harness(cx, true, None, &[proposal(1, &layers(1), false)]).await;
    dispatch(cx, "task::ShowProposal");
    let ws = workspace.clone();
    wait_for_in(cx, "the task picker", |cx| has_modal(&ws, cx)).await;
    cx.simulate_input("Billing");
    cx.run_until_parked();
    dispatch(cx, "menu::Confirm");
    let ws = workspace.clone();
    wait_for_in(cx, "the view", |cx| the_view(&ws, cx).is_some()).await;
    let view = the_view(&workspace, cx).unwrap();
    assert_eq!(view.read_with(cx, |view, _| view.number()), 1);
}

#[gpui::test]
async fn showing_the_same_proposal_twice_keeps_one_view(cx: &mut TestAppContext) {
    let (_h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let first = show(&workspace, cx).await;
    dispatch(cx, "task::ShowProposal");
    let count = workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<ProposalView>(cx).count()
    });
    assert_eq!(count, 1);
    assert_eq!(the_view(&workspace, cx).unwrap(), first);
}

#[gpui::test]
async fn dispatch_by_name_works_while_on_and_says_so_while_off(cx: &mut TestAppContext) {
    // While off: every action only toasts, and nothing opens.
    let (_h, workspace, cx) = harness(cx, false, Some("b"), &[proposal(1, LAYERS, false)]).await;
    let mut names = vec![
        "task::ShowProposal",
        "task::EditProposal",
        "proposal::Descend",
        "proposal::Up",
    ];
    #[cfg(debug_assertions)]
    names.push("proposal::DumpView");
    for name in &names {
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<BuilderToast>(), cx)
        });
        dispatch(cx, name);
        assert!(toast_shown(&workspace, cx), "{name} shows the off toast");
    }
    assert!(the_view(&workspace, cx).is_none());
    assert!(!workspace.read_with(cx, |workspace, cx| {
        workspace.active_item_as::<Editor>(cx).is_some()
    }));

    // While on: show and edit open an item, the dump says there is no view when there is none.
    set_settings_in(cx, true);
    workspace.update(cx, |workspace, cx| {
        workspace.dismiss_notification(&NotificationId::unique::<BuilderToast>(), cx)
    });
    #[cfg(debug_assertions)]
    {
        dispatch(cx, "proposal::DumpView");
        assert!(toast_shown(&workspace, cx), "no view to dump");
    }
    dispatch(cx, "task::EditProposal");
    let _editor = open_editor(&workspace, cx).await;
    dispatch(cx, "task::ShowProposal");
    let ws = workspace.clone();
    wait_for_in(cx, "the view", |cx| the_view(&ws, cx).is_some()).await;
}

pub(crate) fn set_settings_in(cx: &mut VisualTestContext, builder: bool) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_builder.get_or_insert_default().enabled = Some(builder);
            });
        });
    });
}

#[cfg(debug_assertions)]
#[gpui::test]
async fn the_dump_writes_the_breadcrumb_the_problems_and_the_rows(cx: &mut TestAppContext) {
    let (_h, workspace, cx) = harness(cx, true, Some("b"), &[proposal(2, BROKEN, false)]).await;
    let view = show(&workspace, cx).await;
    let _ = view;
    let dir = TempDir::new().unwrap();
    workspace.update(cx, |workspace, cx| {
        crate::dump::dump_into(workspace, dir.path(), cx)
    });
    cx.run_until_parked();
    let written = std::fs::read_to_string(dir.path().join(crate::dump::DUMP_FILE)).unwrap();
    assert!(
        written.starts_with("proposal 2\nbreadcrumb: billing\n"),
        "{written}"
    );
    assert!(written.contains("problems: 2 errors"), "{written}");
    assert!(written.contains("line 10: error: cycle"), "{written}");
    assert!(written.contains("line 13: error: cross-level"), "{written}");
    assert!(written.contains("schema: Schema"), "{written}");
    assert!(written.contains("api: Api"), "{written}");
}
