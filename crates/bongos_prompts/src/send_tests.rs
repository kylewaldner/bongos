//! Tests of `prompts: send`, `send without marking` and `send region` (D14, D16): the mark and
//! the move to the next block (a new one after the last), a marker inserted above a block without
//! one, the confirmation before a re-send, the empty block, every error leaving the buffer
//! byte-identical, a block edited between the send and its result, the save and its conflict, and
//! the refusals while off. Every test compares the body rows before and after byte for byte.

use std::path::Path;

use bongos_chat::{SendError, SendOptions};
use bongos_prompts_model::{MarkerSyntax, parse, split_rows};
use editor::Editor;
use gpui::{Entity, TestAppContext, VisualTestContext};
use language::Point;
use workspace::Workspace;

use crate::actions::OFF_MESSAGE;
use crate::attach_tests::{
    PATH, THREE, answer, chat, has_modal, head, put, select, sent, set_chats_enabled, setup, text,
    toasts, update_prompts_settings,
};
use crate::mode_tests::dispatch;
use crate::send::{CHANGED_MESSAGE, CONFLICT_MESSAGE, EMPTY_MESSAGE};

const T1: &str = "2026-11-05T10:00:00Z";

/// The rows of every body, in order.
fn bodies(text: &str) -> Vec<Vec<String>> {
    let rows = split_rows(text);
    parse(text, &MarkerSyntax::default())
        .iter()
        .map(|block| {
            rows[block.body.start as usize..block.body.end as usize]
                .iter()
                .map(|row| row.to_string())
                .collect()
        })
        .collect()
}

fn row(text: &str, row: usize) -> String {
    split_rows(text)[row].to_string()
}

/// Whether `line` is the marker created `created` and stamped sent.
fn is_sent_marker(line: &str, created: &str) -> bool {
    let marker = MarkerSyntax::default().parse(line).expect("a marker row");
    marker.sent.is_some() && marker.created.format("%Y-%m-%d %H:%M").to_string() == created
}

/// Whether the texts differ only in row `changed`.
fn differ_only_in(before: &str, after: &str, changed: usize) {
    let before = split_rows(before);
    let after = split_rows(after);
    assert_eq!(before.len(), after.len());
    for (ix, (old, new)) in before.iter().zip(&after).enumerate() {
        if ix != changed {
            assert_eq!(old, new, "row {ix}");
        }
    }
}

async fn running(
    cx: &mut TestAppContext,
    row: u32,
) -> (
    Entity<Workspace>,
    Entity<Editor>,
    std::rc::Rc<crate::attach_tests::FakeChats>,
    &mut VisualTestContext,
) {
    let (workspace, editor, fake, cx) = setup(cx, true, row).await;
    fake.chats.replace(vec![chat("a", true, T1)]);
    (workspace, editor, fake, cx)
}

async fn disk(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    fs.load(Path::new(PATH)).await.unwrap()
}

#[gpui::test]
async fn send_marks_a_pending_block_and_moves_to_the_next(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = running(cx, 5).await;
    dispatch("prompts::Send", cx);
    assert_eq!(
        *fake.sends.borrow(),
        vec![(
            "a".to_string(),
            "second\n  more".to_string(),
            SendOptions { submit: true }
        )]
    );
    let after = text(&editor, cx);
    differ_only_in(THREE, &after, 3);
    assert!(is_sent_marker(&row(&after, 3), "2026-11-05 14:10"));
    assert_eq!(bodies(&after), bodies(THREE));
    assert_eq!(head(&editor, cx), Point::new(7, 0));
    assert!(toasts().is_empty());
    // Saved, since `prompts.save_on_send` is on.
    cx.run_until_parked();
    assert_eq!(disk(&workspace, cx).await, after);
}

#[gpui::test]
async fn send_on_the_last_block_creates_a_new_one(cx: &mut TestAppContext) {
    let (_workspace, editor, _fake, cx) = running(cx, 7).await;
    dispatch("prompts::Send", cx);
    let after = text(&editor, cx);
    let rows = split_rows(&after);
    assert_eq!(rows.len(), 10);
    assert!(is_sent_marker(rows[6], "2026-11-05 14:30"));
    assert_eq!(rows[7], "third");
    let marker = MarkerSyntax::default().parse(rows[8]).unwrap();
    assert!(!marker.is_sent());
    assert_eq!(rows[9], "");
    let new = bodies(&after);
    assert_eq!(new[..4], bodies(THREE)[..]);
    assert_eq!(new[4], vec![String::new()]);
    assert_eq!(head(&editor, cx), Point::new(9, 0));
}

