//! Tests of the navigation, new and copy commands (D12, D19, D24): `prompts: new` at the end and
//! in the middle, previous and next across sent and pending blocks with a count, next pending
//! and its wrap, copy of a block and of a selection yanked back with `C-y`, the join guard after
//! `new`, and dispatch by name with the mode on and off.

use std::time::Duration;

use editor::actions::{Backspace, KillRingYank, Undo};
use editor::{Editor, SelectionEffects};
use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext};
use language::Point;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use crate::commands::{
    Copied, Moved, copy_text, new_block, next_block, next_pending_block, previous_block,
};
use crate::mode_tests::{dispatch, open_editor, open_in, workspace_on};
use crate::{PromptsToast, mode_of};

/// Blocks: 0 the preamble (row 0), 1 sent (rows 2), 2 pending (rows 4 to 5), 3 pending (row 7).
const THREE: &str = "\
preamble
=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===
first, sent
=== prompt 2026-11-05 14:10 ===
second
  more
=== prompt 2026-11-05 14:30 ===
third";

const ALL_SENT: &str = "\
=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===
first
=== prompt 2026-11-05 14:10 | sent 2026-11-05 14:20 ===
second";

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        editor.selections.newest::<Point>(&snapshot).head()
    })
}

fn select(editor: &Entity<Editor>, from: Point, to: Point, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges([from..to])
        })
    });
    cx.run_until_parked();
}

fn put(editor: &Entity<Editor>, at: Point, cx: &mut VisualTestContext) {
    select(editor, at, at, cx);
}

fn focus(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
}

fn blocks(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.read_with(cx, |editor, _| {
        mode_of(editor).unwrap().borrow().index().block_count()
    })
}

fn skip_moves(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.read_with(cx, |editor, _| {
        mode_of(editor).unwrap().borrow().stats().skip_moves
    })
}

fn reverts(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.read_with(cx, |editor, _| {
        mode_of(editor).unwrap().borrow().stats().guard_reverts
    })
}

/// Whether the last block is pending and starts at `row`.
fn last_marker_row(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (u32, bool) {
    editor.read_with(cx, |editor, _| {
        let mode = mode_of(editor).unwrap();
        let mode = mode.borrow();
        let marker = *mode.index().markers().last().unwrap();
        (marker.row, !marker.marker.is_sent())
    })
}

fn toast_shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<PromptsToast>())
    })
}

fn dismiss_toast(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update(cx, |workspace, cx| {
        workspace.dismiss_notification(&NotificationId::unique::<PromptsToast>(), cx)
    });
}

fn dispatch_with(name: &str, data: serde_json::Value, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, Some(data)).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

/// A workspace with `prompts.txt` (`THREE`) open, focused, point at `row`.
async fn opened(
    cx: &mut TestAppContext,
    enabled: bool,
    row: u32,
) -> (Entity<Workspace>, Entity<Editor>, &mut VisualTestContext) {
    let (workspace, cx) = workspace_on(cx, enabled).await;
    let editor = open_in(&workspace, "prompts.txt", cx).await;
    focus(&editor, cx);
    put(&editor, Point::new(row, 0), cx);
    (workspace, editor, cx)
}

fn is_marker_line(line: &str) -> bool {
    line.starts_with("=== prompt ") && line.ends_with(" ===") && !line.contains("sent")
}

#[gpui::test]
async fn new_at_the_end_appends_an_empty_body_row(cx: &mut TestAppContext) {
    let (_workspace, editor, cx) = opened(cx, true, 7).await;
    let moves = skip_moves(&editor, cx);
    dispatch("prompts::New", cx);
    let after = text(&editor, cx);
    let (old, appended) = after.split_at(THREE.len());
    assert_eq!(old, THREE, "the text before is untouched");
    let rows: Vec<&str> = appended.split('\n').collect();
    assert_eq!(rows.len(), 3, "{appended:?}");
    assert_eq!(rows[0], "", "the line break that ends `third`");
    assert!(is_marker_line(rows[1]), "{appended:?}");
    assert_eq!(rows[2], "", "an empty body row");
    assert_eq!(blocks(&editor, cx), 5);
    assert_eq!(last_marker_row(&editor, cx), (8, true));
    assert_eq!(head(&editor, cx), Point::new(9, 0), "point in the new body");
    assert_eq!(skip_moves(&editor, cx), moves, "skipping did not move it");
}

