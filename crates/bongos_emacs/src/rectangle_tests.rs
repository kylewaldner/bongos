use std::cell::RefCell;
use std::rc::Rc;

use editor::actions::{Redo, Undo};
use editor::test::editor_test_context::EditorTestContext;
use editor::{Editor, EditorMode};
use gpui::{Action, AppContext as _, TestAppContext};
use multi_buffer::{MultiBuffer, MultiBufferOffset};
use text::{Point, ToOffset as _};
use workspace::MinibufferRouting;

use crate::actions::{
    ClearRectangle, DeleteRectangle, KeepRectangle, OpenRectangle, RectangleMarkMode,
    SetMarkCommand,
};
use crate::apply_tests::{capture_kills, init_test};
use crate::mark::{mark_for, set_mark};

/// Three short, ending-inside and spanning rows between two full ones.
const BLOCK: &str = "abcdefgh\na\nabcde\nabcdeˇfgh";

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    EditorTestContext::new(cx).await
}

/// Sets the state, then the mark at `mark` (a buffer offset).
pub(crate) fn set_rectangle(cx: &mut EditorTestContext, marked: &str, mark: usize) {
    cx.set_state(marked);
    let editor = cx.editor.clone();
    cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let at = snapshot.anchor_before(MultiBufferOffset(mark));
        assert!(set_mark(&editor, at, cx));
    });
}

/// The mark as a buffer offset.
pub(crate) fn mark_offset(cx: &mut EditorTestContext) -> Option<usize> {
    let editor = cx.editor.clone();
    cx.update(|_, cx| {
        let mark = mark_for(&editor, cx)?;
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let (text_anchor, buffer) = snapshot.anchor_to_buffer_anchor(mark)?;
        Some(text_anchor.to_offset(buffer))
    })
}

