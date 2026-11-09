//! Tests of the file commands (D17, D18, D20): `prompts: use file`, `use default` and `open`.
//! Most run against [`FakeChats`], which records the writes of the prompts file. The ones that
//! need the real work tree (the choices, the task that owns a file, `prompts: open` with chats
//! off) build a work root in a temporary directory and use the real link.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_chat::{ChatToast, ChatsSettings};
use bongos_work::{WorkDirs, WorkStore};
use editor::Editor;
use fs::RealFs;
use gpui::{BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::actions::OFF_MESSAGE;
use crate::attach::{ChatLink as _, Chats, PromptChoice, file_path};
use crate::attach_tests::{
    FakeChats, PATH, THREE, answer, chat, has_modal, set_chats_enabled, setup, text, toasts,
    update_prompts_settings,
};
use crate::files::{
    ANOTHER_FILE, DEFAULT_MESSAGE, NO_CHAT_MESSAGE, SAVE_FIRST_MESSAGE, choices_for,
};
use crate::mode_tests::{dispatch, open_in, workspace_on};

const T1: &str = "2026-11-05T10:00:00Z";
const PROJECT_FILE: &str = "/projects/invoicing/prompts.txt";
const TASK_FILE: &str = "/projects/invoicing/tasks/other/prompts.txt";

fn choices() -> Vec<PromptChoice> {
    vec![
        PromptChoice {
            label: "project: Invoicing".to_string(),
            path: PathBuf::from(PROJECT_FILE),
        },
        PromptChoice {
            label: "task: Other".to_string(),
            path: PathBuf::from(TASK_FILE),
        },
    ]
}

/// The attach tests' setup with one running chat `a` that uses the file, in a scope with
/// `choices`.
async fn with_chat(
    cx: &mut TestAppContext,
) -> (
    Entity<Workspace>,
    Entity<Editor>,
    Rc<FakeChats>,
    &mut VisualTestContext,
) {
    let (workspace, editor, fake, cx) = setup(cx, true, 4).await;
    fake.chats.replace(vec![chat("a", true, T1)]);
    fake.scope.replace(Some(PathBuf::from("/dir")));
    fake.choices.replace(choices());
    (workspace, editor, fake, cx)
}

async fn read(workspace: &Entity<Workspace>, path: &str, cx: &mut VisualTestContext) -> String {
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    fs.load(Path::new(path)).await.unwrap_or_default()
}

#[gpui::test]
async fn use_file_to_the_projects_file_writes_the_record(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = with_chat(cx).await;
    dispatch("prompts::UseFile", cx);
    assert!(has_modal(&workspace, cx));
    answer(&workspace, "project", cx).await;
    assert_eq!(
        *fake.file_writes.borrow(),
        vec![("a".to_string(), Some(PathBuf::from(PROJECT_FILE)))]
    );
    assert_eq!(
        *fake.record_file.borrow(),
        Some(PathBuf::from(PROJECT_FILE))
    );
    assert_eq!(toasts(), vec![format!("Using {PROJECT_FILE}")]);
    // The file was created, empty, and the buffer shown now is untouched.
    assert_eq!(read(&workspace, PROJECT_FILE, cx).await, "");
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(read(&workspace, PATH, cx).await, THREE);
}

#[gpui::test]
async fn use_file_keeps_the_text_of_a_file_that_is_there(cx: &mut TestAppContext) {
    let (workspace, _editor, fake, cx) = with_chat(cx).await;
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    fs.as_fake()
        .insert_tree(
            "/projects/invoicing/tasks/other",
            serde_json::json!({
                "prompts.txt": "kept text",
            }),
        )
        .await;
    dispatch("prompts::UseFile", cx);
    answer(&workspace, "task: Other", cx).await;
    assert_eq!(fake.file_writes.borrow().len(), 1);
    assert_eq!(read(&workspace, TASK_FILE, cx).await, "kept text");
}

#[gpui::test]
async fn use_file_asks_for_another_file(cx: &mut TestAppContext) {
    let (workspace, _editor, fake, cx) = with_chat(cx).await;
    dispatch("prompts::UseFile", cx);
    answer(&workspace, ANOTHER_FILE, cx).await;
    assert!(has_modal(&workspace, cx), "the path is asked next");
    assert!(fake.file_writes.borrow().is_empty());
    // A relative path starts at the scope directory; its directories are made.
    answer(&workspace, "notes/mine.txt", cx).await;
    assert_eq!(
        *fake.file_writes.borrow(),
        vec![("a".to_string(), Some(PathBuf::from("/dir/notes/mine.txt")))]
    );
    assert_eq!(read(&workspace, "/dir/notes/mine.txt", cx).await, "");
    let file = fake.record_file.borrow().clone().unwrap();
    assert!(
        workspace
            .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            .is_file(&file)
            .await
    );
}

#[gpui::test]
async fn use_default_clears_the_record(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = with_chat(cx).await;
    dispatch("prompts::UseFile", cx);
    answer(&workspace, "project", cx).await;
    assert!(fake.record_file.borrow().is_some());
    toasts();
    dispatch("prompts::UseDefault", cx);
    assert_eq!(fake.file_writes.borrow()[1], ("a".to_string(), None));
    assert_eq!(*fake.record_file.borrow(), None);
    assert_eq!(toasts(), vec![DEFAULT_MESSAGE.to_string()]);
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn the_panel_chat_is_the_one_that_changes(cx: &mut TestAppContext) {
    let (_workspace, _editor, fake, cx) = with_chat(cx).await;
    fake.chats.replace(Vec::new());
    fake.panel.replace(Some("p".into()));
    dispatch("prompts::UseDefault", cx);
    assert_eq!(*fake.file_writes.borrow(), vec![("p".to_string(), None)]);
}

#[gpui::test]
async fn a_closed_chat_can_change_its_file(cx: &mut TestAppContext) {
    let (_workspace, _editor, fake, cx) = with_chat(cx).await;
    fake.chats.replace(vec![chat("c", false, T1)]);
    dispatch("prompts::UseDefault", cx);
    assert_eq!(*fake.file_writes.borrow(), vec![("c".to_string(), None)]);
}

#[gpui::test]
async fn a_dirty_buffer_refuses_and_nothing_is_dropped(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = with_chat(cx).await;
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("unsaved draft ", window, cx)
    });
    let before = text(&editor, cx);
    assert_ne!(before, THREE);
    assert_eq!(read(&workspace, PATH, cx).await, THREE);
    toasts();

    for action in ["prompts::UseFile", "prompts::UseDefault"] {
        dispatch(action, cx);
        assert_eq!(toasts(), vec![SAVE_FIRST_MESSAGE.to_string()], "{action}");
        assert!(!has_modal(&workspace, cx), "{action}");
        assert!(fake.file_writes.borrow().is_empty(), "{action}");
        assert_eq!(text(&editor, cx).as_bytes(), before.as_bytes(), "{action}");
        assert_eq!(read(&workspace, PATH, cx).await, THREE, "{action}");
    }
    assert_eq!(*fake.record_file.borrow(), None);
}

