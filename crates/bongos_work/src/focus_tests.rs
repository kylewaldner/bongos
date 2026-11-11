//! `task: focus` and its neighbors in a real `MultiWorkspace` window. The promise (D21): focusing
//! a task and focusing back never saves, discards or closes a buffer of the workspace left
//! behind. The window's projects live on a fake file system, so a save would show there; the
//! work tree is a temporary directory read by the real store.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_work_model::commands::default_todos;
use bongos_work_model::layout;
use bongos_work_model::{Extra, Kind, ResultKind, TaskFile, TaskResult, serialize_task};
use db::AppDatabase;
use db::kvp::KeyValueStore;
use editor::Editor;
use fs::{FakeFs, Fs as _, RealFs};
use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, WeakEntity};
use language::Buffer;
use project::{DisableAiSettings, Project};
use serde_json::json;
use settings::Settings as _;
use tempfile::TempDir;
use workspace::{AppState, MultiWorkspace, OpenOptions, Workspace};

use crate::command_tests::{dispatch, wait_until};
use crate::commands::{WorkToast, init_palette, register};
use crate::focus::{FOCUS_KEY, Focus, KVP_NAMESPACE};
use crate::modals::ChoicePicker;
use crate::status::FocusStatus;
use crate::store_tests::{canonical, project_toml, set_enabled, write};
use crate::{GlobalWorkStore, WorkDirs, WorkSettings, WorkStore, focus};

const MAIN: &str = "/code/main.rs";
const SAVED: &str = "fn main() {}\n";
const PR: &str = "https://github.com/acme/invoicing/pull/7";

fn task_toml(id: &str, goal: &str, leaf: bool, edit: impl FnOnce(&mut TaskFile)) -> String {
    let mut file = TaskFile::new(goal, Kind::PrShip);
    file.id = id.to_string();
    if leaf {
        file.todos = default_todos(Kind::PrShip);
    }
    edit(&mut file);
    serialize_task(&file).unwrap()
}

/// A project `invoicing` with the leaf `backfill` (`l1`, with a worktree), and the group `phase`
/// (`g2`) holding `alpha` (`l2`, no worktree) and `beta` (`l3`, whose result is a PR).
fn seed(root: &Path, worktree: &Path) {
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(
        &layout::task_file(&project),
        &task_toml("g1", "Invoicing", false, |_| {}),
    );
    let backfill = layout::child_dir(&project, "backfill");
    write(
        &layout::task_file(&backfill),
        &task_toml("l1", "Backfill", true, |file| {
            file.branch = Some("backfill".to_string());
            file.worktree = Some(worktree.to_string_lossy().into_owned());
        }),
    );
    let phase = layout::child_dir(&project, "phase");
    write(
        &layout::task_file(&phase),
        &task_toml("g2", "Phase", false, |_| {}),
    );
    let alpha = layout::child_dir(&phase, "alpha");
    write(
        &layout::task_file(&alpha),
        &task_toml("l2", "Alpha", true, |_| {}),
    );
    let beta = layout::child_dir(&phase, "beta");
    write(
        &layout::task_file(&beta),
        &task_toml("l3", "Beta", true, |file| {
            file.result = Some(TaskResult {
                kind: ResultKind::Pr,
                url: Some(PR.to_string()),
                state: None,
                urls: Vec::new(),
                extra: Extra::new(),
            });
        }),
    );
}

struct Window {
    multi: Entity<MultiWorkspace>,
    /// The workspace the window starts with.
    home: Entity<Workspace>,
    fs: Arc<FakeFs>,
    root: PathBuf,
    worktree: PathBuf,
    _temp: TempDir,
}

/// A window whose first workspace holds `/code` (or nothing, when `folders` is false), over a
/// store of the seeded tree. Every workspace the window opens gets the commands.
async fn window(
    cx: &mut TestAppContext,
    enabled: bool,
    folders: bool,
) -> (Window, &mut VisualTestContext) {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    let worktree = canonical(&temp).join("repos/invoicing/.bongos-worktrees/backfill");
    std::fs::create_dir_all(&worktree).unwrap();
    seed(&root, &worktree);
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        command_palette_hooks::init(cx);
        WorkSettings::register(cx);
        cx.set_global(AppDatabase::test_new());
        app_state
    });
    set_enabled(cx, enabled);
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let store = cx.new(|cx| WorkStore::new(fs, WorkDirs::for_test(root.clone()), cx));
    cx.update(|cx| {
        cx.set_global(GlobalWorkStore(store.clone()));
        init_palette(cx);
        focus::init(cx);
        cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
            .detach();
    });

    // The window's file system mirrors the directories focus opens.
    let fs = app_state.fs.as_fake();
    fs.insert_tree("/code", json!({ "main.rs": SAVED })).await;
    fs.insert_tree(&worktree, json!({ "lib.rs": "" })).await;
    for path in [
        vec![],
        vec!["backfill"],
        vec!["phase"],
        vec!["phase", "alpha"],
        vec!["phase", "beta"],
    ] {
        fs.create_dir(&task_dir(&root, &path)).await.unwrap();
    }

    let roots: Vec<&Path> = if folders {
        vec![Path::new("/code")]
    } else {
        Vec::new()
    };
    let project = Project::test(app_state.fs.clone(), roots, cx).await;
    let (multi, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let home = multi.read_with(cx, |multi, _| multi.workspace().clone());
    if enabled {
        wait_until(cx, "the first load", |cx| {
            store.read_with(cx, |store, _| store.snapshot().is_some())
        })
        .await;
        cx.executor().timer(Duration::from_millis(500)).await;
    }
    (
        Window {
            multi,
            home,
            fs,
            root,
            worktree,
            _temp: temp,
        },
        cx,
    )
}

