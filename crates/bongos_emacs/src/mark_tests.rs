use std::cell::RefCell;
use std::rc::Rc;

use editor::actions::{SelectAll, SelectRight};
use editor::test::editor_test_context::EditorTestContext;
use editor::{Editor, EditorMode};
use gpui::{AppContext as _, Entity, Modifiers, MouseButton, TestAppContext};
use multi_buffer::MultiBuffer;
use text::ToOffset as _;
use workspace::MinibufferRouting;

use crate::actions::{ExchangePointAndMark, ForwardWord, KeyboardQuit, SetMarkCommand};

use crate::apply_tests::init_test;
use crate::mark::{mark_for, push_mark, ring_for};

async fn test_context(cx: &mut TestAppContext, marked: &str) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state(marked);
    cx
}

fn push(cx: &mut EditorTestContext) -> bool {
    let editor = cx.editor.clone();
    cx.update(|window, cx| push_mark(&editor, None, false, window, cx))
}

fn offset_of(editor: &Entity<Editor>, anchor: multi_buffer::Anchor, cx: &gpui::App) -> usize {
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let (text_anchor, buffer) = snapshot.anchor_to_buffer_anchor(anchor).unwrap();
    text_anchor.to_offset(buffer)
}

/// The mark and the ring (newest first) of the editor's buffer, as offsets.
fn marks(cx: &mut EditorTestContext) -> (Option<usize>, Vec<usize>) {
    let editor = cx.editor.clone();
    cx.update(|_, cx| {
        let mark = mark_for(&editor, cx).map(|anchor| offset_of(&editor, anchor, cx));
        let ring = ring_for(&editor, cx)
            .into_iter()
            .map(|anchor| offset_of(&editor, anchor, cx))
            .collect();
        (mark, ring)
    })
}

#[gpui::test]
async fn push_mark_twice_puts_the_first_mark_on_the_ring(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    assert_eq!(marks(&mut cx), (None, vec![]));

    assert!(push(&mut cx));
    assert_eq!(marks(&mut cx), (Some(0), vec![]));

    cx.set_selections_state("one ˇtwo three");
    assert!(push(&mut cx));
    assert_eq!(marks(&mut cx), (Some(4), vec![0]));

    // The same place again neither duplicates the ring entry nor puts the mark on the ring.
    assert!(push(&mut cx));
    assert_eq!(marks(&mut cx), (Some(4), vec![0]));

    cx.set_selections_state("one two ˇthree");
    assert!(push(&mut cx));
    assert_eq!(marks(&mut cx), (Some(8), vec![4, 0]));
}

#[gpui::test]
async fn push_mark_at_a_given_anchor(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo three").await;
    let editor = cx.editor.clone();
    let at = cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        snapshot.anchor_before(multi_buffer::MultiBufferOffset(8))
    });
    assert!(cx.update(|window, cx| push_mark(&editor, Some(at), false, window, cx)));
    assert_eq!(marks(&mut cx), (Some(8), vec![]));
}

#[gpui::test]
async fn the_mark_survives_an_insertion_before_it(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo three").await;
    assert!(push(&mut cx));
    cx.update_buffer(|buffer, cx| buffer.edit([(0..0, "XX")], None, cx));
    assert_eq!(marks(&mut cx), (Some(6), vec![]));

    cx.update_buffer(|buffer, cx| buffer.edit([(0..2, "")], None, cx));
    assert_eq!(marks(&mut cx), (Some(4), vec![]));
}

#[gpui::test]
async fn deleting_the_text_of_a_ring_entry_drops_it_but_not_the_mark(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one tˇwo three").await;
    assert!(push(&mut cx));
    cx.set_selections_state("one two thrˇee");
    assert!(push(&mut cx));
    assert_eq!(marks(&mut cx), (Some(11), vec![5]));

    // Delete "two" and its space: the ring entry sat inside it.
    cx.update_buffer(|buffer, cx| buffer.edit([(3..7, "")], None, cx));
    assert_eq!(marks(&mut cx), (Some(7), vec![]));

    // The mark itself is never dropped: its character goes and it stays where it was.
    cx.update_buffer(|buffer, cx| buffer.edit([(4..7, "")], None, cx));
    assert_eq!(marks(&mut cx), (Some(4), vec![]));
}

