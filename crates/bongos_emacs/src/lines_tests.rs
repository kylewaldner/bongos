use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use editor::Editor;
use editor::actions::Undo;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{Action as _, Focusable as _, TestAppContext, VisualTestContext};

use crate::actions::{CopyFromAbove, DuplicateLine, MoveLineDown, MoveLineUp};
use crate::apply_tests::{capture_kills, init_test};

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, _| buffer.set_group_interval(Duration::ZERO));
    cx
}

#[gpui::test]
async fn move_line_down_and_up_move_the_line_by_the_count(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("oneˇ\ntwo\nthree\nfour");
    cx.dispatch_action(MoveLineDown { count: None });
    cx.assert_editor_state("two\noneˇ\nthree\nfour");

    cx.set_state("oneˇ\ntwo\nthree\nfour");
    cx.dispatch_action(MoveLineDown { count: Some(3) });
    cx.assert_editor_state("two\nthree\nfour\noneˇ");

    cx.set_state("one\ntwo\nthree\nfoˇur");
    cx.dispatch_action(MoveLineUp { count: Some(2) });
    cx.assert_editor_state("one\nfoˇur\ntwo\nthree");

    // A negative count goes the other way; zero does nothing.
    cx.set_state("oneˇ\ntwo\nthree");
    cx.dispatch_action(MoveLineUp { count: Some(-1) });
    cx.assert_editor_state("two\noneˇ\nthree");
    cx.dispatch_action(MoveLineDown { count: Some(-1) });
    cx.assert_editor_state("oneˇ\ntwo\nthree");
    cx.dispatch_action(MoveLineDown { count: Some(0) });
    cx.assert_editor_state("oneˇ\ntwo\nthree");
}

#[gpui::test]
async fn move_line_stops_at_the_edge_of_the_text(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("oneˇ\ntwo\nthree");
    cx.dispatch_action(MoveLineDown {
        count: Some(1_000_000),
    });
    cx.assert_editor_state("two\nthree\noneˇ");

    cx.dispatch_action(MoveLineUp {
        count: Some(1_000_000),
    });
    cx.assert_editor_state("oneˇ\ntwo\nthree");

    cx.dispatch_action(MoveLineUp { count: None });
    cx.assert_editor_state("oneˇ\ntwo\nthree");
}

#[gpui::test]
async fn a_three_line_region_moves_as_a_block(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("a\n«b\nc\ndˇ»\ne\nf\ng");
    cx.dispatch_action(MoveLineDown { count: Some(2) });
    cx.assert_editor_state("a\ne\nf\n«b\nc\ndˇ»\ng");

    cx.dispatch_action(MoveLineUp { count: Some(3) });
    cx.assert_editor_state("«b\nc\ndˇ»\na\ne\nf\ng");
}

#[gpui::test]
async fn n_moves_are_one_undo_step(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("oneˇ\ntwo\nthree\nfour");
    cx.dispatch_action(MoveLineDown { count: Some(3) });
    assert_eq!(cx.buffer_text(), "two\nthree\nfour\none");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "one\ntwo\nthree\nfour");

    // Two commands in a row are two steps.
    cx.set_state("oneˇ\ntwo\nthree\nfour");
    cx.dispatch_action(MoveLineDown { count: Some(2) });
    cx.dispatch_action(MoveLineUp { count: None });
    assert_eq!(cx.buffer_text(), "two\none\nthree\nfour");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "two\nthree\none\nfour");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "one\ntwo\nthree\nfour");
}

#[gpui::test]
async fn move_line_in_a_read_only_editor_changes_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("oneˇ\ntwo");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    cx.dispatch_action(MoveLineDown { count: Some(2) });
    cx.assert_editor_state("oneˇ\ntwo");
}

#[gpui::test]
async fn duplicate_line_makes_n_copies_below(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(DuplicateLine { count: None });
    cx.assert_editor_state("one\ntwo\ntwˇo\nthree");

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(DuplicateLine { count: Some(3) });
    cx.assert_editor_state("one\ntwo\ntwˇo\ntwo\ntwo\nthree");

    // The last line has no newline to copy.
    cx.set_state("one\ntˇwo");
    cx.dispatch_action(DuplicateLine { count: Some(2) });
    cx.assert_editor_state("one\ntwo\ntˇwo\ntwo");

    // Zero and negative counts do nothing.
    cx.set_state("one\ntˇwo");
    cx.dispatch_action(DuplicateLine { count: Some(0) });
    cx.dispatch_action(DuplicateLine { count: Some(-2) });
    cx.assert_editor_state("one\ntˇwo");

    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn duplicate_line_copies_a_region_once_after_itself(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("one «twoˇ» three");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(DuplicateLine { count: Some(3) });
    cx.assert_editor_state("one twotwoˇ three");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));

    // A multi-line region is copied as it is.
    cx.set_state("a«b\ncˇ»d");
    cx.dispatch_action(DuplicateLine { count: None });
    cx.assert_editor_state("ab\ncb\ncˇd");
}

