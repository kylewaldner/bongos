use editor::actions::Undo;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;

use crate::actions::{DeleteBlankLines, DeleteHorizontalSpace, JustOneSpace};
use crate::apply_tests::{capture_kills, init_test};

#[gpui::test]
async fn delete_horizontal_space_removes_both_sides_without_a_count(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("ab \t ˇ  cd");
    cx.dispatch_action(DeleteHorizontalSpace { count: None });
    cx.assert_editor_state("abˇcd");

    // Any count, even 0, deletes only the spaces before point.
    cx.set_state("ab  ˇ  cd");
    cx.dispatch_action(DeleteHorizontalSpace { count: Some(0) });
    cx.assert_editor_state("abˇ  cd");

    cx.set_state("ab  ˇ  cd");
    cx.dispatch_action(DeleteHorizontalSpace { count: Some(-3) });
    cx.assert_editor_state("abˇ  cd");

    cx.set_state("ab  ˇ  cd");
    cx.dispatch_action(DeleteHorizontalSpace { count: Some(5) });
    cx.assert_editor_state("abˇ  cd");

    // Newlines stay.
    cx.set_state("ab \nˇ \n cd");
    cx.dispatch_action(DeleteHorizontalSpace { count: None });
    cx.assert_editor_state("ab \nˇ\n cd");

    // Nothing is killed.
    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn delete_horizontal_space_with_nothing_to_delete_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("abˇcd");
    cx.dispatch_action(DeleteHorizontalSpace { count: None });
    cx.assert_editor_state("abˇcd");
    cx.dispatch_action(Undo);
    cx.assert_editor_state("ˇ");
}

#[gpui::test]
async fn delete_horizontal_space_clears_the_region_and_acts_at_point(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("a «b  ˇ»  c");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(DeleteHorizontalSpace { count: None });
    cx.assert_editor_state("a bˇc");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));

    // Nothing to delete: the region is still cleared.
    cx.set_state("«abˇ»cd");
    cx.dispatch_action(DeleteHorizontalSpace { count: None });
    cx.assert_editor_state("abˇcd");
}

#[gpui::test]
async fn just_one_space_leaves_n_spaces(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("a \t ˇ  b");
    cx.dispatch_action(JustOneSpace { count: None });
    cx.assert_editor_state("a ˇb");

    cx.set_state("a  ˇ  b");
    cx.dispatch_action(JustOneSpace { count: Some(3) });
    cx.assert_editor_state("a   ˇb");

    cx.set_state("a  ˇ  b");
    cx.dispatch_action(JustOneSpace { count: Some(0) });
    cx.assert_editor_state("aˇb");

    // No spaces around point: one is inserted.
    cx.set_state("aˇb");
    cx.dispatch_action(JustOneSpace { count: None });
    cx.assert_editor_state("a ˇb");

    // Newlines stay for a positive count.
    cx.set_state("a \nˇ  b");
    cx.dispatch_action(JustOneSpace { count: None });
    cx.assert_editor_state("a \n ˇb");

    // A negative count leaves -N spaces and also removes newlines.
    cx.set_state("a\n \nˇ \nb");
    cx.dispatch_action(JustOneSpace { count: Some(-2) });
    cx.assert_editor_state("a  ˇb");
}

#[gpui::test]
async fn just_one_space_clears_the_region_and_acts_at_point(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("a «b ˇ» c");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(JustOneSpace { count: None });
    cx.assert_editor_state("a b ˇc");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
}

#[gpui::test]
async fn whitespace_commands_handle_multi_byte_text(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("é \u{00a0}ˇ\t ü");
    cx.dispatch_action(DeleteHorizontalSpace { count: None });
    cx.assert_editor_state("é \u{00a0}ˇü");

    cx.set_state("日本  ˇ  語é");
    cx.dispatch_action(JustOneSpace { count: None });
    cx.assert_editor_state("日本 ˇ語é");

    cx.set_state("日本語\n\nˇ\n\nüñ");
    cx.dispatch_action(DeleteBlankLines);
    cx.assert_editor_state("日本語\nˇ\nüñ");
}

#[gpui::test]
async fn delete_blank_lines_follows_the_emacs_cases(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    // One of several blank lines: the others go.
    cx.set_state("a\n\nˇ\n\nb");
    cx.dispatch_action(DeleteBlankLines);
    cx.assert_editor_state("a\nˇ\nb");

    // A lone blank line goes.
    cx.set_state("a\nˇ\nb");
    cx.dispatch_action(DeleteBlankLines);
    cx.assert_editor_state("a\nˇb");

    // On a nonblank line the blank lines after it go.
    cx.set_state("aˇ\n\n \n\nb");
    cx.dispatch_action(DeleteBlankLines);
    cx.assert_editor_state("aˇ\nb");

    // A blank run at the end of the text leaves no blank line behind.
    cx.set_state("a\nˇ\n\n");
    cx.dispatch_action(DeleteBlankLines);
    cx.assert_editor_state("a\nˇ");

    // Nothing to do.
    cx.set_state("aˇ\nb");
    cx.dispatch_action(DeleteBlankLines);
    cx.assert_editor_state("aˇ\nb");
}

#[gpui::test]
async fn delete_blank_lines_clears_the_region(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("a\n«\n\nˇ»\nb");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(DeleteBlankLines);
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
    cx.assert_editor_state("a\nˇ\nb");
}

#[gpui::test]
async fn two_carets_are_one_undo_step_per_command(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("a  ˇ  b\nc \t ˇ d");
    cx.dispatch_action(DeleteHorizontalSpace { count: None });
    cx.assert_editor_state("aˇb\ncˇd");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "a    b\nc \t  d");

    cx.set_state("a  ˇ  b\nc \t ˇ d");
    cx.dispatch_action(JustOneSpace { count: None });
    cx.assert_editor_state("a ˇb\nc ˇd");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "a    b\nc \t  d");

    cx.set_state("aˇ\n\n\nb\ncˇ\n\n\nd");
    cx.dispatch_action(DeleteBlankLines);
    cx.assert_editor_state("aˇ\nb\ncˇ\nd");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "a\n\n\nb\nc\n\n\nd");
}

#[gpui::test]
async fn read_only_buffer_is_unchanged(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("a  ˇ  b\n\nc");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    cx.dispatch_action(DeleteHorizontalSpace { count: None });
    cx.dispatch_action(JustOneSpace { count: None });
    cx.dispatch_action(DeleteBlankLines);
    cx.assert_editor_state("a  ˇ  b\n\nc");
}
