use editor::actions::Undo;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;

use crate::actions::{
    SetMarkCommand, TransposeChars, TransposeLines, TransposeWords, TransposeWordsReverse,
};
use crate::apply_tests::{capture_kills, init_test};

#[gpui::test]
async fn transpose_chars_with_counts(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("abˇcd");
    cx.dispatch_action(TransposeChars { count: None });
    cx.assert_editor_state("acbˇd");

    cx.set_state("aˇbcd");
    cx.dispatch_action(TransposeChars { count: Some(2) });
    cx.assert_editor_state("bcaˇd");

    cx.set_state("abˇcd");
    cx.dispatch_action(TransposeChars { count: Some(-1) });
    cx.assert_editor_state("bˇacd");

    // Zero does nothing.
    cx.set_state("abˇcd");
    cx.dispatch_action(TransposeChars { count: Some(0) });
    cx.assert_editor_state("abˇcd");

    // A large count stops at the end of the text.
    cx.set_state("aˇbcd");
    cx.dispatch_action(TransposeChars { count: Some(10) });
    cx.assert_editor_state("bcdaˇ");

    // At the end of a line the two chars before point swap.
    cx.set_state("abcˇ\nd");
    cx.dispatch_action(TransposeChars { count: None });
    cx.assert_editor_state("acbˇ\nd");

    // Nothing is killed.
    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn transpose_chars_with_nothing_to_do_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("ˇabc");
    cx.dispatch_action(TransposeChars { count: None });
    cx.assert_editor_state("ˇabc");

    cx.set_state("ˇ");
    cx.dispatch_action(TransposeChars { count: None });
    cx.assert_editor_state("ˇ");
}

#[gpui::test]
async fn transpose_chars_clears_the_region_and_acts_at_point(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("a«bcˇ»de");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(TransposeChars { count: None });
    cx.assert_editor_state("abdcˇe");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));

    // Nothing to swap: the region is still cleared.
    cx.set_state("«ˇab»c");
    cx.dispatch_action(TransposeChars { count: None });
    cx.assert_editor_state("ˇabc");
}

#[gpui::test]
async fn transpose_words_with_counts(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one twoˇ three four");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one three twoˇ four");

    cx.set_state("one twoˇ three four");
    cx.dispatch_action(TransposeWords { count: Some(2) });
    cx.assert_editor_state("one three four twoˇ");

    // Point inside a word moves that word.
    cx.set_state("one tˇwo three");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one three twoˇ");

    // Zero does nothing.
    cx.set_state("one twoˇ three");
    cx.dispatch_action(TransposeWords { count: Some(0) });
    cx.assert_editor_state("one twoˇ three");

    // Negative moves the word before point backward.
    cx.set_state("one two threeˇ");
    cx.dispatch_action(TransposeWords { count: Some(-1) });
    cx.assert_editor_state("one threeˇ two");

    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn transpose_words_reverse_moves_the_word_backward(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one two threeˇ four");
    cx.dispatch_action(TransposeWordsReverse { count: None });
    cx.assert_editor_state("one threeˇ two four");

    cx.set_state("one two three fourˇ");
    cx.dispatch_action(TransposeWordsReverse { count: Some(2) });
    cx.assert_editor_state("one fourˇ two three");

    cx.set_state("one twoˇ three four");
    cx.dispatch_action(TransposeWordsReverse { count: Some(-1) });
    cx.assert_editor_state("one three twoˇ four");

    cx.set_state("one twoˇ three");
    cx.dispatch_action(TransposeWordsReverse { count: Some(0) });
    cx.assert_editor_state("one twoˇ three");
}

#[gpui::test]
async fn transpose_words_with_nothing_to_do_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    // Only one word.
    cx.set_state("onˇe");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("onˇe");

    // No word before point.
    cx.set_state("ˇ  one");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("ˇ  one");

    // No previous word.
    cx.set_state("oneˇ two");
    cx.dispatch_action(TransposeWordsReverse { count: None });
    cx.assert_editor_state("oneˇ two");
}

