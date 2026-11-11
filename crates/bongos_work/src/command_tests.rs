//! The `task` and `todo` commands driven the way Kyle drives them: an action dispatched in a
//! workspace, pickers and prompts answered, and the result read back from the store. The store
//! reads and writes a temporary directory with the real file system, as in `store_tests`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_work_model::commands::default_todos;
use bongos_work_model::derive::{self, Shape, State};
use bongos_work_model::layout;
use bongos_work_model::store::{Task as WorkTask, WorkTree};
use bongos_work_model::{Kind, Status, TaskFile, TodoState, serialize_task};
use command_palette_hooks::CommandPaletteFilter;
use fs::RealFs;
use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use project::Project;
use settings::Settings as _;
use tempfile::TempDir;
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::commands::{WorkToast, action_types, init_palette, register};
use crate::modals::{ChoicePicker, TextPrompt};
use crate::store_tests::{canonical, project_toml, set_enabled, write};
use crate::{GlobalWorkStore, WorkDirs, WorkSettings, WorkStore};

pub(crate) fn leaf_toml(id: &str, goal: &str, kind: Kind) -> String {
    let mut file = TaskFile::new(goal, kind);
    file.id = id.to_string();
    file.todos = default_todos(kind);
    serialize_task(&file).unwrap()
}

pub(crate) fn group_toml(id: &str, goal: &str) -> String {
    let mut file = TaskFile::new(goal, Kind::PrShip);
    file.id = id.to_string();
    serialize_task(&file).unwrap()
}

/// A project `invoicing` with the leaf `backfill` (`l1`, pr-ship) and the group `phase` (`g2`)
/// holding the open leaves `alpha` (`l2`) and `beta` (`l3`).
fn seed(root: &Path) {
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(&layout::task_file(&project), &group_toml("g1", "Invoicing"));
    let backfill = layout::child_dir(&project, "backfill");
    write(
        &layout::task_file(&backfill),
        &leaf_toml("l1", "Backfill", Kind::PrShip),
    );
    let phase = layout::child_dir(&project, "phase");
    write(&layout::task_file(&phase), &group_toml("g2", "Phase"));
    for (id, slug, goal) in [("l2", "alpha", "Alpha"), ("l3", "beta", "Beta")] {
        let dir = layout::child_dir(&phase, slug);
        write(&layout::task_file(&dir), &leaf_toml(id, goal, Kind::PrShip));
    }
}

pub(crate) struct Harness {
    pub(crate) store: Entity<WorkStore>,
    pub(crate) workspace: Entity<Workspace>,
    pub(crate) project: Entity<Project>,
    pub(crate) root: PathBuf,
    _temp: TempDir,
}

/// A store over a temporary tree (seeded when `seeded`) and a workspace with the commands
/// registered.
pub(crate) async fn harness(
    cx: &mut TestAppContext,
    enabled: bool,
    seeded: bool,
) -> (Harness, &mut VisualTestContext) {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    if seeded {
        seed(&root);
    }
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        command_palette_hooks::init(cx);
        WorkSettings::register(cx);
        app_state
    });
    set_enabled(cx, enabled);
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let store = cx.new(|cx| WorkStore::new(fs, WorkDirs::for_test(root.clone()), cx));
    cx.update(|cx| {
        cx.set_global(GlobalWorkStore(store.clone()));
        init_palette(cx);
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        let multi_workspace = MultiWorkspace::test_new(project.clone(), window, cx);
        multi_workspace
            .workspace()
            .update(cx, |workspace, _| register(workspace));
        multi_workspace
    });
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx)
    });
    if enabled {
        wait_until(cx, "the first load", |cx| {
            store.read_with(cx, |store, _| store.snapshot().is_some())
        })
        .await;
        // Let the watches settle before writing anything.
        cx.executor().timer(Duration::from_millis(1000)).await;
    }
    (
        Harness {
            store,
            workspace,
            project,
            root,
            _temp: temp,
        },
        cx,
    )
}

