use editor::actions::ScrollCursorCenterTopBottom;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{KeyBinding, TestAppContext};
use multi_buffer::MultiBufferOffset;

use crate::actions::{Recenter, Reposition};
use crate::apply_tests::init_test;

const LINES: usize = 100;

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-l", Recenter::default(), Some("Editor")),
            KeyBinding::new("ctrl-f", editor::actions::MoveRight, Some("Editor")),
            KeyBinding::new("alt-r", Reposition, Some("Editor")),
        ])
    });
    let text = (0..LINES)
        .map(|ix| format!("line {ix}"))
        .collect::<Vec<_>>()
        .join("\n");
    cx.set_state(&format!("ˇ{text}"));
    cx.update_editor(|editor, _window, cx| {
        editor.set_vertical_scroll_margin(2, cx);
    });
    cx
}

/// The caret row, and its row inside the window.
fn rows(cx: &mut EditorTestContext) -> (u32, u32) {
    cx.update_editor(|editor, _window, cx| {
        let display = editor.display_snapshot(cx);
        let row = editor.selections.newest_display(&display).head().row().0;
        let top = editor.scroll_position(cx).y as u32;
        (row, row - top)
    })
}

fn put_caret(cx: &mut EditorTestContext, row: u32) {
    cx.update_editor(|editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_display_ranges([editor::display_map::DisplayPoint::new(
                editor::display_map::DisplayRow(row),
                0,
            )
                ..editor::display_map::DisplayPoint::new(editor::display_map::DisplayRow(row), 0)])
        })
    });
}

#[gpui::test]
async fn recenter_cycles_middle_top_bottom_middle(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    put_caret(&mut cx, 50);
    let mut seen = Vec::new();
    for _ in 0..4 {
        cx.simulate_keystrokes("ctrl-l");
        seen.push(rows(&mut cx).1);
    }
    // 45 visible rows, margin 2: middle 22, top 2, bottom 42.
    assert_eq!(seen, [22, 2, 42, 22]);
}

#[gpui::test]
async fn recenter_with_a_region_clears_it_and_still_starts_the_cycle(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.update_editor(|editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([MultiBufferOffset(300)..MultiBufferOffset(310)])
        })
    });
    cx.simulate_keystrokes("ctrl-l");
    cx.update_editor(|editor, _, cx| {
        let display = editor.display_snapshot(cx);
        let selection = editor.selections.newest_display(&display);
        assert!(selection.is_empty());
    });
    let first = rows(&mut cx).1;
    cx.simulate_keystrokes("ctrl-l");
    assert_eq!((first, rows(&mut cx).1), (22, 2));
}

#[gpui::test]
async fn another_command_or_a_caret_move_restarts_the_cycle(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    put_caret(&mut cx, 50);
    cx.simulate_keystrokes("ctrl-l ctrl-l");
    assert_eq!(rows(&mut cx).1, 2);
    cx.simulate_keystrokes("ctrl-f ctrl-l");
    assert_eq!(rows(&mut cx).1, 22);

    cx.simulate_keystrokes("ctrl-l");
    assert_eq!(rows(&mut cx).1, 2);
    put_caret(&mut cx, 60);
    cx.simulate_keystrokes("ctrl-l");
    assert_eq!(rows(&mut cx).1, 22);
}

#[gpui::test]
async fn recenter_with_a_count_places_the_line_without_cycling(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    put_caret(&mut cx, 50);
    cx.dispatch_action(Recenter { count: Some(0) });
    // Zero asks for the top row and -1 for the last; the margin keeps two rows clear.
    assert_eq!(rows(&mut cx).1, 2);
    cx.dispatch_action(Recenter { count: Some(-1) });
    assert_eq!(rows(&mut cx).1, 42);
    cx.dispatch_action(Recenter { count: Some(5) });
    assert_eq!(rows(&mut cx).1, 5);
    cx.dispatch_action(Recenter { count: Some(5) });
    assert_eq!(rows(&mut cx).1, 5);
}

#[gpui::test]
async fn reposition_moves_the_caret_through_the_cycle_and_drops_other_carets(
    cx: &mut TestAppContext,
) {
    let mut cx = test_context(cx).await;
    cx.update_editor(|editor, window, cx| {
        editor.set_scroll_position(gpui::point(0.0, 30.0), window, cx);
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([
                MultiBufferOffset(2)..MultiBufferOffset(2),
                MultiBufferOffset(400)..MultiBufferOffset(400),
                MultiBufferOffset(410)..MultiBufferOffset(420),
            ])
        });
        editor.set_scroll_position(gpui::point(0.0, 30.0), window, cx);
    });
    let mut seen = Vec::new();
    for _ in 0..4 {
        cx.simulate_keystrokes("alt-r");
        let (row, _) = rows(&mut cx);
        seen.push(row);
    }
    assert_eq!(seen, [52, 32, 72, 52]);
    cx.update_editor(|editor, _, cx| {
        let display = editor.display_snapshot(cx);
        assert_eq!(editor.selections.all_display(&display).len(), 1);
        assert!(editor.selections.newest_display(&display).is_empty());
    });
}

#[gpui::test]
async fn zeds_scroll_cursor_center_top_bottom_is_unchanged(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    put_caret(&mut cx, 50);
    cx.dispatch_action(ScrollCursorCenterTopBottom);
    let middle = rows(&mut cx).1;
    cx.dispatch_action(ScrollCursorCenterTopBottom);
    let top = rows(&mut cx).1;
    assert!(middle > top);
    assert_eq!(top, 2);
}
