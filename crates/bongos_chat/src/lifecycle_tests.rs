//! The commands of part 2 and the lifecycle offer: archive and restore never delete and keep every
//! field, archive ends a running chat first, delete asks and removes the record and its launch
//! directory only, a rename sticks through a send, and closing a task or archiving a project offers
//! to archive its open chats and does nothing without the click.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use bongos_chat_model::{
    Agent, ChatRecord, Layout, NamedBy, project_slug, read_record, record_path, write_record,
};
use gpui::{Entity, TestAppContext, VisualTestContext};
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::commands::{archive_chat, delete_chat, rename_chat, restore_chat, reveal};
use crate::commands_tests::{answer, command_window, dispatch, has_modal, toast_shown};
use crate::lifecycle::{Lifecycle, LifecycleToast};
use crate::runtime_tests::{
    Harness, STARTUP, WITHIN, harness, process_alive, set_chat_settings, wait_for, write,
};
use crate::{ChatRuntime, ChatState, SendOptions, send};

/// A chat record in `scope`, written to disk (not running). `change` edits it first.
fn write_chat(
    h: &Harness,
    scope: &Path,
    name: &str,
    change: impl FnOnce(&mut ChatRecord),
) -> ChatRecord {
    let mut record = ChatRecord::new(Agent::Claude, "plan", h.cwd.clone());
    record.name = name.to_string();
    change(&mut record);
    let path = record_path(scope, &record.id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_record(&path, &record).unwrap();
    record
}

fn on_disk(scope: &Path, id: &str) -> ChatRecord {
    read_record(&record_path(scope, id)).unwrap()
}

async fn wait_indexed(h: &Harness, ids: &[&str], cx: &mut TestAppContext) {
    wait_for(cx, "the chats in the index", STARTUP, |cx| {
        h.store
            .read_with(cx, |store, _| ids.iter().all(|id| store.find(id).is_some()))
    })
    .await;
}

fn set_global_runtime(h: &Harness, cx: &mut TestAppContext) {
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[gpui::test]
async fn test_archive_and_restore_never_delete_and_keep_every_field(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    set_global_runtime(&h, cx);
    let record = write_chat(&h, &h.scope, "Usage audit", |record| {
        record.named_by = NamedBy::Kyle;
        record.session_id = Some("8c1e4f8a-0000-4000-8000-0000000000aa".to_string());
        record.model = Some("a-model".to_string());
        record.layout = Layout::ChatOnly;
        record.prompted = true;
        record.parent = Some("a-parent".to_string());
        record.prompts_file = Some(PathBuf::from("elsewhere/prompts.txt"));
        record.context_digest = Some("digest".to_string());
    });
    let path = record_path(&h.scope, &record.id);
    // A key of a later version, which an update keeps.
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("future_field = \"kept\"\n");
    std::fs::write(&path, text).unwrap();
    let before = read_record(&path).unwrap();
    assert!(!before.extra.is_empty());
    let files_before = files_in(&h.scope.join("chats"));
    wait_indexed(&h, &[&record.id], cx).await;

    cx.update(|cx| archive_chat(&record.id, cx)).await.unwrap();
    let archived = on_disk(&h.scope, &record.id);
    assert!(archived.archived);
    assert!(archived.archived_at.is_some());
    let mut expected = before.clone();
    expected.archived = true;
    expected.archived_at = archived.archived_at;
    assert_eq!(archived, expected, "every other field is kept");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("future_field = \"kept\"")
    );
    assert_eq!(files_in(&h.scope.join("chats")), files_before);

    // Archiving what is archived changes nothing, `archived_at` included.
    cx.update(|cx| archive_chat(&record.id, cx)).await.unwrap();
    assert_eq!(on_disk(&h.scope, &record.id), archived);

    cx.update(|cx| restore_chat(&record.id, cx)).await.unwrap();
    assert_eq!(on_disk(&h.scope, &record.id), before);
    assert_eq!(files_in(&h.scope.join("chats")), files_before);
}

#[gpui::test]
async fn test_archive_of_a_running_chat_ends_the_process_first(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    set_global_runtime(&h, cx);
    // Nothing was said in this chat and Claude's storage is there to look in, so the never-prompted
    // cleanup would delete it: only an archive that skips the cleanup keeps it.
    std::fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &[&id], cx).await;
    let pid = h.runtime.update(cx, |runtime, cx| {
        runtime
            .live(&id)
            .unwrap()
            .terminal()
            .read(cx)
            .pid()
            .unwrap()
    });
    assert!(process_alive(pid.as_u32()));

    let archive = cx.update(|cx| archive_chat(&id, cx));
    // The process is ended before the record changes.
    assert_eq!(h.state(&id, cx), ChatState::Closed);
    assert!(!on_disk(&h.scope, &id).archived);
    archive.await.unwrap();
    if cfg!(target_os = "linux") {
        wait_for(cx, "the process to be gone", WITHIN, |_| {
            !process_alive(pid.as_u32())
        })
        .await;
    }
    cx.executor().timer(WITHIN).await;
    cx.run_until_parked();
    let record = on_disk(&h.scope, &id);
    assert!(record.archived && record.archived_at.is_some());
    assert!(h.runtime_root.join(&id).is_dir(), "the launch files stay");
}

