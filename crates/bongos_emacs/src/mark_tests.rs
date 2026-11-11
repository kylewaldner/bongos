use std::cell::RefCell;
use std::rc::Rc;

use editor::actions::{SelectAll, SelectRight};
use editor::test::editor_test_context::EditorTestContext;
use editor::{Editor, EditorMode};
use gpui::{AppContext as _, Entity, Modifiers, MouseButton, TestAppContext};
use multi_buffer::MultiBuffer;
use text::ToOffset as _;
use workspace::MinibufferRouting;

use crate::actions::{ExchangePointAndMark, ForwardWord, KeyboardQuit, PopMark, SetMarkCommand};

use crate::apply_tests::init_test;
use crate::mark::{mark_for, push_mark, ring_for, set_mark};

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

/// Sets the mark at the cursor and deactivates the region (C-SPC C-SPC).
fn drop_mark(cx: &mut EditorTestContext) {
    set_mark_command(cx);
    set_mark_command(cx);
}

fn mark_mode(cx: &mut EditorTestContext) -> bool {
    cx.update_editor(|editor, _, _| editor.selection_mark_mode())
}

/// Marks at 0, 4 and 8 of "one two three", the cursor at the end: the mark is 8, the ring 4, 0.
async fn three_marks(cx: &mut TestAppContext) -> EditorTestContext {
    let mut cx = test_context(cx, "ˇone two three").await;
    drop_mark(&mut cx);
    cx.set_selections_state("one ˇtwo three");
    drop_mark(&mut cx);
    cx.set_selections_state("one two ˇthree");
    drop_mark(&mut cx);
    cx.set_selections_state("one two threeˇ");
    assert_eq!(marks(&mut cx), (Some(8), vec![4, 0]));
    cx
}

#[gpui::test]
async fn pop_mark_returns_to_the_mark_and_ends_mark_mode(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    drop_mark(&mut cx);
    cx.set_selections_state("one two ˇthree");
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("ˇone two three");
    assert!(!mark_mode(&mut cx));
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
}

#[gpui::test]
async fn pop_mark_clears_an_active_region(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    drop_mark(&mut cx);
    forward_word(&mut cx);
    set_mark_command(&mut cx);
    cx.set_selections_state("one ˇtwo three");
    set_mark_command(&mut cx);
    forward_word(&mut cx);
    cx.assert_editor_state("one «twoˇ» three");
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one ˇtwo three");
    assert!(!mark_mode(&mut cx));
}

#[gpui::test]
async fn set_mark_command_after_a_pop_keeps_popping(cx: &mut TestAppContext) {
    let mut cx = three_marks(cx).await;
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one two ˇthree");
    assert_eq!(marks(&mut cx), (Some(4), vec![0]));
    set_mark_command(&mut cx);
    cx.assert_editor_state("one ˇtwo three");
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
    set_mark_command(&mut cx);
    cx.assert_editor_state("ˇone two three");
    assert!(!mark_mode(&mut cx));
}

#[gpui::test]
async fn another_command_breaks_the_pop_chain(cx: &mut TestAppContext) {
    let mut cx = three_marks(cx).await;
    let messages = echo(&mut cx);
    cx.dispatch_action(PopMark);
    cx.dispatch_action(ForwardWord {
        count: None,
        select: false,
    });
    cx.assert_editor_state("one two threeˇ");
    set_mark_command(&mut cx);
    assert_eq!(*messages.borrow(), vec!["Mark set"]);
    assert!(mark_mode(&mut cx));
    assert_eq!(marks(&mut cx), (Some(13), vec![4, 0]));
}

#[gpui::test]
async fn a_set_mark_after_a_set_mark_does_not_pop(cx: &mut TestAppContext) {
    let mut cx = three_marks(cx).await;
    cx.dispatch_action(PopMark);
    cx.dispatch_action(PopMark);
    cx.dispatch_action(PopMark);
    // The chain ended with the pops; a new mark set and deactivated is not a pop.
    cx.dispatch_action(ForwardWord {
        count: None,
        select: false,
    });
    cx.set_selections_state("one two ˇthree");
    set_mark_command(&mut cx);
    assert!(mark_mode(&mut cx));
    set_mark_command(&mut cx);
    assert!(!mark_mode(&mut cx));
    cx.assert_editor_state("one two ˇthree");
    assert_eq!(marks(&mut cx), (Some(8), vec![0]));
}

