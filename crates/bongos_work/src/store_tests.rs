//! `WorkStore` against the real file system. `FakeFs::watch` only reports changes made through
//! `FakeFs`, while the model reads and writes with `std::fs` and these tests make external edits
//! the way an agent would, so they use `RealFs` in a `TempDir`, with parking allowed so real
//! watcher events and real time can pass.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::activity::activity_time;
use bongos_work_model::commands::{self, Context, NewTodo};
use bongos_work_model::layout;
use bongos_work_model::read_task;
use bongos_work_model::store::{Task as WorkTask, WorkTree};
use fs::RealFs;
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::{Stats, WorkDirs, WorkSettings, WorkStore, WriteError};

/// Long enough for the watcher latency, a reload and slack on a loaded machine.
const SETTLE: Duration = Duration::from_millis(1000);

pub(crate) fn task_toml(id: &str, goal: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"pr-ship\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n"
    )
}

pub(crate) fn project_toml(title: &str) -> String {
    format!("schema = 1\ntitle = \"{title}\"\nrepositories = [\"invoicing\"]\n")
}

/// The temporary directory with symbolic links resolved: snapshot paths are under the canonical
/// root, and the temporary directory may sit behind a link.
pub(crate) fn canonical(temp: &TempDir) -> PathBuf {
    std::fs::canonicalize(temp.path()).unwrap()
}

pub(crate) fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A project `invoicing` with one leaf task `backfill` (id `l1`); returns the leaf's directory.
fn seed(root: &Path) -> PathBuf {
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(&layout::task_file(&project), &task_toml("g1", "Invoicing"));
    let backfill = layout::child_dir(&project, "backfill");
    write(&layout::task_file(&backfill), &task_toml("l1", "Backfill"));
    backfill
}

pub(crate) fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

fn new_store(cx: &mut TestAppContext, root: &Path, enabled: bool) -> Entity<WorkStore> {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        WorkSettings::register(cx);
    });
    set_enabled(cx, enabled);
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let dirs = WorkDirs::for_test(root.to_path_buf());
    cx.new(|cx| WorkStore::new(fs, dirs, cx))
}

async fn settle(cx: &mut TestAppContext) {
    cx.executor().timer(SETTLE).await;
}

/// Waits up to ten seconds for `check` to hold on the store.
async fn wait_until(
    cx: &mut TestAppContext,
    store: &Entity<WorkStore>,
    what: &str,
    check: impl Fn(&WorkStore) -> bool,
) {
    for _ in 0..200 {
        if store.read_with(cx, |store, _| check(store)) {
            return;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn wait_loaded(cx: &mut TestAppContext, store: &Entity<WorkStore>) {
    wait_until(cx, store, "the first load", |store| {
        store.snapshot().is_some()
    })
    .await;
    // Let the watches settle before editing anything.
    settle(cx).await;
}

fn stats(cx: &mut TestAppContext, store: &Entity<WorkStore>) -> Stats {
    store.read_with(cx, |store, _| store.stats())
}

fn task_at<'a>(tree: &'a WorkTree, dir: &Path) -> &'a WorkTask {
    tree.task_by_dir(dir)
        .unwrap_or_else(|| panic!("no task at {}", dir.display()))
}

fn snapshot_task(store: &WorkStore, dir: &Path) -> Option<WorkTask> {
    store
        .snapshot()
        .and_then(|tree| tree.task_by_dir(dir).cloned())
}

/// The artifact list of D22 without the manifest: every file the task owns outside the reserved
/// names.
fn artifacts(task: &WorkTask) -> Vec<PathBuf> {
    task.files
        .keys()
        .filter(|path| {
            path.components()
                .next()
                .and_then(|first| first.as_os_str().to_str())
                .is_some_and(|first| !layout::is_reserved(first))
        })
        .cloned()
        .collect()
}

fn add_todo(name: &str) -> impl Fn(&WorkTree, &Context) -> commands::Outcome + Send + 'static {
    let name = name.to_string();
    move |tree, ctx| {
        commands::todo_add(
            tree,
            ctx,
            "l1",
            NewTodo {
                name: name.clone(),
                role: None,
                workflow: None,
            },
            None,
        )
    }
}

