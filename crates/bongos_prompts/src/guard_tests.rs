//! Tests of the boundary guard (D10): joins that would pull a rule into a line are reverted with a
//! toast and leave the text as it was, a region kill across a rule merges, undo and redo and
//! external reloads never trigger it, and with a group interval only the join is reverted.

use std::time::Duration;

use editor::actions::{Backspace, Cut, Delete, DeleteToNextWordEnd, KillRingCut, Redo, Undo};
use editor::{Editor, SelectionEffects};
use gpui::{Action, Entity, Focusable as _, TestAppContext, VisualTestContext};
use language::Point;
use project::Project;

use crate::mode_tests::{init, open_editor, open_in, workspace_on};
use crate::{PromptsToast, finalize_own_edit, mode_of, register_language};

const THREE: &str = "\
preamble
=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===
first, sent
=== prompt 2026-11-05 14:10 ===
second
  more
=== prompt 2026-11-05 14:30 ===
third";

/// Rows 3 and 4 are markers in a row.
const RUNS: &str = "\
preamble
=== prompt 2026-11-05 13:40 ===
first
=== prompt 2026-11-05 14:10 ===
=== prompt 2026-11-05 14:20 ===
second";

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn reverts(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.read_with(cx, |editor, _| {
        mode_of(editor).unwrap().borrow().stats().guard_reverts
    })
}

fn rules(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.read_with(cx, |editor, _| {
        mode_of(editor).unwrap().borrow().rule_count()
    })
}

fn focus(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
}

fn act(action: impl Action, cx: &mut VisualTestContext) {
    cx.dispatch_action(action);
    cx.run_until_parked();
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

fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        editor.selections.newest::<Point>(&snapshot).head()
    })
}

fn type_text(editor: &Entity<Editor>, typed: &str, cx: &mut VisualTestContext) {
    for ch in typed.chars() {
        editor.update_in(cx, |editor, window, cx| {
            editor.handle_input(&ch.to_string(), window, cx)
        });
        cx.run_until_parked();
    }
}

fn set_group_interval(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.update(cx, |buffer, _| {
            buffer.set_group_interval(Duration::from_millis(500))
        });
    });
}

fn kill_word() -> DeleteToNextWordEnd {
    DeleteToNextWordEnd {
        ignore_newlines: false,
        ignore_brackets: false,
    }
}

#[gpui::test]
async fn joins_are_reverted_with_the_toast(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_on(cx, true).await;
    let editor = open_in(&workspace, "prompts.txt", cx).await;
    focus(&editor, cx);

    // Backspace at a body's first column.
    put(&editor, Point::new(4, 0), cx);
    act(Backspace, cx);
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(reverts(&editor, cx), 1);
    assert_eq!(head(&editor, cx), Point::new(4, 0), "point put back");
    assert!(workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&workspace::notifications::NotificationId::unique::<
                PromptsToast,
            >())
    }));

    // C-d, C-k and M-d at the end of the row above a marker.
    for (n, action) in [
        Box::new(Delete) as Box<dyn Action>,
        Box::new(KillRingCut),
        Box::new(kill_word()),
    ]
    .into_iter()
    .enumerate()
    {
        put(&editor, Point::new(5, 6), cx);
        cx.update(|window, cx| window.dispatch_action(action, cx));
        cx.run_until_parked();
        assert_eq!(text(&editor, cx), THREE, "join {n}");
        assert_eq!(reverts(&editor, cx), n + 2);
        assert_eq!(rules(&editor, cx), 3);
    }

    // A reverted join leaves no redo entry.
    act(Redo, cx);
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(reverts(&editor, cx), 4);
}

#[gpui::test]
async fn a_region_kill_across_a_rule_merges_and_undo_restores_it(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    focus(&editor, cx);
    select(&editor, Point::new(5, 2), Point::new(7, 2), cx);
    act(Cut, cx);
    assert_eq!(reverts(&editor, cx), 0);
    assert_eq!(rules(&editor, cx), 2);
    assert!(
        text(&editor, cx).ends_with("second\n  ird"),
        "{}",
        text(&editor, cx)
    );
    act(Undo, cx);
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(rules(&editor, cx), 3);
    act(Redo, cx);
    assert_eq!(rules(&editor, cx), 2);
    assert_eq!(reverts(&editor, cx), 0);
}