#[gpui::test]
async fn test_delete_asks_and_removes_only_the_record_and_its_launch_directory(
    cx: &mut TestAppContext,
) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let id = h.start(Agent::Claude, cx).await;
    let record = on_disk(&h.scope, &id);
    let sibling = write_chat(&h, &h.scope, "Sibling", |_| {});
    wait_indexed(&h, &[&id, &sibling.id], cx).await;

    // Things a delete must leave alone: a neighbor's launch directory, a stray file beside it, and
    // the CLI's own transcript of this very chat.
    let runtime_dir = h.runtime_root.join(&id);
    assert!(runtime_dir.is_dir());
    write(&h.runtime_root.join(&sibling.id).join("keep.txt"), "keep\n");
    write(&h.runtime_root.join("stray.txt"), "keep\n");
    let session = record.session_id.clone().unwrap();
    let transcript = h
        .home
        .join(".claude/projects")
        .join(project_slug(&record.cwd))
        .join(format!("{session}.jsonl"));
    write(&transcript, "{}\n");
    write(&h.scope.join("notes/mine.md"), "notes\n");
    let record_file = record_path(&h.scope, &id);

    let shown_now = workspace.update_in(cx, |workspace, window, cx| {
        reveal(&id, workspace, window, cx)
    });
    assert!(shown_now);
    // Declined: nothing is deleted, the chat keeps running.
    dispatch(cx, "chat::Delete");
    answer(&workspace, "Keep", cx).await;
    cx.executor().timer(WITHIN).await;
    cx.run_until_parked();
    assert!(record_file.is_file());
    assert!(runtime_dir.is_dir());
    assert!(crate::live_modes::is_open(h.state(&id, cx)));

    // Confirmed: the record and its launch directory go, and only those.
    dispatch(cx, "chat::Delete");
    answer(&workspace, "Delete", cx).await;
    wait_for(cx, "the record to be removed", STARTUP, |_| {
        !record_file.exists()
    })
    .await;
    wait_for(cx, "the launch directory to be removed", STARTUP, |_| {
        !runtime_dir.exists()
    })
    .await;
    assert_eq!(h.state(&id, cx), ChatState::Closed);
    assert!(record_path(&h.scope, &sibling.id).is_file());
    assert!(h.runtime_root.join(&sibling.id).join("keep.txt").is_file());
    assert!(h.runtime_root.join("stray.txt").is_file());
    assert!(transcript.is_file(), "the CLI's transcript is not ours");
    assert!(h.scope.join("notes/mine.md").is_file());
    assert!(h.scope.join("task.toml").is_file());
    assert_eq!(
        files_in(&h.scope.join("chats")),
        [format!("{}.toml", sibling.id)]
    );
    assert!(!has_modal(&workspace, cx));
}

