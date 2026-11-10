use editor::Editor;
use editor::actions::{Paste, Undo};
use editor::test::editor_test_context::EditorTestContext;
use gpui::{Action as _, Focusable as _, TestAppContext, VisualTestContext};

use crate::actions::{
    BackwardKillWord, CapitalizePreviousWord, CapitalizeWord, DowncasePreviousWord, DowncaseWord,
    KillWord, UpcasePreviousWord, UpcaseWord,
};
use crate::apply_tests::{capture_kills, init_test};
use crate::kill_sink::{KillDirection, KillKind};

#[gpui::test]
async fn kill_word_with_counts(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("ˇone two three four");
    cx.dispatch_action(KillWord { count: None });
    cx.assert_editor_state("ˇ two three four");

    cx.set_state("ˇone two three four");
    cx.dispatch_action(KillWord { count: Some(3) });
    cx.assert_editor_state("ˇ four");

    // Zero kills nothing and saves nothing.
    kills.borrow_mut().clear();
    cx.set_state("one twˇo three four");
    cx.dispatch_action(KillWord { count: Some(0) });
    cx.assert_editor_state("one twˇo three four");
    assert!(kills.borrow().is_empty());

    // Negative reverses.
    cx.set_state("one two thˇree four");
    cx.dispatch_action(KillWord { count: Some(-2) });
    cx.assert_editor_state("one ˇree four");

    // A large count stops at the end of the text.
    cx.set_state("ˇone two");
    cx.dispatch_action(KillWord { count: Some(10) });
    cx.assert_editor_state("ˇ");
}

#[gpui::test]
async fn backward_kill_word_with_counts(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one two threeˇ four");
    cx.dispatch_action(BackwardKillWord { count: None });
    cx.assert_editor_state("one two ˇ four");

    cx.set_state("one two three fourˇ");
    cx.dispatch_action(BackwardKillWord { count: Some(3) });
    cx.assert_editor_state("one ˇ");

    kills.borrow_mut().clear();
    cx.set_state("one twˇo three four");
    cx.dispatch_action(BackwardKillWord { count: Some(0) });
    cx.assert_editor_state("one twˇo three four");
    assert!(kills.borrow().is_empty());

    cx.set_state("one ˇtwo three four");
    cx.dispatch_action(BackwardKillWord { count: Some(-2) });
    cx.assert_editor_state("one ˇ four");
}

#[gpui::test]
async fn kills_are_saved_with_their_kind_direction_and_command(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("ˇone two");
    cx.dispatch_action(KillWord { count: None });
    cx.set_state("one twoˇ");
    cx.dispatch_action(BackwardKillWord { count: None });

    let kills = kills.borrow();
    assert_eq!(kills.len(), 2);
    assert_eq!(kills[0].pieces, vec!["one".to_string()]);
    assert_eq!(kills[0].kind, KillKind::Kill);
    assert_eq!(kills[0].direction, KillDirection::Forward);
    assert_eq!(kills[0].command, "emacs::KillWord");
    assert_eq!(kills[1].pieces, vec!["two".to_string()]);
    assert_eq!(kills[1].direction, KillDirection::Backward);
    assert_eq!(kills[1].command, "emacs::BackwardKillWord");
}

#[gpui::test]
async fn kill_commands_kill_the_region_and_end_mark_mode(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one «two threeˇ» four");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(KillWord { count: Some(1) });
    cx.assert_editor_state("one ˇ four");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
    assert_eq!(kills.borrow()[0].pieces, vec!["two three".to_string()]);

    cx.set_state("one «ˇtwo» three");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(BackwardKillWord { count: Some(2) });
    cx.assert_editor_state("one ˇ three");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
    assert_eq!(kills.borrow()[1].pieces, vec!["two".to_string()]);
}

#[gpui::test]
async fn a_kill_with_mark_mode_but_no_region_ends_mark_mode(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("ˇone two");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(KillWord { count: None });
    cx.assert_editor_state("ˇ two");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
}

#[gpui::test]
async fn forward_case_commands_move_point_past_the_words(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("ˇone two three");
    cx.dispatch_action(UpcaseWord { count: None });
    cx.assert_editor_state("ONEˇ two three");

    cx.set_state("ˇone two three");
    cx.dispatch_action(UpcaseWord { count: Some(2) });
    cx.assert_editor_state("ONE TWOˇ three");

    cx.set_state("ˇONE TWO THREE");
    cx.dispatch_action(DowncaseWord { count: Some(3) });
    cx.assert_editor_state("one two threeˇ");

    cx.set_state("ˇhELLO wORLD");
    cx.dispatch_action(CapitalizeWord { count: Some(2) });
    cx.assert_editor_state("Hello Worldˇ");

    // Zero does nothing.
    cx.set_state("oˇne two");
    cx.dispatch_action(UpcaseWord { count: Some(0) });
    cx.assert_editor_state("oˇne two");

    // Negative acts before point and leaves it.
    cx.set_state("one two threeˇ four");
    cx.dispatch_action(UpcaseWord { count: Some(-2) });
    cx.assert_editor_state("one TWO THREEˇ four");

    // Case commands delete nothing, so nothing reaches the kill sink.
    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn previous_word_case_commands_leave_point(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one two threeˇ four");
    cx.dispatch_action(UpcasePreviousWord { count: None });
    cx.assert_editor_state("one two THREEˇ four");

    cx.set_state("one two threeˇ four");
    cx.dispatch_action(UpcasePreviousWord { count: Some(2) });
    cx.assert_editor_state("one TWO THREEˇ four");

    cx.set_state("ONE TWO THREEˇ FOUR");
    cx.dispatch_action(DowncasePreviousWord { count: Some(3) });
    cx.assert_editor_state("one two threeˇ FOUR");

    cx.set_state("hELLO wORLDˇ");
    cx.dispatch_action(CapitalizePreviousWord { count: None });
    cx.assert_editor_state("hELLO Worldˇ");

    cx.set_state("one twoˇ three");
    cx.dispatch_action(UpcasePreviousWord { count: Some(0) });
    cx.assert_editor_state("one twoˇ three");

    // Negative reverses: the words after point.
    cx.set_state("one ˇtwo three four");
    cx.dispatch_action(UpcasePreviousWord { count: Some(-2) });
    cx.assert_editor_state("one ˇTWO THREE four");
}

#[gpui::test]
async fn case_commands_apply_to_the_region_and_end_mark_mode(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one «two threeˇ» four");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(UpcaseWord { count: None });
    cx.assert_editor_state("one «TWO THREEˇ» four");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));

    cx.set_state("ONE «TWO THREEˇ» FOUR");
    cx.dispatch_action(DowncasePreviousWord { count: Some(5) });
    cx.assert_editor_state("ONE «two threeˇ» FOUR");

    cx.set_state("one «twoˇ» three");
    cx.dispatch_action(CapitalizeWord { count: None });
    cx.assert_editor_state("one «Twoˇ» three");
}