#[gpui::test]
async fn text_typed_while_the_picker_is_open_is_not_dropped(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = with_chat(cx).await;
    dispatch("prompts::UseFile", cx);
    assert!(has_modal(&workspace, cx));
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("typed meanwhile ", window, cx)
    });
    let before = text(&editor, cx);
    toasts();
    answer(&workspace, "project", cx).await;
    assert_eq!(toasts(), vec![SAVE_FIRST_MESSAGE.to_string()]);
    assert!(fake.file_writes.borrow().is_empty());
    assert_eq!(text(&editor, cx).as_bytes(), before.as_bytes());
}

#[gpui::test]
async fn a_refused_switch_leaves_no_new_file_behind(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = with_chat(cx).await;
    dispatch("prompts::UseFile", cx);
    answer(&workspace, ANOTHER_FILE, cx).await;
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("typed meanwhile ", window, cx)
    });
    toasts();
    answer(&workspace, "notes/never.txt", cx).await;
    assert_eq!(toasts(), vec![SAVE_FIRST_MESSAGE.to_string()]);
    assert!(fake.file_writes.borrow().is_empty());
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    assert!(!fs.is_file("/dir/notes/never.txt".as_ref()).await);
    assert!(!fs.is_dir("/dir/notes".as_ref()).await);
}

#[gpui::test]
async fn a_saved_buffer_may_switch(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = with_chat(cx).await;
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("saved draft ", window, cx)
    });
    dispatch("editor::Cancel", cx);
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let saved = text(&editor, cx);
    assert_eq!(read(&workspace, PATH, cx).await, saved);
    toasts();
    dispatch("prompts::UseDefault", cx);
    assert_eq!(fake.file_writes.borrow().len(), 1);
    assert_eq!(toasts(), vec![DEFAULT_MESSAGE.to_string()]);
}