#[gpui::test]
async fn c_k_above_two_markers_in_a_row_is_reverted(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", RUNS).await;
    focus(&editor, cx);
    put(&editor, Point::new(2, 5), cx);
    act(KillRingCut, cx);
    assert_eq!(text(&editor, cx), RUNS);
    assert_eq!(reverts(&editor, cx), 1);
    assert_eq!(rules(&editor, cx), 3);
}

#[gpui::test]
async fn an_external_reload_never_triggers_the_guard(cx: &mut TestAppContext) {
    let app_state = init(cx, true);
    let fs = app_state.fs.as_fake();
    fs.insert_tree("/dir", serde_json::json!({ "prompts.txt": THREE }))
        .await;
    let project = Project::test(app_state.fs.clone(), ["/dir".as_ref()], cx).await;
    register_language(&project.read_with(cx, |project, _| project.languages().clone()));
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/prompts.txt", cx)
        })
        .await
        .unwrap();
    let (editor, cx) =
        cx.add_window_view(|window, cx| Editor::for_buffer(buffer, Some(project), window, cx));
    cx.run_until_parked();
    focus(&editor, cx);
    put(&editor, Point::new(0, 0), cx);

    // On disk, the second marker is joined into the line above.
    let joined = THREE.replace(
        "first, sent\n=== prompt 2026-11-05 14:10",
        "first, sent=== prompt 2026-11-05 14:10",
    );
    fs.insert_file("/dir/prompts.txt", joined.clone().into_bytes())
        .await;
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), joined, "reloaded");
    assert_eq!(reverts(&editor, cx), 0);
    assert_eq!(rules(&editor, cx), 2);

    // A later edit of Kyle's elsewhere is not taken for that join.
    type_text(&editor, "x", cx);
    assert_eq!(text(&editor, cx), format!("x{joined}"));
    assert_eq!(reverts(&editor, cx), 0);
}

/// Types "xyz" at `at`, moves to `join_at` and runs `join`, all within the group interval.
async fn typed_then_join(
    cx: &mut TestAppContext,
    at: Point,
    join_at: Point,
    join: Box<dyn Action>,
) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    focus(&editor, cx);
    set_group_interval(&editor, cx);
    put(&editor, at, cx);
    type_text(&editor, "xyz", cx);
    let typed = text(&editor, cx);
    assert_ne!(typed, THREE);
    put(&editor, join_at, cx);
    cx.update(|window, cx| window.dispatch_action(join, cx));
    cx.run_until_parked();
    assert_eq!(reverts(&editor, cx), 1, "the join is reverted");
    assert_eq!(text(&editor, cx), typed, "the typed text survives");

    // Undo removes the whole typed run in one step; redo brings it back. Neither is guarded.
    act(Undo, cx);
    assert_eq!(text(&editor, cx), THREE);
    act(Redo, cx);
    assert_eq!(text(&editor, cx), typed);
    act(Undo, cx);
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(reverts(&editor, cx), 1);
}

#[gpui::test]
async fn typing_then_c_d_at_once_reverts_only_the_join(cx: &mut TestAppContext) {
    typed_then_join(cx, Point::new(5, 6), Point::new(5, 9), Box::new(Delete)).await;
}

#[gpui::test]
async fn typing_then_c_k_at_once_reverts_only_the_join(cx: &mut TestAppContext) {
    typed_then_join(
        cx,
        Point::new(5, 6),
        Point::new(5, 9),
        Box::new(KillRingCut),
    )
    .await;
}

#[gpui::test]
async fn typing_then_backspace_at_once_reverts_only_the_join(cx: &mut TestAppContext) {
    typed_then_join(cx, Point::new(4, 0), Point::new(4, 0), Box::new(Backspace)).await;
}

