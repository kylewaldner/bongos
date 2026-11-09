//! The plan todo, the plan chat and the chat instructions, against the fake CLIs of
//! `bongos_chat`'s test fixtures, run in a real PTY. Parking is allowed, so timers run in real
//! time. Nothing here touches the real home directory: the chats' home, the fakes' log and the
//! builder's format file are under a temporary directory.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bongos_chat::model::{read_scope_chats, seed};
use bongos_chat::{CONTEXT_FILE, ChatDirs, ChatPanel};
use bongos_work::model::store::{Task, WorkTree};
use bongos_work::model::{Kind, TodoRole};
use bongos_work::{WorkDirs, WorkStore};
use fs::RealFs;
use gpui::{Entity, Focusable as _, TestAppContext, UpdateGlobal as _, VisualTestContext};
use project::Project;
use settings::{ChatAgentContent, Settings as _, SettingsStore};
use tempfile::TempDir;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::{
    BuilderDirs, MAX_PRINTED_LINES, PLAN_TYPE, TaskBuilderSettings, plan_section, pointer_section,
};

const STARTUP: Duration = Duration::from_secs(10);

/// The human-only artifacts of the test task: path, description.
const SECRETS: [(&str, &str); 3] = [
    ("secret-root.md", "Root-level compensation numbers"),
    ("notes/salary-review.md", "Salary review notes"),
    ("prompts.txt", "The private prompts Kyle typed"),
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../bongos_chat/test-fixtures")
        .join(name)
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn task_toml(id: &str, goal: &str, kind: &str, todos: &[(&str, Option<&str>)]) -> String {
    let mut text = format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"{kind}\"\nstatus = \"open\"\n\
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
    text
}

/// A project `invoicing` with the leaf `backfill` (`t1`, one plan todo, one shared file and the
/// human-only files of `SECRETS`) and the group `big` with `children` leaves of two todos each.
fn seed_tree(root: &Path, children: usize) {
    let project = root.join("projects/invoicing");
    write(
        &project.join("project.toml"),
        "schema = 1\ntitle = \"Invoicing\"\n",
    );
    write(
        &project.join("task.toml"),
        &task_toml("top", "Invoicing", "other", &[]),
    );
    let backfill = project.join("tasks/backfill");
    write(
        &backfill.join("task.toml"),
        &task_toml(
            "t1",
            "Backfill usage events",
            "other",
            &[("plan", Some("plans")), ("implement", None)],
        ),
    );
    write(&backfill.join("docs/design.md"), "# design\n");
    let mut manifest = String::from(
        "schema = 1\n\n[[artifact]]\npath = \"docs/design.md\"\ndescription = \"The backfill \
         design\"\nvisibility = \"shared\"\n",
    );
    for (path, description) in SECRETS {
        write(&backfill.join(path), "private\n");
        manifest.push_str(&format!(
            "\n[[artifact]]\npath = \"{path}\"\ndescription = \"{description}\"\n\
             visibility = \"human-only\"\n"
        ));
    }
    write(&backfill.join("artifacts.toml"), &manifest);

    let big = project.join("tasks/big");
    write(
        &big.join("task.toml"),
        &task_toml("big", "Big group", "other", &[]),
    );
    for index in 0..children {
        write(
            &big.join(format!("tasks/c{index:03}/task.toml")),
            &task_toml(
                &format!("c{index:03}"),
                &format!("Child number {index:03}"),
                "other",
                &[("implement", None), ("review", None)],
            ),
        );
    }
}

struct Harness {
    _temp: TempDir,
    log: PathBuf,
    runtime_root: PathBuf,
    format_path: PathBuf,
    /// The directory of the task `t1`.
    scope: PathBuf,
    /// The directory of the group `big`.
    big: PathBuf,
}

fn set_settings(cx: &mut TestAppContext, builder: bool) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(true);
                content.task_builder.get_or_insert_default().enabled = Some(builder);
                let chats = content.chats.get_or_insert_default();
                chats.enabled = Some(true);
                chats.idle_after_secs = Some(60);
                let agents = chats.agents.get_or_insert_default();
                agents.claude = Some(ChatAgentContent {
                    command: Some(fixture("fake-claude").to_string_lossy().into_owned()),
                    extra_args: None,
                });
                agents.codex = Some(ChatAgentContent {
                    command: Some(fixture("fake-codex").to_string_lossy().into_owned()),
                    extra_args: None,
                });
            });
        });
    });
}

fn set_builder_enabled(cx: &mut VisualTestContext, enabled: bool) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_builder.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