#[gpui::test]
async fn a_block_without_a_marker_gets_one_above(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 0).await;
    dispatch("prompts::Send", cx);
    assert_eq!(fake.sends.borrow()[0].1, "preamble");
    let after = text(&editor, cx);
    let rows = split_rows(&after);
    let marker = MarkerSyntax::default().parse(rows[0]).unwrap();
    assert_eq!(marker.sent, Some(marker.created));
    assert_eq!(rows[1..].join("\n"), THREE);
    assert_eq!(bodies(&after), bodies(THREE));
    // The next block's first body row.
    assert_eq!(head(&editor, cx), Point::new(3, 0));
}

#[gpui::test]
async fn a_sent_block_asks_first(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = running(cx, 2).await;
    dispatch("prompts::Send", cx);
    assert!(has_modal(&workspace, cx));
    assert!(fake.sends.borrow().is_empty());
    answer(&workspace, "Cancel", cx).await;
    assert!(fake.sends.borrow().is_empty());
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(head(&editor, cx), Point::new(2, 0));

    dispatch("prompts::Send", cx);
    answer(&workspace, "Send again", cx).await;
    assert_eq!(fake.sends.borrow()[0].1, "first, sent");
    let after = text(&editor, cx);
    differ_only_in(THREE, &after, 1);
    assert!(is_sent_marker(&row(&after, 1), "2026-11-05 13:40"));
    assert_ne!(row(&after, 1), row(THREE, 1), "the sent stamp is new");
    assert_eq!(bodies(&after), bodies(THREE));
    assert_eq!(head(&editor, cx), Point::new(4, 0));
}

#[gpui::test]
async fn an_empty_block_sends_nothing(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 7).await;
    editor.update(cx, |editor, cx| {
        let end = editor.buffer().read(cx).snapshot(cx).max_point();
        editor.edit([(end..end, "\n=== prompt 2026-11-05 15:00 ===\n  ")], cx)
    });
    put(&editor, Point::new(9, 0), cx);
    let before = text(&editor, cx);
    toasts();
    for action in [
        "prompts::Send",
        "prompts::SendWithoutMarking",
        "prompts::SendRegion",
    ] {
        dispatch(action, cx);
        assert_eq!(toasts(), vec![EMPTY_MESSAGE.to_string()], "{action}");
    }
    assert!(fake.sends.borrow().is_empty());
    assert_eq!(text(&editor, cx), before);
}

#[gpui::test]
async fn send_without_marking_changes_nothing(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = running(cx, 4).await;
    dispatch("prompts::SendWithoutMarking", cx);
    assert_eq!(fake.sends.borrow()[0].1, "second\n  more");
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(head(&editor, cx), Point::new(4, 0));
    // Nor a sent block: no question, no mark.
    put(&editor, Point::new(2, 0), cx);
    dispatch("prompts::SendWithoutMarking", cx);
    assert!(!has_modal(&workspace, cx));
    assert_eq!(fake.sends.borrow()[1].1, "first, sent");
    assert_eq!(text(&editor, cx), THREE);
    assert!(toasts().is_empty());
}

#[gpui::test]
async fn send_region_sends_the_selection_exactly(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 4).await;
    select(&editor, Point::new(4, 3), Point::new(5, 4), cx);
    dispatch("prompts::SendRegion", cx);
    assert_eq!(fake.sends.borrow()[0].1, "ond\n  mo");
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(head(&editor, cx), Point::new(5, 4));
    // Not trimmed.
    select(&editor, Point::new(5, 0), Point::new(5, 6), cx);
    dispatch("prompts::SendRegion", cx);
    assert_eq!(fake.sends.borrow()[1].1, "  more");
    assert_eq!(text(&editor, cx), THREE);

    // Without a selection it is `prompts: send`.
    put(&editor, Point::new(4, 0), cx);
    dispatch("prompts::SendRegion", cx);
    assert_eq!(fake.sends.borrow()[2].1, "second\n  more");
    assert!(is_sent_marker(
        &row(&text(&editor, cx), 3),
        "2026-11-05 14:10"
    ));
}