#[gpui::test]
async fn no_chat_on_the_file_says_so(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = with_chat(cx).await;
    fake.chats.replace(Vec::new());
    dispatch("prompts::UseFile", cx);
    assert!(!has_modal(&workspace, cx));
    assert_eq!(toasts(), vec![NO_CHAT_MESSAGE.to_string()]);
    assert!(fake.file_writes.borrow().is_empty());
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn nothing_changes_while_prompts_are_off(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = with_chat(cx).await;
    update_prompts_settings(cx, |prompts| prompts.enabled = Some(false));
    for action in ["prompts::UseFile", "prompts::UseDefault", "prompts::Open"] {
        dispatch(action, cx);
        assert_eq!(toasts(), vec![OFF_MESSAGE.to_string()], "{action}");
    }
    assert!(!has_modal(&workspace, cx));
    assert!(fake.file_writes.borrow().is_empty());
    assert_eq!(fake.focus_calls.get() + fake.center_calls.get(), 0);
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn open_with_chats_on_calls_the_focus_prompts_handler(cx: &mut TestAppContext) {
    let (_workspace, _editor, fake, cx) = with_chat(cx).await;
    dispatch("prompts::Open", cx);
    assert_eq!(fake.focus_calls.get(), 1);
    assert_eq!(fake.center_calls.get(), 0);
}

#[gpui::test]
async fn open_with_chats_off_goes_to_the_center_directly(cx: &mut TestAppContext) {
    let (_workspace, _editor, fake, cx) = with_chat(cx).await;
    set_chats_enabled(cx, false);
    dispatch("prompts::Open", cx);
    assert_eq!(
        fake.focus_calls.get(),
        0,
        "the focus handler says 'Chats are off'"
    );
    assert_eq!(fake.center_calls.get(), 1);
    assert!(toasts().is_empty());
}

/// A work root with the project `invoicing` (top task `top`) and the tasks `t1` (backfill) and
/// `t2` (other) under it, and a `one` one-off in the activity `misc`.
struct World {
    _temp: TempDir,
    root: PathBuf,
}

impl World {
    fn dir(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn task_toml(id: &str, goal: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n"
    )
}

fn world() -> World {
    let temp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("work");
    write(
        &root.join("projects/invoicing/project.toml"),
        "schema = 1\ntitle = \"Invoicing\"\n",
    );
    write(
        &root.join("projects/invoicing/task.toml"),
        &task_toml("top", "Invoicing top"),
    );
    write(
        &root.join("projects/invoicing/tasks/backfill/task.toml"),
        &task_toml("t1", "Backfill"),
    );
    write(
        &root.join("projects/invoicing/tasks/other/task.toml"),
        &task_toml("t2", "Other"),
    );
    write(
        &root.join("oneoffs/misc/2020-01-01-one/task.toml"),
        &task_toml("o1", "One thing"),
    );
    World { _temp: temp, root }
}

#[test]
fn the_choices_are_the_project_then_its_tasks() {
    let world = world();
    let tree = bongos_work::model::store::load(&world.root);
    let label = |choices: &[PromptChoice]| -> Vec<String> {
        choices.iter().map(|choice| choice.label.clone()).collect()
    };
    let choices = choices_for(&tree, &world.dir("projects/invoicing/tasks/backfill"));
    assert_eq!(
        label(&choices),
        ["project: Invoicing", "task: Backfill", "task: Other"]
    );
    assert_eq!(choices[0].path, world.dir("projects/invoicing/prompts.txt"));
    assert_eq!(
        choices[2].path,
        world.dir("projects/invoicing/tasks/other/prompts.txt")
    );
    // The project's own chats get the same list.
    assert_eq!(
        choices_for(&tree, &world.dir("projects/invoicing")),
        choices
    );
    // A one-off has its activity's one-offs, and no project.
    let oneoff = choices_for(&tree, &world.dir("oneoffs/misc/2020-01-01-one"));
    assert_eq!(label(&oneoff), ["task: One thing"]);
    // A directory no task owns offers nothing but "Another file...".
    assert!(choices_for(&tree, Path::new("/elsewhere")).is_empty());
}

/// The workspace of `workspace_on` with the real work store over `world` (the task `focus`
/// focused), chats off and the real link.
async fn real_link<'a>(
    world: &World,
    focus: &str,
    cx: &'a mut TestAppContext,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    cx.executor().allow_parking();
    let (workspace, cx) = workspace_on(cx, true).await;
    let root = world.root.clone();
    let db = cx.update(|_, cx| {
        cx.set_global(db::AppDatabase::test_new());
        db::kvp::KeyValueStore::global(cx)
    });
    db.scoped(bongos_work::KVP_NAMESPACE)
        .write(bongos_work::FOCUS_KEY.to_string(), format!("\"{focus}\""))
        .await
        .unwrap();
    let executor = cx.executor();
    cx.update(|_, cx| {
        ChatsSettings::register(cx);
        bongos_work::init(Arc::new(RealFs::new(None, executor)), WorkDirs { root }, cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(true);
                content.chats.get_or_insert_default().enabled = Some(false);
            });
        });
    });
    for _ in 0..200 {
        cx.run_until_parked();
        let loaded = cx.update(|_, cx| {
            WorkStore::global(cx).is_some_and(|store| store.read(cx).snapshot().is_some())
        });
        if loaded {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    (workspace, cx)
}

#[gpui::test]
async fn open_with_chats_off_opens_the_focused_file_in_the_center(cx: &mut TestAppContext) {
    let world = world();
    let (workspace, cx) = real_link(&world, "t2", cx).await;
    let editor = open_in(&workspace, "prompts.txt", cx).await;
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx)
    });
    // Another task is shown, so the file that opens is told from the first.
    let file = world.dir("projects/invoicing/tasks/other/prompts.txt");
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    fs.as_fake()
        .insert_tree(file.parent().unwrap(), serde_json::json!({}))
        .await;
    dispatch("prompts::Open", cx);
    for _ in 0..50 {
        cx.run_until_parked();
        let shown = workspace.update(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .and_then(|editor| file_path(&editor, cx))
        });
        if shown.as_deref() == Some(file.as_path()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    let shown = workspace.update(cx, |workspace, cx| {
        workspace
            .active_item_as::<Editor>(cx)
            .and_then(|editor| file_path(&editor, cx))
    });
    assert_eq!(shown.as_deref(), Some(file.as_path()));
    let ids = workspace.read_with(cx, |workspace, _| workspace.notification_ids());
    assert!(
        !ids.contains(&NotificationId::unique::<ChatToast>()),
        "no 'Chats are off'"
    );
    assert!(toasts().is_empty());
}

