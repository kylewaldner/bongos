//! The send path against the fake CLIs in a real PTY (the harness of `runtime_tests`): one paste
//! and one Enter, no Enter without `submit`, nothing written without bracketed paste, the text kept
//! on a refusal, the names a send and a title give, and `chat: send region` dispatched by name.
//! Also the Enter's timing (the settle wait, the submit delay without a channel) and, on a live
//! channel, the confirmation of the submit, its one retry and the sends that are not retried.

#![cfg(unix)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bongos_chat_model::channel::report::Event;
use bongos_chat_model::{Agent, NamedBy, read_record, record_path, write_record};
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use fs::RealFs;
use gpui::{
    AppContext as _, Entity, Focusable as _, Task, TestAppContext, UpdateGlobal as _,
    VisualTestContext,
};
use project::Project;
use settings::SettingsStore;
use tempfile::TempDir;
use workspace::notifications::NotificationId;
use workspace::{MultiWorkspace, Workspace};

use crate::channel_runtime_tests::{Channels, channels, event_report, needs_input};
use crate::runtime_tests::{Harness, STARTUP, WITHIN, harness, wait_for};
use crate::{ChatRuntime, ChatState, ChatToast, SendError, SendOptions, Sent, Source, send};

/// A multi-line prompt with a blank line, a tab, quotes and a fence.
const PROMPT: &str = "Fix the backfill for invoices\n\n\tkeep the \"dry run\" flag\n```rust\nfn main() {}\n```\nthen stop";

fn bracketed(text: &str) -> Vec<u8> {
    format!("\x1b[200~{text}\x1b[201~").into_bytes()
}

/// The bytes of `bytes` as the fake logs them: three octal digits and a space each.
fn octal(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:03o} ")).collect()
}

/// A harness whose runtime is the global one, so [`send`] finds it, and the file the fake logs its
/// input bytes to.
async fn send_harness(
    cx: &mut TestAppContext,
    env: &[(&str, &str)],
) -> (Harness, TempDir, PathBuf) {
    let input_dir = TempDir::new().unwrap();
    let input = input_dir.path().join("input");
    let input_text = input.to_string_lossy().into_owned();
    let mut env = env.to_vec();
    env.push(("FAKE_CLI_INPUT", &input_text));
    let h = harness(cx, true, 60, &env).await;
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    (h, input_dir, input)
}

/// Everything written to the chat's terminal since the last call.
fn written(h: &Harness, id: &str, cx: &mut TestAppContext) -> Vec<Vec<u8>> {
    h.runtime.update(cx, |runtime, cx| {
        let terminal = runtime.live(id).unwrap().terminal().clone();
        terminal.update(cx, |terminal, _| terminal.take_input_log())
    })
}

async fn send_text(
    id: &str,
    text: &str,
    submit: bool,
    cx: &mut TestAppContext,
) -> Result<Sent, SendError> {
    let task = cx.update(|cx| send(id, text.to_string(), SendOptions { submit }, cx));
    task.await
}

fn record_of(h: &Harness, id: &str) -> bongos_chat_model::ChatRecord {
    read_record(&record_path(&h.scope, id)).unwrap()
}

async fn wait_indexed(h: &Harness, id: &str, cx: &mut TestAppContext) {
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(id).is_some())
    })
    .await;
}

#[gpui::test]
async fn test_a_multiline_send_is_one_paste_and_one_enter(cx: &mut TestAppContext) {
    let (h, _input_dir, input) = send_harness(cx, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    written(&h, &id, cx);

    let sent = send_text(&id, PROMPT, true, cx).await.unwrap();
    assert_eq!(
        sent,
        Sent {
            submitted: true,
            confirmed: false,
            named: Some("Fix the backfill for invoices".to_string()),
        }
    );
    assert_eq!(
        written(&h, &id, cx),
        vec![bracketed(PROMPT), b"\r".to_vec()]
    );
    let mut expected = bracketed(PROMPT);
    expected.push(b'\r');
    let expected = octal(&expected);
    wait_for(cx, "the bytes to reach the fake", STARTUP, |_| {
        std::fs::read_to_string(&input).unwrap_or_default() == expected
    })
    .await;

    let record = record_of(&h, &id);
    assert!(record.prompted);
    assert_eq!(record.name, "Fix the backfill for invoices");
    assert_eq!(record.named_by, NamedBy::Auto);
    assert!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.live(&id).unwrap().prompted())
    );

    // Only the first prompt names the chat.
    let sent = send_text(&id, "Now write the tests", true, cx)
        .await
        .unwrap();
    assert_eq!(sent.named, None);
    assert_eq!(record_of(&h, &id).name, "Fix the backfill for invoices");
}

