use std::time::Duration;

use bongos_emacs_model::prefix_arg::PrefixArg;
use editor::actions::{
    Backspace, Delete, MoveLeft, MovePageDown, MoveRight, Newline, SelectDown, SelectLeft,
    SelectPageDown, SelectRight, SelectUp, Undo,
};
use editor::display_map::{DisplayPoint, DisplayRow};
use editor::test::editor_test_context::EditorTestContext;
use gpui::{Action as _, TestAppContext};

use crate::apply_tests::init_test;
use crate::prefix_arg::{Meaning, meaning};

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, _| buffer.set_group_interval(Duration::ZERO));
    cx
}

fn arg(value: i32) -> PrefixArg {
    let mut arg = if value < 0 {
        PrefixArg::negative()
    } else {
        PrefixArg::digit(0)
    };
    for digit in value.unsigned_abs().to_string().chars() {
        arg.press_digit(digit.to_digit(10).unwrap() as u8);
    }
    assert_eq!(arg.value(), value);
    arg
}

/// Calls the adapter declared for `A` with the argument `value`.
fn call<A: gpui::Action>(cx: &mut EditorTestContext, value: i32) {
    call_named(cx, A::name_for_type(), value);
}

fn call_named(cx: &mut EditorTestContext, name: &str, value: i32) {
    let editor = cx.editor.clone();
    cx.update(|window, cx| {
        let Some(Meaning::Adapter(adapter)) = meaning(cx, name) else {
            panic!("{name} has no adapter");
        };
        adapter(arg(value), &editor, window, cx);
    });
}

#[gpui::test]
async fn every_command_has_an_adapter(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        for name in [
            MoveRight::name_for_type(),
            MoveLeft::name_for_type(),
            SelectRight::name_for_type(),
            SelectLeft::name_for_type(),
            "editor::MoveDown",
            "editor::MoveUp",
            SelectDown::name_for_type(),
            SelectUp::name_for_type(),
            MovePageDown::name_for_type(),
            SelectPageDown::name_for_type(),
            Delete::name_for_type(),
            Backspace::name_for_type(),
            Newline::name_for_type(),
            Undo::name_for_type(),
        ] {
            assert!(
                matches!(meaning(cx, name), Some(Meaning::Adapter(_))),
                "{name}"
            );
        }
    });
}

#[gpui::test]
async fn horizontal_moves_by_the_count(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇabcdefgh");
    call::<MoveRight>(&mut cx, 4);
    cx.assert_editor_state("abcdˇefgh");
    call::<MoveLeft>(&mut cx, 3);
    cx.assert_editor_state("aˇbcdefgh");
    // A negative count reverses.
    call::<MoveRight>(&mut cx, -1);
    cx.assert_editor_state("ˇabcdefgh");
    call::<MoveLeft>(&mut cx, -2);
    cx.assert_editor_state("abˇcdefgh");
    call::<MoveRight>(&mut cx, 0);
    cx.assert_editor_state("abˇcdefgh");
    // The end of the buffer stops it.
    call::<MoveRight>(&mut cx, 1000);
    cx.assert_editor_state("abcdefghˇ");
    call::<MoveLeft>(&mut cx, 1000);
    cx.assert_editor_state("ˇabcdefgh");
}

#[gpui::test]
async fn horizontal_moves_cross_lines_and_selections_extend(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("abˇc\ndef");
    call::<MoveRight>(&mut cx, 3);
    cx.assert_editor_state("abc\ndˇef");
    cx.set_state("abˇcdef");
    call::<SelectRight>(&mut cx, 3);
    cx.assert_editor_state("ab«cdeˇ»f");
    call::<SelectLeft>(&mut cx, 5);
    cx.assert_editor_state("«ˇab»cdef");
    // A plain move out of a region goes to its end and counts as one character.
    cx.set_state("a«bcˇ»def");
    call::<MoveRight>(&mut cx, 2);
    cx.assert_editor_state("abcdˇef");
}

#[gpui::test]
async fn vertical_moves_by_lines_without_an_undo_entry(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let text = (0..20)
        .map(|n| format!("l{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    cx.set_state(&format!("ˇ{text}"));
    call_named(&mut cx, "editor::MoveDown", 10);
    let row = |cx: &mut EditorTestContext| {
        cx.update_editor(|editor, _, cx| {
            let display = editor.display_snapshot(cx);
            editor.selections.newest_display(&display).head().row().0
        })
    };
    assert_eq!(row(&mut cx), 10);
    call_named(&mut cx, "editor::MoveUp", 3);
    assert_eq!(row(&mut cx), 7);
    // A negative count goes the other way, zero does nothing.
    call_named(&mut cx, "editor::MoveUp", -2);
    assert_eq!(row(&mut cx), 9);
    call_named(&mut cx, "editor::MoveDown", 0);
    assert_eq!(row(&mut cx), 9);

    let history = |cx: &mut EditorTestContext| {
        cx.update_buffer(|buffer, _| {
            (
                buffer.version(),
                buffer.peek_undo_stack().map(|entry| entry.transaction_id()),
            )
        })
    };
    let before = history(&mut cx);
    call_named(&mut cx, "editor::MoveDown", 1000);
    assert_eq!(row(&mut cx), 19);
    assert!(history(&mut cx) == before);
}

#[gpui::test]
async fn select_up_and_down_extend_the_region(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("aˇa\nbb\ncc\ndd");
    call::<SelectDown>(&mut cx, 2);
    cx.assert_editor_state("a«a\nbb\ncˇ»c\ndd");
    call::<SelectUp>(&mut cx, 1);
    cx.assert_editor_state("a«a\nbˇ»b\ncc\ndd");
    call::<SelectDown>(&mut cx, -1);
    cx.assert_editor_state("aˇa\nbb\ncc\ndd");
}

fn rows(cx: &mut EditorTestContext) -> (u32, u32) {
    cx.update_editor(|editor, _, cx| {
        let display = editor.display_snapshot(cx);
        (
            editor.selections.newest_display(&display).head().row().0,
            editor.scroll_position(cx).y as u32,
        )
    })
}

fn put_caret(cx: &mut EditorTestContext, row: u32) {
    cx.update_editor(|editor, window, cx| {
        let point = DisplayPoint::new(DisplayRow(row), 0);
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_display_ranges([point..point])
        })
    });
}