fn task_dir(root: &Path, path: &[&str]) -> PathBuf {
    let mut dir = layout::project_dir(root, "invoicing");
    for slug in path {
        dir = layout::child_dir(&dir, slug);
    }
    dir
}

impl Window {
    fn active(&self, cx: &mut VisualTestContext) -> Entity<Workspace> {
        self.multi
            .read_with(cx, |multi, _| multi.workspace().clone())
    }

    fn workspaces(&self, cx: &mut VisualTestContext) -> Vec<Entity<Workspace>> {
        self.multi
            .read_with(cx, |multi, _| multi.workspaces().cloned().collect())
    }

    fn roots(&self, cx: &mut VisualTestContext) -> Vec<PathBuf> {
        let active = self.active(cx);
        active.read_with(cx, |workspace, cx| {
            workspace
                .root_paths(cx)
                .iter()
                .map(|path| path.to_path_buf())
                .collect()
        })
    }

    fn focused(&self, cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|_, cx| {
            Focus::global(cx).and_then(|focus| focus.read(cx).task_id().map(str::to_string))
        })
    }

    fn status(&self, cx: &mut VisualTestContext) -> Option<String> {
        let status = cx.update(|_, cx| cx.new(FocusStatus::new));
        status.read_with(cx, |status, cx| status.text(cx))
    }

    fn picker(&self, cx: &mut VisualTestContext) -> Option<Entity<ChoicePicker>> {
        let active = self.active(cx);
        active.read_with(cx, |workspace, cx| {
            workspace.active_modal::<ChoicePicker>(cx)
        })
    }

    fn has_toast(&self, workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, _| {
            workspace
                .notification_ids()
                .contains(&workspace::notifications::NotificationId::unique::<WorkToast>())
        })
    }

    /// Dispatches `name` in the active workspace and picks `query` in each picker it opens.
    async fn run(&self, cx: &mut VisualTestContext, name: &str, queries: &[&str]) {
        dispatch(cx, name);
        for query in queries {
            wait_until(cx, "a picker", |cx| self.picker(cx).is_some()).await;
            let picker = self.picker(cx).unwrap();
            picker.update_in(cx, |modal, window, cx| {
                modal
                    .picker
                    .update(cx, |picker, cx| picker.set_query(query, window, cx))
            });
            cx.run_until_parked();
            cx.dispatch_action(menu::Confirm);
            cx.run_until_parked();
        }
    }

    /// Focuses the task picked by `query` and waits until the window shows `root`.
    async fn focus(&self, cx: &mut VisualTestContext, name: &str, query: &str, root: &Path) {
        self.run(cx, name, &[query]).await;
        wait_until(cx, &format!("{} to be active", root.display()), |cx| {
            self.roots(cx) == [root.to_path_buf()]
        })
        .await;
    }

    async fn saved_text(&self) -> String {
        self.fs.load(Path::new(MAIN)).await.unwrap()
    }

    fn persisted(&self, cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|_, cx| {
            KeyValueStore::global(cx)
                .scoped(KVP_NAMESPACE)
                .read(FOCUS_KEY)
                .unwrap()
        })
    }
}

/// Opens `/code/main.rs` in the home workspace and types into it without saving. Returns weak
/// handles only, so whatever keeps the buffer alive afterwards is the workspace, not the test.
async fn dirty_main(
    window: &Window,
    cx: &mut VisualTestContext,
) -> (WeakEntity<Editor>, WeakEntity<Buffer>) {
    let item = window
        .home
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(PathBuf::from(MAIN), OpenOptions::default(), window, cx)
        })
        .await
        .unwrap();
    let editor = item
        .downcast::<Editor>()
        .expect("main.rs opens in an editor");
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("// not saved\n", window, cx)
    });
    let buffer = editor.read_with(cx, |editor, cx| {
        editor.buffer().read(cx).as_singleton().unwrap()
    });
    cx.run_until_parked();
    assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    (editor.downgrade(), buffer.downgrade())
}