#[gpui::test]
async fn new_in_the_middle_inserts_after_the_current_block(cx: &mut TestAppContext) {
    let (_workspace, editor, cx) = opened(cx, true, 4).await;
    let moves = skip_moves(&editor, cx);
    dispatch("prompts::New", cx);
    let after = text(&editor, cx);
    let rows: Vec<&str> = after.split('\n').collect();
    let old: Vec<&str> = THREE.split('\n').collect();
    assert_eq!(rows.len(), old.len() + 2);
    assert_eq!(rows[..6], old[..6], "up to the end of the current body");
    assert!(is_marker_line(rows[6]), "{after:?}");
    assert_eq!(rows[7], "", "an empty body row");
    assert_eq!(rows[8..], old[6..], "the later blocks are untouched");
    assert_eq!(blocks(&editor, cx), 5);
    assert_eq!(head(&editor, cx), Point::new(7, 0), "point in the new body");
    assert_eq!(skip_moves(&editor, cx), moves);
    editor.read_with(cx, |editor, _| {
        let mode = mode_of(editor).unwrap();
        let mode = mode.borrow();
        let block = mode.index().block_at(7);
        assert_eq!(block.index, 3);
        assert!(block.is_pending());
    });

    // After a sent block too, from the middle of its body.
    let (_workspace, editor, cx) = opened(cx, true, 2).await;
    dispatch("prompts::New", cx);
    let rows: Vec<String> = text(&editor, cx).split('\n').map(str::to_string).collect();
    assert_eq!(rows[2], "first, sent");
    assert!(is_marker_line(&rows[3]), "{rows:?}");
    assert_eq!(rows[4], "");
    assert_eq!(rows[5], "=== prompt 2026-11-05 14:10 ===");
    assert_eq!(head(&editor, cx), Point::new(4, 0));
}

#[gpui::test]
async fn new_in_a_file_without_markers_starts_the_second_block(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", "only").await;
    editor.update_in(cx, |editor, window, cx| new_block(editor, window, cx));
    cx.run_until_parked();
    let after = text(&editor, cx);
    assert!(after.starts_with("only\n=== prompt "), "{after:?}");
    assert!(after.ends_with(" ===\n"), "{after:?}");
    assert_eq!(head(&editor, cx), Point::new(2, 0));
    assert_eq!(blocks(&editor, cx), 2);
}

#[gpui::test]
async fn a_join_right_after_new_is_reverted_alone(cx: &mut TestAppContext) {
    let (workspace, editor, cx) = opened(cx, true, 4).await;
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.update(cx, |buffer, _| {
            buffer.set_group_interval(Duration::from_millis(500))
        });
    });
    dispatch("prompts::New", cx);
    let with_new = text(&editor, cx);
    assert_ne!(with_new, THREE);
    assert_eq!(head(&editor, cx), Point::new(7, 0));
    assert!(!toast_shown(&workspace, cx));

    // Backspace at the first column of the new body would pull the marker into a line.
    dispatch_editor(&editor, Backspace, cx);
    assert_eq!(reverts(&editor, cx), 1, "the join is reverted");
    assert_eq!(text(&editor, cx), with_new, "the new block survives");
    assert_eq!(blocks(&editor, cx), 5);
    assert_eq!(head(&editor, cx), Point::new(7, 0));
    assert!(toast_shown(&workspace, cx));

    // The new block is an undo step of its own, and the revert left no redo entry.
    dispatch_editor(&editor, Undo, cx);
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(reverts(&editor, cx), 1);
}

fn dispatch_editor(editor: &Entity<Editor>, action: impl gpui::Action, cx: &mut VisualTestContext) {
    focus(editor, cx);
    cx.update(|window, cx| window.dispatch_action(Box::new(action), cx));
    cx.run_until_parked();
}

