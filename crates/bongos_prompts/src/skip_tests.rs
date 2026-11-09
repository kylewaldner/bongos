//! Tests of point skipping (D9): every way of moving point leaves no head on a marker row, a
//! region's tail stays, runs of markers are skipped as one, and no text is ever added.

use editor::{Editor, SelectionEffects, ToPoint as _};
use gpui::{Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext, point, px};
use language::Point;

use crate::mode_of;
use crate::mode_tests::open_editor;
use crate::skip::skip_target;

/// Rows 1, 3 and 4, and 6 (the last row) are markers.
const RUNS: &str = "\
preamble
=== prompt 2026-11-05 13:40 ===
first
=== prompt 2026-11-05 14:10 ===
=== prompt 2026-11-05 14:20 ===
second
=== prompt 2026-11-05 14:30 ===";

const MARKER_LEN: u32 = "=== prompt 2026-11-05 14:30 ===".len() as u32;

fn rows(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

fn target(text: &str, head: Point, prev: Point) -> Option<Point> {
    let lines = rows(text);
    let last_row = lines.len() as u32 - 1;
    skip_target(
        head,
        prev,
        last_row,
        |row| lines[row as usize].starts_with("=== prompt"),
        |row| lines[row as usize].len() as u32,
    )
}

#[test]
fn skip_target_moves_forward_back_and_over_runs() {
    // Not a marker row: stays.
    assert_eq!(target(RUNS, Point::new(2, 1), Point::new(0, 0)), None);
    // Forward from above, to the next body row.
    assert_eq!(
        target(RUNS, Point::new(1, 0), Point::new(0, 3)),
        Some(Point::new(2, 0))
    );
    // Back from below, to the end of the row above.
    assert_eq!(
        target(RUNS, Point::new(1, 0), Point::new(2, 0)),
        Some(Point::new(0, 8))
    );
    // A run of two, forward and back, from either of its rows.
    assert_eq!(
        target(RUNS, Point::new(3, 0), Point::new(2, 5)),
        Some(Point::new(5, 0))
    );
    assert_eq!(
        target(RUNS, Point::new(4, 0), Point::new(5, 0)),
        Some(Point::new(2, 5))
    );
    // A marker on the last row: forward to the end of the file, and the end of the file stays.
    let end = Point::new(6, MARKER_LEN);
    assert_eq!(target(RUNS, Point::new(6, 0), Point::new(5, 2)), Some(end));
    assert_eq!(target(RUNS, end, Point::new(5, 2)), None);
    // A move from the end of the file that stays on its row (the display clipped it to the
    // rule's start) goes back to the end.
    assert_eq!(target(RUNS, Point::new(6, 0), end), Some(end));
}

#[test]
fn skip_target_on_row_zero_goes_forward_and_markers_to_the_end_go_to_the_end() {
    let text = "=== prompt 2026-11-05 13:40 ===\n=== prompt 2026-11-05 13:41 ===\nbody";
    assert_eq!(
        target(text, Point::new(0, 0), Point::new(2, 0)),
        Some(Point::new(2, 0)),
        "row 0 always forward, over the run"
    );
    let text = "body\n=== prompt 2026-11-05 13:40 ===\n=== prompt 2026-11-05 13:41 ===";
    assert_eq!(
        target(text, Point::new(1, 0), Point::new(0, 4)),
        Some(Point::new(2, MARKER_LEN)),
        "only markers to the end: the end of the file"
    );
    assert_eq!(
        target(text, Point::new(1, 0), Point::new(2, MARKER_LEN)),
        Some(Point::new(0, 4)),
        "back from the end: above the first marker of the run"
    );
    let text = "=== prompt 2026-11-05 13:40 ===";
    assert_eq!(
        target(text, Point::new(0, 0), Point::new(0, 0)),
        Some(Point::new(0, MARKER_LEN)),
        "a file of one marker: the end of the file"
    );
}

fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor.selections.newest_anchor().head().to_point(&snapshot)
    })
}

fn tail(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor.selections.newest_anchor().tail().to_point(&snapshot)
    })
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn is_marker_row(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) -> bool {
    editor.read_with(cx, |editor, _| {
        mode_of(editor)
            .unwrap()
            .borrow()
            .index()
            .markers()
            .iter()
            .any(|marker| marker.row == row)
    })
}

fn focus(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
}