/// The buffer is alive, still dirty, still holds the edit, the home workspace's active item is
/// still its editor, and the file on disk is what it was.
async fn assert_unsaved_and_alive(
    window: &Window,
    editor: &WeakEntity<Editor>,
    buffer: &WeakEntity<Buffer>,
    cx: &mut VisualTestContext,
) {
    let buffer = buffer.upgrade().expect("the buffer is alive");
    let editor = editor.upgrade().expect("the editor is alive");
    buffer.read_with(cx, |buffer, _| {
        assert!(buffer.is_dirty(), "the buffer is still dirty");
        assert_eq!(buffer.text(), format!("// not saved\n{SAVED}"));
    });
    let active_item = window
        .home
        .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx));
    assert_eq!(active_item, Some(editor), "the editor is still open");
    assert_eq!(window.saved_text().await, SAVED, "nothing was saved");
    assert!(!cx.has_pending_prompt(), "nothing asked to save or discard");
    assert!(
        window.workspaces(cx).contains(&window.home),
        "the home workspace is still in the window"
    );
}

#[gpui::test]
async fn a_dirty_buffer_stays_unsaved_after_focusing_a_task_and_focusing_back(
    cx: &mut TestAppContext,
) {
    let (window, cx) = window(cx, true, true).await;
    let (editor, buffer) = dirty_main(&window, cx).await;

    window
        .focus(cx, "task::Focus", "backfill", &window.worktree.clone())
        .await;
    assert_ne!(window.active(cx), window.home);
    assert_eq!(window.focused(cx).as_deref(), Some("l1"));
    assert_unsaved_and_alive(&window, &editor, &buffer, cx).await;

    // Focusing another task, then back to the first, reuses the workspace focus made for it.
    let alpha = task_dir(&window.root, &["phase", "alpha"]);
    window.focus(cx, "task::Focus", "alpha", &alpha).await;
    window
        .focus(cx, "task::Focus", "backfill", &window.worktree.clone())
        .await;
    assert_eq!(window.workspaces(cx).len(), 3);
    assert_unsaved_and_alive(&window, &editor, &buffer, cx).await;

    dispatch(cx, "task::Unfocus");
    wait_until(cx, "the home workspace", |cx| {
        window.active(cx) == window.home
    })
    .await;
    assert_eq!(window.focused(cx), None);
    assert_unsaved_and_alive(&window, &editor, &buffer, cx).await;
    // The editor still edits the same buffer, which can still be saved where it was.
    let editor = editor.upgrade().unwrap();
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("// more\n", window, cx)
    });
    assert!(
        buffer
            .upgrade()
            .unwrap()
            .read_with(cx, |buffer, _| buffer.is_dirty())
    );
}

#[gpui::test]
async fn without_the_sidebar_or_folders_the_workspace_left_keeps_its_untitled_buffer(
    cx: &mut TestAppContext,
) {
    let (window, cx) = window(cx, true, false).await;
    cx.update(|_, cx| {
        DisableAiSettings::override_global(DisableAiSettings { disable_ai: true }, cx);
    });
    cx.run_until_parked();
    assert!(
        !window
            .multi
            .read_with(cx, |multi, cx| multi.multi_workspace_enabled(cx))
    );

    let project = window
        .home
        .read_with(cx, |workspace, _| workspace.project().clone());
    let buffer = project.update(cx, |project, cx| {
        project.create_local_buffer("draft", None, false, cx)
    });
    let editor = window.home.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor
    });
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("unsaved ", window, cx)
    });
    let (editor, buffer) = (editor.downgrade(), buffer.downgrade());

    window
        .focus(cx, "task::Focus", "backfill", &window.worktree.clone())
        .await;
    assert!(!cx.has_pending_prompt(), "nothing asked to save or discard");
    assert!(window.workspaces(cx).contains(&window.home));

    dispatch(cx, "task::Unfocus");
    wait_until(cx, "the home workspace", |cx| {
        window.active(cx) == window.home
    })
    .await;
    let buffer = buffer.upgrade().expect("the untitled buffer is alive");
    buffer.read_with(cx, |buffer, _| {
        assert!(buffer.is_dirty());
        assert_eq!(buffer.text(), "unsaved draft");
    });
    let active_item = window
        .home
        .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx));
    assert_eq!(active_item, editor.upgrade());
    assert!(!cx.has_pending_prompt());
}

