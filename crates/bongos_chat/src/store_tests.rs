//! `ChatStore` against the real file system, through a real `WorkStore`: the index follows the
//! work store's watcher, so these tests use `RealFs` in a `TempDir` with parking allowed, as the
//! work store's own tests do.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_chat_model::{Agent, ChatRecord, read_record, record_path, write_record};
use bongos_work::{WorkDirs, WorkSettings, WorkStore};
use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::{self, Context};
use bongos_work_model::layout;
use command_palette_hooks::CommandPaletteFilter;
use fs::RealFs;
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::{ChatDirs, ChatStore, ChatWriteError, ChatsSettings, chat, init_palette};

/// Long enough for the watcher latency, a reload and slack on a loaded machine.
const SETTLE: Duration = Duration::from_millis(1000);

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn task_toml(id: &str, goal: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"pr-ship\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n"
    )
}

/// A project `invoicing` with two leaf tasks, `backfill` (`l1`) and `audit` (`l2`); returns their
/// directories.
fn seed_tree(root: &Path) -> (PathBuf, PathBuf) {
    let project = layout::project_dir(root, "invoicing");
    write(
        &layout::project_file(&project),
        "schema = 1\ntitle = \"Invoicing\"\nrepositories = [\"invoicing\"]\n",
    );
    write(&layout::task_file(&project), &task_toml("g1", "Invoicing"));
    let backfill = layout::child_dir(&project, "backfill");
    write(&layout::task_file(&backfill), &task_toml("l1", "Backfill"));
    let audit = layout::child_dir(&project, "audit");
    write(&layout::task_file(&audit), &task_toml("l2", "Audit"));
    (backfill, audit)
}

/// A record written the way an agent or Kyle would, outside the store.
fn external_record(scope_dir: &Path, name: &str) -> (ChatRecord, PathBuf) {
    let mut record = ChatRecord::new(Agent::Claude, "free", scope_dir.to_path_buf());
    record.name = name.to_string();
    let path = record_path(scope_dir, &record.id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_record(&path, &record).unwrap();
    (record, path)
}

fn set_chats_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.chats.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

struct Setup {
    _temp: TempDir,
    root: PathBuf,
    types_root: PathBuf,
    work: Entity<WorkStore>,
    chats: Entity<ChatStore>,
}

fn setup(cx: &mut TestAppContext, chats_enabled: bool) -> Setup {
    let temp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("work");
    let types_root = base.join("chat-types");
    std::fs::create_dir_all(&root).unwrap();
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        WorkSettings::register(cx);
        ChatsSettings::register(cx);
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(true);
                content.chats.get_or_insert_default().enabled = Some(chats_enabled);
            });
        });
    });
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let work = cx.new(|cx| WorkStore::new(fs, WorkDirs::for_test(root.clone()), cx));
    let dirs = ChatDirs {
        types_root: types_root.clone(),
        runtime: base.join("runtime"),
    };
    let chats = cx.new(|cx| ChatStore::new(work.clone(), dirs, cx));
    Setup {
        _temp: temp,
        root,
        types_root,
        work,
        chats,
    }
}