#[gpui::test]
async fn test_two_sends_at_once_do_not_interleave(cx: &mut TestAppContext) {
    let (h, _input_dir, input) = send_harness(cx, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    written(&h, &id, cx);

    let second_text = "Then\nwrite the tests";
    let first = cx.update(|cx| send(&id, PROMPT.to_string(), SendOptions { submit: true }, cx));
    let second = cx.update(|cx| {
        send(
            &id,
            second_text.to_string(),
            SendOptions { submit: true },
            cx,
        )
    });
    assert!(first.await.unwrap().submitted);
    assert!(second.await.unwrap().submitted);
    assert_eq!(
        written(&h, &id, cx),
        vec![
            bracketed(PROMPT),
            b"\r".to_vec(),
            bracketed(second_text),
            b"\r".to_vec()
        ]
    );
    let mut expected = bracketed(PROMPT);
    expected.push(b'\r');
    expected.extend(bracketed(second_text));
    expected.push(b'\r');
    let expected = octal(&expected);
    wait_for(cx, "both sends to reach the fake", STARTUP, |_| {
        std::fs::read_to_string(&input).unwrap_or_default() == expected
    })
    .await;
}

/// The runtime's clock in milliseconds, the one the send waits by.
fn clock_ms(h: &Harness, cx: &mut TestAppContext) -> u64 {
    h.runtime.read_with(cx, |runtime, _| runtime.now_ms())
}

fn set_submit_delay(cx: &mut TestAppContext, ms: u64) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.chats.get_or_insert_default().submit_delay_ms = Some(ms);
            });
        });
    });
}

#[gpui::test]
async fn test_a_large_paste_into_a_slow_reader_is_one_paste_and_one_enter(cx: &mut TestAppContext) {
    let (h, _input_dir, input) = send_harness(cx, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    written(&h, &id, cx);

    // The fake reads one byte at a time and answers every line, so it is still reading and
    // printing long after the paste was written.
    let mut text: String = (0..16)
        .map(|line| format!("step {line:02} of the plan\n"))
        .collect();
    text.push_str("then stop");
    let sent = send_text(&id, &text, true, cx).await.unwrap();
    assert!(sent.submitted);
    assert!(!sent.confirmed);
    assert_eq!(written(&h, &id, cx), vec![bracketed(&text), b"\r".to_vec()]);
    let mut expected = bracketed(&text);
    expected.push(b'\r');
    let expected = octal(&expected);
    wait_for(
        cx,
        "the whole paste and its Enter to reach the fake",
        STARTUP,
        |_| std::fs::read_to_string(&input).unwrap_or_default() == expected,
    )
    .await;
    // No second Enter follows: the send that would press it has finished.
    cx.run_until_parked();
    assert!(written(&h, &id, cx).is_empty());
}

#[gpui::test]
async fn test_the_settle_wait_runs_without_a_channel(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) = send_harness(cx, &[]).await;
    set_submit_delay(cx, 0);
    let id = h.start(Agent::Codex, cx).await;
    written(&h, &id, cx);
    let start = clock_ms(&h, cx);
    let sent = send_text(&id, PROMPT, true, cx).await.unwrap();
    // On the runtime's own clock, which the send waits by: the settle wait cannot end sooner.
    assert!(clock_ms(&h, cx) - start >= 100);
    assert!(sent.submitted);
    assert!(!sent.confirmed);
    assert_eq!(
        written(&h, &id, cx),
        vec![bracketed(PROMPT), b"\r".to_vec()]
    );
}

