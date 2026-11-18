use std::cell::RefCell;
use std::rc::Rc;

use bongos_emacs_model::kill_ring::{Entry, EntryKind};
use editor::kill_ring;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{KeyBinding, TestAppContext};

use crate::actions::{
    CopyRectangleAsKill, DeleteRectangle, KillRectangle, Yank, YankPop, YankRectangle,
};
use crate::apply_tests::init_test;
use crate::browse_kill_ring_tests::{caret, mark, open, session, text, workspace_editor};
use crate::rectangle_tests::{echo, history, mark_offset, set_rectangle};

/// Two rows, the rectangle between columns 1 and 3.
const SQUARE: &str = "abcd\nefgˇh";

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    bind_keys(&mut cx);
    cx
}

fn bind_keys(cx: &mut EditorTestContext) {
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-y", Yank::default(), Some("Editor")),
            KeyBinding::new("alt-y", YankPop::default(), Some("Editor")),
            KeyBinding::new("ctrl-/", editor::actions::Undo, Some("Editor")),
            KeyBinding::new("alt-1", CopyRectangleAsKill, Some("Editor")),
            KeyBinding::new("alt-2", KillRectangle, Some("Editor")),
            KeyBinding::new("alt-3", YankRectangle, Some("Editor")),
            KeyBinding::new("alt-4", DeleteRectangle, Some("Editor")),
        ])
    });
}

/// The ring's entries, newest first, as (kind, text).
fn ring(cx: &mut EditorTestContext) -> Vec<(EntryKind, String)> {
    cx.update(|_, cx| {
        kill_ring::read(cx).map_or(Vec::new(), |ring| {
            (0..ring.len())
                .filter_map(|ix| ring.get(ix).map(|entry| (entry.kind, entry.text.clone())))
                .collect()
        })
    })
}

fn rows(rows: &[&str]) -> Entry {
    Entry::rectangle(&rows.iter().map(|row| row.to_string()).collect::<Vec<_>>())
}

/// Pushes `entries`, oldest first.
fn seed(cx: &mut EditorTestContext, entries: Vec<Entry>) {
    cx.update(|_, cx| {
        for entry in entries {
            kill_ring::update(cx, |ring| ring.push(entry));
        }
    });
}

#[gpui::test]
async fn copy_leaves_buffer_point_and_mark_and_never_writes_the_clipboard(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    set_rectangle(&mut cx, SQUARE, 1);
    let before = history(&mut cx);
    cx.simulate_keystrokes("alt-1");
    cx.assert_editor_state(SQUARE);
    assert!(history(&mut cx) == before);
    assert_eq!(mark_offset(&mut cx), Some(1));
    assert_eq!(
        ring(&mut cx),
        [(EntryKind::Rectangle, "bc\nfg".to_string())]
    );
    assert!(cx.update(|_, cx| cx.read_from_clipboard()).is_none());

    // A read-only buffer can still be copied from.
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    cx.simulate_keystrokes("alt-1");
    cx.assert_editor_state(SQUARE);
    assert_eq!(ring(&mut cx).len(), 2);
}

#[gpui::test]
async fn one_undo_restores_a_killed_rectangle(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    set_rectangle(&mut cx, SQUARE, 1);
    cx.simulate_keystrokes("alt-2");
    cx.assert_editor_state("aˇd\neh");
    cx.simulate_keystrokes("ctrl-/");
    assert_eq!(cx.buffer_text(), "abcd\nefgh");
}

#[gpui::test]
async fn kill_stores_a_rectangle_before_the_text_goes_and_delete_stores_none(
    cx: &mut TestAppContext,
) {
    let mut cx = test_context(cx).await;
    set_rectangle(&mut cx, SQUARE, 1);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let multibuffer = cx.update_editor(|editor, _, _| editor.buffer().clone());
    cx.update(|_, cx| {
        let seen = seen.clone();
        cx.subscribe(&multibuffer, move |_, event, cx| {
            if matches!(event, multi_buffer::Event::Edited { .. }) {
                let newest = kill_ring::read(cx)
                    .and_then(|ring| ring.newest())
                    .map(|entry| (entry.kind, entry.text.clone()));
                seen.borrow_mut().push(newest);
            }
        })
        .detach();
    });
    cx.simulate_keystrokes("alt-2");
    assert_eq!(
        seen.borrow().first().cloned().flatten(),
        Some((EntryKind::Rectangle, "bc\nfg".to_string()))
    );

    set_rectangle(&mut cx, SQUARE, 1);
    cx.simulate_keystrokes("alt-4");
    cx.assert_editor_state("aˇd\neh");
    assert_eq!(ring(&mut cx).len(), 1);
}

#[gpui::test]
async fn yanks_of_a_kill_in_another_buffer_insert_row_for_row(cx: &mut TestAppContext) {
    let mut first = test_context(cx).await;
    set_rectangle(&mut first, SQUARE, 1);
    first.simulate_keystrokes("alt-2");

    let mut other = EditorTestContext::new(cx).await;
    other.set_state("1ˇ23\n");
    other.simulate_keystrokes("ctrl-y");
    // The short row is padded out to the column.
    other.assert_editor_state("1bc23\n fgˇ");
    assert_eq!(mark_offset(&mut other), Some(1));

    other.set_state("1ˇ23\n");
    other.simulate_keystrokes("alt-3");
    other.assert_editor_state("1bc23\n fgˇ");
}