#[gpui::test]
async fn errors_leave_the_buffer_as_it_was(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 4).await;
    for error in [
        SendError::Off,
        SendError::Empty,
        SendError::Unknown("a".into()),
        SendError::NotAcceptingPastes {
            agent: "Claude Code",
        },
    ] {
        *fake.reply.borrow_mut() = Err(error.clone());
        dispatch("prompts::Send", cx);
        assert_eq!(toasts(), vec![error.to_string()]);
        assert_eq!(text(&editor, cx), THREE);
        assert_eq!(head(&editor, cx), Point::new(4, 0));
    }
    assert_eq!(fake.sends.borrow().len(), 4);
}

#[gpui::test]
async fn not_running_offers_a_resume_and_marks_only_when_it_sent(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 4).await;
    *fake.reply.borrow_mut() = Err(SendError::NotRunning {
        name: "chat a".into(),
    });
    dispatch("prompts::Send", cx);
    assert_eq!(fake.offers.borrow().len(), 1);
    assert_eq!(fake.offers.borrow()[0].name, "chat a");
    assert_eq!(text(&editor, cx), THREE);

    // Cancel: the offer never calls back.
    fake.offers.borrow_mut().clear();
    assert_eq!(text(&editor, cx), THREE);

    // Yes: the offer sent; the block is marked.
    dispatch("prompts::Send", cx);
    let on_sent = fake.offers.borrow_mut()[0].on_sent.take().unwrap();
    cx.update(|window, cx| on_sent(Ok(sent()), window, cx));
    cx.run_until_parked();
    let after = text(&editor, cx);
    differ_only_in(THREE, &after, 3);
    assert!(is_sent_marker(&row(&after, 3), "2026-11-05 14:10"));

    // A resume whose send failed marks nothing.
    put(&editor, Point::new(7, 0), cx);
    dispatch("prompts::Send", cx);
    let on_sent = fake.offers.borrow_mut()[1].on_sent.take().unwrap();
    cx.update(|window, cx| on_sent(Err(SendError::Off), window, cx));
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), after);
}

#[gpui::test]
async fn not_submitted_still_marks(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 4).await;
    let error = SendError::NotSubmitted {
        name: "chat a".into(),
    };
    *fake.reply.borrow_mut() = Err(error.clone());
    dispatch("prompts::Send", cx);
    assert_eq!(toasts(), vec![error.to_string()]);
    assert!(is_sent_marker(
        &row(&text(&editor, cx), 3),
        "2026-11-05 14:10"
    ));
}

#[gpui::test]
async fn not_submitted_is_still_said_when_a_conflict_follows(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = running(cx, 4).await;
    editor.update(cx, |editor, cx| {
        editor.edit([(Point::new(7, 5)..Point::new(7, 5), "!")], cx)
    });
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    fs.as_fake()
        .insert_file(PATH, b"changed elsewhere".to_vec())
        .await;
    cx.run_until_parked();
    let error = SendError::NotSubmitted {
        name: "chat a".into(),
    };
    *fake.reply.borrow_mut() = Err(error.clone());
    toasts();
    dispatch("prompts::Send", cx);
    cx.run_until_parked();
    // The toast that replaces the error's (they share one id) repeats it.
    assert_eq!(
        toasts(),
        vec![error.to_string(), format!("{error}. {CONFLICT_MESSAGE}"),]
    );
}

#[gpui::test]
async fn edits_between_send_and_result_keep_the_mark_in_place(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 4).await;
    fake.hold.set(true);
    dispatch("prompts::Send", cx);
    assert_eq!(fake.sends.borrow().len(), 1);
    // Kyle adds a row above and types at the end of the block meanwhile.
    editor.update(cx, |editor, cx| {
        editor.edit([(Point::new(0, 0)..Point::new(0, 0), "added\n")], cx);
        editor.edit([(Point::new(6, 6)..Point::new(6, 6), " typed")], cx);
    });
    put(&editor, Point::new(6, 12), cx);
    let before = text(&editor, cx);
    assert_eq!(row(&before, 6), "  more typed");
    fake.held.borrow_mut().remove(0).send(Ok(sent())).unwrap();
    cx.run_until_parked();
    let after = text(&editor, cx);
    differ_only_in(&before, &after, 4);
    assert!(is_sent_marker(&row(&after, 4), "2026-11-05 14:10"));
    assert_eq!(bodies(&after), bodies(&before));
    // Point was still in the block: it moves on.
    assert_eq!(head(&editor, cx), Point::new(8, 0));
}