#[gpui::test]
async fn nothing_loads_or_watches_while_disabled(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    let backfill = seed(&root);
    let missing_root = canonical(&temp).join("never");

    let store = new_store(cx, &root, false);
    let other = {
        let fs = Arc::new(RealFs::new(None, cx.executor()));
        let dirs = WorkDirs::for_test(missing_root.clone());
        cx.new(|cx| WorkStore::new(fs, dirs, cx))
    };
    settle(cx).await;
    write(&layout::task_file(&backfill), &task_toml("l1", "Edited"));
    settle(cx).await;
    for store in [&store, &other] {
        assert!(store.read_with(cx, |store, _| store.snapshot().is_none()));
        assert_eq!(stats(cx, store), Stats::default());
    }
    assert!(!missing_root.exists(), "a disabled store created its root");
    let refused = store
        .update(cx, |store, cx| {
            store.write(
                Context::at_keyboard(PersonAtKeyboard::for_test()),
                add_todo("x"),
                cx,
            )
        })
        .await;
    assert_eq!(refused, Err(WriteError::Off));

    set_enabled(cx, true);
    wait_loaded(cx, &store).await;
    store.read_with(cx, |store, _| {
        let tree = store.snapshot().unwrap();
        assert_eq!(task_at(&tree, &backfill).goal(), "Edited");
        assert_eq!(store.stats().loads, 1);
    });

    set_enabled(cx, false);
    assert!(store.read_with(cx, |store, _| store.snapshot().is_none()));
    let before = stats(cx, &store);
    write(
        &layout::task_file(&backfill),
        &task_toml("l1", "Edited again"),
    );
    settle(cx).await;
    assert!(store.read_with(cx, |store, _| store.snapshot().is_none()));
    assert_eq!(stats(cx, &store), before);

    // Enabling again starts over from the disk.
    set_enabled(cx, true);
    wait_until(cx, &store, "the second load", |store| {
        store
            .snapshot()
            .is_some_and(|tree| task_at(&tree, &backfill).goal() == "Edited again")
    })
    .await;
    assert_eq!(stats(cx, &store).loads, 2);
}

#[gpui::test]
async fn external_edits_to_model_files_reach_the_snapshot(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let project = layout::project_dir(&root, "invoicing");
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;

    write(&layout::project_file(&project), &project_toml("Billing"));
    wait_until(cx, &store, "project.toml", |store| {
        store.snapshot().is_some_and(|tree| {
            tree.projects[0]
                .file
                .as_ref()
                .is_some_and(|file| file.title == "Billing")
        })
    })
    .await;

    write(
        &layout::task_file(&backfill),
        &task_toml("l1", "Backfill v2"),
    );
    wait_until(cx, &store, "task.toml", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| task.goal() == "Backfill v2")
    })
    .await;

    write(
        &layout::artifacts_file(&backfill),
        "schema = 1\n\n[[artifact]]\npath = \"notes/plan.md\"\ndescription = \"The plan\"\n",
    );
    wait_until(cx, &store, "artifacts.toml", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| {
            task.artifacts.as_ref().is_some_and(|file| {
                file.artifacts.len() == 1 && file.artifacts[0].description == "The plan"
            })
        })
    })
    .await;

    // A new task made by an agent, in a directory that did not exist, appears too.
    let new_child = layout::child_dir(&project, "reports");
    write(&layout::task_file(&new_child), &task_toml("l2", "Reports"));
    wait_until(cx, &store, "a new task", |store| {
        snapshot_task(store, &new_child).is_some_and(|task| task.goal() == "Reports")
    })
    .await;
}