#[gpui::test]
async fn previous_and_next_walk_the_blocks(cx: &mut TestAppContext) {
    let (workspace, editor, cx) = opened(cx, true, 0).await;
    // Preamble, sent, pending, pending: one block at a time.
    for want in [Point::new(2, 0), Point::new(4, 0), Point::new(7, 0)] {
        dispatch("prompts::Next", cx);
        assert_eq!(head(&editor, cx), want);
    }
    assert!(!toast_shown(&workspace, cx));
    for want in [Point::new(4, 0), Point::new(2, 0), Point::new(0, 0)] {
        dispatch("prompts::Previous", cx);
        assert_eq!(head(&editor, cx), want);
    }

    // From the middle of a body, to the start of the neighbor.
    put(&editor, Point::new(5, 2), cx);
    dispatch("prompts::Previous", cx);
    assert_eq!(head(&editor, cx), Point::new(2, 0));
    put(&editor, Point::new(5, 2), cx);
    dispatch("prompts::Next", cx);
    assert_eq!(head(&editor, cx), Point::new(7, 0));

    // The D24 count, and a count past the ends stops at them.
    put(&editor, Point::new(0, 0), cx);
    dispatch_with("prompts::Next", serde_json::json!({ "count": 2 }), cx);
    assert_eq!(head(&editor, cx), Point::new(4, 0));
    dispatch_with("prompts::Next", serde_json::json!({ "count": 10 }), cx);
    assert_eq!(head(&editor, cx), Point::new(7, 0));
    dispatch_with("prompts::Previous", serde_json::json!({ "count": 2 }), cx);
    assert_eq!(head(&editor, cx), Point::new(2, 0));
    dispatch_with("prompts::Previous", serde_json::json!({ "count": 10 }), cx);
    assert_eq!(head(&editor, cx), Point::new(0, 0));
    assert!(!toast_shown(&workspace, cx));
}

#[gpui::test]
async fn previous_and_next_at_the_ends_stay_and_say_so(cx: &mut TestAppContext) {
    let (workspace, editor, cx) = opened(cx, true, 7).await;
    dispatch("prompts::Next", cx);
    assert_eq!(head(&editor, cx), Point::new(7, 0));
    assert!(
        toast_shown(&workspace, cx),
        "next on the last prompt says so"
    );
    dismiss_toast(&workspace, cx);

    put(&editor, Point::new(0, 0), cx);
    dispatch("prompts::Previous", cx);
    assert_eq!(head(&editor, cx), Point::new(0, 0));
    assert!(toast_shown(&workspace, cx), "previous on the first says so");

    let outcomes = editor.update_in(cx, |editor, window, cx| {
        (
            previous_block(editor, None, window, cx),
            next_block(editor, Some(3), window, cx),
            next_block(editor, None, window, cx),
        )
    });
    assert_eq!(
        outcomes,
        (Some(Moved::AtEnd), Some(Moved::To(3)), Some(Moved::AtEnd))
    );
}

#[gpui::test]
async fn next_pending_skips_sent_blocks_and_wraps_once(cx: &mut TestAppContext) {
    // From the preamble over the sent block to the next pending one.
    let (workspace, editor, cx) = opened(cx, true, 0).await;
    dispatch("prompts::NextPending", cx);
    assert_eq!(head(&editor, cx), Point::new(4, 0));
    assert!(!toast_shown(&workspace, cx), "no wrap, nothing to say");
    dispatch("prompts::NextPending", cx);
    assert_eq!(head(&editor, cx), Point::new(7, 0));

    // From the last block it wraps to the first pending one (the preamble) and says so.
    dispatch("prompts::NextPending", cx);
    assert_eq!(head(&editor, cx), Point::new(0, 0));
    assert!(toast_shown(&workspace, cx), "the wrap is announced");

    let wrapped = editor.update_in(cx, |editor, window, cx| {
        put_row(editor, 7, window, cx);
        next_pending_block(editor, window, cx)
    });
    assert_eq!(wrapped, Some(Moved::Wrapped(0)));
    let plain = editor.update_in(cx, |editor, window, cx| {
        put_row(editor, 0, window, cx);
        next_pending_block(editor, window, cx)
    });
    assert_eq!(plain, Some(Moved::To(2)));
}

fn put_row(
    editor: &mut Editor,
    row: u32,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Editor>,
) {
    editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
        s.select_ranges([Point::new(row, 0)..Point::new(row, 0)])
    });
}