#[gpui::test]
async fn three_carets_kill_one_piece_each_in_one_undo_step(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("ˇone two\nˇthree four\nˇfive six");
    cx.dispatch_action(KillWord { count: None });
    cx.assert_editor_state("ˇ two\nˇ four\nˇ six");
    assert_eq!(kills.borrow().len(), 1);
    assert_eq!(
        kills.borrow()[0].pieces,
        vec!["one".to_string(), "three".to_string(), "five".to_string()]
    );

    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "one two\nthree four\nfive six");
}

#[gpui::test]
async fn three_caret_case_command_is_one_undo_step(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("ˇone two\nˇthree four\nˇfive six");
    cx.dispatch_action(UpcaseWord { count: None });
    cx.assert_editor_state("ONEˇ two\nTHREEˇ four\nFIVEˇ six");
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "one two\nthree four\nfive six");
}

#[gpui::test]
async fn the_clipboard_holds_the_killed_words_and_paste_restores_each(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    // The default sink is the system clipboard.
    cx.set_state("ˇone two\nˇthree four\nˇfive six");
    cx.dispatch_action(KillWord { count: None });
    cx.assert_editor_state("ˇ two\nˇ four\nˇ six");
    let clipboard = cx.update(|_, cx| cx.read_from_clipboard()).unwrap();
    assert_eq!(clipboard.text().as_deref(), Some("one\nthree\nfive"));

    cx.dispatch_action(Paste);
    cx.assert_editor_state("oneˇ two\nthreeˇ four\nfiveˇ six");

    // Backward kills join in document order too.
    cx.set_state("one twoˇ\nthree fourˇ\nfive sixˇ");
    cx.dispatch_action(BackwardKillWord { count: None });
    cx.assert_editor_state("one ˇ\nthree ˇ\nfive ˇ");
    let clipboard = cx.update(|_, cx| cx.read_from_clipboard()).unwrap();
    assert_eq!(clipboard.text().as_deref(), Some("two\nfour\nsix"));
    cx.dispatch_action(Paste);
    cx.assert_editor_state("one twoˇ\nthree fourˇ\nfive sixˇ");
}

#[gpui::test]
async fn two_carets_in_one_word_kill_it_once(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("oˇnˇe two");
    cx.dispatch_action(KillWord { count: None });
    cx.assert_editor_state("oˇ two");
    assert_eq!(kills.borrow().len(), 1);
    assert_eq!(kills.borrow()[0].pieces, vec!["ne".to_string()]);
}

#[gpui::test]
async fn read_only_buffer_is_unchanged_and_kills_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);
    cx.set_state("ˇone two");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));

    cx.dispatch_action(KillWord { count: None });
    cx.assert_editor_state("ˇone two");
    cx.dispatch_action(BackwardKillWord { count: None });
    cx.dispatch_action(UpcaseWord { count: None });
    cx.dispatch_action(CapitalizePreviousWord { count: None });
    cx.assert_editor_state("ˇone two");
    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn a_single_line_editor_kills_and_converts_words(cx: &mut TestAppContext) {
    init_test(cx);
    let kills = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = kills.clone();
    cx.update(|cx| crate::set_kill_sink(cx, move |kill, _, _| sink.borrow_mut().push(kill)));

    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text("one two three", window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    let text = |cx: &mut VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));
    let place = |cx: &mut VisualTestContext, at: usize| {
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges([
                    multi_buffer::MultiBufferOffset(at)..multi_buffer::MultiBufferOffset(at)
                ])
            })
        })
    };

    place(cx, 4);
    cx.update(|window, cx| window.dispatch_action(KillWord { count: None }.boxed_clone(), cx));
    assert_eq!(text(cx), "one  three");
    assert_eq!(kills.borrow()[0].pieces, vec!["two".to_string()]);

    place(cx, 0);
    cx.update(|window, cx| window.dispatch_action(UpcaseWord { count: Some(2) }.boxed_clone(), cx));
    assert_eq!(text(cx), "ONE  THREE");
}