/// Dispatches an `editor` action by name to the focused editor.
fn act(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(&format!("editor::{name}"), None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn put(editor: &Entity<Editor>, at: Point, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges([at..at])
        })
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn line_moves_skip_rules_and_runs_and_never_append(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", RUNS).await;
    focus(&editor, cx);
    put(&editor, Point::new(0, 0), cx);

    act("MoveDown", cx);
    assert_eq!(head(&editor, cx).row, 2, "C-n over the rule");
    act("MoveDown", cx);
    assert_eq!(head(&editor, cx).row, 5, "C-n over two rules in a row");
    act("MoveDown", cx);
    assert_eq!(
        head(&editor, cx),
        Point::new(6, MARKER_LEN),
        "C-n onto a last-row rule: the end of the file"
    );
    act("MoveDown", cx);
    assert_eq!(head(&editor, cx), Point::new(6, MARKER_LEN));
    assert_eq!(text(&editor, cx), RUNS, "no text appended");

    act("MoveUp", cx);
    assert_eq!(head(&editor, cx).row, 5, "C-p from the end: above the run");
    act("MoveUp", cx);
    assert_eq!(head(&editor, cx), Point::new(2, 5), "C-p over two rules");
    act("MoveUp", cx);
    assert_eq!(head(&editor, cx), Point::new(0, 8), "C-p over the rule");
    assert_eq!(text(&editor, cx), RUNS);
}

#[gpui::test]
async fn char_moves_skip_rules(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", RUNS).await;
    focus(&editor, cx);
    put(&editor, Point::new(2, 5), cx);
    act("MoveRight", cx);
    assert_eq!(head(&editor, cx), Point::new(5, 0), "C-f at a body's end");
    act("MoveLeft", cx);
    assert_eq!(head(&editor, cx), Point::new(2, 5), "C-b at a body's start");
    put(&editor, Point::new(0, 8), cx);
    act("MoveRight", cx);
    assert_eq!(head(&editor, cx), Point::new(2, 0));
    act("MoveLeft", cx);
    assert_eq!(head(&editor, cx), Point::new(0, 8));
    assert_eq!(text(&editor, cx), RUNS);
}

#[gpui::test]
async fn buffer_ends_skip_rules(cx: &mut TestAppContext) {
    let text_on_row_0 = "\
=== prompt 2026-11-05 13:40 ===
body
=== prompt 2026-11-05 14:30 ===";
    let (editor, cx) = open_editor(cx, true, "prompts.txt", text_on_row_0).await;
    focus(&editor, cx);
    put(&editor, Point::new(1, 2), cx);
    act("MoveToBeginning", cx);
    assert_eq!(head(&editor, cx), Point::new(1, 0), "M-< onto a row 0 rule");
    act("MoveToEnd", cx);
    assert_eq!(
        head(&editor, cx),
        Point::new(2, MARKER_LEN),
        "M-> stays at the end"
    );
    act("MoveUp", cx);
    assert_eq!(head(&editor, cx).row, 1);
    assert_eq!(text(&editor, cx), text_on_row_0, "no text appended");
}

#[gpui::test]
async fn a_region_keeps_its_tail_and_a_search_hit_is_moved(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", RUNS).await;
    focus(&editor, cx);
    put(&editor, Point::new(0, 2), cx);
    act("SelectDown", cx);
    assert_eq!(
        tail(&editor, cx),
        Point::new(0, 2),
        "the region's tail kept"
    );
    assert_eq!(head(&editor, cx).row, 2);
    act("SelectDown", cx);
    assert_eq!(tail(&editor, cx), Point::new(0, 2));
    assert_eq!(head(&editor, cx).row, 5, "the region spans two rules");

    // A search hit inside a marker line, reached from above.
    put(&editor, Point::new(0, 0), cx);
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_ranges([Point::new(1, 4)..Point::new(1, 10)])
        })
    });
    cx.run_until_parked();
    assert_eq!(head(&editor, cx), Point::new(2, 0));
    assert_eq!(tail(&editor, cx), Point::new(1, 4), "the hit's tail kept");
    assert_eq!(text(&editor, cx), RUNS);
}

#[gpui::test]
async fn page_down_never_stops_on_a_rule(cx: &mut TestAppContext) {
    let mut long = String::new();
    for n in 0..300 {
        long.push_str(&format!("=== prompt 2026-11-05 13:40 ===\nbody {n}\n"));
    }
    long.push_str("tail");
    let (editor, cx) = open_editor(cx, true, "prompts.txt", &long).await;
    focus(&editor, cx);
    put(&editor, Point::new(1, 0), cx);
    let lines = editor.read_with(cx, |editor, _| editor.visible_line_count());
    assert!(lines.is_some(), "the editor was laid out");
    let mut last = head(&editor, cx).row;
    for _ in 0..4 {
        act("MovePageDown", cx);
        let row = head(&editor, cx).row;
        assert!(row > last, "C-v moved down");
        assert!(
            !is_marker_row(&editor, row, cx),
            "C-v left point on row {row}"
        );
        last = row;
    }
    assert_eq!(text(&editor, cx), long);
}

#[gpui::test]
async fn a_click_on_a_rule_moves_point_off_it(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", RUNS).await;
    focus(&editor, cx);
    put(&editor, Point::new(0, 0), cx);
    let (top, line_height) = editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.snapshot(window, cx);
        let pixel =
            |row: u32, editor: &mut Editor, window: &mut gpui::Window, cx: &mut gpui::App| {
                editor
                    .display_to_pixel_point(
                        editor::DisplayPoint::new(editor::display_map::DisplayRow(row), 0),
                        &snapshot,
                        window,
                        cx,
                    )
                    .unwrap()
                    .y
            };
        let first = pixel(0, editor, window, cx);
        let second = pixel(1, editor, window, cx);
        (first, second - first)
    });
    // The rule of the marker on row 1 is display row 1; click its middle, right of any text.
    let at = point(px(600.), top + line_height * 1.5);
    cx.simulate_click(at, Modifiers::none());
    cx.run_until_parked();
    let row = head(&editor, cx).row;
    assert!(
        !is_marker_row(&editor, row, cx),
        "the click left point on row {row}"
    );
    assert_eq!(row, 2, "a click below the head goes forward");
    assert_eq!(text(&editor, cx), RUNS);
}