#[gpui::test]
async fn test_delete_of_a_chat_that_is_gone_or_has_a_hostile_id_touches_nothing(
    cx: &mut TestAppContext,
) {
    let h = harness(cx, true, 60, &[]).await;
    set_global_runtime(&h, cx);
    write(&h.runtime_root.join("stray.txt"), "keep\n");
    let error = cx
        .update(|cx| delete_chat("no-such-chat", cx))
        .await
        .unwrap_err();
    assert!(error.contains("No chat has the id"), "{error}");
    assert!(h.runtime_root.join("stray.txt").is_file());
}

#[gpui::test]
async fn test_a_renamed_chat_keeps_its_name_after_a_send(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    set_global_runtime(&h, cx);
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &[&id], cx).await;
    let default_name = on_disk(&h.scope, &id).name;

    let empty = cx
        .update(|cx| rename_chat(&id, "  ".to_string(), cx))
        .await
        .unwrap_err();
    assert!(empty.contains("needs a name"), "{empty}");
    assert_eq!(on_disk(&h.scope, &id).name, default_name);

    cx.update(|cx| rename_chat(&id, "  Usage audit ".to_string(), cx))
        .await
        .unwrap();
    let record = on_disk(&h.scope, &id);
    assert_eq!(record.name, "Usage audit");
    assert_eq!(record.named_by, NamedBy::Kyle);

    // A send, which names an unnamed chat from its first line, and a title leave it.
    let sent = cx
        .update(|cx| {
            send(
                &id,
                "Fix the backfill for invoices".to_string(),
                SendOptions { submit: true },
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(sent.named, None);
    wait_for(cx, "the prompt on record", STARTUP, |_| {
        on_disk(&h.scope, &id).prompted
    })
    .await;
    h.type_line(&id, "title Fixing the login redirect", cx);
    cx.executor().timer(WITHIN).await;
    cx.run_until_parked();
    let record = on_disk(&h.scope, &id);
    assert_eq!(record.name, "Usage audit");
    assert_eq!(record.named_by, NamedBy::Kyle);
}

#[gpui::test]
async fn test_restore_lists_the_archived_and_restores_the_pick(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    dispatch(cx, "chat::Restore");
    assert!(toast_shown(&workspace, cx), "no archived chat yet");
    assert!(!has_modal(&workspace, cx));
    workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));

    let open = write_chat(&h, &h.scope, "Still open", |_| {});
    let archived = write_chat(&h, &h.scope, "Old audit", |record| {
        record.archived = true;
        record.archived_at = Some(bongos_work_model::now());
    });
    wait_indexed(&h, &[&open.id, &archived.id], cx).await;
    let before = on_disk(&h.scope, &archived.id);
    dispatch(cx, "chat::Restore");
    // Only the archived chat is listed.
    answer(&workspace, "Old audit", cx).await;
    wait_for(cx, "the restore", STARTUP, |_| {
        !on_disk(&h.scope, &archived.id).archived
    })
    .await;
    let after = on_disk(&h.scope, &archived.id);
    assert_eq!(after.archived_at, None);
    let mut expected = before;
    expected.archived = false;
    expected.archived_at = None;
    assert_eq!(after, expected);
    assert!(!on_disk(&h.scope, &open.id).archived);
}

// ---------------------------------------------------------------------------------------------
// The offer to archive a finished task's chats.

fn offer_shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<LifecycleToast>())
    })
}

fn offered(cx: &mut TestAppContext) -> Vec<String> {
    let lifecycle = cx.update(|cx| Lifecycle::global(cx).unwrap());
    let mut ids = lifecycle.read_with(cx, |lifecycle, _| lifecycle.offered());
    ids.sort();
    ids
}

async fn wait_for_task(h: &Harness, id: &str, cx: &mut TestAppContext) {
    wait_for(cx, "the task in the tree", STARTUP, |cx| {
        h.work.read_with(cx, |work, _| {
            work.snapshot()
                .is_some_and(|tree| tree.task_by_id(id).is_some())
        })
    })
    .await;
}