/// The app's wiring over temporary directories and the fake CLIs, with a window that has the work,
/// chat and builder commands, the focused task `t1` and `builder` as `task_builder.enabled`.
async fn harness(
    cx: &mut TestAppContext,
    builder: bool,
    children: usize,
) -> (Harness, Entity<Workspace>, &mut VisualTestContext) {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("work");
    seed_tree(&root, children);
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let types_root = base.join("chat-types");
    seed(&types_root).unwrap();
    let log = base.join("fake.log");
    let runtime_root = base.join("runtime");
    let dirs = BuilderDirs::new(&home);

    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        // Saved panel state (the key value store) belongs to this test alone.
        cx.set_global(db::AppDatabase::test_new());
        editor::init(cx);
        bongos_prompts::PromptsSettings::register(cx);
        app_state
    });
    cx.update(|cx| {
        bongos_work::WorkSettings::register(cx);
        bongos_chat::ChatsSettings::register(cx);
        TaskBuilderSettings::register(cx);
    });
    set_settings(cx, builder);
    let db = cx.update(|cx| db::kvp::KeyValueStore::global(cx));
    db.scoped(bongos_work::KVP_NAMESPACE)
        .write(bongos_work::FOCUS_KEY.to_string(), "\"t1\"".to_string())
        .await
        .unwrap();

    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let env: BTreeMap<String, String> = [
        ("FAKE_CLI_LOG", log.to_string_lossy().into_owned()),
        ("FAKE_CLI_HOME", home.to_string_lossy().into_owned()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect();
    cx.update(|cx| {
        bongos_work::init(fs.clone(), WorkDirs { root }, cx);
        bongos_chat::init_in(
            home.clone(),
            env,
            ChatDirs {
                types_root,
                runtime: runtime_root.clone(),
            },
            cx,
        );
        crate::init(fs.clone(), dirs.clone(), cx);
    });
    let work = cx.update(|cx| WorkStore::global(cx).unwrap());
    wait_for(cx, "the first load", STARTUP, |cx| {
        work.read_with(cx, |work, _| work.snapshot().is_some())
    })
    .await;
    let scope = base.join("work/projects/invoicing/tasks/backfill");
    let big = base.join("work/projects/invoicing/tasks/big");

    let project = Project::test(app_state.fs.clone(), [scope.as_path()], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(move |window, cx| {
        let multi_workspace = MultiWorkspace::test_new(project, window, cx);
        multi_workspace.workspace().update(cx, |workspace, cx| {
            bongos_work::register(workspace);
            bongos_chat::register(workspace);
            crate::register(workspace);
            bongos_chat::init_panel(workspace, window, cx);
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
            log,
            runtime_root,
            format_path: dirs.format_path(),
            scope,
            big,
        },
        workspace,
        cx,
    )
}

impl Harness {
    /// The fake CLI's log lines of `name`, without the prefix.
    fn log_lines(&self, name: &str) -> Vec<String> {
        let prefix = format!("{name} ");
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    /// The ids of the chats on the task directory `scope`, with their types.
    fn chats_in(&self, scope: &Path) -> Vec<(String, String)> {
        read_scope_chats(scope)
            .unwrap()
            .into_iter()
            .filter_map(|entry| entry.chat.ok())
            .map(|record| (record.id, record.chat_type))
            .collect()
    }

    fn context_of(&self, id: &str) -> String {
        std::fs::read_to_string(self.runtime_root.join(id).join(CONTEXT_FILE)).unwrap()
    }

    /// Everything the chat `id` was launched with: its launch files, and the arguments and
    /// environment the fake logged.
    fn launch_outputs(&self, id: &str) -> Vec<String> {
        let mut outputs = Vec::new();
        for entry in std::fs::read_dir(self.runtime_root.join(id))
            .unwrap()
            .flatten()
        {
            if entry.path().is_file() {
                outputs.push(std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        for line in self.log_lines("claude") {
            if line.starts_with("arg ") || line.starts_with("env ") {
                outputs.push(line);
            }
        }
        outputs
    }
}

async fn wait_for(
    cx: &mut TestAppContext,
    what: &str,
    limit: Duration,
    check: impl Fn(&mut TestAppContext) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if check(cx) {
            return;
        }
        assert!(
            start.elapsed() <= limit,
            "timed out after {limit:?} waiting for {what}"
        );
        cx.executor().timer(Duration::from_millis(10)).await;
    }
}

async fn wait_for_in(
    cx: &mut VisualTestContext,
    what: &str,
    limit: Duration,
    check: impl Fn(&mut VisualTestContext) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if check(cx) {
            return;
        }
        assert!(
            start.elapsed() <= limit,
            "timed out after {limit:?} waiting for {what}"
        );
        cx.executor().timer(Duration::from_millis(10)).await;
    }
}

fn dispatch(cx: &mut VisualTestContext, name: &str) {
    let action = cx.update(|_, cx| {
        cx.build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"))
    });
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn has_modal(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.has_active_modal(window, cx)
    })
}

/// Waits for a modal (a picker or a prompt), types `text` into it and confirms.
async fn answer(workspace: &Entity<Workspace>, text: &str, cx: &mut VisualTestContext) {
    let ws = workspace.clone();
    wait_for_in(cx, &format!("a modal for `{text}`"), STARTUP, |cx| {
        has_modal(&ws, cx)
    })
    .await;
    cx.simulate_input(text);
    cx.run_until_parked();
    dispatch(cx, "menu::Confirm");
}

fn snapshot(cx: &mut VisualTestContext) -> Arc<WorkTree> {
    cx.update(|_, cx| WorkStore::global(cx).unwrap().read(cx).snapshot().unwrap())
}

fn task_with_goal(tree: &WorkTree, goal: &str) -> Option<Task> {
    tree.tasks()
        .into_iter()
        .find(|task| task.goal() == goal)
        .cloned()
}

/// Runs `name` (which asks for a goal) with `goal` and waits for the task to appear.
async fn create(
    workspace: &Entity<Workspace>,
    name: &str,
    goal: &str,
    cx: &mut VisualTestContext,
) -> Task {
    dispatch(cx, name);
    answer(workspace, goal, cx).await;
    wait_for_in(cx, &format!("`{goal}` in the tree"), STARTUP, |cx| {
        task_with_goal(&snapshot(cx), goal).is_some()
    })
    .await;
    task_with_goal(&snapshot(cx), goal).unwrap()
}

fn todo_names(task: &Task) -> Vec<String> {
    task.file
        .as_ref()
        .unwrap()
        .todos
        .iter()
        .map(|todo| todo.name.clone())
        .collect()
}

#[gpui::test]
async fn a_new_task_has_one_plan_todo_while_enabled_and_the_kind_defaults_otherwise(
    cx: &mut TestAppContext,
) {
    let (_h, workspace, cx) = harness(cx, true, 0).await;

    // While enabled: one `plan` todo, role `plans`, no workflow.
    let planned = create(&workspace, "task::New", "Planned task", cx).await;
    let todos = &planned.file.as_ref().unwrap().todos;
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].name, "plan");
    assert_eq!(todos[0].role, Some(TodoRole::Plans));
    assert_eq!(todos[0].workflow, None);

    // Through `task: new without plan`: the kind's defaults.
    let unplanned = create(&workspace, "task::NewWithoutPlan", "Unplanned task", cx).await;
    let file = unplanned.file.as_ref().unwrap();
    let defaults = bongos_work::model::commands::default_todos(file.kind);
    assert_eq!(
        todo_names(&unplanned),
        defaults
            .iter()
            .map(|todo| todo.name.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        file.todos
            .iter()
            .all(|todo| todo.role != Some(TodoRole::Plans))
    );

    // While disabled: the kind's defaults again.
    set_builder_enabled(cx, false);
    let plain = create(&workspace, "task::New", "Plain task", cx).await;
    assert_eq!(
        todo_names(&plain),
        defaults
            .iter()
            .map(|todo| todo.name.clone())
            .collect::<Vec<_>>()
    );
    assert_ne!(
        plain.file.as_ref().unwrap().kind,
        Kind::PrShip,
        "a one-off is not a pull request"
    );
}

#[gpui::test]
async fn task_plan_starts_a_plan_chat_on_the_task_at_point_and_shows_it_the_second_time(
    cx: &mut TestAppContext,
) {
    let (h, workspace, cx) = harness(cx, true, 0).await;
    dispatch(cx, "task::Plan");
    answer(&workspace, "claude:", cx).await;
    let scope = h.scope.clone();
    wait_for_in(cx, "the plan chat", STARTUP, |_| {
        !h.chats_in(&scope).is_empty()
    })
    .await;
    let chats = h.chats_in(&h.scope);
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].1, PLAN_TYPE);
    let id = chats[0].0.clone();

    // The opening line is pasted and, with auto-submit on, submitted.
    // The fake logs the paste with its bracketed-paste markers, once its Enter arrives.
    let expected = format!(
        "input \x1b[200~Plan this task and write proposal 1 at {}.\x1b[201~",
        h.scope.join("proposals/1.proposal.txt").display()
    );
    wait_for_in(cx, "the opening line in the fake's log", STARTUP, |_| {
        h.log_lines("claude").contains(&expected)
    })
    .await;
    assert_eq!(
        h.log_lines("claude")
            .iter()
            .filter(|line| line.contains("Plan this task and write proposal"))
            .count(),
        1,
        "pasted once"
    );

    // The plan todo got started with the chat.
    wait_for_in(cx, "the plan todo started", STARTUP, |cx| {
        let tree = snapshot(cx);
        let task = tree.task_by_id("t1").unwrap();
        task.file.as_ref().unwrap().todos[0].started.is_some()
    })
    .await;

    // A second `task: plan` shows the same chat: nothing is asked and no chat is made.
    dispatch(cx, "task::Plan");
    cx.executor().timer(Duration::from_millis(300)).await;
    cx.run_until_parked();
    assert!(!has_modal(&workspace, cx), "nothing is asked");
    assert_eq!(h.chats_in(&h.scope), chats);
    let shown = workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<ChatPanel>(cx)
            .and_then(|panel| panel.read(cx).shown_chat().map(str::to_string))
    });
    assert_eq!(shown.as_deref(), Some(id.as_str()));
    assert_eq!(
        h.log_lines("claude")
            .iter()
            .filter(|line| line.contains("Plan this task and write proposal"))
            .count(),
        1,
        "the opening line is not pasted again"
    );
}