#[gpui::test]
async fn edit_between_snapshot_and_write_is_kept_and_the_command_reapplied(
    cx: &mut TestAppContext,
) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let command = {
        let calls = calls.clone();
        let task_path = layout::task_file(&backfill);
        let add = add_todo("Write the migration");
        move |tree: &WorkTree, ctx: &Context| {
            // The first time, an agent edits the file after the snapshot was taken and before
            // the writer writes.
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::write(&task_path, task_toml("l1", "Edited by an agent")).unwrap();
            }
            add(tree, ctx)
        }
    };
    let change = store
        .update(cx, |store, cx| {
            store.write(
                Context::at_keyboard(PersonAtKeyboard::for_test()),
                command,
                cx,
            )
        })
        .await
        .expect("the write succeeds on the fresh tree");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the command ran again");
    assert_eq!(change.file().unwrap().goal, "Edited by an agent");

    let on_disk = read_task(&layout::task_file(&backfill)).unwrap();
    assert_eq!(
        on_disk.goal, "Edited by an agent",
        "the external edit is kept"
    );
    assert_eq!(on_disk.todos.len(), 1);
    assert_eq!(on_disk.todos[0].name, "Write the migration");
    store.read_with(cx, |store, _| {
        let task = snapshot_task(store, &backfill).unwrap();
        assert_eq!(task.file.as_ref(), Some(&on_disk));
        assert_eq!(store.stats().retries, 1);
        assert_eq!(store.stats().writes, 1);
    });
}

#[gpui::test]
async fn a_file_that_keeps_changing_is_never_overwritten(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let command = {
        let calls = calls.clone();
        let task_path = layout::task_file(&backfill);
        let add = add_todo("Never written");
        move |tree: &WorkTree, ctx: &Context| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(&task_path, task_toml("l1", &format!("Edit {call}"))).unwrap();
            add(tree, ctx)
        }
    };
    let result = store
        .update(cx, |store, cx| {
            store.write(
                Context::at_keyboard(PersonAtKeyboard::for_test()),
                command,
                cx,
            )
        })
        .await;
    assert_eq!(result, Err(WriteError::Contended));
    assert_eq!(calls.load(Ordering::SeqCst), crate::MAX_RETRIES + 1);
    let on_disk = read_task(&layout::task_file(&backfill)).unwrap();
    assert_eq!(on_disk.goal, format!("Edit {}", crate::MAX_RETRIES));
    assert!(on_disk.todos.is_empty());
}

#[gpui::test]
async fn own_write_does_not_reload(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;
    let before = stats(cx, &store);

    store
        .update(cx, |store, cx| {
            store.write(
                Context::at_keyboard(PersonAtKeyboard::for_test()),
                add_todo("Write the migration"),
                cx,
            )
        })
        .await
        .unwrap();
    store.read_with(cx, |store, _| {
        let task = snapshot_task(store, &backfill).unwrap();
        assert_eq!(
            task.file.unwrap().todos.len(),
            1,
            "the snapshot has the write"
        );
    });
    settle(cx).await;
    let after = stats(cx, &store);
    assert_eq!(
        after.event_reloads, before.event_reloads,
        "the write echoed"
    );
    assert!(
        after.ignored_batches > before.ignored_batches,
        "the write's events arrived and were dropped"
    );
    assert_eq!(after.writes, 1);

    // The watcher still works: an external edit after the write reloads.
    write(&layout::task_file(&backfill), &task_toml("l1", "Edited"));
    wait_until(cx, &store, "the external edit", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| task.goal() == "Edited")
    })
    .await;
    assert!(stats(cx, &store).event_reloads > before.event_reloads);
}

