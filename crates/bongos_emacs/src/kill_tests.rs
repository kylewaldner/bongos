use editor::actions::{Paste, Undo};
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;

use crate::actions::{CopyRegionOrLine, KillLine, KillRegionOrLine, KillWholeLine};
use crate::apply_tests::{capture_kills, init_test};
use crate::kill_sink::{KillDirection, KillKind};

fn mark_mode(cx: &mut EditorTestContext) {
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
}

fn no_group_interval(cx: &mut EditorTestContext) {
    cx.update_buffer(|buffer, _| buffer.set_group_interval(std::time::Duration::ZERO));
}

#[gpui::test]
async fn kill_line_kills_to_the_end_and_through_the_newline_when_blank(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one ˇtwo\nthree");
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("one ˇ\nthree");
    assert_eq!(kills.borrow()[0].pieces, vec!["two".to_string()]);
    assert_eq!(kills.borrow()[0].kind, KillKind::Kill);
    assert_eq!(kills.borrow()[0].command, "emacs::KillLine");

    // Only whitespace left: the newline goes too.
    cx.set_state("one ˇ  \t\nthree");
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("one ˇthree");
    assert_eq!(kills.borrow()[1].pieces, vec!["  \t\n".to_string()]);

    // Column 0: the whole line and its newline.
    cx.set_state("ˇone\nthree");
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("ˇthree");
    assert_eq!(kills.borrow()[2].pieces, vec!["one\n".to_string()]);

    // At the end of a line: only the newline.
    cx.set_state("oneˇ\nthree");
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("oneˇthree");
}

#[gpui::test]
async fn kill_line_with_counts(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("aˇa\nbb\ncc\ndd");
    cx.dispatch_action(KillLine { count: Some(2) });
    cx.assert_editor_state("aˇcc\ndd");
    assert_eq!(kills.borrow()[0].pieces, vec!["a\nbb\n".to_string()]);

    // Zero kills back to the line start.
    cx.set_state("aa\nbˇb\ncc");
    cx.dispatch_action(KillLine { count: Some(0) });
    cx.assert_editor_state("aa\nˇb\ncc");
    assert_eq!(kills.borrow()[1].pieces, vec!["b".to_string()]);
    assert_eq!(kills.borrow()[1].direction, KillDirection::Backward);

    // Negative kills back to the start of the line N above.
    cx.set_state("aa\nbb\ncˇc\ndd");
    cx.dispatch_action(KillLine { count: Some(-2) });
    cx.assert_editor_state("ˇc\ndd");
    assert_eq!(kills.borrow()[2].pieces, vec!["aa\nbb\nc".to_string()]);
}

#[gpui::test]
async fn kill_line_with_nothing_to_kill_saves_and_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    // Count 0 at the line start.
    cx.set_state("one\nˇtwo");
    cx.dispatch_action(KillLine { count: Some(0) });
    cx.assert_editor_state("one\nˇtwo");

    // End of the buffer.
    cx.set_state("one\ntwoˇ");
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("one\ntwoˇ");
    cx.dispatch_action(KillLine { count: Some(3) });
    cx.assert_editor_state("one\ntwoˇ");
    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn kill_line_with_a_region_kills_the_region_and_ends_mark_mode(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one «two\nthˇ»ree");
    mark_mode(&mut cx);
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("one ˇree");
    assert_eq!(kills.borrow()[0].pieces, vec!["two\nth".to_string()]);
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
}

#[gpui::test]
async fn kill_line_handles_multi_byte_text(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("héˇllo wörld\nñ");
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("héˇ\nñ");
    assert_eq!(kills.borrow()[0].pieces, vec!["llo wörld".to_string()]);

    cx.set_state("日本ˇ語\n字");
    cx.dispatch_action(KillLine { count: Some(0) });
    cx.assert_editor_state("ˇ語\n字");
}

#[gpui::test]
async fn kill_line_works_in_a_single_line_editor(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);
    cx.set_state("one ˇtwo three");
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("one ˇ");
    assert_eq!(kills.borrow()[0].pieces, vec!["two three".to_string()]);
}

#[gpui::test]
async fn kill_region_or_line_kills_whole_lines_and_paste_restores_a_line(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("one\nˇthree");
    let clipboard = cx.read_from_clipboard().unwrap();
    assert_eq!(clipboard.text().as_deref(), Some("two\n"));

    // Paste puts the line back as a whole line above the caret's line.
    cx.set_state("one\nthrˇee");
    cx.dispatch_action(Paste);
    cx.assert_editor_state("one\ntwo\nthrˇee");

    // Count 2, and a negative count taking the lines above.
    cx.set_state("aˇa\nbb\ncc");
    cx.dispatch_action(KillRegionOrLine { count: Some(2) });
    cx.assert_editor_state("ˇcc");
    assert_eq!(
        cx.read_from_clipboard().unwrap().text().as_deref(),
        Some("aa\nbb\n")
    );
    cx.set_state("aa\nbb\ncˇc\ndd");
    cx.dispatch_action(KillRegionOrLine { count: Some(-2) });
    assert_eq!(
        cx.read_from_clipboard().unwrap().text().as_deref(),
        Some("aa\nbb\n")
    );
    cx.assert_editor_state("ˇcc\ndd");
}