pub(crate) fn echo(cx: &mut EditorTestContext) -> Rc<RefCell<Vec<String>>> {
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

/// The buffer's version and newest undo entry, to see that nothing was recorded.
pub(crate) fn history(cx: &mut EditorTestContext) -> impl PartialEq + use<> {
    cx.update_buffer(|buffer, _| {
        (
            buffer.version(),
            buffer.peek_undo_stack().map(|entry| entry.transaction_id()),
        )
    })
}

/// Dispatches `action` to the focused editor.
fn dispatch(cx: &mut EditorTestContext, action: &dyn Action) {
    cx.update(|window, cx| window.dispatch_action(action.boxed_clone(), cx));
    cx.run_until_parked();
}

#[gpui::test]
async fn each_command_gives_the_planned_text_and_point(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let cases: [(&dyn Action, &str); 4] = [
        (&DeleteRectangle, "abˇfgh\na\nab\nabfgh"),
        (&OpenRectangle, "abˇ   cdefgh\na    \nab   cde\nab   cdefgh"),
        (&ClearRectangle, "abˇ   fgh\na\nab   \nab   fgh"),
        (&KeepRectangle, "ˇcde\n\ncde\ncde"),
    ];
    for (action, expected) in cases {
        set_rectangle(&mut cx, BLOCK, 2);
        dispatch(&mut cx, action);
        cx.assert_editor_state(expected);
    }
}

#[gpui::test]
async fn one_undo_restores_the_buffer_and_carets_and_redo_reapplies(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    set_rectangle(&mut cx, BLOCK, 2);
    cx.dispatch_action(DeleteRectangle);
    cx.assert_editor_state("abˇfgh\na\nab\nabfgh");
    cx.dispatch_action(Undo);
    cx.assert_editor_state(BLOCK);
    cx.dispatch_action(Redo);
    assert_eq!(cx.buffer_text(), "abfgh\na\nab\nabfgh");
}

#[gpui::test]
async fn without_a_mark_nothing_changes(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let messages = echo(&mut cx);
    cx.set_state(BLOCK);
    let before = history(&mut cx);
    cx.dispatch_action(DeleteRectangle);
    cx.dispatch_action(KeepRectangle);
    cx.assert_editor_state(BLOCK);
    assert!(history(&mut cx) == before);
    assert_eq!(
        *messages.borrow(),
        vec!["The mark is not set", "The mark is not set"]
    );
}

#[gpui::test]
async fn secondary_carets_are_dropped_and_come_back_on_undo(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let marked = "ˇabcdefgh\naˇ\nabcde\nabcdeˇfgh";
    set_rectangle(&mut cx, marked, 2);
    cx.dispatch_action(DeleteRectangle);
    cx.assert_editor_state("abˇfgh\na\nab\nabfgh");
    cx.dispatch_action(Undo);
    cx.assert_editor_state(marked);
}

#[gpui::test]
async fn a_read_only_buffer_is_refused(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let messages = echo(&mut cx);
    set_rectangle(&mut cx, BLOCK, 2);
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    let before = history(&mut cx);
    cx.dispatch_action(DeleteRectangle);
    cx.dispatch_action(OpenRectangle);
    cx.assert_editor_state(BLOCK);
    assert!(history(&mut cx) == before);
    assert_eq!(
        *messages.borrow(),
        vec!["Buffer is read-only", "Buffer is read-only"]
    );
}

#[gpui::test]
async fn tab_and_wide_rows_are_cut_at_the_displayed_column(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    // Columns 2 to 5: the tab (columns 1 to 4) is cut, the wide characters at columns 2 and 4
    // start inside.
    let marked = "abcdefgh\na\tbcd\n日本語x\nabcdeˇfgh";
    set_rectangle(&mut cx, marked, 2);
    cx.dispatch_action(DeleteRectangle);
    cx.assert_editor_state("abˇfgh\na cd\n日x\nabfgh");

    set_rectangle(&mut cx, marked, 2);
    cx.dispatch_action(ClearRectangle);
    cx.assert_editor_state("abˇ   fgh\na    cd\n日    x\nab   fgh");
}

#[gpui::test]
async fn the_mark_stays_where_it_was_and_mark_mode_ends(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("abˇcdefgh\na\nabcde\nabcdefgh");
    cx.dispatch_action(SetMarkCommand { count: None });
    assert!(cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
    cx.set_selections_state(BLOCK);
    cx.dispatch_action(OpenRectangle);
    cx.assert_editor_state("abˇ   cdefgh\na    \nab   cde\nab   cdefgh");
    assert_eq!(mark_offset(&mut cx), Some(2));
    assert!(!cx.update_editor(|editor, _, _| editor.selection_mark_mode()));
}

#[gpui::test]
async fn a_mark_in_another_excerpt_changes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let messages = echo(&mut cx);
    let editor = cx.update(|window, cx| {
        let multibuffer = MultiBuffer::build_multi(
            [(
                "abcd\nx\ny\nz\nefgh",
                vec![
                    Point::new(0, 0)..Point::new(0, 4),
                    Point::new(4, 0)..Point::new(4, 4),
                ],
            )],
            cx,
        );
        cx.new(|cx| Editor::new(EditorMode::full(), multibuffer, None, window, cx))
    });
    cx.update(|window, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let mark = snapshot.anchor_before(Point::new(0, 1));
        assert!(set_mark(&editor, mark, cx));
        editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges([Point::new(1, 3)..Point::new(1, 3)])
            })
        });
        crate::rectangle::delete_rectangle_command(&DeleteRectangle, &editor, window, cx);
        assert_eq!(editor.read(cx).text(cx), "abcd\nefgh");
        let buffers = editor.read(cx).buffer().read(cx).all_buffers();
        let buffer = buffers.iter().next().unwrap();
        assert_eq!(buffer.read(cx).text(), "abcd\nx\ny\nz\nefgh");
    });
    assert_eq!(
        *messages.borrow(),
        vec!["The rectangle spans more than one excerpt"]
    );
}

#[gpui::test]
async fn a_zero_width_rectangle_edits_only_with_keep(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let marked = "abcdefgh\na\nabcde\nabˇcdefgh";
    let actions: [&dyn Action; 3] = [&DeleteRectangle, &OpenRectangle, &ClearRectangle];
    for action in actions {
        set_rectangle(&mut cx, marked, 2);
        let before = history(&mut cx);
        dispatch(&mut cx, action);
        assert_eq!(cx.buffer_text(), "abcdefgh\na\nabcde\nabcdefgh");
        assert!(history(&mut cx) == before, "{} made an edit", action.name());
    }

    set_rectangle(&mut cx, marked, 2);
    cx.dispatch_action(KeepRectangle);
    cx.assert_editor_state("ˇ\n\n\n");
}

#[gpui::test]
async fn the_other_rectangle_commands_are_not_implemented_yet(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let messages = echo(&mut cx);
    let kills = capture_kills(&mut cx);
    set_rectangle(&mut cx, BLOCK, 2);
    dispatch(&mut cx, &RectangleMarkMode);
    cx.assert_editor_state(BLOCK);
    assert_eq!(*messages.borrow(), vec!["Not implemented yet"]);
    assert!(kills.borrow().is_empty());
}