#[gpui::test]
async fn temp_file_events_are_ignored(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;
    let before = stats(cx, &store);

    let temp_file = backfill.join(".task.toml.tmp-123-abcdefgh");
    write(&temp_file, "partial");
    std::fs::remove_file(&temp_file).unwrap();
    write(&backfill.join(layout::OP_FILE), "partial journal");
    settle(cx).await;
    let after = stats(cx, &store);
    assert_eq!(after.event_reloads, before.event_reloads);
    assert!(after.ignored_batches > before.ignored_batches);
}

#[gpui::test]
async fn a_users_own_git_directory_never_reloads(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    std::fs::create_dir_all(backfill.join(".git")).unwrap();
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;
    let before = stats(cx, &store);

    write(&backfill.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&backfill.join(".git/refs/heads/main"), "abc\n");
    write(&backfill.join(".git/refs/heads/main"), "def\n");
    settle(cx).await;
    let after = stats(cx, &store);
    assert_eq!(after.event_reloads, before.event_reloads);
    let task = store
        .read_with(cx, |store, _| snapshot_task(store, &backfill))
        .unwrap();
    assert!(task.files.keys().all(|path| !path.starts_with(".git")));
}

#[gpui::test]
async fn creating_a_git_directory_in_a_task_never_reloads(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;
    let before = stats(cx, &store);

    write(&backfill.join(".git/HEAD"), "ref: refs/heads/main\n");
    settle(cx).await;
    let after = stats(cx, &store);
    assert_eq!(after.event_reloads, before.event_reloads);
    assert!(after.ignored_batches > before.ignored_batches);

    std::fs::remove_dir_all(backfill.join(".git")).unwrap();
    settle(cx).await;
    assert_eq!(stats(cx, &store).event_reloads, before.event_reloads);
}

#[gpui::test]
async fn chats_and_runs_reach_activity_time_only(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;
    let before = store.read_with(cx, |store, _| snapshot_task(store, &backfill).unwrap());

    write(&layout::chats_dir(&backfill).join("one.md"), "chat");
    write(&layout::runs_dir(&backfill).join("run.log"), "log");
    wait_until(cx, &store, "chats and runs", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| {
            task.files.contains_key(Path::new("chats/one.md"))
                && task.files.contains_key(Path::new("runs/run.log"))
        })
    })
    .await;
    let after = store.read_with(cx, |store, _| snapshot_task(store, &backfill).unwrap());
    assert_eq!(after.file, before.file);
    assert_eq!(after.artifacts, before.artifacts);
    assert_eq!(artifacts(&after), artifacts(&before));
    assert!(activity_time(&after) > activity_time(&before));

    // A later edit inside the now-watched `chats/` moves activity time again.
    let first = activity_time(&after);
    std::thread::sleep(Duration::from_millis(20));
    write(&layout::chats_dir(&backfill).join("one.md"), "chat, longer");
    wait_until(cx, &store, "a chat edit", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| activity_time(&task) > first)
    })
    .await;
}

#[gpui::test]
async fn notes_and_docs_reach_artifacts_and_activity_time(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;
    let before = store.read_with(cx, |store, _| snapshot_task(store, &backfill).unwrap());
    assert!(artifacts(&before).is_empty());

    write(&backfill.join("notes/plan.md"), "plan");
    write(&backfill.join("docs/deep/design.md"), "design");
    wait_until(cx, &store, "notes and docs", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| artifacts(&task).len() == 2)
    })
    .await;
    let after = store.read_with(cx, |store, _| snapshot_task(store, &backfill).unwrap());
    assert_eq!(
        artifacts(&after),
        vec![
            PathBuf::from("docs/deep/design.md"),
            PathBuf::from("notes/plan.md")
        ]
    );
    assert_eq!(after.file, before.file);
    assert!(activity_time(&after) > activity_time(&before));
}