#[gpui::test]
async fn a_plan_chat_gets_the_plan_section_and_an_implement_chat_the_pointer(
    cx: &mut TestAppContext,
) {
    let (h, workspace, cx) = harness(cx, true, 0).await;
    dispatch(cx, "task::Plan");
    answer(&workspace, "claude:", cx).await;
    let scope = h.scope.clone();
    wait_for_in(cx, "the plan chat", STARTUP, |_| {
        !h.chats_in(&scope).is_empty()
    })
    .await;
    let plan_id = h.chats_in(&h.scope)[0].0.clone();
    let plan_context = h.context_of(&plan_id);

    workspace.update_in(cx, |workspace, window, cx| {
        bongos_chat::start_chat_on_task_of_type(
            "t1".to_string(),
            "implement",
            workspace,
            window,
            cx,
        )
    });
    answer(&workspace, "claude:", cx).await;
    wait_for_in(cx, "the implement chat", STARTUP, |_| {
        h.chats_in(&scope).len() == 2
    })
    .await;
    let implement_id = h
        .chats_in(&h.scope)
        .into_iter()
        .find(|(_, chat_type)| chat_type == "implement")
        .unwrap()
        .0;
    let implement_context = h.context_of(&implement_id);

    // The plan chat: the section, with the splitting test, the format, the path of the format
    // file, where to write and the subtree.
    assert!(plan_context.contains("## Task builder"), "{plan_context}");
    assert!(plan_context.contains("Splitting test: anything with its own result"));
    assert!(
        plan_context.contains("do not make a leaf for tests unless")
            || plan_context.contains("Do not make a leaf for tests unless")
    );
    assert!(plan_context.contains(bongos_builder_model::help::FORMAT));
    assert!(plan_context.contains(&h.format_path.display().to_string()));
    assert!(plan_context.contains(&format!(
        "{}",
        h.scope.join("proposals/1.proposal.txt").display()
    )));
    assert!(plan_context.contains("by chat $BONGOS_CHAT_ID"));
    assert!(plan_context.contains("for t1"), "the printed subtree");
    assert!(plan_context.contains("task backfill: Backfill usage events"));
    assert!(plan_context.contains("todo plan | role plans"));

    // The implement chat: only the pointer.
    assert!(
        implement_context.contains("## Task builder"),
        "{implement_context}"
    );
    assert!(implement_context.contains(&h.format_path.display().to_string()));
    assert!(implement_context.contains("write a proposal"));
    assert!(!implement_context.contains("Splitting test"));
    assert!(!implement_context.contains("for t1"));
    assert!(!implement_context.contains(bongos_builder_model::help::FORMAT));

    // No hidden artifact path, file name or description in either chat's launch, and the shared
    // file stays listed.
    assert!(plan_context.contains("docs/design.md"));
    for id in [&plan_id, &implement_id] {
        for output in h.launch_outputs(id) {
            for (path, description) in SECRETS {
                let file_name = Path::new(path).file_name().unwrap().to_str().unwrap();
                for needle in [path, file_name, description] {
                    assert!(!output.contains(needle), "{needle} found in:\n{output}");
                }
            }
        }
    }
}