#[gpui::test]
async fn test_without_a_channel_the_enter_waits_the_submit_delay(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) = send_harness(cx, &[]).await;
    set_submit_delay(cx, 1200);
    let id = h.start(Agent::Claude, cx).await;
    written(&h, &id, cx);
    let start = clock_ms(&h, cx);
    let sent = send_text(&id, "Fix it", true, cx).await.unwrap();
    assert!(clock_ms(&h, cx) - start >= 1200);
    assert!(!sent.confirmed);
    assert_eq!(
        written(&h, &id, cx),
        vec![bracketed("Fix it"), b"\r".to_vec()]
    );
}

#[test]
fn test_the_settle_limit_grows_with_the_text() {
    assert_eq!(crate::send::settle_limit(0), Duration::from_secs(1));
    assert_eq!(
        crate::send::settle_limit(256 * 1024),
        Duration::from_secs(2)
    );
    assert_eq!(
        crate::send::settle_limit(128 * 1024),
        Duration::from_millis(1500)
    );
    assert_eq!(
        crate::send::settle_limit(100 * 1024 * 1024),
        Duration::from_secs(30)
    );
}

/// A Claude chat on a live channel, idle from its `session_start` report, with `confirm_wait` and
/// a submit delay far longer than any wait the test allows.
async fn live_chat(confirm_wait: Duration, cx: &mut TestAppContext) -> (Channels, String, String) {
    let c = channels(cx).await;
    set_submit_delay(cx, 60_000);
    c.h.runtime
        .update(cx, |runtime, _| runtime.set_confirm_wait(confirm_wait));
    let (id, token) = c.start_live(cx).await;
    written(&c.h, &id, cx);
    (c, id, token)
}

fn start_send(c: &Channels, id: &str, cx: &mut TestAppContext) -> Task<Result<Sent, SendError>> {
    c.h.runtime.update(cx, |runtime, cx| {
        runtime.send(id, PROMPT.to_string(), SendOptions { submit: true }, cx)
    })
}

/// Waits until `count` Enters were written after the paste; `log` keeps everything written.
async fn wait_enters(
    c: &Channels,
    id: &str,
    count: usize,
    log: &RefCell<Vec<Vec<u8>>>,
    cx: &mut TestAppContext,
) {
    wait_for(cx, "the Enter", WITHIN * 3, |cx| {
        log.borrow_mut().extend(written(&c.h, id, cx));
        log.borrow().iter().filter(|bytes| *bytes == b"\r").count() == count
    })
    .await;
}

fn state_of(c: &Channels, id: &str, cx: &mut TestAppContext) -> (ChatState, Source) {
    c.h.runtime.read_with(cx, |runtime, _| {
        let chat = runtime.live(id).unwrap();
        (chat.state(), chat.state_source())
    })
}

#[gpui::test]
async fn test_a_reported_submit_confirms_the_send(cx: &mut TestAppContext) {
    let (c, id, token) = live_chat(Duration::from_secs(10), cx).await;
    let task = start_send(&c, &id, cx);
    let log = RefCell::new(Vec::new());
    // Well before the submit delay: a chat with a channel does not wait for it.
    wait_enters(&c, &id, 1, &log, cx).await;
    c.deliver(event_report(&id, &token, 2, Event::PromptSubmitted), cx);
    let sent = task.await.unwrap();
    assert!(sent.submitted);
    assert!(sent.confirmed);
    log.borrow_mut().extend(written(&c.h, &id, cx));
    assert_eq!(*log.borrow(), vec![bracketed(PROMPT), b"\r".to_vec()]);
}