#[gpui::test]
async fn a_count_pops_exactly_one_entry(cx: &mut TestAppContext) {
    let mut cx = three_marks(cx).await;
    let messages = echo(&mut cx);
    cx.dispatch_action(SetMarkCommand { count: Some(4) });
    cx.assert_editor_state("one two ˇthree");
    assert!(messages.borrow().is_empty());
    assert!(!mark_mode(&mut cx));
    assert_eq!(marks(&mut cx), (Some(4), vec![0]));
}

#[gpui::test]
async fn three_jumps_pop_back_in_reverse_and_the_fourth_says_the_ring_is_empty(
    cx: &mut TestAppContext,
) {
    let mut cx = three_marks(cx).await;
    let messages = echo(&mut cx);
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one two ˇthree");
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one ˇtwo three");
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("ˇone two three");
    assert!(messages.borrow().is_empty());
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("ˇone two three");
    assert_eq!(*messages.borrow(), vec!["Mark ring is empty"]);
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
}

#[gpui::test]
async fn pop_with_an_empty_ring_and_the_cursor_at_the_mark_says_so(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo").await;
    drop_mark(&mut cx);
    let messages = echo(&mut cx);
    cx.dispatch_action(PopMark);
    assert_eq!(*messages.borrow(), vec!["Mark ring is empty"]);
    cx.assert_editor_state("one ˇtwo");
    assert_eq!(marks(&mut cx), (Some(4), vec![]));
}

#[gpui::test]
async fn pop_skips_ring_entries_at_the_new_cursor_place(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    assert!(push(&mut cx));
    cx.set_selections_state("one ˇtwo three");
    assert!(push(&mut cx));
    cx.set_selections_state("one two ˇthree");
    assert!(push(&mut cx));
    assert_eq!(marks(&mut cx), (Some(8), vec![4, 0]));
    // Put the newest ring entry on the mark.
    let editor = cx.editor.clone();
    cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        set_mark(
            &editor,
            snapshot.anchor_before(multi_buffer::MultiBufferOffset(4)),
            cx,
        )
    });
    assert_eq!(marks(&mut cx), (Some(4), vec![4, 0]));
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one ˇtwo three");
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
}

#[gpui::test]
async fn pop_with_the_cursor_at_the_mark_goes_to_the_next_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two three").await;
    drop_mark(&mut cx);
    cx.set_selections_state("one ˇtwo three");
    drop_mark(&mut cx);
    assert_eq!(marks(&mut cx), (Some(4), vec![0]));
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("ˇone two three");
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
}

#[gpui::test]
async fn pop_skips_dead_ring_entries(cx: &mut TestAppContext) {
    let mut cx = three_marks(cx).await;
    // Delete " two": the ring entry at 4 dies and the mark at 8 moves to 4.
    cx.update_buffer(|buffer, cx| buffer.edit([(3..7, "")], None, cx));
    cx.set_selections_state("one threeˇ");
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one ˇthree");
    assert_eq!(marks(&mut cx), (Some(0), vec![]));
}

#[gpui::test]
async fn pop_moves_only_the_primary_cursor(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "ˇone two\nthree").await;
    drop_mark(&mut cx);
    cx.set_selections_state("one ˇtwo\nthˇree");
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("ˇone ˇtwo\nthree");
    assert!(!mark_mode(&mut cx));
}

#[gpui::test]
async fn pop_without_a_mark_says_so_and_a_following_set_mark_sets(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one ˇtwo").await;
    let messages = echo(&mut cx);
    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one ˇtwo");
    assert_eq!(*messages.borrow(), vec!["No mark set in this buffer"]);
    assert!(!mark_mode(&mut cx));
    set_mark_command(&mut cx);
    assert_eq!(messages.borrow().last().unwrap(), "Mark set");
    assert!(mark_mode(&mut cx));
}