#[gpui::test]
async fn duplicate_line_is_one_undo_step_at_two_carets(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("aˇ\nb\ncˇ");
    cx.dispatch_action(DuplicateLine { count: Some(2) });
    cx.assert_editor_state("a\naˇ\na\nb\nc\ncˇ\nc");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "a\nb\nc");
}

#[gpui::test]
async fn copy_from_above_copies_n_characters_or_to_the_line_end(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("hello world\nˇ");
    cx.dispatch_action(CopyFromAbove { count: Some(5) });
    cx.assert_editor_state("hello world\nhelloˇ");

    cx.set_state("hello world\nˇ");
    cx.dispatch_action(CopyFromAbove { count: None });
    cx.assert_editor_state("hello world\nhello worldˇ");

    // Copying starts at the caret's column.
    cx.set_state("hello world\nabcˇ");
    cx.dispatch_action(CopyFromAbove { count: Some(2) });
    cx.assert_editor_state("hello world\nabcloˇ");

    cx.set_state("hello world\nabcˇ");
    cx.dispatch_action(CopyFromAbove { count: None });
    cx.assert_editor_state("hello world\nabclo worldˇ");

    // Blank lines are skipped.
    cx.set_state("hello\n  \n\nˇ");
    cx.dispatch_action(CopyFromAbove { count: Some(3) });
    cx.assert_editor_state("hello\n  \n\nhelˇ");

    // Nothing above, a column past the end of the line above, a zero count, a region.
    cx.set_state("ˇhello");
    cx.dispatch_action(CopyFromAbove { count: None });
    cx.assert_editor_state("ˇhello");
    cx.set_state("ab\nabcdˇ");
    cx.dispatch_action(CopyFromAbove { count: None });
    cx.assert_editor_state("ab\nabcdˇ");
    cx.set_state("hello\nˇ");
    cx.dispatch_action(CopyFromAbove { count: Some(0) });
    cx.assert_editor_state("hello\nˇ");
    cx.set_state("hello\n«abˇ»");
    cx.dispatch_action(CopyFromAbove { count: None });
    cx.assert_editor_state("hello\n«abˇ»");

    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn copy_from_above_is_one_undo_step_at_two_carets(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("abc\nˇ\ndef\nˇ");
    cx.dispatch_action(CopyFromAbove { count: Some(2) });
    cx.assert_editor_state("abc\nabˇ\ndef\ndeˇ");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "abc\n\ndef\n");
}

#[gpui::test]
async fn read_only_editor_duplicates_and_copies_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("oneˇ\n");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    cx.dispatch_action(DuplicateLine { count: None });
    cx.dispatch_action(CopyFromAbove { count: None });
    cx.assert_editor_state("oneˇ\n");
}

/// A single-line editor has no lines to work on, so every line command leaves its text alone
/// and passes the key on to the next handler.
#[gpui::test]
async fn a_single_line_editor_propagates_the_key(cx: &mut TestAppContext) {
    init_test(cx);
    let passed = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let count = passed.clone();
        cx.on_action(move |_: &MoveLineUp, _| count.set(count.get() + 1));
        let count = passed.clone();
        cx.on_action(move |_: &MoveLineDown, _| count.set(count.get() + 1));
        let count = passed.clone();
        cx.on_action(move |_: &DuplicateLine, _| count.set(count.get() + 1));
        let count = passed.clone();
        cx.on_action(move |_: &CopyFromAbove, _| count.set(count.get() + 1));
    });
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text("one two", window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));

    cx.update(|window, cx| {
        window.dispatch_action(MoveLineUp { count: None }.boxed_clone(), cx);
        window.dispatch_action(MoveLineDown { count: Some(2) }.boxed_clone(), cx);
        window.dispatch_action(DuplicateLine { count: None }.boxed_clone(), cx);
        window.dispatch_action(CopyFromAbove { count: None }.boxed_clone(), cx);
    });
    assert_eq!(passed.get(), 4);
    assert_eq!(editor.update(cx, |editor, cx| editor.text(cx)), "one two");
}