/// The harness with a second task `t9`, chats on it and on `t1`, the lifecycle watching and a
/// workspace to show the offer in. Returns the ids of the open chats of `t1`, of its archived chat
/// and of the chat of `t9`.
async fn lifecycle_window<'a>(
    h: &Harness,
    cx: &'a mut TestAppContext,
) -> (
    Entity<Workspace>,
    &'a mut VisualTestContext,
    Vec<String>,
    String,
    String,
) {
    let other = h.root.join("projects/invoicing/tasks/other");
    write(
        &other.join("task.toml"),
        "schema = 1\nid = \"t9\"\ngoal = \"Other work\"\nkind = \"pr-ship\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n",
    );
    let first = write_chat(h, &h.scope, "First", |_| {});
    let second = write_chat(h, &h.scope, "Second", |record| record.prompted = true);
    let old = write_chat(h, &h.scope, "Old", |record| {
        record.archived = true;
        record.archived_at = Some(bongos_work_model::now());
    });
    let elsewhere = write_chat(h, &other, "Elsewhere", |_| {});
    let (workspace, cx) = command_window(h, cx).await;
    wait_for_task(h, "t1", cx).await;
    wait_for_task(h, "t9", cx).await;
    wait_indexed(h, &[&first.id, &second.id, &old.id, &elsewhere.id], cx).await;
    cx.update(|_, cx| crate::lifecycle::init(&h.store, cx));
    (
        workspace,
        cx,
        vec![first.id, second.id],
        old.id,
        elsewhere.id,
    )
}

fn rewrite(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{from} in {}", path.display());
    std::fs::write(path, text.replace(from, to)).unwrap();
}

#[gpui::test]
async fn test_closing_a_task_offers_to_archive_its_open_chats_and_waits_for_the_click(
    cx: &mut TestAppContext,
) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx, open, old, elsewhere) = lifecycle_window(&h, cx).await;
    let old_before = on_disk(&h.scope, &old);
    assert!(!offer_shown(&workspace, cx), "nothing was closed yet");

    rewrite(
        &h.scope.join("task.toml"),
        "status = \"open\"",
        "status = \"done\"\nclosed = 2020-01-02T10:00:00Z",
    );
    wait_for(cx, "the offer", STARTUP, |cx| {
        cx.run_until_parked();
        let mut shown = false;
        cx.update(|cx| shown = !Lifecycle::global(cx).unwrap().read(cx).offered().is_empty());
        shown
    })
    .await;
    cx.run_until_parked();
    assert!(offer_shown(&workspace, cx));
    let mut expected = open.clone();
    expected.sort();
    assert_eq!(
        offered(cx),
        expected,
        "its open chats, not the archived one or t9's"
    );

    // Without the click nothing happens.
    cx.executor().timer(WITHIN).await;
    cx.run_until_parked();
    for id in &open {
        assert!(!on_disk(&h.scope, id).archived);
    }

    // The click archives exactly those.
    let lifecycle = cx.update(|_, cx| Lifecycle::global(cx).unwrap());
    lifecycle.update(cx, |lifecycle, cx| lifecycle.accept(cx));
    wait_for(cx, "the chats to be archived", STARTUP, |_| {
        open.iter().all(|id| on_disk(&h.scope, id).archived)
    })
    .await;
    for id in &open {
        assert!(on_disk(&h.scope, id).archived_at.is_some());
    }
    assert_eq!(on_disk(&h.scope, &old), old_before);
    let other = h.root.join("projects/invoicing/tasks/other");
    assert!(!on_disk(&other, &elsewhere).archived);
    assert!(offered(cx).is_empty(), "the offer is spent");
}