#[gpui::test]
async fn test_an_unreported_submit_is_retried_once_then_not_confirmed(cx: &mut TestAppContext) {
    let (c, id, _token) = live_chat(Duration::from_millis(300), cx).await;
    let error = start_send(&c, &id, cx).await.unwrap_err();
    let SendError::NotConfirmed { name } = &error else {
        panic!("expected NotConfirmed, got {error:?}");
    };
    assert_eq!(
        error.to_string(),
        format!("Pasted into {name}, but the submit was not confirmed; check the chat")
    );
    assert_eq!(
        written(&c.h, &id, cx),
        vec![bracketed(PROMPT), b"\r".to_vec(), b"\r".to_vec()]
    );
    // The text reached the CLI: the chat counts as prompted.
    assert!(read_record(&record_path(&c.h.scope, &id)).unwrap().prompted);
    cx.run_until_parked();
    assert!(written(&c.h, &id, cx).is_empty());
}

#[gpui::test]
async fn test_a_submit_reported_after_the_retry_confirms(cx: &mut TestAppContext) {
    let (c, id, token) = live_chat(Duration::from_secs(2), cx).await;
    let task = start_send(&c, &id, cx);
    let log = RefCell::new(Vec::new());
    wait_enters(&c, &id, 2, &log, cx).await;
    c.deliver(event_report(&id, &token, 2, Event::PromptSubmitted), cx);
    assert!(task.await.unwrap().confirmed);
    log.borrow_mut().extend(written(&c.h, &id, cx));
    assert_eq!(
        *log.borrow(),
        vec![bracketed(PROMPT), b"\r".to_vec(), b"\r".to_vec()]
    );
}

#[gpui::test]
async fn test_a_send_into_a_running_turn_does_not_wait(cx: &mut TestAppContext) {
    let (c, id, token) = live_chat(Duration::from_secs(60), cx).await;
    c.deliver(event_report(&id, &token, 2, Event::PromptSubmitted), cx);
    assert_eq!(state_of(&c, &id, cx), (ChatState::Running, Source::Exact));
    // The submit delay and the confirm wait are a minute: the send returning at all is the proof.
    let sent = start_send(&c, &id, cx).await.unwrap();
    assert!(sent.submitted);
    assert!(!sent.confirmed);
    cx.run_until_parked();
    assert_eq!(
        written(&c.h, &id, cx),
        vec![bracketed(PROMPT), b"\r".to_vec()]
    );
}

#[gpui::test]
async fn test_a_send_to_a_waiting_chat_gets_no_second_enter(cx: &mut TestAppContext) {
    let (c, id, token) = live_chat(Duration::from_millis(300), cx).await;
    c.deliver(
        event_report(&id, &token, 2, needs_input("permission_prompt")),
        cx,
    );
    assert_eq!(state_of(&c, &id, cx), (ChatState::Waiting, Source::Exact));
    let sent = start_send(&c, &id, cx).await.unwrap();
    assert!(sent.submitted);
    assert!(!sent.confirmed);
    // The paste and its Enter did not end the exact waiting state.
    assert_eq!(state_of(&c, &id, cx), (ChatState::Waiting, Source::Exact));
    cx.run_until_parked();
    assert_eq!(
        written(&c.h, &id, cx),
        vec![bracketed(PROMPT), b"\r".to_vec()]
    );
}

#[gpui::test]
async fn test_a_chat_that_stops_before_its_enter(cx: &mut TestAppContext) {
    let (h, _input_dir, input) = send_harness(cx, &[]).await;
    // Far longer than the test runs: the Enter can only come after the close, and the send must
    // notice the close instead of waiting the delay out.
    set_submit_delay(cx, 600_000);
    let id = h.start(Agent::Claude, cx).await;
    let name = record_of(&h, &id).name;
    wait_indexed(&h, &id, cx).await;
    let terminal = h.runtime.read_with(cx, |runtime, _| {
        runtime.live(&id).unwrap().terminal().clone()
    });
    written(&h, &id, cx);

    let task = cx.update(|cx| send(&id, PROMPT.to_string(), SendOptions { submit: true }, cx));
    // The paste is in the terminal's input log the moment it is written.
    let log = RefCell::new(Vec::new());
    wait_for(cx, "the paste", STARTUP, |cx| {
        log.borrow_mut()
            .extend(terminal.update(cx, |terminal, _| terminal.take_input_log()));
        !log.borrow().is_empty()
    })
    .await;
    assert_eq!(log.take(), vec![bracketed(PROMPT)]);
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    let error = task.await.unwrap_err();
    assert_eq!(error, SendError::NotSubmitted { name: name.clone() });
    assert_eq!(
        error.to_string(),
        format!("The text was pasted into {name} but not submitted: it stopped")
    );
    assert!(record_of(&h, &id).prompted);
    // No Enter was written after the close.
    assert!(
        terminal
            .update(cx, |terminal, _| terminal.take_input_log())
            .is_empty()
    );
    // The fake read some or all of the paste, and nothing after it.
    let input = std::fs::read_to_string(&input).unwrap_or_default();
    assert!(octal(&bracketed(PROMPT)).starts_with(&input), "{input}");
}