#[gpui::test]
async fn a_group_a_leaf_without_a_worktree_and_a_project_focus_their_directories(
    cx: &mut TestAppContext,
) {
    let (window, cx) = window(cx, true, true).await;
    let phase = task_dir(&window.root, &["phase"]);
    window.focus(cx, "task::Focus", "phase", &phase).await;
    assert_eq!(window.focused(cx).as_deref(), Some("g2"));
    assert_eq!(window.status(cx).as_deref(), Some("invoicing > phase"));

    let alpha = task_dir(&window.root, &["phase", "alpha"]);
    window.focus(cx, "task::Focus", "alpha", &alpha).await;
    assert_eq!(
        window.status(cx).as_deref(),
        Some("invoicing > phase > alpha [implement] pr-ship")
    );

    let project = task_dir(&window.root, &[]);
    window
        .focus(cx, "project::Focus", "invoicing", &project)
        .await;
    assert_eq!(window.focused(cx).as_deref(), Some("g1"));
    assert_eq!(window.status(cx).as_deref(), Some("invoicing"));
    assert!(window.workspaces(cx).contains(&window.home));
}

#[gpui::test]
async fn the_status_names_the_path_current_todo_and_kind(cx: &mut TestAppContext) {
    let (window, cx) = window(cx, true, true).await;
    assert_eq!(window.status(cx), None, "nothing is focused");
    window
        .focus(cx, "task::Focus", "backfill", &window.worktree.clone())
        .await;
    assert_eq!(
        window.status(cx).as_deref(),
        Some("invoicing > backfill [implement] pr-ship")
    );
    set_enabled(cx, false);
    cx.run_until_parked();
    assert_eq!(window.status(cx), None, "nothing while work.enabled is off");
}

#[gpui::test]
async fn focus_is_written_to_and_restored_from_the_key_value_store(cx: &mut TestAppContext) {
    let (window, cx) = window(cx, true, true).await;
    window
        .focus(cx, "task::Focus", "backfill", &window.worktree.clone())
        .await;
    wait_until(cx, "the focus to be written", |cx| {
        window.persisted(cx).as_deref() == Some("\"l1\"")
    })
    .await;

    // A new start reads it back.
    cx.update(|_, cx| focus::init(cx));
    assert_eq!(window.focused(cx).as_deref(), Some("l1"));
    assert_eq!(
        window.status(cx).as_deref(),
        Some("invoicing > backfill [implement] pr-ship")
    );

    // An id that names no task any more is dropped.
    let db = cx.update(|_, cx| KeyValueStore::global(cx));
    db.scoped(KVP_NAMESPACE)
        .write(FOCUS_KEY.to_string(), "\"gone\"".to_string())
        .await
        .unwrap();
    cx.update(|_, cx| focus::init(cx));
    assert_eq!(window.focused(cx), None);
    assert_eq!(window.status(cx), None);

    // Unfocus clears what is stored.
    window
        .focus(cx, "task::Focus", "backfill", &window.worktree.clone())
        .await;
    dispatch(cx, "task::Unfocus");
    wait_until(cx, "the focus to be cleared", |cx| {
        window.persisted(cx).is_none()
    })
    .await;
}

#[gpui::test]
async fn open_and_copy_commands_by_name(cx: &mut TestAppContext) {
    let (window, cx) = window(cx, true, true).await;
    let home = window.home.clone();

    // Nothing is focused, so unfocus says so.
    dispatch(cx, "task::Unfocus");
    assert!(window.has_toast(&home, cx));
    home.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));

    window.run(cx, "task::CopyPath", &["backfill"]).await;
    let copied = cx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied, Some(window.worktree.to_string_lossy().into_owned()));
    window.run(cx, "task::CopyPath", &["alpha"]).await;
    let copied = cx.read_from_clipboard().and_then(|item| item.text());
    let alpha = task_dir(&window.root, &["phase", "alpha"]);
    assert_eq!(copied, Some(alpha.to_string_lossy().into_owned()));

    window.run(cx, "task::OpenPr", &["beta"]).await;
    assert_eq!(cx.opened_url().as_deref(), Some(PR));

    window.run(cx, "task::OpenWorktree", &["backfill"]).await;
    wait_until(cx, "the worktree folder", |cx| {
        home.read_with(cx, |workspace, cx| {
            workspace
                .root_paths(cx)
                .iter()
                .any(|path| **path == *window.worktree)
        })
    })
    .await;
    assert_eq!(window.active(cx), home, "open worktree switches nothing");

    // Every new action opens its picker while the store is on.
    for name in [
        "task::Focus",
        "project::Focus",
        "task::OpenWorktree",
        "task::OpenPr",
    ] {
        dispatch(cx, name);
        assert!(window.picker(cx).is_some(), "{name} opened no picker");
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
    }
}