#[gpui::test]
async fn undo_and_redo_never_trigger_the_guard(cx: &mut TestAppContext) {
    // A joined line, split by Enter: its undo is a join the guard must leave alone.
    let joined = THREE.replace(
        "first, sent\n=== prompt 2026-11-05 14:10",
        "first, sent=== prompt 2026-11-05 14:10",
    );
    let (editor, cx) = open_editor(cx, true, "prompts.txt", &joined).await;
    focus(&editor, cx);
    set_group_interval(&editor, cx);
    put(&editor, Point::new(2, 11), cx);
    type_text(&editor, "\n", cx);
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(rules(&editor, cx), 3);
    act(Undo, cx);
    assert_eq!(text(&editor, cx), joined, "undo joins again");
    act(Redo, cx);
    assert_eq!(text(&editor, cx), THREE);
    act(Undo, cx);
    assert_eq!(text(&editor, cx), joined);
    assert_eq!(reverts(&editor, cx), 0);

    // Undo after a revert undoes the edit before the join, and redo restores it.
    act(Redo, cx);
    put(&editor, Point::new(4, 0), cx);
    act(Backspace, cx);
    assert_eq!(reverts(&editor, cx), 1);
    assert_eq!(text(&editor, cx), THREE);
    act(Undo, cx);
    assert_eq!(text(&editor, cx), joined);
    act(Redo, cx);
    assert_eq!(text(&editor, cx), THREE);
    assert_eq!(reverts(&editor, cx), 1);
}

/// An edit of the mode's own (a stamp, a new block) made like the later commands make it: a plain
/// buffer edit, then [`finalize_own_edit`].
fn own_edit(
    editor: &Entity<Editor>,
    range: std::ops::Range<Point>,
    new: &str,
    cx: &mut VisualTestContext,
) {
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(range, new)], None, cx));
        finalize_own_edit(editor, cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn a_join_right_after_mark_sent_is_reverted_alone(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    focus(&editor, cx);
    set_group_interval(&editor, cx);
    put(&editor, Point::new(5, 6), cx);
    type_text(&editor, "x", cx);
    let marker = "=== prompt 2026-11-05 14:10 ===";
    let stamped = "=== prompt 2026-11-05 14:10 | sent 2026-11-05 15:00 ===";
    own_edit(
        &editor,
        Point::new(3, 0)..Point::new(3, marker.len() as u32),
        stamped,
        cx,
    );
    let after_stamp = text(&editor, cx);
    assert!(after_stamp.contains(stamped));
    act(Delete, cx);
    assert_eq!(reverts(&editor, cx), 1);
    assert_eq!(text(&editor, cx), after_stamp, "the stamp survives");
    act(Undo, cx);
    assert_eq!(
        text(&editor, cx),
        THREE.replace("  more", "  morex"),
        "undo takes the stamp alone"
    );
}

#[gpui::test]
async fn a_join_right_after_a_new_block_is_reverted_alone(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    focus(&editor, cx);
    set_group_interval(&editor, cx);
    let end = Point::new(7, 5);
    own_edit(&editor, end..end, "\n=== prompt 2026-11-06 09:00 ===\n", cx);
    let with_new = text(&editor, cx);
    assert_eq!(rules(&editor, cx), 4);
    put(&editor, end, cx);
    act(Delete, cx);
    assert_eq!(reverts(&editor, cx), 1);
    assert_eq!(text(&editor, cx), with_new, "the new block survives");
    assert_eq!(rules(&editor, cx), 4);
}

#[gpui::test]
async fn while_disabled_nothing_skips_or_guards(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, false, "prompts.txt", THREE).await;
    focus(&editor, cx);
    put(&editor, Point::new(2, 3), cx);
    let move_down = cx.update(|_, cx| cx.build_action("editor::MoveDown", None).unwrap());
    cx.update(|window, cx| window.dispatch_action(move_down, cx));
    cx.run_until_parked();
    assert_eq!(
        head(&editor, cx),
        Point::new(3, 3),
        "point stays on the marker row"
    );
    put(&editor, Point::new(4, 0), cx);
    act(Backspace, cx);
    assert_eq!(
        text(&editor, cx),
        THREE.replace("14:10 ===\nsecond", "14:10 ===second"),
        "a plain editor joins"
    );
}