#[gpui::test]
async fn nothing_is_added_while_disabled(cx: &mut TestAppContext) {
    let (h, workspace, cx) = harness(cx, false, 0).await;
    for chat_type in ["plan", "implement"] {
        workspace.update_in(cx, |workspace, window, cx| {
            bongos_chat::start_chat_on_task_of_type(
                "t1".to_string(),
                chat_type,
                workspace,
                window,
                cx,
            )
        });
        answer(&workspace, "claude:", cx).await;
        let scope = h.scope.clone();
        wait_for_in(cx, &format!("the {chat_type} chat"), STARTUP, |_| {
            h.chats_in(&scope)
                .iter()
                .any(|(_, found)| found == chat_type)
        })
        .await;
        let id = h
            .chats_in(&h.scope)
            .into_iter()
            .find(|(_, found)| found == chat_type)
            .unwrap()
            .0;
        let context = h.context_of(&id);
        assert!(!context.contains("Task builder"), "{context}");
        assert!(!context.contains("proposal"), "{context}");
    }
    // `task: plan` only says that the builder is off.
    dispatch(cx, "task::Plan");
    assert!(!has_modal(&workspace, cx));
    assert_eq!(h.chats_in(&h.scope).len(), 2);
}

#[gpui::test]
async fn a_printed_subtree_over_200_lines_becomes_the_pointer(cx: &mut TestAppContext) {
    let (h, workspace, cx) = harness(cx, true, 100).await;
    let tree = snapshot(cx);
    let printed = bongos_builder_model::print(&tree, "big", &Default::default()).unwrap();
    assert!(printed.lines().count() > MAX_PRINTED_LINES);

    workspace.update_in(cx, |workspace, window, cx| {
        bongos_chat::start_chat_on_task_of_type("big".to_string(), "plan", workspace, window, cx)
    });
    answer(&workspace, "claude:", cx).await;
    let big = h.big.clone();
    wait_for_in(cx, "the plan chat", STARTUP, |_| {
        !h.chats_in(&big).is_empty()
    })
    .await;
    let id = h.chats_in(&h.big)[0].0.clone();
    let context = h.context_of(&id);
    assert!(context.contains("## Task builder"));
    assert!(
        context.contains("Splitting test"),
        "the rest of the section stays"
    );
    assert!(context.contains("`task: propose`"), "{context}");
    assert!(
        !context.contains("Child number 050"),
        "the subtree is not printed"
    );
    assert!(!context.contains("for big"));
}