#[gpui::test]
async fn test_no_enter_without_submit(cx: &mut TestAppContext) {
    let (h, _input_dir, input) = send_harness(cx, &[]).await;
    let id = h.start(Agent::Codex, cx).await;
    written(&h, &id, cx);

    let sent = send_text(&id, PROMPT, false, cx).await.unwrap();
    assert!(!sent.submitted);
    // Longer than the submit delay: no Enter comes later either.
    cx.run_until_parked();
    assert_eq!(written(&h, &id, cx), vec![bracketed(PROMPT)]);
    let expected = octal(&bracketed(PROMPT));
    wait_for(cx, "the paste to reach the fake", STARTUP, |_| {
        std::fs::read_to_string(&input).unwrap_or_default() == expected
    })
    .await;
}

#[gpui::test]
async fn test_without_bracketed_paste_nothing_is_written(cx: &mut TestAppContext) {
    let (h, _input_dir, input) = send_harness(cx, &[("FAKE_CLI_NO_BRACKETED", "1")]).await;
    h.runtime
        .update(cx, |runtime, _| runtime.set_bracketed_wait(Duration::ZERO));
    let id = h.start(Agent::Claude, cx).await;
    written(&h, &id, cx);

    let error = send_text(&id, PROMPT, true, cx).await.unwrap_err();
    assert_eq!(
        error,
        SendError::NotAcceptingPastes {
            agent: "Claude Code"
        }
    );
    assert_eq!(error.to_string(), "Claude Code is not accepting pastes");
    cx.run_until_parked();
    assert!(written(&h, &id, cx).is_empty());
    assert_eq!(std::fs::read_to_string(&input).unwrap_or_default(), "");
    assert!(!record_of(&h, &id).prompted);
}

#[gpui::test]
async fn test_a_send_to_a_closed_chat_is_refused(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) = send_harness(cx, &[]).await;
    // A chat that never ran.
    let request = h.request(Agent::Claude, "plan");
    let never = request.record.id.clone();
    let name = request.record.name.clone();
    wait_indexed(&h, &never, cx).await;
    let error = send_text(&never, PROMPT, true, cx).await.unwrap_err();
    assert_eq!(error, SendError::NotRunning { name: name.clone() });
    assert_eq!(error.to_string(), format!("{name} is not running"));

    // A chat that ran and was closed.
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    assert!(matches!(
        send_text(&id, PROMPT, true, cx).await,
        Err(SendError::NotRunning { .. })
    ));

    assert_eq!(
        send_text("no-such-chat", PROMPT, true, cx).await,
        Err(SendError::Unknown("no-such-chat".to_string()))
    );
    assert!(!record_of(&h, &never).prompted);
}

#[gpui::test]
async fn test_empty_text_and_chats_off_are_refused(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) = send_harness(cx, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    written(&h, &id, cx);
    assert_eq!(
        send_text(&id, " \n\t\n", true, cx).await,
        Err(SendError::Empty)
    );
    crate::runtime_tests::set_chat_settings(cx, false, 60);
    assert_eq!(send_text(&id, PROMPT, true, cx).await, Err(SendError::Off));
    // Turning chats off ended the chat, so nothing could be written to it.
    assert_eq!(h.state(&id, cx), crate::ChatState::Closed);
}