#[gpui::test]
async fn transpose_words_with_a_region_swaps_it_with_the_next_word(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one «two threeˇ» four five");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one four two threeˇ five");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));

    // The count is ignored with a region.
    cx.set_state("one «twoˇ» three four");
    cx.dispatch_action(TransposeWords { count: Some(3) });
    cx.assert_editor_state("one three twoˇ four");

    // A region with no word after it does nothing.
    cx.set_state("one «twoˇ»");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one twoˇ");
}

#[gpui::test]
async fn transpose_words_reverse_with_a_region_swaps_it_with_the_previous_word(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one two «three fourˇ» five");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(TransposeWordsReverse { count: None });
    cx.assert_editor_state("one three fourˇ two five");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));

    cx.set_state("one two «threeˇ» four");
    cx.dispatch_action(TransposeWordsReverse { count: Some(4) });
    cx.assert_editor_state("one threeˇ two four");
}

#[gpui::test]
async fn transpose_lines_with_counts(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one\ntwoˇ\nthree\nfour");
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("two\noneˇ\nthree\nfour");

    cx.set_state("one\ntwoˇ\nthree\nfour");
    cx.dispatch_action(TransposeLines { count: Some(2) });
    cx.assert_editor_state("two\nthree\noneˇ\nfour");

    cx.set_state("one\ntwo\nthreeˇ\nfour");
    cx.dispatch_action(TransposeLines { count: Some(0) });
    cx.assert_editor_state("one\ntwo\nthreeˇ\nfour");

    cx.set_state("one\ntwo\nthreeˇ\nfour");
    cx.dispatch_action(TransposeLines { count: Some(-1) });
    cx.assert_editor_state("twoˇ\none\nthree\nfour");

    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn transpose_lines_with_nothing_to_do_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("oˇne\ntwo");
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("oˇne\ntwo");
}

#[gpui::test]
async fn transpose_lines_clears_the_region_and_acts_at_point(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one\ntw«o\nthˇ»ree");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("one\nthree\ntwoˇ");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
}

#[gpui::test]
async fn transpose_commands_handle_multi_byte_text(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("aéˇ日b");
    cx.dispatch_action(TransposeChars { count: None });
    cx.assert_editor_state("a日éˇb");

    cx.set_state("éˇñü");
    cx.dispatch_action(TransposeChars { count: Some(2) });
    cx.assert_editor_state("ñüéˇ");

    cx.set_state("café naïveˇ 日本語");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("café 日本語 naïveˇ");

    cx.set_state("café naïve 日本語ˇ");
    cx.dispatch_action(TransposeWordsReverse { count: None });
    cx.assert_editor_state("café 日本語ˇ naïve");

    cx.set_state("日本\nüñˇ\nçé");
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("üñ\n日本ˇ\nçé");
}

#[gpui::test]
async fn two_carets_are_one_undo_step_per_command(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("abˇcd\nefˇgh");
    cx.dispatch_action(TransposeChars { count: None });
    cx.assert_editor_state("acbˇd\negfˇh");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "abcd\nefgh");

    cx.set_state("one twoˇ three\nfour fiveˇ six");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one three twoˇ\nfour six fiveˇ");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "one two three\nfour five six");

    cx.set_state("one two threeˇ\nfour five sixˇ");
    cx.dispatch_action(TransposeWordsReverse { count: None });
    cx.assert_editor_state("one threeˇ two\nfour sixˇ five");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "one two three\nfour five six");

    cx.set_state("a\nbˇ\nc\nd\neˇ\nf");
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("b\naˇ\nc\ne\ndˇ\nf");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "a\nb\nc\nd\ne\nf");
}

#[gpui::test]
async fn read_only_buffer_is_unchanged(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("one twoˇ three\nfour");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    cx.dispatch_action(TransposeChars { count: None });
    cx.dispatch_action(TransposeWords { count: None });
    cx.dispatch_action(TransposeWordsReverse { count: None });
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("one twoˇ three\nfour");
}