fn fixture_task(dir: &str) -> Task {
    Task {
        slug: "backfill".to_string(),
        dir: PathBuf::from(dir),
        file: None,
        artifacts: None,
        errors: Vec::new(),
        broken_edges: Vec::new(),
        files: BTreeMap::new(),
        hashes: BTreeMap::new(),
        children: Vec::new(),
    }
}

#[test]
fn the_subtree_is_printed_up_to_200_lines_and_a_pointer_replaces_more() {
    let task = fixture_task("/work/backfill");
    let format_path = Path::new("/home/.bongos/task-builder/format.txt");
    let lines = |count: usize| {
        (0..count)
            .map(|line| format!("task line-{line}: Line {line}\n"))
            .collect::<String>()
    };

    let at_limit = plan_section(format_path, &task, 3, Some(&lines(MAX_PRINTED_LINES)));
    assert!(at_limit.contains("task line-199: Line 199"));
    assert!(!at_limit.contains("`task: propose`"));
    assert!(at_limit.contains("/work/backfill/proposals/3.proposal.txt"));

    let over = plan_section(format_path, &task, 3, Some(&lines(MAX_PRINTED_LINES + 1)));
    assert!(!over.contains("task line-0: Line 0"));
    assert!(over.contains("`task: propose`"));

    let none = plan_section(format_path, &task, 3, None);
    assert!(none.contains("`task: propose`"));
    assert!(!none.contains("too long"), "{none}");
    assert!(none.contains("could not be printed"), "{none}");
}

#[test]
fn neither_section_names_an_artifact() {
    let task = fixture_task("/work/backfill");
    let format_path = Path::new("/home/.bongos/task-builder/format.txt");
    let sections = [
        plan_section(format_path, &task, 1, Some("proposal 1\nfor t1\n")),
        pointer_section(format_path),
    ];
    for section in &sections {
        for (path, description) in SECRETS {
            let file_name = Path::new(path).file_name().unwrap().to_str().unwrap();
            for needle in [path, file_name, description, "artifacts.toml"] {
                assert!(!section.contains(needle), "{needle} found in:\n{section}");
            }
        }
    }
}
