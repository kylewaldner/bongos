//! Quitting with running chats (design D18): the prompt text, the quit decision and what the
//! app-quit hook does to the chats and their records.

#![cfg(unix)]

use std::time::Duration;

use bongos_chat_model::{Agent, read_record, record_path, write_record};
use gpui::TestAppContext;

use crate::runtime_tests::{WITHIN, harness, try_wait_for, wait_for};
use crate::{ChatRuntime, ChatState, end_chats_on_quit, live_chats, quit_prompt, quit_prompt_text};

#[test]
fn test_quit_prompt_text_names_the_chats() {
    assert_eq!(quit_prompt_text(&[]), None);
    assert_eq!(
        quit_prompt_text(&["fix-bug".to_string()]).as_deref(),
        Some("1 chat is running (fix-bug). Quitting ends it; it resumes from the Chats panel.")
    );
    assert_eq!(
        quit_prompt_text(&["a".to_string(), "b".to_string()]).as_deref(),
        Some("2 chats are running (a, b). Quitting ends them; they resume from the Chats panel.")
    );
}

#[gpui::test]
async fn test_quit_with_no_chat_asks_nothing_extra(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    cx.update(|cx| {
        ChatRuntime::set_global(h.runtime.clone(), cx);
        end_chats_on_quit(&h.runtime, cx);
        assert!(live_chats(cx).is_empty());
        assert_eq!(quit_prompt(cx), None);
    });
    // The hook has nothing to end and quitting still completes.
    cx.quit();
    assert_eq!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.stats().active_writes),
        0
    );
}

#[gpui::test]
async fn test_quit_asks_while_a_chat_runs_and_cancel_keeps_it(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "the chat in the index", WITHIN * 10, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    let name = h.store.read_with(cx, |store, _| {
        store
            .find(&id)
            .unwrap()
            .entry
            .record()
            .unwrap()
            .name
            .clone()
    });

    let question = cx
        .update(|cx| quit_prompt(cx))
        .expect("a running chat is asked about");
    assert!(question.starts_with("1 chat is running ("), "{question}");
    assert!(question.contains(&name), "{question}");

    // Cancel asks nothing more of the chats: it is still running, with its record open.
    cx.run_until_parked();
    assert!(!matches!(
        h.state(&id, cx),
        ChatState::Exited { .. } | ChatState::Closed
    ));
    assert!(cx.update(|cx| live_chats(cx)).contains(&name));
    assert!(!read_record(&record_path(&h.scope, &id)).unwrap().archived);
}

#[gpui::test]
async fn test_quit_ends_the_chats_and_leaves_their_records_open(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    // No write but the quit's own lands during the test.
    h.runtime.update(cx, |runtime, _| {
        runtime.set_active_debounce(Duration::from_secs(3600))
    });
    cx.update(|cx| {
        ChatRuntime::set_global(h.runtime.clone(), cx);
        end_chats_on_quit(&h.runtime, cx);
    });
    let id = h.start(Agent::Claude, cx).await;
    let pid = h.runtime.update(cx, |runtime, cx| {
        runtime
            .live(&id)
            .unwrap()
            .terminal()
            .read(cx)
            .pid()
            .unwrap()
    });

    // The record says the chat was last active long ago.
    let path = record_path(&h.scope, &id);
    let mut record = read_record(&path).unwrap();
    let old = "2020-01-01T10:00:00Z".parse().unwrap();
    record.last_active = old;
    write_record(&path, &record).unwrap();
    let writes = h
        .runtime
        .read_with(cx, |runtime, _| runtime.stats().active_writes);

    cx.quit();

    let after = read_record(&path).unwrap();
    assert_ne!(after.last_active, old, "last_active is written on quit");
    assert!(
        !after.archived && after.archived_at.is_none(),
        "the record stays open"
    );
    assert!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.stats().active_writes)
            > writes,
        "the runtime wrote last_active"
    );
    wait_for(cx, "the chat's process to end", WITHIN * 10, |cx| {
        matches!(h.state(&id, cx), ChatState::Exited { .. })
    })
    .await;
    if cfg!(target_os = "linux") {
        assert!(
            try_wait_for(cx, WITHIN * 10, |_| !crate::runtime_tests::process_alive(
                pid.as_u32()
            ))
            .await
        );
    }
    // The record is still there for the Chats panel to list after the next launch.
    assert!(path.is_file());
    assert!(!read_record(&path).unwrap().archived);
}

#[gpui::test]
async fn test_nothing_runs_on_quit_while_chats_are_off(cx: &mut TestAppContext) {
    let h = harness(cx, false, 60, &[]).await;
    cx.update(|cx| {
        ChatRuntime::set_global(h.runtime.clone(), cx);
        end_chats_on_quit(&h.runtime, cx);
        assert_eq!(quit_prompt(cx), None);
    });
    cx.quit();
    assert_eq!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.stats().active_writes),
        0
    );
}

/// `App::shutdown` blocks the main thread on the hook's future and runs no main-thread task
/// meanwhile, so the future must wait on the writer's reply channels alone. It is `Send`, so it
/// holds no entity or main-thread handle, and a thread that only polls it sees the write finish
/// (the test scheduler runs the writer, a background task, while the main thread parks).
#[gpui::test]
async fn test_the_quit_future_waits_on_the_writer_alone(cx: &mut TestAppContext) {
    fn assert_send<T: Send>(_: &T) {}

    let h = harness(cx, true, 60, &[]).await;
    h.runtime.update(cx, |runtime, _| {
        runtime.set_active_debounce(Duration::from_secs(3600))
    });
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    let id = h.start(Agent::Claude, cx).await;
    let path = record_path(&h.scope, &id);
    let mut record = read_record(&path).unwrap();
    let old = "2020-01-01T10:00:00Z".parse().unwrap();
    record.last_active = old;
    write_record(&path, &record).unwrap();

    let end = h.runtime.update(cx, |runtime, cx| runtime.end_for_quit(cx));
    assert_send(&end);
    let waiter = std::thread::spawn(move || futures::executor::block_on(end));
    wait_for(cx, "the quit future to finish", WITHIN * 10, |_| {
        waiter.is_finished()
    })
    .await;
    waiter.join().unwrap();
    assert_ne!(read_record(&path).unwrap().last_active, old);
    assert!(!read_record(&path).unwrap().archived);
}