pub(crate) async fn wait_until(
    cx: &mut VisualTestContext,
    what: &str,
    check: impl Fn(&mut VisualTestContext) -> bool,
) {
    for _ in 0..200 {
        cx.run_until_parked();
        if check(cx) {
            return;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

impl Harness {
    pub(crate) fn tree(&self, cx: &mut VisualTestContext) -> Arc<WorkTree> {
        self.store
            .read_with(cx, |store, _| store.snapshot())
            .expect("the store has loaded")
    }

    pub(crate) fn task_dir(&self, path: &[&str]) -> PathBuf {
        let mut dir = layout::project_dir(&self.root, "invoicing");
        for slug in path {
            dir = layout::child_dir(&dir, slug);
        }
        dir
    }

    pub(crate) fn writes(&self, cx: &mut VisualTestContext) -> usize {
        self.store.read_with(cx, |store, _| store.stats().writes)
    }

    pub(crate) fn file(&self, cx: &mut VisualTestContext, path: &[&str]) -> TaskFile {
        let dir = self.task_dir(path);
        let tree = self.tree(cx);
        tree.task_by_dir(&dir)
            .and_then(|task| task.file.clone())
            .unwrap_or_else(|| panic!("no task at {}", dir.display()))
    }

    pub(crate) fn has_toast(&self, cx: &mut VisualTestContext) -> bool {
        self.workspace.read_with(cx, |workspace, _| {
            workspace
                .notification_ids()
                .contains(&NotificationId::unique::<WorkToast>())
        })
    }

    pub(crate) fn clear_toasts(&self, cx: &mut VisualTestContext) {
        self.workspace
            .update(cx, |workspace, cx| workspace.clear_all_notifications(cx));
    }

    pub(crate) fn picker(&self, cx: &mut VisualTestContext) -> Option<Entity<ChoicePicker>> {
        self.workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<ChoicePicker>(cx)
        })
    }

    pub(crate) fn prompt(&self, cx: &mut VisualTestContext) -> Option<Entity<TextPrompt>> {
        self.workspace
            .read_with(cx, |workspace, cx| workspace.active_modal::<TextPrompt>(cx))
    }

    /// The rows of the open picker, as labels.
    pub(crate) fn choices(&self, cx: &mut VisualTestContext) -> Vec<String> {
        let picker = self.picker(cx).expect("a picker is open");
        picker.read_with(cx, |picker, cx| {
            picker.picker.read(cx).delegate.choices.clone()
        })
    }
}

/// Dispatches the action `name`, as the command palette does.
pub(crate) fn dispatch(cx: &mut VisualTestContext, name: &str) {
    let action = cx.update(|_, cx| {
        cx.build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"))
    });
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

pub(crate) enum Input {
    /// Types `query` in the open picker and confirms the first match.
    Pick(&'static str),
    /// Replaces the text of the open prompt and confirms it.
    Text(&'static str),
}

impl Harness {
    /// Dispatches `name` and answers each modal it opens with the next of `inputs`.
    pub(crate) async fn run(&self, cx: &mut VisualTestContext, name: &str, inputs: &[Input]) {
        dispatch(cx, name);
        for input in inputs {
            self.answer(cx, input).await;
        }
    }

    pub(crate) async fn answer(&self, cx: &mut VisualTestContext, input: &Input) {
        match input {
            Input::Pick(query) => {
                wait_until(cx, "a picker", |cx| self.picker(cx).is_some()).await;
                let picker = self.picker(cx).unwrap();
                picker.update_in(cx, |modal, window, cx| {
                    modal
                        .picker
                        .update(cx, |picker, cx| picker.set_query(query, window, cx))
                });
                cx.run_until_parked();
            }
            Input::Text(text) => {
                wait_until(cx, "a prompt", |cx| self.prompt(cx).is_some()).await;
                let prompt = self.prompt(cx).unwrap();
                prompt.update_in(cx, |prompt, window, cx| {
                    prompt
                        .editor
                        .update(cx, |editor, cx| editor.set_text(*text, window, cx))
                });
            }
        }
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
    }

    /// Runs the flow and waits for the write it ends in.
    pub(crate) async fn run_and_wait(
        &self,
        cx: &mut VisualTestContext,
        name: &str,
        inputs: &[Input],
    ) {
        let before = self.writes(cx);
        self.run(cx, name, inputs).await;
        wait_until(cx, &format!("{name} to be written"), |cx| {
            self.writes(cx) > before
        })
        .await;
    }
}

pub(crate) fn find<'a>(tree: &'a WorkTree, goal: &str) -> &'a WorkTask {
    tree.tasks()
        .into_iter()
        .find(|task| task.goal() == goal)
        .unwrap_or_else(|| panic!("no task with the goal {goal}"))
}

#[gpui::test]
async fn new_with_only_a_goal_creates_a_leaf_with_kind_todos_and_state(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(cx, "task::New", &[Input::Text("Proration spec")])
        .await;

    let tree = harness.tree(cx);
    let task = find(&tree, "Proration spec");
    let file = task.file.as_ref().unwrap();
    assert_eq!(derive::shape(task, &tree), Shape::Leaf);
    assert_eq!(file.kind, Kind::Spike);
    assert_eq!(file.status, Status::Open);
    assert_eq!(file.activity.as_deref(), Some(crate::DEFAULT_ACTIVITY));
    let todos: Vec<&str> = file.todos.iter().map(|todo| todo.name.as_str()).collect();
    assert_eq!(todos, ["explore", "write up"]);
    assert_eq!(derive::state(task, &tree), State::Ready);
    assert_eq!(file.history[0].event, "created");
    assert!(harness.prompt(cx).is_none(), "the prompt closed");
    assert!(!harness.has_toast(cx));
}

/// The todos the test hook hands to a new task.
fn hook_todos() -> Vec<bongos_work_model::Todo> {
    ["plan it", "build it"]
        .into_iter()
        .map(|name| bongos_work_model::Todo {
            name: name.to_string(),
            role: None,
            workflow: None,
            state: TodoState::Open,
            started: None,
            extra: bongos_work_model::Extra::new(),
        })
        .collect()
}

#[gpui::test]
async fn a_todos_hook_fills_new_and_new_one_off(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    cx.update(|_, cx| crate::set_new_task_todos(|_| Some(hook_todos()), cx));
    harness
        .run_and_wait(cx, "task::New", &[Input::Text("Hooked task")])
        .await;
    harness
        .run_and_wait(
            cx,
            "task::NewOneOff",
            &[Input::Text("Research"), Input::Text("Hooked one off")],
        )
        .await;

    let tree = harness.tree(cx);
    for goal in ["Hooked task", "Hooked one off"] {
        let file = find(&tree, goal).file.as_ref().unwrap();
        let names: Vec<&str> = file.todos.iter().map(|todo| todo.name.as_str()).collect();
        assert_eq!(names, ["plan it", "build it"], "{goal}");
    }

    // A hook with nothing to say leaves the kind's todos.
    cx.update(|_, cx| crate::set_new_task_todos(|_| None, cx));
    harness
        .run_and_wait(cx, "task::New", &[Input::Text("Plain task")])
        .await;
    let tree = harness.tree(cx);
    let file = find(&tree, "Plain task").file.as_ref().unwrap();
    let names: Vec<&str> = file.todos.iter().map(|todo| todo.name.as_str()).collect();
    assert_eq!(names, ["explore", "write up"]);
}

#[gpui::test]
async fn an_empty_goal_keeps_the_prompt_open(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let before = harness.writes(cx);
    harness.run(cx, "task::New", &[Input::Text("   ")]).await;
    assert!(harness.prompt(cx).is_some());
    assert_eq!(harness.writes(cx), before);
}

#[gpui::test]
async fn new_child_goes_under_the_picked_task(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(
            cx,
            "task::NewChild",
            &[Input::Pick("phase"), Input::Text("Gamma")],
        )
        .await;
    let tree = harness.tree(cx);
    let phase = tree.task_by_dir(&harness.task_dir(&["phase"])).unwrap();
    let slugs: Vec<&str> = phase
        .children
        .iter()
        .map(|child| child.slug.as_str())
        .collect();
    assert!(slugs.contains(&"gamma"), "{slugs:?}");
}

#[gpui::test]
async fn advance_walks_every_todo_when_no_todo_has_a_workflow(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    assert!(
        harness
            .file(cx, &["backfill"])
            .todos
            .iter()
            .all(|todo| todo.workflow.is_none())
    );
    for done in 1..=3 {
        harness
            .run_and_wait(cx, "task::Advance", &[Input::Pick("backfill")])
            .await;
        let file = harness.file(cx, &["backfill"]);
        let finished = file
            .todos
            .iter()
            .filter(|todo| todo.state == TodoState::Done)
            .count();
        assert_eq!(finished, done);
    }
    let tree = harness.tree(cx);
    let task = tree.task_by_dir(&harness.task_dir(&["backfill"])).unwrap();
    assert!(derive::leaf_done(task));

    // Nothing is left to advance on it: the picker lists the other leaves only.
    dispatch(cx, "task::Advance");
    let rows = harness.choices(cx);
    assert!(rows.iter().all(|row| !row.contains("Backfill")), "{rows:?}");
}

#[gpui::test]
async fn close_reopen_and_edit_change_the_picked_task(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(cx, "task::Close", &[Input::Pick("backfill")])
        .await;
    assert_eq!(harness.file(cx, &["backfill"]).status, Status::Done);

    harness
        .run_and_wait(cx, "task::Reopen", &[Input::Pick("backfill")])
        .await;
    assert_eq!(harness.file(cx, &["backfill"]).status, Status::Open);

    harness
        .run_and_wait(
            cx,
            "task::Edit",
            &[
                Input::Pick("backfill"),
                Input::Text("Backfill March"),
                Input::Pick("document"),
            ],
        )
        .await;
    let file = harness.file(cx, &["backfill"]);
    assert_eq!(file.goal, "Backfill March");
    assert_eq!(file.kind, Kind::Document);
}

#[gpui::test]
async fn editing_nothing_reaches_a_toast(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness.clear_toasts(cx);
    harness
        .run(
            cx,
            "task::Edit",
            &[
                Input::Pick("backfill"),
                Input::Text("Backfill"),
                Input::Pick("pr-ship"),
            ],
        )
        .await;
    wait_until(cx, "the refusal toast", |cx| harness.has_toast(cx)).await;
}

#[gpui::test]
async fn drop_on_a_group_asks_first(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let before = harness.writes(cx);

    // Dropping the group opens a confirmation that names what else is cancelled, and the
    // default answer keeps everything.
    harness.run(cx, "task::Drop", &[Input::Pick("phase")]).await;
    let rows = harness.choices(cx);
    assert!(rows[0].starts_with("Drop phase and cancel"), "{rows:?}");
    assert!(
        rows[0].contains("alpha") && rows[0].contains("beta"),
        "{rows:?}"
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    cx.executor().timer(Duration::from_millis(300)).await;
    assert_eq!(harness.writes(cx), before);
    assert_eq!(harness.file(cx, &["phase", "alpha"]).status, Status::Open);

    // Confirming drops the group and its open tasks.
    harness
        .run_and_wait(
            cx,
            "task::Drop",
            &[Input::Pick("phase"), Input::Pick("Drop phase")],
        )
        .await;
    for path in [&["phase"][..], &["phase", "alpha"], &["phase", "beta"]] {
        assert_eq!(harness.file(cx, path).status, Status::Cancelled, "{path:?}");
    }
    assert_eq!(harness.file(cx, &["backfill"]).status, Status::Open);
}

#[gpui::test]
async fn dropping_a_leaf_needs_no_confirmation(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(cx, "task::Drop", &[Input::Pick("backfill")])
        .await;
    assert_eq!(harness.file(cx, &["backfill"]).status, Status::Cancelled);
    assert!(harness.picker(cx).is_none());
}

#[gpui::test]
async fn todo_commands_edit_the_picked_todo(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let names = |file: &TaskFile| -> Vec<String> {
        file.todos.iter().map(|todo| todo.name.clone()).collect()
    };

    harness
        .run_and_wait(
            cx,
            "todo::Add",
            &[Input::Pick("backfill"), Input::Text("qa")],
        )
        .await;
    assert_eq!(
        names(&harness.file(cx, &["backfill"])),
        ["implement", "review", "ship", "qa"]
    );

    harness
        .run_and_wait(
            cx,
            "todo::Move",
            &[
                Input::Pick("backfill"),
                Input::Pick("qa"),
                Input::Pick("1."),
            ],
        )
        .await;
    assert_eq!(
        names(&harness.file(cx, &["backfill"])),
        ["qa", "implement", "review", "ship"]
    );

    harness
        .run_and_wait(
            cx,
            "todo::SetWorkflow",
            &[
                Input::Pick("backfill"),
                Input::Pick("review"),
                Input::Text("review-pr"),
            ],
        )
        .await;
    assert_eq!(
        harness.file(cx, &["backfill"]).todos[2].workflow.as_deref(),
        Some("review-pr")
    );
    harness
        .run_and_wait(
            cx,
            "todo::SetWorkflow",
            &[
                Input::Pick("backfill"),
                Input::Pick("review"),
                Input::Text(""),
            ],
        )
        .await;
    assert_eq!(harness.file(cx, &["backfill"]).todos[2].workflow, None);

    harness
        .run_and_wait(
            cx,
            "todo::SetRole",
            &[
                Input::Pick("backfill"),
                Input::Pick("ship"),
                Input::Pick("none"),
            ],
        )
        .await;
    assert_eq!(harness.file(cx, &["backfill"]).todos[3].role, None);

    harness
        .run_and_wait(
            cx,
            "todo::Start",
            &[Input::Pick("backfill"), Input::Pick("review")],
        )
        .await;
    assert!(harness.file(cx, &["backfill"]).todos[2].started.is_some());

    harness
        .run_and_wait(
            cx,
            "todo::Skip",
            &[Input::Pick("backfill"), Input::Pick("qa")],
        )
        .await;
    assert_eq!(
        harness.file(cx, &["backfill"]).todos[0].state,
        TodoState::Skipped
    );

    harness
        .run_and_wait(
            cx,
            "todo::Reopen",
            &[Input::Pick("backfill"), Input::Pick("qa")],
        )
        .await;
    assert_eq!(
        harness.file(cx, &["backfill"]).todos[0].state,
        TodoState::Open
    );

    harness
        .run_and_wait(
            cx,
            "todo::Remove",
            &[Input::Pick("backfill"), Input::Pick("qa")],
        )
        .await;
    assert_eq!(
        names(&harness.file(cx, &["backfill"])),
        ["implement", "review", "ship"]
    );
}

#[gpui::test]
async fn a_refused_todo_command_reaches_a_toast(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness.clear_toasts(cx);
    // The ships role is taken by the `ship` todo; giving it to `review` is refused.
    harness
        .run(
            cx,
            "todo::SetRole",
            &[
                Input::Pick("backfill"),
                Input::Pick("review"),
                Input::Pick("ships"),
            ],
        )
        .await;
    wait_until(cx, "the refusal toast", |cx| harness.has_toast(cx)).await;
}

/// Every action these commands register, but the debug-only `work::DumpSessionContext` and
/// `work::DumpView`.
const NAMES: [&str; 58] = [
    "task::New",
    "task::NewChild",
    "task::Advance",
    "task::AdvanceOverride",
    "task::Close",
    "task::Drop",
    "task::Reopen",
    "task::Edit",
    "todo::Add",
    "todo::Remove",
    "todo::Skip",
    "todo::Reopen",
    "todo::Move",
    "todo::SetWorkflow",
    "todo::SetRole",
    "todo::Start",
    "task::NewOneOff",
    "task::AddEdge",
    "task::RemoveEdge",
    "task::ShowBlockers",
    "task::SetUnblockPoint",
    "task::SetMergeShip",
    "task::Retarget",
    "task::ClearRebase",
    "task::SetResult",
    "task::ClearResult",
    "task::Split",
    "task::Move",
    "task::MoveToProject",
    "task::MakeOneOff",
    "task::Start",
    "task::Focus",
    "task::Unfocus",
    "task::OpenWorktree",
    "task::OpenPr",
    "task::CopyPath",
    "project::New",
    "project::Archive",
    "project::Unarchive",
    "project::OpenFolder",
    "project::Focus",
    "work::AddRepository",
    "task::SetDefaultVisibility",
    "task::Think",
    "task::Mode",
    "artifact::Hide",
    "artifact::Share",
    "artifact::HideAll",
    "artifact::ShareAll",
    "artifact::Link",
    "work::TogglePanel",
    "work::Up",
    "work::ToggleRow",
    "work::FilterTodo",
    "work::FilterKind",
    "work::ShowStale",
    "work::ShowClosed",
    "work::Graph",
];

/// [`NAMES`], and in a debug build the debug-only actions.
fn names() -> Vec<&'static str> {
    let mut names = NAMES.to_vec();
    if cfg!(debug_assertions) {
        names.push("work::DumpSessionContext");
        names.push("work::DumpView");
    }
    names
}

#[gpui::test]
async fn while_disabled_the_actions_are_hidden_and_refuse(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, false, true).await;
    let names = names();
    assert_eq!(action_types().len(), names.len());
    let hidden = |cx: &mut VisualTestContext| -> Vec<bool> {
        cx.update(|_, cx| {
            let filter = CommandPaletteFilter::try_global(cx).unwrap();
            names
                .iter()
                .map(|name| filter.is_hidden(&*cx.build_action(name, None).unwrap()))
                .collect()
        })
    };
    assert!(hidden(cx).iter().all(|hidden| *hidden), "hidden while off");

    for name in names.iter().copied() {
        harness.clear_toasts(cx);
        dispatch(cx, name);
        assert!(harness.picker(cx).is_none(), "{name} opened a picker");
        assert!(harness.prompt(cx).is_none(), "{name} opened a prompt");
        assert!(harness.has_toast(cx), "{name} showed no toast");
    }
    // Nothing was read or written.
    assert!(
        harness
            .store
            .read_with(cx, |store, _| store.snapshot().is_none())
    );
    assert_eq!(harness.writes(cx), 0);
    assert!(!harness.task_dir(&["gamma"]).exists());

    set_enabled(cx, true);
    assert!(hidden(cx).iter().all(|hidden| !*hidden), "shown while on");
    set_enabled(cx, false);
    assert!(hidden(cx).iter().all(|hidden| *hidden), "hidden again");
}