#[gpui::test]
async fn a_yank_at_the_end_adds_lines_that_one_undo_removes(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    seed(&mut cx, vec![rows(&["bc", "fg", "jk"])]);
    cx.set_state("xyˇ");
    cx.simulate_keystrokes("alt-3");
    cx.assert_editor_state("xybc\n  fg\n  jkˇ");
    cx.simulate_keystrokes("ctrl-/");
    assert_eq!(cx.buffer_text(), "xy");
}

#[gpui::test]
async fn an_all_empty_rectangle_inserts_nothing_and_leaves_point(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    // A zero-width copy still stores its empty rows.
    let marked = "abcd\nefˇgh";
    set_rectangle(&mut cx, marked, 2);
    cx.simulate_keystrokes("alt-1");
    assert_eq!(ring(&mut cx), [(EntryKind::Rectangle, "\n".to_string())]);

    let before = history(&mut cx);
    cx.simulate_keystrokes("alt-3");
    cx.assert_editor_state(marked);
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state(marked);
    assert!(history(&mut cx) == before);
}

#[gpui::test]
async fn yank_pop_from_a_rectangle_to_text_and_back_leaves_only_the_new_text(
    cx: &mut TestAppContext,
) {
    let mut cx = test_context(cx).await;
    seed(&mut cx, vec![Entry::plain("T".into()), rows(&["bc", "fg"])]);
    cx.set_state("1ˇ23\n456");
    cx.simulate_keystrokes("ctrl-y");
    cx.assert_editor_state("1bc23\n4fgˇ56");
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("1Tˇ23\n456");
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("1bc23\n4fgˇ56");
}

#[gpui::test]
async fn after_a_yank_and_a_yank_pop_one_undo_removes_all_yanked_text(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    seed(&mut cx, vec![rows(&["bc", "fg"]), Entry::plain("T".into())]);
    cx.set_state("1ˇ23\n456");
    cx.simulate_keystrokes("ctrl-y alt-y");
    cx.assert_editor_state("1bc23\n4fgˇ56");
    cx.simulate_keystrokes("ctrl-/");
    assert_eq!(cx.buffer_text(), "123\n456");
}

#[gpui::test]
async fn browse_kill_ring_yanks_a_rectangle_row_as_a_rectangle(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("123\n456", 1, cx).await;
    let cx = &mut cx;
    cx.update(|_, cx| {
        kill_ring::update(cx, |ring| {
            ring.push(Entry::plain("T".into()));
            ring.push(rows(&["bc", "fg"]));
        })
    });
    let kind = cx.update(|_, cx| {
        crate::browse_kill_ring::candidates("", cx)
            .first()
            .and_then(|candidate| candidate.kind.clone())
            .map(|kind| kind.to_string())
    });
    assert_eq!(kind.as_deref(), Some("rectangle"));

    open(&workspace, cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(session(&workspace, cx).is_none());
    assert_eq!(text(&editor, cx), "1bc23\n4fg56");
    assert_eq!(caret(&editor, cx), 9);
    assert_eq!(mark(&editor, cx), Some(1));
}

#[gpui::test]
async fn a_masked_editor_never_stores_a_rectangle(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    // A masked editor is a one-line secret field.
    cx.update_editor(|editor, _, cx| editor.set_masked(true, cx));
    set_rectangle(&mut cx, "abcˇd", 1);
    cx.simulate_keystrokes("alt-1 alt-2");
    assert_eq!(cx.buffer_text(), "abcd");
    assert!(ring(&mut cx).is_empty());
}

#[gpui::test]
async fn without_a_rectangle_or_its_undo_step_nothing_is_yanked(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let messages = echo(&mut cx);
    seed(&mut cx, vec![Entry::plain("T".into())]);
    cx.set_state("1ˇ23\n456");
    cx.simulate_keystrokes("alt-3");
    cx.assert_editor_state("1ˇ23\n456");

    seed(&mut cx, vec![rows(&["bc", "fg"])]);
    cx.simulate_keystrokes("ctrl-y");
    // The yank's undo step leaves the history, as when it is forgotten.
    cx.update_editor(|editor, _, cx| {
        let buffer = editor.buffer().clone();
        let transaction = buffer.read(cx).as_singleton().and_then(|buffer| {
            buffer
                .read(cx)
                .peek_undo_stack()
                .map(|entry| entry.transaction_id())
        });
        if let Some(transaction) = transaction {
            buffer.update(cx, |buffer, cx| buffer.forget_transaction(transaction, cx));
        }
    });
    cx.simulate_keystrokes("alt-y");
    cx.assert_editor_state("1bc23\n4fgˇ56");
    assert_eq!(
        *messages.borrow(),
        ["No rectangle to yank", "Previous command was not a yank"]
    );
}