#[gpui::test]
async fn kill_region_or_line_on_the_last_line_takes_the_newline_before_it(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one\ntwˇo");
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("oneˇ");
    assert_eq!(kills.borrow()[0].pieces, vec!["two\n".to_string()]);
    assert!(kills.borrow()[0].metadata[0].is_entire_line);

    // A single line: the text is all there is, and the piece still ends in a newline.
    cx.set_state("onˇe");
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("ˇ");
    assert_eq!(kills.borrow()[1].pieces, vec!["one\n".to_string()]);
}

#[gpui::test]
async fn kill_region_or_line_with_nothing_to_kill_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one\ntwˇo");
    cx.dispatch_action(KillRegionOrLine { count: Some(0) });
    cx.assert_editor_state("one\ntwˇo");
    cx.set_state("ˇ");
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("ˇ");
    assert!(kills.borrow().is_empty());
}

#[gpui::test]
async fn kill_region_or_line_with_a_region_kills_the_region_not_a_line(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one «two\nˇ»three");
    mark_mode(&mut cx);
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("one ˇthree");
    // The region piece ends in a newline but is not a whole line.
    assert_eq!(kills.borrow()[0].pieces, vec!["two\n".to_string()]);
    assert!(!kills.borrow()[0].metadata[0].is_entire_line);
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
}

#[gpui::test]
async fn a_mixed_region_and_active_empty_caret_kill_only_the_region(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    // With mark mode on, the empty caret on "three" is an active empty region: it kills nothing.
    cx.set_state("«one\nˇ»two\nthˇree\nfour");
    mark_mode(&mut cx);
    cx.dispatch_action(KillRegionOrLine { count: None });
    let kills = kills.borrow();
    assert_eq!(kills[0].pieces, vec!["one\n".to_string()]);
    assert_eq!(
        kills[0]
            .metadata
            .iter()
            .map(|selection| selection.is_entire_line)
            .collect::<Vec<_>>(),
        vec![false]
    );
    cx.assert_editor_state("ˇtwo\nthˇree\nfour");
}

#[gpui::test]
async fn an_active_empty_region_kills_nothing_and_ends_mark_mode(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one\ntwˇo\nthree");
    mark_mode(&mut cx);
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("one\ntwˇo\nthree");
    assert!(kills.borrow().is_empty());
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));

    // Without mark mode and without a region the line still goes.
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("one\nˇthree");
    assert_eq!(kills.borrow().len(), 1);
    assert_eq!(kills.borrow()[0].pieces, vec!["two\n".to_string()]);
}

#[gpui::test]
async fn copying_an_active_empty_region_copies_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one\ntwˇo\nthree");
    mark_mode(&mut cx);
    cx.dispatch_action(CopyRegionOrLine { count: None });
    cx.assert_editor_state("one\ntwˇo\nthree");
    assert!(kills.borrow().is_empty());

    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(false, cx));
    cx.dispatch_action(CopyRegionOrLine { count: None });
    assert_eq!(kills.borrow()[0].pieces, vec!["two\n".to_string()]);
}

#[gpui::test]
async fn copy_labels_pieces_after_overlapping_carets_merge(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    // Two plain carets on one line merge into one piece, so the region piece is second.
    cx.set_state("ˇaˇa\nbb\nc«c\nˇ»dd");
    cx.dispatch_action(CopyRegionOrLine { count: None });
    let kills = kills.borrow();
    assert_eq!(kills[0].pieces, vec!["aa\n".to_string(), "c\n".to_string()]);
    let labels = kills[0]
        .metadata
        .iter()
        .map(|selection| selection.is_entire_line)
        .collect::<Vec<_>>();
    assert_eq!(labels, vec![true, false]);
}