#[gpui::test]
async fn two_editors_on_one_buffer_share_the_mark_and_the_ring(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo three").await;
    assert!(push(&mut cx));
    cx.set_selections_state("one two ˇthree");
    assert!(push(&mut cx));

    let buffer = cx.multibuffer(|multibuffer, _| multibuffer.as_singleton().unwrap());
    let second = cx.update(|window, cx| {
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        cx.new(|cx| Editor::new(EditorMode::full(), multibuffer, None, window, cx))
    });
    let (mark, ring) = cx.update(|_, cx| {
        (
            mark_for(&second, cx).map(|anchor| offset_of(&second, anchor, cx)),
            ring_for(&second, cx)
                .into_iter()
                .map(|anchor| offset_of(&second, anchor, cx))
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!((mark, ring), (Some(8), vec![4]));

    // A push from the second editor lands in the shared ring.
    cx.update(|window, cx| {
        assert!(push_mark(&second, None, false, window, cx));
    });
    assert_eq!(marks(&mut cx), (Some(0), vec![8, 4]));
}

#[gpui::test]
async fn push_mark_with_mark_mode_on_does_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    assert!(push(&mut cx));
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.set_selections_state("one ˇtwo three");
    assert!(!push(&mut cx));
    assert_eq!(marks(&mut cx), (Some(0), vec![]));

    // Activating still pushes.
    let editor = cx.editor.clone();
    assert!(cx.update(|window, cx| push_mark(&editor, None, true, window, cx)));
    assert_eq!(marks(&mut cx), (Some(4), vec![0]));
}

#[gpui::test]
async fn activating_turns_mark_mode_on(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo").await;
    let editor = cx.editor.clone();
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
    assert!(cx.update(|window, cx| push_mark(&editor, None, true, window, cx)));
    assert!(cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}

#[gpui::test]
async fn a_mouse_drag_sets_the_mark_at_the_drag_start_without_a_push(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    assert!(push(&mut cx));
    cx.set_selections_state("ˇone two three");
    cx.run_until_parked();
    // An earlier mark would land on the ring if the drag pushed.
    let start = cx.pixel_position("one ˇtwo three");
    let end = cx.pixel_position("one two ˇthree");
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
    assert!(cx.update_editor(|editor, _, _| editor.selections.pending_anchor().is_some()));
    assert_eq!(marks(&mut cx), (Some(4), vec![]));
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    assert_eq!(marks(&mut cx), (Some(4), vec![]));
    cx.assert_editor_state("one «two ˇ»three");
}

#[gpui::test]
async fn a_click_without_a_drag_does_not_set_the_mark(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    let position = cx.pixel_position("one ˇtwo three");
    cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::none());
    assert_eq!(marks(&mut cx), (None, vec![]));
}

#[gpui::test]
async fn shift_selection_and_select_all_do_not_set_the_mark(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    cx.dispatch_action(SelectRight);
    cx.dispatch_action(SelectRight);
    cx.assert_editor_state("«onˇ»e two three");
    assert_eq!(marks(&mut cx), (None, vec![]));

    cx.dispatch_action(SelectAll);
    assert_eq!(marks(&mut cx), (None, vec![]));

    // A mark set earlier is left where it was.
    cx.set_selections_state("one ˇtwo three");
    assert!(push(&mut cx));
    cx.dispatch_action(SelectRight);
    cx.dispatch_action(SelectAll);
    assert_eq!(marks(&mut cx), (Some(4), vec![]));
}

fn echo(cx: &mut EditorTestContext) -> Rc<RefCell<Vec<String>>> {
    let messages = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    messages
}

fn set_mark_command(cx: &mut EditorTestContext) {
    cx.dispatch_action(SetMarkCommand { count: None });
}

/// A forward word as the keymap sends it in selection mode, with `select`.
fn forward_word(cx: &mut EditorTestContext) {
    cx.dispatch_action(ForwardWord {
        count: None,
        select: true,
    });
}

#[gpui::test]
async fn set_mark_command_sets_the_mark_and_the_region_follows_motions(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    let messages = echo(&mut cx);
    set_mark_command(&mut cx);
    assert_eq!(*messages.borrow(), vec!["Mark set"]);
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
    assert!(cx.update_editor(|editor, _, _| editor.selection_mark_mode()));

    forward_word(&mut cx);
    cx.assert_editor_state("«oneˇ» two three");
    forward_word(&mut cx);
    cx.assert_editor_state("«one twoˇ» three");
}

#[gpui::test]
async fn set_mark_command_twice_deactivates_and_keeps_the_mark(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    let messages = echo(&mut cx);
    set_mark_command(&mut cx);
    forward_word(&mut cx);
    set_mark_command(&mut cx);
    assert_eq!(*messages.borrow(), vec!["Mark set", "Mark deactivated"]);
    cx.assert_editor_state("oneˇ two three");
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
    assert_eq!(marks(&mut cx), (Some(0), vec![]));

    // C-SPC C-SPC leaves the mark set and the previous mark on the ring.
    cx.set_selections_state("one tˇwo three");
    set_mark_command(&mut cx);
    assert_eq!(marks(&mut cx), (Some(5), vec![0]));
    set_mark_command(&mut cx);
    assert_eq!(marks(&mut cx), (Some(5), vec![0]));
    cx.assert_editor_state("one tˇwo three");

    // C-x C-x with the mark at the cursor selects nothing and turns mark mode on.
    cx.dispatch_action(ExchangePointAndMark);
    cx.assert_editor_state("one tˇwo three");
    assert!(cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}

#[gpui::test]
async fn exchange_after_deactivation_restores_the_region(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    set_mark_command(&mut cx);
    forward_word(&mut cx);
    set_mark_command(&mut cx);
    cx.dispatch_action(ExchangePointAndMark);
    cx.assert_editor_state("«ˇone» two three");
    assert_eq!(marks(&mut cx), (Some(3), vec![]));
}

#[gpui::test]
async fn exchange_swaps_the_ends_of_an_active_region(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    set_mark_command(&mut cx);
    forward_word(&mut cx);
    cx.dispatch_action(ExchangePointAndMark);
    cx.assert_editor_state("«ˇone» two three");
    assert_eq!(marks(&mut cx), (Some(3), vec![]));
    cx.dispatch_action(ExchangePointAndMark);
    cx.assert_editor_state("«oneˇ» two three");
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
}

#[gpui::test]
async fn a_mouse_made_selection_swaps_its_ends(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    let start = cx.pixel_position("one ˇtwo three");
    let end = cx.pixel_position("one two ˇthree");
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    cx.assert_editor_state("one «two ˇ»three");
    cx.dispatch_action(ExchangePointAndMark);
    cx.assert_editor_state("one «ˇtwo »three");
    assert_eq!(marks(&mut cx), (Some(8), vec![]));
}

#[gpui::test]
async fn deactivating_a_selection_made_outside_mark_mode_remembers_its_tail(
    cx: &mut TestAppContext,
) {
    let mut cx = test_context(cx, "one «twoˇ» three").await;
    set_mark_command(&mut cx);
    cx.assert_editor_state("one twoˇ three");
    assert_eq!(marks(&mut cx), (Some(4), vec![]));
    cx.dispatch_action(ExchangePointAndMark);
    cx.assert_editor_state("one «ˇtwo» three");
}

#[gpui::test]
async fn three_cursors_extend_three_regions_and_swap_all_three(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone\nˇtwo\nˇthree").await;
    set_mark_command(&mut cx);
    forward_word(&mut cx);
    cx.assert_editor_state("«oneˇ»\n«twoˇ»\n«threeˇ»");
    cx.dispatch_action(ExchangePointAndMark);
    cx.assert_editor_state("«ˇone»\n«ˇtwo»\n«ˇthree»");
}

#[gpui::test]
async fn exchange_without_a_mark_says_so_and_changes_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo").await;
    let messages = echo(&mut cx);
    cx.dispatch_action(ExchangePointAndMark);
    assert_eq!(*messages.borrow(), vec!["No mark set in this buffer"]);
    cx.assert_editor_state("one ˇtwo");
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}

#[gpui::test]
async fn exchange_without_a_region_jumps_to_the_mark_and_never_pushes(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    set_mark_command(&mut cx);
    set_mark_command(&mut cx);
    cx.set_selections_state("one two ˇthree");
    cx.dispatch_action(ExchangePointAndMark);
    cx.assert_editor_state("«ˇone two »three");
    assert_eq!(marks(&mut cx), (Some(8), vec![]));
}

#[gpui::test]
async fn keyboard_quit_deactivates_the_region(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    set_mark_command(&mut cx);
    forward_word(&mut cx);
    cx.dispatch_action(KeyboardQuit);
    cx.assert_editor_state("oneˇ two three");
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}

#[gpui::test]
async fn set_mark_command_with_a_count_sets_without_an_echo(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo").await;
    let messages = echo(&mut cx);
    cx.dispatch_action(SetMarkCommand { count: Some(4) });
    assert!(messages.borrow().is_empty());
    assert_eq!(marks(&mut cx), (Some(4), vec![]));
    assert!(cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}