#[gpui::test]
async fn next_pending_with_nothing_pending_stays(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", ALL_SENT).await;
    put(&editor, Point::new(1, 0), cx);
    let moved = editor.update_in(cx, |editor, window, cx| {
        next_pending_block(editor, window, cx)
    });
    assert_eq!(moved, Some(Moved::NoPending));
    assert_eq!(head(&editor, cx), Point::new(1, 0));
}

#[gpui::test]
async fn copy_puts_the_trimmed_block_on_the_clipboard_and_the_kill_ring(cx: &mut TestAppContext) {
    let (workspace, editor, cx) = opened(cx, true, 4).await;
    dispatch("prompts::Copy", cx);
    assert!(toast_shown(&workspace, cx));
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("second\n  more".to_string())
    );
    assert_eq!(text(&editor, cx), THREE, "copying changes nothing");

    // C-y yanks it.
    put(&editor, Point::new(7, 5), cx);
    dispatch_editor(&editor, KillRingYank, cx);
    assert_eq!(text(&editor, cx), format!("{THREE}second\n  more"));
}

#[gpui::test]
async fn copy_trims_blank_rows_around_the_body(cx: &mut TestAppContext) {
    let padded = "=== prompt 2026-11-05 14:10 ===\n\n  \n\tkeep  \n\n  inner\n \n\n=== prompt 2026-11-05 14:30 ===\nnext";
    let (editor, cx) = open_editor(cx, true, "prompts.txt", padded).await;
    put(&editor, Point::new(3, 1), cx);
    let copied = editor.update(cx, |editor, cx| copy_text(editor, cx));
    assert_eq!(copied, Copied::Block);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("\tkeep  \n\n  inner".to_string())
    );

    // An empty block has nothing to copy, and the clipboard keeps what it had.
    let (editor, cx) = open_editor(
        cx,
        true,
        "prompts.txt",
        "=== prompt 2026-11-05 14:10 ===\n=== prompt 2026-11-05 14:30 ===\n\n",
    )
    .await;
    put(&editor, Point::new(2, 0), cx);
    let copied = editor.update(cx, |editor, cx| copy_text(editor, cx));
    assert_eq!(copied, Copied::Nothing);
}

#[gpui::test]
async fn copy_with_a_selection_copies_the_selection(cx: &mut TestAppContext) {
    let (_workspace, editor, cx) = opened(cx, true, 4).await;
    // Across a rule too: the selection is taken as it is.
    select(&editor, Point::new(4, 3), Point::new(7, 2), cx);
    dispatch("prompts::Copy", cx);
    let want = "ond\n  more\n=== prompt 2026-11-05 14:30 ===\nth";
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(want.to_string())
    );
    put(&editor, Point::new(7, 5), cx);
    dispatch_editor(&editor, KillRingYank, cx);
    assert_eq!(text(&editor, cx), format!("{THREE}{want}"));
}

#[gpui::test]
async fn every_command_dispatches_by_name_while_on(cx: &mut TestAppContext) {
    let (_workspace, editor, cx) = opened(cx, true, 4).await;
    for (name, want) in [
        ("prompts::Next", Point::new(7, 0)),
        ("prompts::Previous", Point::new(4, 0)),
        ("prompts::NextPending", Point::new(7, 0)),
    ] {
        dispatch(name, cx);
        assert_eq!(head(&editor, cx), want, "{name}");
    }
    dispatch("prompts::Copy", cx);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("third".to_string())
    );
    dispatch("prompts::New", cx);
    assert_eq!(blocks(&editor, cx), 5);
}

#[gpui::test]
async fn with_the_mode_off_the_commands_show_the_toast_and_change_nothing(cx: &mut TestAppContext) {
    let (workspace, editor, cx) = opened(cx, false, 4).await;
    assert!(editor.read_with(cx, |editor, _| mode_of(editor).is_none()));
    for name in [
        "prompts::New",
        "prompts::Previous",
        "prompts::Next",
        "prompts::NextPending",
        "prompts::Copy",
    ] {
        dismiss_toast(&workspace, cx);
        dispatch(name, cx);
        assert!(toast_shown(&workspace, cx), "{name} shows the off toast");
        assert_eq!(text(&editor, cx), THREE, "{name}");
        assert_eq!(head(&editor, cx), Point::new(4, 0), "{name}");
        assert!(editor.read_with(cx, |editor, _| mode_of(editor).is_none()));
    }
}