fn type_into(cx: &mut EditorTestContext, range: Option<std::ops::Range<usize>>, text: &str) {
    use gpui::EntityInputHandler as _;
    cx.update_editor(|editor, window, cx| editor.replace_text_in_range(range, text, window, cx));
}

fn set_user_settings(
    cx: &mut EditorTestContext,
    change: impl FnOnce(&mut settings::SettingsContent),
) {
    use gpui::BorrowAppContext as _;
    cx.update(|_, cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, change)
        });
    });
}

#[gpui::test]
async fn typing_over_a_region_inserts_at_point_and_keeps_the_text(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one «twoˇ» three").await;
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    type_into(&mut cx, None, "X");
    cx.assert_editor_state("one twoXˇ three");
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}

#[gpui::test]
async fn typing_over_a_backwards_region_inserts_at_point(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one «ˇtwo» three").await;
    type_into(&mut cx, Some(5..5), "X");
    cx.assert_editor_state("one Xˇtwo three");
}

#[gpui::test]
async fn delete_selection_setting_makes_typing_replace(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one «twoˇ» three").await;
    set_user_settings(&mut cx, |content| {
        content.emacs.get_or_insert_default().delete_selection = Some(true);
    });
    type_into(&mut cx, None, "X");
    cx.assert_editor_state("one Xˇ three");
}

#[gpui::test]
async fn a_non_emacs_base_keymap_makes_typing_replace(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one «twoˇ» three").await;
    set_user_settings(&mut cx, |content| {
        content.base_keymap = Some(settings::BaseKeymapContent::VSCode);
    });
    type_into(&mut cx, None, "X");
    cx.assert_editor_state("one Xˇ three");
}

#[gpui::test]
async fn a_single_line_editor_replaces_the_selection(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let editor = cx.update(|window, cx| cx.new(|cx| Editor::single_line(window, cx)));
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            use gpui::EntityInputHandler as _;
            editor.set_text("hello", window, cx);
            editor.select_all(&SelectAll, window, cx);
            editor.replace_text_in_range(None, "X", window, cx);
            assert_eq!(editor.text(cx), "X");
        });
    });
}

#[gpui::test]
async fn ime_composition_gives_zeds_result(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler as _;
    let mut cx = test_context(cx, "one «twoˇ» three").await;
    cx.update_editor(|editor, window, cx| {
        editor.replace_and_mark_text_in_range(None, "a", None, window, cx)
    });
    cx.assert_editor_state("one aˇ three");
    cx.update_editor(|editor, window, cx| editor.replace_text_in_range(None, "b", window, cx));
    cx.assert_editor_state("one bˇ three");
}

#[gpui::test]
async fn an_explicit_non_empty_range_replaces_as_zed_does(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one two ˇthree").await;
    type_into(&mut cx, Some(4..7), "X");
    cx.assert_editor_state("one X ˇthree");
}

#[gpui::test]
async fn an_empty_explicit_range_collapses_like_none(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one «twoˇ» three").await;
    type_into(&mut cx, Some(2..2), "X");
    cx.assert_editor_state("one twoXˇ three");
}

#[gpui::test]
async fn typing_tab_and_undo_end_mark_mode(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one «twoˇ» three").await;
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    type_into(&mut cx, None, "X");
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));

    cx.set_state("one «twoˇ» three");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(editor::actions::Tab);
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));

    cx.set_state("one «twoˇ» three");
    type_into(&mut cx, None, "Y");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(editor::actions::Undo);
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}

#[gpui::test]
async fn an_edit_in_a_second_editor_leaves_the_first_region(cx: &mut TestAppContext) {
    let mut cx = test_context(cx, "one «twoˇ» three").await;
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    let buffer = cx.multibuffer(|multibuffer, _| multibuffer.as_singleton().unwrap());
    let second = cx.update(|window, cx| {
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        cx.new(|cx| Editor::new(EditorMode::full(), multibuffer, None, window, cx))
    });
    cx.update(|window, cx| {
        second.update(cx, |editor, cx| {
            editor.insert("ZZ", window, cx);
        });
    });
    assert!(cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}