/// C-SPC C-SPC at the current caret: the mark is set and the region deactivated.
fn set_mark_here(cx: &mut EditorTestContext) {
    cx.dispatch_action(SetMarkCommand { count: None });
    cx.dispatch_action(SetMarkCommand { count: None });
}

#[gpui::test]
async fn transpose_words_with_a_mark_swaps_the_words_at_point_and_mark(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one ˇtwo three four");
    set_mark_here(&mut cx);
    cx.set_selections_state("one two three ˇfour");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one four three twoˇ");

    // The mark stays with its text, so the swap can be undone in one step.
    cx.dispatch_action(Undo);
    cx.assert_editor_state("one two three ˇfour");

    // A zero count takes the mark form too.
    cx.dispatch_action(TransposeWords { count: Some(0) });
    cx.assert_editor_state("one four three twoˇ");
    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn transpose_words_with_the_mark_after_point(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one two three ˇfour");
    set_mark_here(&mut cx);
    cx.set_selections_state("one ˇtwo three four");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one fourˇ three two");
}

#[gpui::test]
async fn transpose_words_with_the_mark_in_the_same_word_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one tˇwo three");
    set_mark_here(&mut cx);
    cx.set_selections_state("one twˇo three");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one twˇo three");
}

#[gpui::test]
async fn transpose_words_with_the_mark_at_point_uses_the_plain_form(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one twoˇ three four");
    cx.dispatch_action(SetMarkCommand { count: None });
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one three twoˇ four");
}

#[gpui::test]
async fn transpose_words_with_a_mark_but_another_state_uses_the_plain_form(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    // No mark.
    cx.set_state("one twoˇ three four");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one three twoˇ four");

    // A count other than none or 0.
    cx.set_state("ˇone two three four");
    set_mark_here(&mut cx);
    cx.set_selections_state("one twoˇ three four");
    cx.dispatch_action(TransposeWords { count: Some(2) });
    cx.assert_editor_state("one three four twoˇ");

    // A non-empty region.
    cx.set_state("ˇone two three four");
    set_mark_here(&mut cx);
    cx.set_selections_state("one «twoˇ» three four");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one three twoˇ four");

    // Two carets.
    cx.set_state("ˇone two three four five six");
    set_mark_here(&mut cx);
    cx.set_selections_state("one twoˇ three fourˇ five six");
    cx.dispatch_action(TransposeWords { count: None });
    cx.assert_editor_state("one three twoˇ five fourˇ six");
}

#[gpui::test]
async fn transpose_lines_with_a_mark_swaps_the_lines_at_point_and_mark(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("oˇne\ntwo\nthree\nfour");
    set_mark_here(&mut cx);
    cx.set_selections_state("one\ntwo\nthˇree\nfour");
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("three\ntwo\none\nˇfour");
    cx.dispatch_action(Undo);
    cx.assert_editor_state("one\ntwo\nthˇree\nfour");

    // The last line without a newline.
    cx.set_selections_state("one\ntwo\nthree\nfˇour");
    cx.dispatch_action(TransposeLines { count: Some(0) });
    cx.assert_editor_state("four\ntwo\nthree\noneˇ");
}

#[gpui::test]
async fn transpose_lines_with_the_mark_on_the_same_line_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one\ntˇwo\nthree");
    set_mark_here(&mut cx);
    cx.set_selections_state("one\ntwˇo\nthree");
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("one\ntwˇo\nthree");
}

#[gpui::test]
async fn transpose_lines_with_a_mark_but_another_state_uses_the_plain_form(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    // Mark at point.
    cx.set_state("one\ntwoˇ\nthree");
    cx.dispatch_action(SetMarkCommand { count: None });
    cx.dispatch_action(TransposeLines { count: None });
    cx.assert_editor_state("two\noneˇ\nthree");

    // A count of 2 and a region keep the plain form.
    cx.set_state("ˇone\ntwo\nthree\nfour");
    set_mark_here(&mut cx);
    cx.set_selections_state("one\ntwoˇ\nthree\nfour");
    cx.dispatch_action(TransposeLines { count: Some(2) });
    cx.assert_editor_state("two\nthree\noneˇ\nfour");
}