#[gpui::test]
async fn the_file_names_its_task_whatever_is_focused(cx: &mut TestAppContext) {
    let world = world();
    let (_workspace, cx) = real_link(&world, "t2", cx).await;
    let task = |dir: &str, cx: &mut VisualTestContext| {
        let dir = world.dir(dir);
        cx.update(|_, cx| Chats.task_for_dir(&dir, cx))
    };
    // `t2` is focused; the file of `t1` starts a chat on `t1`.
    assert_eq!(
        task("projects/invoicing/tasks/backfill", cx).as_deref(),
        Some("t1")
    );
    assert_eq!(task("projects/invoicing", cx).as_deref(), Some("top"));
    assert_eq!(task("elsewhere", cx), None);
    let choices = cx
        .update(|_, cx| Chats.prompt_choices(&world.dir("projects/invoicing/tasks/backfill"), cx));
    assert_eq!(choices.len(), 3);
}

#[gpui::test]
async fn a_human_only_prompts_file_is_not_in_the_context_and_still_sends(cx: &mut TestAppContext) {
    let world = world();
    let task_dir = world.dir("projects/invoicing/tasks/backfill");
    write(&task_dir.join("prompts.txt"), "private prompt\n");
    write(&task_dir.join("notes.md"), "shared\n");
    write(
        &task_dir.join("artifacts.toml"),
        "schema = 1\n\n[[artifact]]\npath = \"prompts.txt\"\ndescription = \"Private\"\n\
         visibility = \"human-only\"\n\n[[artifact]]\npath = \"notes.md\"\n\
         description = \"Shared\"\nvisibility = \"shared\"\n",
    );
    let context = bongos_work::model::session_context(&task_dir).unwrap();
    let paths: Vec<_> = context.artifacts.iter().map(|a| a.path.clone()).collect();
    assert!(paths.contains(&task_dir.join("notes.md")));
    assert!(
        !paths.iter().any(|path| path.ends_with("prompts.txt")),
        "{paths:?}"
    );
    // The send goes through the terminal only: the flag changes nothing about it.
    let (_workspace, editor, fake, cx) = with_chat(cx).await;
    dispatch("prompts::SendWithoutMarking", cx);
    let sends = fake.sends.borrow();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, "a");
    assert_eq!(sends[0].1, "second\n  more");
    drop(sends);
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn the_provider_gives_another_file_the_prompts_language_each_time(cx: &mut TestAppContext) {
    use crate::mode_tests::{attached, open_buffer};
    use bongos_chat::PromptPaneProvider as _;
    for enabled in [true, false] {
        let (project, buffer) = open_buffer(cx, enabled, "notes.txt", THREE).await;
        let name = |cx: &mut VisualTestContext| {
            buffer.read_with(cx, |buffer, _| {
                buffer
                    .language()
                    .map(|language| language.name().to_string())
            })
        };
        let (_, cx) = cx.add_window_view(|_, _| gpui::Empty);
        assert_eq!(name(cx).as_deref(), Some("Plain Text"));
        let item = cx.update(|window, cx| {
            crate::PromptsPaneProvider.build_item(buffer.clone(), project.clone(), window, cx)
        });
        cx.run_until_parked();
        let editor = cx.update(|_, cx| item.act_as::<Editor>(cx)).unwrap();
        if enabled {
            assert_eq!(name(cx).as_deref(), Some("Prompts"));
            assert!(attached(&editor, cx));
        } else {
            assert_eq!(
                name(cx).as_deref(),
                Some("Plain Text"),
                "nothing changes while off"
            );
            assert!(!attached(&editor, cx));
        }
    }
}