#[gpui::test]
async fn test_kyle_name_is_never_overwritten(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) = send_harness(cx, &[]).await;
    let mut request = h.request(Agent::Claude, "implement");
    request.record.name = "Kyle's backfill".to_string();
    request.record.named_by = NamedBy::Kyle;
    write_record(&request.record_path, &request.record).unwrap();
    let id = request.record.id.clone();
    h.launch(request, &h.project, cx).await.unwrap();
    h.wait_content(&id, "ready", cx).await;

    // Neither a title after typing nor a send renames it.
    h.type_line(&id, "title Fixing the login redirect", cx);
    wait_for(cx, "the title", STARTUP, |cx| {
        h.runtime.read_with(cx, |runtime, _| {
            runtime.live(&id).unwrap().title() == Some("Fixing the login redirect")
        })
    })
    .await;
    let sent = send_text(&id, PROMPT, true, cx).await.unwrap();
    assert_eq!(sent.named, None);
    cx.run_until_parked();
    let record = record_of(&h, &id);
    assert_eq!(record.name, "Kyle's backfill");
    assert_eq!(record.named_by, NamedBy::Kyle);
    assert!(record.prompted);
}

#[gpui::test]
async fn test_a_title_names_a_chat_prompted_by_typing(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) =
        send_harness(cx, &[("FAKE_CLI_TITLE", "Before any prompt")]).await;
    let id = h.start(Agent::Claude, cx).await;
    let default_name = record_of(&h, &id).name;
    wait_for(cx, "the first title", STARTUP, |cx| {
        h.runtime.read_with(cx, |runtime, _| {
            runtime.live(&id).unwrap().title() == Some("Before any prompt")
        })
    })
    .await;
    // A title before any input does not name the chat, nor does the program's own name.
    cx.run_until_parked();
    assert_eq!(record_of(&h, &id).name, default_name);
    h.type_line(&id, "title ⠋ Claude Code", cx);
    // The title was taken (and judged) once the chat reports it.
    wait_for(cx, "the spinner title", STARTUP, |cx| {
        h.runtime.read_with(cx, |runtime, _| {
            runtime.live(&id).unwrap().title() == Some("⠋ Claude Code")
        })
    })
    .await;
    assert_eq!(record_of(&h, &id).name, default_name);

    h.type_line(&id, "title ✳ Fixing the login redirect", cx);
    wait_for(cx, "the name from the title", STARTUP, |_| {
        record_of(&h, &id).name == "Fixing the login redirect"
    })
    .await;
    assert_eq!(record_of(&h, &id).named_by, NamedBy::Auto);

    // Later titles and a send leave the name.
    h.type_line(&id, "title Writing tests", cx);
    let sent = send_text(&id, PROMPT, true, cx).await.unwrap();
    assert_eq!(sent.named, None);
    wait_for(cx, "the later title", STARTUP, |cx| {
        h.runtime.read_with(cx, |runtime, _| {
            runtime.live(&id).unwrap().title() == Some("Writing tests")
        })
    })
    .await;
    assert_eq!(record_of(&h, &id).name, "Fixing the login redirect");
}

/// A workspace over the test task's directory (on the real file system), with `chat: send
/// region` registered and an editor on `prompts.txt` holding `text`, focused.
async fn prompts_editor<'a>(
    h: &Harness,
    text: &str,
    cx: &'a mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, &'a mut VisualTestContext) {
    let prompts = h.scope.join("prompts.txt");
    std::fs::write(&prompts, text).unwrap();
    cx.update(|cx| {
        editor::init(cx);
    });
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let project = Project::test(fs, [h.scope.as_path()], cx).await;
    let (multi_workspace, cx) = cx.add_window_view({
        let project = project.clone();
        move |window, cx| {
            let multi_workspace = MultiWorkspace::test_new(project, window, cx);
            multi_workspace
                .workspace()
                .update(cx, |workspace, _| crate::register(workspace));
            multi_workspace
        }
    });
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    let buffer = project
        .update(cx, |project, cx| project.open_local_buffer(&prompts, cx))
        .await
        .unwrap();
    let editor = workspace.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor
    });
    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx)
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