#[gpui::test]
async fn page_scrolls_by_lines_and_moves_the_cursor_only_if_it_left(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let text = (0..200)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    cx.set_state(&format!("ˇ{text}"));
    put_caret(&mut cx, 10);
    cx.update_editor(|editor, window, cx| {
        editor.set_scroll_position(gpui::point(0., 0.), window, cx)
    });
    call::<MovePageDown>(&mut cx, 5);
    // The window moved five rows and the caret, still in view, stayed.
    assert_eq!(rows(&mut cx), (10, 5));

    put_caret(&mut cx, 5);
    cx.update_editor(|editor, window, cx| {
        editor.set_scroll_position(gpui::point(0., 5.), window, cx)
    });
    call::<MovePageDown>(&mut cx, 5);
    // Now the caret left the window at the top and went to its first row.
    assert_eq!(rows(&mut cx), (10, 10));
}

#[gpui::test]
async fn page_select_extends_when_the_head_left(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let text = (0..200)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    cx.set_state(&format!("ˇ{text}"));
    cx.update_editor(|editor, window, cx| {
        editor.set_scroll_position(gpui::point(0., 0.), window, cx)
    });
    call::<SelectPageDown>(&mut cx, 5);
    let (row, top) = rows(&mut cx);
    assert_eq!((row, top), (5, 5));
    let anchored = cx.update_editor(|editor, _, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.newest_display(&display).tail().row().0
    });
    assert_eq!(anchored, 0);
}

#[gpui::test]
async fn delete_and_backspace_remove_the_count_as_one_step(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("abˇcdefgh");
    call::<Delete>(&mut cx, 3);
    cx.assert_editor_state("abˇfgh");
    cx.update_editor(|editor, window, cx| editor.undo(&Undo, window, cx));
    cx.assert_editor_state("abˇcdefgh");

    call::<Backspace>(&mut cx, 2);
    cx.assert_editor_state("ˇcdefgh");
    cx.update_editor(|editor, window, cx| editor.undo(&Undo, window, cx));
    cx.assert_editor_state("abˇcdefgh");

    // A negative count reverses.
    call::<Delete>(&mut cx, -2);
    cx.assert_editor_state("ˇcdefgh");
    cx.set_state("abˇcdefgh");
    call::<Backspace>(&mut cx, -2);
    cx.assert_editor_state("abˇefgh");

    // Zero and the end of the buffer do nothing harmful.
    call::<Delete>(&mut cx, 0);
    cx.assert_editor_state("abˇefgh");
    call::<Delete>(&mut cx, 1000);
    cx.assert_editor_state("abˇ");
}

#[gpui::test]
async fn delete_and_backspace_with_a_region_delete_it_once(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("a«bcˇ»defgh");
    call::<Delete>(&mut cx, 3);
    cx.assert_editor_state("aˇdefgh");
    cx.set_state("a«bcˇ»defgh");
    call::<Backspace>(&mut cx, 3);
    cx.assert_editor_state("aˇdefgh");
}

#[gpui::test]
async fn newline_inserts_auto_indented_newlines_in_one_step(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("    fooˇ");
    call::<Newline>(&mut cx, 3);
    cx.assert_editor_state("    foo\n\n\n    ˇ");
    cx.update_editor(|editor, window, cx| editor.undo(&Undo, window, cx));
    cx.assert_editor_state("    fooˇ");

    call::<Newline>(&mut cx, 0);
    cx.assert_editor_state("    fooˇ");
    call::<Newline>(&mut cx, -2);
    cx.assert_editor_state("    fooˇ");
}

#[gpui::test]
async fn undo_runs_the_count_as_separate_steps(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    cx.set_state("ˇ");
    for text in ["a", "b", "c", "d"] {
        cx.update_editor(|editor, window, cx| {
            editor.handle_input(text, window, cx);
            editor.finalize_last_transaction(cx);
        });
    }
    cx.assert_editor_state("abcdˇ");
    call::<Undo>(&mut cx, 2);
    cx.assert_editor_state("abˇ");
    cx.update_editor(|editor, window, cx| editor.undo(&Undo, window, cx));
    cx.assert_editor_state("aˇ");
    call::<Undo>(&mut cx, 1000);
    cx.assert_editor_state("ˇ");
}