#[gpui::test]
async fn external_edit_right_after_own_write_reaches_the_snapshot(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;

    store
        .update(cx, |store, cx| {
            store.write(
                Context::at_keyboard(PersonAtKeyboard::for_test()),
                add_todo("Write the migration"),
                cx,
            )
        })
        .await
        .unwrap();
    // Inside the echo window of the write, before its events arrive.
    write(
        &layout::task_file(&backfill),
        &task_toml("l1", "Edited right after"),
    );
    wait_until(cx, &store, "the edit after the write", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| task.goal() == "Edited right after")
    })
    .await;
}

#[gpui::test]
async fn external_edit_and_back_within_the_echo_window_reach_the_snapshot(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let task_path = layout::task_file(&backfill);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;

    store
        .update(cx, |store, cx| {
            store.write(
                Context::at_keyboard(PersonAtKeyboard::for_test()),
                add_todo("Write the migration"),
                cx,
            )
        })
        .await
        .unwrap();
    settle(cx).await;
    let ours = std::fs::read_to_string(&task_path).unwrap();

    write(&task_path, &task_toml("l1", "Changed"));
    wait_until(cx, &store, "the edit", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| task.goal() == "Changed")
    })
    .await;
    // Back to exactly what bongos wrote, still within the echo window.
    write(&task_path, &ours);
    wait_until(cx, &store, "the edit back", |store| {
        snapshot_task(store, &backfill).is_some_and(|task| {
            task.goal() == "Backfill" && task.file.as_ref().unwrap().todos.len() == 1
        })
    })
    .await;
}

#[gpui::test]
async fn disabling_before_a_queued_write_runs_writes_nothing(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp);
    let backfill = seed(&root);
    let task_path = layout::task_file(&backfill);
    let store = new_store(cx, &root, true);
    wait_loaded(cx, &store).await;
    let before = std::fs::read_to_string(&task_path).unwrap();

    let write_task = store.update(cx, |store, cx| {
        store.write(
            Context::at_keyboard(PersonAtKeyboard::for_test()),
            add_todo("Never written"),
            cx,
        )
    });
    set_enabled(cx, false);
    assert_eq!(write_task.await, Err(WriteError::Off));
    settle(cx).await;
    assert_eq!(std::fs::read_to_string(&task_path).unwrap(), before);
}

#[gpui::test]
async fn a_write_that_ran_reports_its_result_even_if_the_session_ended() {
    let (sender, receiver) = futures::channel::oneshot::channel();
    // The session ended: the finished job's reply is dropped instead of sent by the store.
    drop(crate::store::reply_for_test(
        sender,
        Err(WriteError::Contended),
    ));
    assert_eq!(receiver.await, Ok(Err(WriteError::Contended)));
}

#[cfg(unix)]
#[gpui::test]
async fn a_root_reached_through_a_symbolic_link_is_watched(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let real = temp.path().join("real");
    let backfill = seed(&real);
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let store = new_store(cx, &link, true);
    wait_loaded(cx, &store).await;

    let canonical = std::fs::canonicalize(&backfill).unwrap();
    write(
        &layout::task_file(&backfill),
        &task_toml("l1", "Through the link"),
    );
    wait_until(cx, &store, "the edit under a linked root", |store| {
        snapshot_task(store, &canonical).is_some_and(|task| task.goal() == "Through the link")
    })
    .await;
}

#[test]
fn work_dirs_new_puts_worktrees_under_the_data_dir() {
    let dirs = WorkDirs::new(Path::new("/home/u"), Path::new("/data"));
    assert_eq!(dirs.root, Path::new("/home/u/.bongos"));
    assert_eq!(dirs.worktrees, Path::new("/data/worktrees"));
}

#[test]
fn work_dirs_for_test_puts_worktrees_outside_the_root() {
    let dirs = WorkDirs::for_test("/tmp/t/work");
    assert_eq!(dirs.root, Path::new("/tmp/t/work"));
    assert_eq!(dirs.worktrees, Path::new("/tmp/t/central-worktrees"));
    assert!(!dirs.worktrees.starts_with(&dirs.root));
}