#[gpui::test]
async fn copy_region_or_line_leaves_the_caret_and_the_text(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(CopyRegionOrLine { count: None });
    cx.assert_editor_state("one\ntwˇo\nthree");
    assert_eq!(kills.borrow()[0].pieces, vec!["two\n".to_string()]);
    assert_eq!(kills.borrow()[0].kind, KillKind::Copy);
    assert!(kills.borrow()[0].metadata[0].is_entire_line);

    // Last line without a newline.
    cx.set_state("one\ntwˇo");
    cx.dispatch_action(CopyRegionOrLine { count: None });
    cx.assert_editor_state("one\ntwˇo");
    assert_eq!(kills.borrow()[1].pieces, vec!["two\n".to_string()]);

    // A region is copied as it is and not as a line.
    cx.set_state("o«ne\ntˇ»wo");
    mark_mode(&mut cx);
    cx.dispatch_action(CopyRegionOrLine { count: None });
    assert_eq!(kills.borrow()[2].pieces, vec!["ne\nt".to_string()]);
    assert!(!kills.borrow()[2].metadata[0].is_entire_line);
    cx.assert_editor_state("o«ne\ntˇ»wo");
}

#[gpui::test]
async fn three_carets_give_three_pieces_and_one_undo_step(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    no_group_interval(&mut cx);
    let kills = capture_kills(&mut cx);

    cx.set_state("aˇa\nbˇb\ncˇc\ndd");
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("ˇdd");
    assert_eq!(
        kills.borrow()[0].pieces,
        vec!["aa\n".to_string(), "bb\n".to_string(), "cc\n".to_string()]
    );

    cx.set_state("oˇne\ntˇwo\nthˇree");
    cx.dispatch_action(KillLine { count: None });
    cx.assert_editor_state("oˇ\ntˇ\nthˇ");
    assert_eq!(
        kills.borrow()[1].pieces,
        vec!["ne".to_string(), "wo".to_string(), "ree".to_string()]
    );
    cx.dispatch_action(Undo);
    cx.assert_editor_state("oˇne\ntˇwo\nthˇree");
}

#[gpui::test]
async fn a_read_only_editor_still_copies_but_never_kills(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);
    cx.set_state("one\ntwˇo\nthree");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));

    cx.dispatch_action(CopyRegionOrLine { count: None });
    assert_eq!(kills.borrow().len(), 1);
    assert_eq!(kills.borrow()[0].pieces, vec!["two\n".to_string()]);

    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.dispatch_action(KillLine { count: None });
    assert_eq!(kills.borrow().len(), 1);
    cx.assert_editor_state("one\ntwˇo\nthree");
}

#[gpui::test]
async fn kill_region_or_line_handles_multi_byte_text(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("héllo\nwörˇld\nñ");
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("héllo\nˇñ");
    assert_eq!(kills.borrow()[0].pieces, vec!["wörld\n".to_string()]);

    cx.set_state("日本\n語ˇ");
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.assert_editor_state("日本ˇ");
    assert_eq!(kills.borrow()[1].pieces, vec!["語\n".to_string()]);
}

#[gpui::test]
async fn kill_region_or_line_ends_mark_mode_without_a_region(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("one\ntˇwo\nthree");
    mark_mode(&mut cx);
    cx.dispatch_action(KillRegionOrLine { count: None });
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
}

#[gpui::test]
async fn kill_whole_line_kills_the_line_with_its_newline_from_anywhere_on_it(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    for state in ["one\ntˇwo\nthree", "one\nˇtwo\nthree", "one\ntwoˇ\nthree"] {
        cx.set_state(state);
        cx.dispatch_action(KillWholeLine { count: None });
        cx.assert_editor_state("one\nˇthree");
        let kills = kills.borrow();
        let kill = kills.last().unwrap();
        assert_eq!(kill.pieces, vec!["two\n".to_string()]);
        assert_eq!(kill.kind, KillKind::Kill);
        assert_eq!(kill.command, "emacs::KillWholeLine");
        assert!(kill.metadata[0].is_entire_line);
    }
}

#[gpui::test]
async fn kill_whole_line_counts_three_zero_and_negative(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one\ntˇwo\nthree\nfour\nfive");
    cx.dispatch_action(KillWholeLine { count: Some(3) });
    cx.assert_editor_state("one\nˇfive");
    assert_eq!(
        kills.borrow().last().unwrap().pieces,
        vec!["two\nthree\nfour\n".to_string()]
    );

    // Zero kills the text of the line and leaves its newline.
    cx.set_state("one\ntˇwo\nthree");
    cx.dispatch_action(KillWholeLine { count: Some(0) });
    cx.assert_editor_state("one\nˇ\nthree");
    assert_eq!(
        kills.borrow().last().unwrap().pieces,
        vec!["two".to_string()]
    );

    // A negative count takes the line and the lines above it.
    cx.set_state("aa\nbb\ncˇc\ndd");
    cx.dispatch_action(KillWholeLine { count: Some(-2) });
    cx.assert_editor_state("aa\nˇdd");
    let kills = kills.borrow();
    let kill = kills.last().unwrap();
    assert_eq!(kill.pieces, vec!["bb\ncc\n".to_string()]);
    assert_eq!(kill.direction, KillDirection::Backward);
}