fn dispatch(cx: &mut VisualTestContext, name: &str) {
    let action = cx.update(|_, cx| {
        cx.build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"))
    });
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn toast_shown(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<ChatToast>())
    })
}

fn editor_text(editor: &Entity<Editor>, cx: &mut TestAppContext) -> String {
    editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn editor_focused(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).is_focused(window)
    })
}

#[gpui::test]
async fn test_send_region_dispatched_by_name(cx: &mut TestAppContext) {
    let (h, _input_dir, input) = send_harness(cx, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let (workspace, editor, cx) = prompts_editor(&h, PROMPT, cx).await;
    written(&h, &id, cx);

    // Nothing selected: the whole buffer goes.
    dispatch(cx, "chat::SendRegion");
    let mut expected = bracketed(PROMPT);
    expected.push(b'\r');
    let expected = octal(&expected);
    wait_for(cx, "the buffer to reach the fake", STARTUP, |_| {
        std::fs::read_to_string(&input).unwrap_or_default() == expected
    })
    .await;
    assert_eq!(
        written(&h, &id, cx),
        vec![bracketed(PROMPT), b"\r".to_vec()]
    );
    assert_eq!(editor_text(&editor, cx), PROMPT);
    assert!(editor_focused(&editor, cx), "focus stays in the editor");
    assert!(!toast_shown(&workspace, cx));

    // A selection: only it goes.
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(0)..MultiBufferOffset(16)])
        })
    });
    dispatch(cx, "chat::SendRegion");
    let selection = octal(b"\x1b[200~Fix the backfill\x1b[201~\r");
    wait_for(cx, "the selection to be sent", STARTUP, |_| {
        std::fs::read_to_string(&input)
            .unwrap_or_default()
            .ends_with(&selection)
    })
    .await;
    assert_eq!(
        written(&h, &id, cx),
        vec![bracketed("Fix the backfill"), b"\r".to_vec()]
    );
    assert_eq!(editor_text(&editor, cx), PROMPT);
    assert!(editor_focused(&editor, cx));
}

#[gpui::test]
async fn test_send_region_refusals_keep_the_buffer(cx: &mut TestAppContext) {
    let (h, _input_dir, input) = send_harness(cx, &[("FAKE_CLI_NO_BRACKETED", "1")]).await;
    h.runtime
        .update(cx, |runtime, _| runtime.set_bracketed_wait(Duration::ZERO));
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let (workspace, editor, cx) = prompts_editor(&h, PROMPT, cx).await;
    written(&h, &id, cx);

    // Without bracketed paste: refused, nothing written, the buffer as it was.
    dispatch(cx, "chat::SendRegion");
    wait_for(cx, "the refusal", STARTUP, |cx| toast_shown(&workspace, cx)).await;
    assert!(written(&h, &id, cx).is_empty());
    assert_eq!(std::fs::read_to_string(&input).unwrap_or_default(), "");
    assert_eq!(editor_text(&editor, cx), PROMPT);
    assert!(editor_focused(&editor, cx));

    // Closed: "Resume <name> and send?" is asked; declining leaves the buffer as it was.
    workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    dispatch(cx, "chat::SendRegion");
    let asked = |cx: &mut VisualTestContext| {
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.has_active_modal(window, cx)
        })
    };
    for _ in 0..200 {
        if asked(cx) {
            break;
        }
        cx.executor().timer(Duration::from_millis(25)).await;
        cx.run_until_parked();
    }
    assert!(asked(cx), "the resume question");
    assert!(!toast_shown(&workspace, cx));
    dispatch(cx, "menu::Cancel");
    assert!(!asked(cx));
    assert_eq!(editor_text(&editor, cx), PROMPT);
    assert_eq!(std::fs::read_to_string(&input).unwrap_or_default(), "");
    assert_eq!(h.state(&id, cx), crate::ChatState::Closed);
}

