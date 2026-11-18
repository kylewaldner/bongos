use std::collections::BTreeSet;
use std::ops::Range;

use editor::actions::{SelectDown, SelectRight, SelectToEndOfLine};
use editor::test::editor_test_context::EditorTestContext;
use gpui::{Action, TestAppContext};
use multi_buffer::{MultiBufferOffset, ToOffset as _, ToPoint as _};

use crate::actions::{DeleteRectangle, ForwardWord, KeyboardQuit, RectangleMarkMode};
use crate::apply_tests::init_test;
use crate::mark::ring_for;
use crate::rectangle_mode::{is_on, motions};
use crate::rectangle_tests::{mark_offset, set_rectangle};

/// A long row, a row short of the left column, a row ending inside the rectangle and the
/// point's row.
const BLOCK: &str = "abcdefgh\na\nabcd\nabcdeˇfgh";

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    EditorTestContext::new(cx).await
}

fn mode_on(cx: &mut EditorTestContext) -> bool {
    cx.update_editor(|editor, _, _| is_on(editor))
}

fn mark_mode(cx: &mut EditorTestContext) -> bool {
    cx.update_editor(|editor, _, _| editor.selection_mark_mode())
}

/// Sets `marked` with the mark at `mark` and an active region, then turns the mode on.
fn start(cx: &mut EditorTestContext, marked: &str, mark: usize) {
    set_rectangle(cx, marked, mark);
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(RectangleMarkMode);
}

/// The newest selection's tail and head, as buffer offsets.
fn newest(cx: &mut EditorTestContext) -> (usize, usize) {
    cx.update_editor(|editor, _, cx| {
        let display = editor.display_snapshot(cx);
        let newest = editor.selections.newest::<MultiBufferOffset>(&display);
        let (start, end) = (newest.start.0, newest.end.0);
        if newest.reversed {
            (end, start)
        } else {
            (start, end)
        }
    })
}

/// The past-line-end highlights as buffer rows and cell ranges.
fn highlights(cx: &mut EditorTestContext) -> Vec<(u32, Range<u32>)> {
    cx.update_editor(|editor, _, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .past_line_end_highlights()
            .iter()
            .map(|(anchor, cells)| (anchor.to_point(&snapshot).row, cells.clone()))
            .collect()
    })
}

fn ring(cx: &mut EditorTestContext) -> Vec<usize> {
    let editor = cx.editor.clone();
    cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        ring_for(&editor, cx)
            .into_iter()
            .map(|anchor| anchor.to_offset(&snapshot).0)
            .collect()
    })
}

#[gpui::test]
async fn the_toggle_pushes_the_mark_at_point_unless_the_region_is_active(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    set_rectangle(&mut cx, "abcˇdefgh", 0);
    cx.dispatch_action(RectangleMarkMode);
    assert!(mode_on(&mut cx));
    assert!(mark_mode(&mut cx));
    assert_eq!(mark_offset(&mut cx), Some(3));
    assert_eq!(ring(&mut cx), vec![0]);
    cx.assert_editor_state("abcˇdefgh");

    cx.dispatch_action(RectangleMarkMode);
    assert!(!mode_on(&mut cx));

    set_rectangle(&mut cx, "abcdefgh\nabcˇdefgh", 1);
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    let ring_before = ring(&mut cx);
    cx.dispatch_action(RectangleMarkMode);
    assert!(mode_on(&mut cx));
    assert_eq!(mark_offset(&mut cx), Some(1));
    assert_eq!(ring(&mut cx), ring_before);
    cx.assert_editor_state("a«bcˇ»defgh\na«bcˇ»defgh");
}

#[gpui::test]
async fn toggling_builds_one_selection_per_row_reaching_the_left_column(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    start(&mut cx, BLOCK, 2);
    cx.assert_editor_state("ab«cdeˇ»fgh\na\nab«cdˇ»\nab«cdeˇ»fgh");
    assert_eq!(newest(&mut cx), (18, 21));
}

#[gpui::test]
async fn past_end_highlights_cover_the_short_rows(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    start(&mut cx, BLOCK, 2);
    assert_eq!(highlights(&mut cx), vec![(1, 1..4), (2, 0..1)]);
    cx.dispatch_action(RectangleMarkMode);
    assert!(highlights(&mut cx).is_empty());
}