#[gpui::test]
async fn point_that_left_the_block_stays(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 4).await;
    fake.hold.set(true);
    dispatch("prompts::Send", cx);
    put(&editor, Point::new(0, 3), cx);
    fake.held.borrow_mut().remove(0).send(Ok(sent())).unwrap();
    cx.run_until_parked();
    assert!(is_sent_marker(
        &row(&text(&editor, cx), 3),
        "2026-11-05 14:10"
    ));
    assert_eq!(head(&editor, cx), Point::new(0, 3));
}

#[gpui::test]
async fn a_marker_changed_meanwhile_is_not_marked(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 4).await;
    fake.hold.set(true);
    dispatch("prompts::Send", cx);
    editor.update(cx, |editor, cx| {
        editor.edit([(Point::new(3, 26)..Point::new(3, 27), "1")], cx)
    });
    let before = text(&editor, cx);
    assert_eq!(row(&before, 3), "=== prompt 2026-11-05 14:11 ===");
    toasts();
    fake.held.borrow_mut().remove(0).send(Ok(sent())).unwrap();
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), before);
    assert_eq!(toasts(), vec![CHANGED_MESSAGE.to_string()]);
}

#[gpui::test]
async fn a_conflict_with_disk_skips_the_save(cx: &mut TestAppContext) {
    let (workspace, editor, _fake, cx) = running(cx, 4).await;
    // Unsaved typing, then the file changes on disk.
    editor.update(cx, |editor, cx| {
        editor.edit([(Point::new(7, 5)..Point::new(7, 5), "!")], cx)
    });
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    fs.as_fake()
        .insert_file(PATH, b"changed elsewhere".to_vec())
        .await;
    cx.run_until_parked();
    let conflict = editor.read_with(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.read(cx).has_conflict()
    });
    assert!(conflict);
    toasts();
    dispatch("prompts::Send", cx);
    cx.run_until_parked();
    assert!(is_sent_marker(
        &row(&text(&editor, cx), 3),
        "2026-11-05 14:10"
    ));
    assert_eq!(disk(&workspace, cx).await, "changed elsewhere");
    assert_eq!(toasts(), vec![CONFLICT_MESSAGE.to_string()]);
}

#[gpui::test]
async fn save_on_send_off_leaves_the_file_unsaved(cx: &mut TestAppContext) {
    let (workspace, editor, _fake, cx) = running(cx, 4).await;
    update_prompts_settings(cx, |prompts| prompts.save_on_send = Some(false));
    dispatch("prompts::Send", cx);
    cx.run_until_parked();
    assert!(is_sent_marker(
        &row(&text(&editor, cx), 3),
        "2026-11-05 14:10"
    ));
    assert_eq!(disk(&workspace, cx).await, THREE);
    assert!(editor.read_with(cx, |editor, cx| editor.buffer().read(cx).is_dirty(cx)));
}

#[gpui::test]
async fn sends_are_refused_while_off(cx: &mut TestAppContext) {
    // `prompts.enabled` off: no mode, no send, only the off toast.
    let (workspace, editor, fake, cx) = setup(cx, false, 4).await;
    fake.chats.replace(vec![chat("a", true, T1)]);
    for action in [
        "prompts::Send",
        "prompts::SendWithoutMarking",
        "prompts::SendRegion",
        "prompts::ToggleAutoSubmit",
    ] {
        dispatch(action, cx);
        assert_eq!(toasts(), vec![OFF_MESSAGE.to_string()], "{action}");
    }
    assert!(!has_modal(&workspace, cx));
    assert!(fake.sends.borrow().is_empty());
    assert!(fake.writes.borrow().is_empty());
    assert!(!crate::mode_tests::attached(&editor, cx));
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn sends_are_refused_while_chats_are_off(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = running(cx, 4).await;
    set_chats_enabled(cx, false);
    dispatch("prompts::Send", cx);
    assert_eq!(toasts(), vec![SendError::Off.to_string()]);
    assert!(fake.sends.borrow().is_empty());
    assert_eq!(text(&editor, cx), THREE);
}
