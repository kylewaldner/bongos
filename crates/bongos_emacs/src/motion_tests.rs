use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;

use crate::actions::{BackwardWord, ForwardWord};
use crate::apply_tests::init_test;

fn forward(count: Option<i32>, select: bool) -> ForwardWord {
    ForwardWord { count, select }
}

fn backward(count: Option<i32>, select: bool) -> BackwardWord {
    BackwardWord { count, select }
}

#[gpui::test]
async fn forward_word_with_counts(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("ˇone two three four");
    cx.dispatch_action(forward(None, false));
    cx.assert_editor_state("oneˇ two three four");

    cx.set_state("ˇone two three four");
    cx.dispatch_action(forward(Some(3), false));
    cx.assert_editor_state("one two threeˇ four");

    cx.set_state("one twˇo three four");
    cx.dispatch_action(forward(Some(0), false));
    cx.assert_editor_state("one twˇo three four");

    cx.set_state("one two thˇree four");
    cx.dispatch_action(forward(Some(-2), false));
    cx.assert_editor_state("one ˇtwo three four");

    // Large counts stop at the end.
    cx.set_state("ˇone two");
    cx.dispatch_action(forward(Some(10), false));
    cx.assert_editor_state("one twoˇ");
}

#[gpui::test]
async fn backward_word_with_counts(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one two three fourˇ");
    cx.dispatch_action(backward(None, false));
    cx.assert_editor_state("one two three ˇfour");

    cx.set_state("one two three fourˇ");
    cx.dispatch_action(backward(Some(3), false));
    cx.assert_editor_state("one ˇtwo three four");

    cx.set_state("one twˇo three four");
    cx.dispatch_action(backward(Some(0), false));
    cx.assert_editor_state("one twˇo three four");

    cx.set_state("one twˇo three four");
    cx.dispatch_action(backward(Some(-2), false));
    cx.assert_editor_state("one two threeˇ four");
}

#[gpui::test]
async fn word_motions_with_select_extend_the_region(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("oneˇ two three");
    cx.dispatch_action(forward(None, true));
    cx.assert_editor_state("one« twoˇ» three");
    cx.dispatch_action(forward(None, true));
    cx.assert_editor_state("one« two threeˇ»");
    cx.dispatch_action(backward(Some(3), true));
    cx.assert_editor_state("«ˇone» two three");

    // Without select the region collapses at the target.
    cx.set_state("one« twoˇ» three");
    cx.dispatch_action(forward(None, false));
    cx.assert_editor_state("one two threeˇ");
}

#[gpui::test]
async fn word_motions_move_every_caret(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("ˇone two\nˇthree four\nˇfive");
    cx.dispatch_action(forward(Some(2), false));
    cx.assert_editor_state("one twoˇ\nthree fourˇ\nfiveˇ");
}