#[gpui::test]
async fn test_send_region_on_another_file_is_refused(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) = send_harness(cx, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    // The chat's prompts file is elsewhere, so the scope's `prompts.txt` is not its file.
    let mut record = record_of(&h, &id);
    record.prompts_file = Some(PathBuf::from("notes/other-prompts.txt"));
    write_record(&record_path(&h.scope, &id), &record).unwrap();
    wait_for(cx, "the index to see the prompts file", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| {
            store
                .find(&id)
                .and_then(|chat| chat.entry.record()?.prompts_file.clone())
                .is_some()
        })
    })
    .await;
    let (workspace, editor, cx) = prompts_editor(&h, PROMPT, cx).await;
    written(&h, &id, cx);
    dispatch(cx, "chat::SendRegion");
    assert!(toast_shown(&workspace, cx));
    assert!(written(&h, &id, cx).is_empty());
    assert_eq!(editor_text(&editor, cx), PROMPT);
    assert_eq!(
        cx.update(|_, cx| crate::chat_for_prompts_file(
            &h.scope.join("notes/other-prompts.txt"),
            cx
        )),
        Some(id)
    );
}

#[gpui::test]
async fn test_set_prompts_file_stores_none_for_the_scope_file(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) = send_harness(cx, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let project_file = h.root.join("projects/invoicing/prompts.txt");

    // Another scope's file: the absolute path.
    let set = cx.update(|cx| crate::set_prompts_file(&id, Some(project_file.clone()), cx));
    set.await.unwrap();
    assert_eq!(record_of(&h, &id).prompts_file, Some(project_file.clone()));

    // The scope's own file: nothing, as for a new chat.
    let set = cx.update(|cx| crate::set_prompts_file(&id, Some(h.scope.join("prompts.txt")), cx));
    set.await.unwrap();
    assert_eq!(record_of(&h, &id).prompts_file, None);
    let set = cx.update(|cx| crate::set_prompts_file(&id, Some(project_file.clone()), cx));
    set.await.unwrap();
    let set = cx.update(|cx| crate::set_prompts_file(&id, None, cx));
    set.await.unwrap();
    assert_eq!(record_of(&h, &id).prompts_file, None);

    // The auto-submit override, and back to the default.
    let set = cx.update(|cx| crate::set_auto_submit(&id, Some(false), cx));
    set.await.unwrap();
    assert_eq!(record_of(&h, &id).auto_submit, Some(false));
    let set = cx.update(|cx| crate::set_auto_submit(&id, None, cx));
    set.await.unwrap();
    assert_eq!(record_of(&h, &id).auto_submit, None);

    // An unknown chat is refused.
    let set = cx.update(|cx| crate::set_prompts_file("nope", None, cx));
    assert!(set.await.is_err());
}

#[gpui::test]
async fn test_chats_for_prompts_file_lists_running_first(cx: &mut TestAppContext) {
    let (h, _input_dir, _input) = send_harness(cx, &[]).await;
    let closed = h.start(Agent::Claude, cx).await;
    assert!(
        h.runtime
            .update(cx, |runtime, cx| runtime.close(&closed, cx))
    );
    let running = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &closed, cx).await;
    wait_indexed(&h, &running, cx).await;
    let path = h.scope.join("prompts.txt");
    let chats = cx.update(|cx| crate::chats_for_prompts_file(&path, cx));
    let ids: Vec<(&str, bool)> = chats
        .iter()
        .map(|chat| (chat.id.as_str(), chat.running))
        .collect();
    assert_eq!(ids, [(running.as_str(), true), (closed.as_str(), false)]);
    assert_eq!(chats[0].name, record_of(&h, &running).name);
    assert_eq!(
        cx.update(|cx| crate::chat_for_prompts_file(&path, cx)),
        Some(running)
    );
    let other = h.root.join("projects/invoicing/prompts.txt");
    assert!(
        cx.update(|cx| crate::chats_for_prompts_file(&other, cx))
            .is_empty()
    );
}