#[gpui::test]
async fn test_dropping_a_task_and_archiving_a_project_offer_too(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx, open, _, elsewhere) = lifecycle_window(&h, cx).await;
    // A chat of the project itself.
    let project_dir = h.root.join("projects/invoicing");
    let top = write_chat(&h, &project_dir, "Project chat", |_| {});
    wait_indexed(&h, &[&top.id], cx).await;

    // Dropping t9 offers only its chat.
    rewrite(
        &h.root.join("projects/invoicing/tasks/other/task.toml"),
        "status = \"open\"",
        "status = \"cancelled\"\nclosed = 2020-01-02T10:00:00Z",
    );
    wait_for(cx, "the offer for t9", STARTUP, |cx| {
        offered(cx) == [elsewhere.clone()]
    })
    .await;
    assert!(offer_shown(&workspace, cx));

    // Archiving the project offers the chats of its tasks that are still open (t9's chat too: it
    // is still an open chat of the project).
    rewrite(
        &project_dir.join("project.toml"),
        "title = \"Invoicing\"",
        "title = \"Invoicing\"\narchived = true",
    );
    let mut all = open.clone();
    all.push(top.id.clone());
    all.push(elsewhere.clone());
    all.sort();
    wait_for(cx, "the offer for the project", STARTUP, |cx| {
        offered(cx) == all
    })
    .await;
    for id in &open {
        assert!(!on_disk(&h.scope, id).archived, "not without the click");
    }
}

#[gpui::test]
async fn test_a_task_without_open_chats_offers_nothing(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx, _, _, _) = lifecycle_window(&h, cx).await;
    // `notes` has no chat.
    let notes = h.root.join("projects/invoicing/tasks/notes");
    write(
        &notes.join("task.toml"),
        "schema = 1\nid = \"t7\"\ngoal = \"Notes\"\nkind = \"document\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n",
    );
    wait_for_task(&h, "t7", cx).await;
    rewrite(
        &notes.join("task.toml"),
        "status = \"open\"",
        "status = \"done\"\nclosed = 2020-01-02T10:00:00Z",
    );
    wait_for(cx, "t7 closed in the tree", STARTUP, |cx| {
        h.work.read_with(cx, |work, _| {
            work.snapshot().is_some_and(|tree| {
                tree.task_by_id("t7")
                    .and_then(|task| task.file.as_ref())
                    .is_some_and(|file| file.status != bongos_work_model::Status::Open)
            })
        })
    })
    .await;
    cx.executor().timer(WITHIN).await;
    cx.run_until_parked();
    assert!(!offer_shown(&workspace, cx));
    assert!(offered(cx).is_empty());
}

#[gpui::test]
async fn test_nothing_watches_while_chats_are_off(cx: &mut TestAppContext) {
    let h = harness(cx, false, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let note = write_chat(&h, &h.scope, "Note", |_| {});
    wait_for(cx, "t1 in the tree", STARTUP, |cx| {
        h.work.read_with(cx, |work, _| {
            work.snapshot()
                .is_some_and(|tree| tree.task_by_id("t1").is_some())
        })
    })
    .await;
    cx.update(|_, cx| crate::lifecycle::init(&h.store, cx));
    let lifecycle = cx.update(|_, cx| Lifecycle::global(cx).unwrap());
    assert!(!lifecycle.read_with(cx, |lifecycle, _| lifecycle.is_watching()));

    rewrite(
        &h.scope.join("task.toml"),
        "status = \"open\"",
        "status = \"done\"\nclosed = 2020-01-02T10:00:00Z",
    );
    cx.executor().timer(WITHIN).await;
    cx.run_until_parked();
    assert!(!offer_shown(&workspace, cx));
    assert!(!on_disk(&h.scope, &note.id).archived);
    assert!(!lifecycle.read_with(cx, |lifecycle, _| lifecycle.is_watching()));

    // Turned on, it watches, and what was closed before is not offered.
    set_chat_settings(cx, true, 60);
    cx.run_until_parked();
    assert!(lifecycle.read_with(cx, |lifecycle, _| lifecycle.is_watching()));
    cx.executor().timer(WITHIN).await;
    cx.run_until_parked();
    assert!(!offer_shown(&workspace, cx));
}