#[gpui::test]
async fn motions_move_the_point_corner_and_rebuild(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    start(&mut cx, "abcdefgh\nabcˇdefgh\nabcd efgh ij", 1);
    cx.assert_editor_state("a«bcˇ»defgh\na«bcˇ»defgh\nabcd efgh ij");
    cx.dispatch_action(SelectDown);
    cx.assert_editor_state("a«bcˇ»defgh\na«bcˇ»defgh\na«bcˇ»d efgh ij");
    cx.dispatch_action(SelectRight);
    cx.assert_editor_state("a«bcdˇ»efgh\na«bcdˇ»efgh\na«bcdˇ» efgh ij");
    cx.dispatch_action(ForwardWord {
        count: None,
        select: true,
    });
    cx.assert_editor_state("a«bcdefghˇ»\na«bcdefghˇ»\na«bcd efghˇ» ij");
    cx.dispatch_action(SelectToEndOfLine::default());
    cx.assert_editor_state("a«bcdefghˇ»\na«bcdefghˇ»\na«bcd efgh ijˇ»");
    assert!(mode_on(&mut cx));
    assert_eq!(newest(&mut cx), (19, 30));
}

#[gpui::test]
async fn a_reversed_rectangle_puts_the_heads_on_the_left(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    start(&mut cx, "abcdefgh\nabcdefgh\nabˇcdefgh", 5);
    cx.assert_editor_state("ab«ˇcde»fgh\nab«ˇcde»fgh\nab«ˇcde»fgh");
    assert_eq!(newest(&mut cx), (23, 20));
}

#[gpui::test]
async fn typing_edits_every_long_row_and_ends_the_mode(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler as _;
    let mut cx = test_context(cx).await;
    start(&mut cx, BLOCK, 2);
    cx.update_editor(|editor, window, cx| editor.replace_text_in_range(None, "X", window, cx));
    cx.run_until_parked();
    cx.assert_editor_state("abcdeXˇfgh\na\nabcdXˇ\nabcdeXˇfgh");
    assert!(!mode_on(&mut cx));
    assert!(highlights(&mut cx).is_empty());
}

#[gpui::test]
async fn keyboard_quit_ends_the_mode_and_keeps_the_mark(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    start(&mut cx, BLOCK, 2);
    cx.dispatch_action(KeyboardQuit);
    assert!(!mode_on(&mut cx));
    assert!(!mark_mode(&mut cx));
    assert!(highlights(&mut cx).is_empty());
    assert_eq!(mark_offset(&mut cx), Some(2));
    cx.assert_editor_state("abcdefgh\na\nabcd\nabcdeˇfgh");
}

#[gpui::test]
async fn a_rectangle_command_uses_the_corners_of_the_mode(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    start(&mut cx, "abcdefgh\nabcˇdefgh\nabcdefgh", 1);
    cx.dispatch_action(SelectDown);
    cx.dispatch_action(DeleteRectangle);
    cx.assert_editor_state("aˇdefgh\nadefgh\nadefgh");
    assert!(!mode_on(&mut cx));
    assert!(highlights(&mut cx).is_empty());
}

/// The action names of the region-selection block of an Emacs keymap.
fn selection_mode_actions(keymap: &str) -> BTreeSet<String> {
    let sections: serde_json::Value = settings::parse_json_with_comments(keymap).unwrap();
    let block = sections
        .as_array()
        .unwrap()
        .iter()
        .find(|section| section["context"] == "Editor && selection_mode")
        .expect("the keymap has a region-selection block");
    block["bindings"]
        .as_object()
        .unwrap()
        .values()
        .map(|action| match action {
            serde_json::Value::Array(parts) => parts[0].as_str().unwrap().to_string(),
            other => other.as_str().unwrap().to_string(),
        })
        .collect()
}

#[test]
fn the_motions_are_every_action_of_the_region_selection_block() {
    let listed: BTreeSet<String> = motions()
        .into_iter()
        .map(|(_, name)| name.to_string())
        .collect();
    assert_eq!(listed.len(), motions().len());
    for keymap in [
        include_str!("../../../assets/keymaps/linux/emacs.json"),
        include_str!("../../../assets/keymaps/macos/emacs.json"),
    ] {
        let mut actions = selection_mode_actions(keymap);
        assert!(actions.remove(KeyboardQuit.name()));
        assert_eq!(actions, listed);
    }
}