/// Waits up to ten seconds for `check` to hold on the chat store.
async fn wait_until(
    cx: &mut TestAppContext,
    chats: &Entity<ChatStore>,
    what: &str,
    check: impl Fn(&ChatStore) -> bool,
) {
    for _ in 0..200 {
        if chats.read_with(cx, |store, _| check(store)) {
            return;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn wait_work_loaded(cx: &mut TestAppContext, work: &Entity<WorkStore>) {
    for _ in 0..200 {
        if work.read_with(cx, |store, _| store.snapshot().is_some()) {
            // Let the watches settle before editing anything.
            cx.executor().timer(SETTLE).await;
            return;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for the work tree");
}

fn indexed_name(store: &ChatStore, id: &str) -> Option<String> {
    store
        .find(id)
        .and_then(|chat| chat.entry.record().map(|record| record.name.clone()))
}

#[gpui::test]
async fn an_external_edit_reaches_the_index(cx: &mut TestAppContext) {
    let setup = setup(cx, true);
    let (backfill, _) = seed_tree(&setup.root);
    wait_work_loaded(cx, &setup.work).await;

    let (record, path) = external_record(&backfill, "First name");
    wait_until(cx, &setup.chats, "the new record", |store| {
        indexed_name(store, &record.id).as_deref() == Some("First name")
    })
    .await;
    let chat = setup
        .chats
        .read_with(cx, |store, _| store.find(&record.id).cloned().unwrap());
    assert_eq!(chat.scope_dir, backfill);
    assert_eq!(chat.task_id.as_deref(), Some("l1"));

    let mut edited = record.clone();
    edited.name = "Edited by hand".to_string();
    write_record(&path, &edited).unwrap();
    wait_until(cx, &setup.chats, "the edited record", |store| {
        indexed_name(store, &record.id).as_deref() == Some("Edited by hand")
    })
    .await;

    std::fs::remove_file(&path).unwrap();
    wait_until(cx, &setup.chats, "the deleted record to leave", |store| {
        store.find(&record.id).is_none()
    })
    .await;
}

#[gpui::test]
async fn a_record_in_a_moved_task_is_still_indexed(cx: &mut TestAppContext) {
    let setup = setup(cx, true);
    let (backfill, audit) = seed_tree(&setup.root);
    let (record, _) = external_record(&backfill, "Moving along");
    wait_work_loaded(cx, &setup.work).await;
    wait_until(cx, &setup.chats, "the record", |store| {
        store.find(&record.id).is_some()
    })
    .await;

    setup
        .work
        .update(cx, |store, cx| {
            store.write(
                Context::at_keyboard(PersonAtKeyboard::for_test()),
                |tree, ctx| commands::move_task(tree, ctx, "l1", "l2"),
                cx,
            )
        })
        .await
        .unwrap();
    let moved = layout::child_dir(&audit, "backfill");
    wait_until(cx, &setup.chats, "the record under its new path", |store| {
        store
            .find(&record.id)
            .is_some_and(|chat| chat.scope_dir == moved)
    })
    .await;
    let chat = setup
        .chats
        .read_with(cx, |store, _| store.find(&record.id).cloned().unwrap());
    assert_eq!(chat.task_id.as_deref(), Some("l1"));
    assert_eq!(chat.entry.path, record_path(&moved, &record.id));
    assert_eq!(chat.entry.record().unwrap().name, "Moving along");
    assert_eq!(setup.chats.read_with(cx, |store, _| store.index().len()), 1);
}

#[gpui::test]
async fn a_record_id_is_one_path_component_and_never_changes(cx: &mut TestAppContext) {
    let setup = setup(cx, true);
    let (backfill, _) = seed_tree(&setup.root);
    wait_work_loaded(cx, &setup.work).await;

    for bad in ["../escape", "a/b", "..", ".", "", "a\\b"] {
        let mut record = ChatRecord::new(Agent::Claude, "free", backfill.clone());
        record.id = bad.to_string();
        let result = setup
            .chats
            .update(cx, |store, cx| {
                store.create(backfill.clone(), record.clone(), cx)
            })
            .await;
        assert!(
            matches!(result, Err(ChatWriteError::Failed(_))),
            "{bad:?}: {result:?}"
        );
    }
    assert!(!backfill.join("escape.toml").exists());
    assert!(
        !layout::project_dir(&setup.root, "invoicing")
            .join("escape.toml")
            .exists()
    );

    let record = ChatRecord::new(Agent::Claude, "free", backfill.clone());
    let path = setup
        .chats
        .update(cx, |store, cx| {
            store.create(backfill.clone(), record.clone(), cx)
        })
        .await
        .unwrap();
    let result = setup
        .chats
        .update(cx, |store, cx| {
            store.update(
                path.clone(),
                |record| {
                    record.id = "another".to_string();
                    record.prompted = true;
                    Ok(())
                },
                cx,
            )
        })
        .await;
    assert!(
        matches!(result, Err(ChatWriteError::Failed(_))),
        "{result:?}"
    );
    // Nothing was written: not even the other change.
    let on_disk = read_record(&path).unwrap();
    assert_eq!(on_disk.id, record.id);
    assert!(!on_disk.prompted);
}

#[gpui::test]
async fn a_write_after_an_external_edit_keeps_the_edit(cx: &mut TestAppContext) {
    let setup = setup(cx, true);
    let (backfill, _) = seed_tree(&setup.root);
    wait_work_loaded(cx, &setup.work).await;

    let record = ChatRecord::new(Agent::Codex, "plan", backfill.clone());
    let path = setup
        .chats
        .update(cx, |store, cx| {
            store.create(backfill.clone(), record.clone(), cx)
        })
        .await
        .unwrap();
    assert_eq!(path, record_path(&backfill, &record.id));

    // Kyle renames the chat by hand and adds a key this version does not know.
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace(&format!("name = \"{}\"", record.name), "name = \"By hand\"");
    write(&path, &format!("{text}later_feature = \"kept\"\n"));

    let written = setup
        .chats
        .update(cx, |store, cx| {
            store.update(
                path.clone(),
                |record| {
                    record.prompted = true;
                    Ok(())
                },
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(written.name, "By hand");
    let on_disk = read_record(&path).unwrap();
    assert_eq!(on_disk.name, "By hand");
    assert!(on_disk.prompted);
    assert_eq!(
        on_disk
            .extra
            .get("later_feature")
            .and_then(|value| value.as_str()),
        Some("kept")
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("later_feature")
    );
}

#[gpui::test]
async fn a_write_to_a_deleted_record_is_refused(cx: &mut TestAppContext) {
    let setup = setup(cx, true);
    let (backfill, _) = seed_tree(&setup.root);
    wait_work_loaded(cx, &setup.work).await;
    let (record, path) = external_record(&backfill, "Soon gone");
    std::fs::remove_file(&path).unwrap();

    let result = setup
        .chats
        .update(cx, |store, cx| {
            store.update(
                path.clone(),
                |record| {
                    record.prompted = true;
                    Ok(())
                },
                cx,
            )
        })
        .await;
    assert_eq!(result, Err(ChatWriteError::Missing(path.clone())));
    assert!(!path.exists());

    // Creating a chat that already exists is refused too, and leaves the file alone.
    let (_, kept) = external_record(&backfill, "Kept");
    let original = std::fs::read_to_string(&kept).unwrap();
    let existing = read_record(&kept).unwrap();
    let mut clash = record;
    clash.id = existing.id.clone();
    let result = setup
        .chats
        .update(cx, |store, cx| store.create(backfill.clone(), clash, cx))
        .await;
    assert!(
        matches!(result, Err(ChatWriteError::Failed(_))),
        "{result:?}"
    );
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), original);
}

#[gpui::test]
async fn nothing_indexes_or_seeds_while_disabled(cx: &mut TestAppContext) {
    let setup = setup(cx, false);
    let (backfill, _) = seed_tree(&setup.root);
    let (record, path) = external_record(&backfill, "Waiting");
    wait_work_loaded(cx, &setup.work).await;
    // An edit while disabled reaches the work store but not the chat index.
    let mut edited = record.clone();
    edited.name = "Still waiting".to_string();
    write_record(&path, &edited).unwrap();
    cx.executor().timer(SETTLE).await;

    setup.chats.read_with(cx, |store, _| {
        assert!(!store.is_enabled());
        assert!(store.index().is_empty());
        assert_eq!(store.stats().parses, 0);
        assert_eq!(store.stats().seeds, 0);
    });
    assert!(!setup.types_root.exists());
    let result = setup
        .chats
        .update(cx, |store, cx| store.update(path.clone(), |_| Ok(()), cx))
        .await;
    assert_eq!(result, Err(ChatWriteError::Off));
    let result = setup
        .chats
        .update(cx, |store, cx| {
            store.create(
                backfill.clone(),
                ChatRecord::new(Agent::Claude, "free", backfill.clone()),
                cx,
            )
        })
        .await;
    assert_eq!(result, Err(ChatWriteError::Off));
    assert_eq!(read_record(&path).unwrap(), edited);

    // Turning chats on seeds the types and indexes what is there.
    set_chats_enabled(cx, true);
    wait_until(cx, &setup.chats, "the index and the seed", |store| {
        indexed_name(store, &record.id).as_deref() == Some("Still waiting")
            && store.stats().seeds == 1
    })
    .await;
    assert!(setup.types_root.join("free").join("type.toml").is_file());

    // Turning them off again clears the index.
    set_chats_enabled(cx, false);
    setup.chats.read_with(cx, |store, _| {
        assert!(store.index().is_empty());
    });
}

#[gpui::test]
async fn actions_are_hidden_while_disabled(cx: &mut TestAppContext) {
    let setup = setup(cx, false);
    cx.update(|cx| {
        command_palette_hooks::init(cx);
        init_palette(cx);
    });
    let hidden = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let filter = CommandPaletteFilter::try_global(cx).unwrap();
            [
                filter.is_hidden(&chat::New),
                filter.is_hidden(&chat::TogglePanel),
                filter.is_hidden(&chat::SendRegion),
                filter.is_hidden(&chat::ToggleAllTasks),
            ]
        })
    };
    assert_eq!(hidden(cx), [true; 4]);
    set_chats_enabled(cx, true);
    assert_eq!(hidden(cx), [false; 4]);
    set_chats_enabled(cx, false);
    assert_eq!(hidden(cx), [true; 4]);
    drop(setup);
}

/// Chats are on in the default settings, so nothing needs to set `chats.enabled` and the actions
/// are shown; only a user who sets it false hides them.
#[gpui::test]
async fn chats_are_enabled_by_default(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        ChatsSettings::register(cx);
        command_palette_hooks::init(cx);
        init_palette(cx);
        assert!(ChatsSettings::get_global(cx).enabled);
        let filter = CommandPaletteFilter::try_global(cx).unwrap();
        assert!(!filter.is_hidden(&chat::New));
        assert!(!filter.is_hidden(&chat::TogglePanel));
    });
    set_chats_enabled(cx, false);
    cx.update(|cx| {
        assert!(!ChatsSettings::get_global(cx).enabled);
        let filter = CommandPaletteFilter::try_global(cx).unwrap();
        assert!(filter.is_hidden(&chat::New));
    });
}
