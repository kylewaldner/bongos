use bongos_avy_model::ScreenPoint;
use bongos_emacs::actions::ExchangePointAndMark;
use bongos_emacs::mark::{mark_for, push_mark, ring_for};
use bongos_window::test_support::split_center;
use editor::{
    Anchor, Editor, MultiBuffer, MultiBufferOffset, SelectionEffects, ToOffset as _, ToPoint as _,
};
use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext, point};
use language::Point;
use workspace::{Pane, Workspace};

use crate::jump::jump;
use crate::test_support::{add_buffer_editor, add_text_editor, move_cursor, paint, setup, visible};
use crate::visible::{Candidate, Mode, collect};

/// A workspace whose editors get the emacs commands (C-x C-x among them).
async fn emacs_setup(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = setup(cx).await;
    cx.update(|_, cx| bongos_emacs::init(cx));
    (workspace, cx)
}

fn only_editor(
    workspace: &Entity<Workspace>,
    text: &str,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    cx.update(|window, cx| {
        let pane = workspace.read(cx).active_pane().clone();
        add_text_editor(workspace, &pane, text, window, cx)
    })
}

/// Two center panes side by side with an editor of the given text in each.
fn two_panes(
    workspace: &Entity<Workspace>,
    left: &str,
    right: &str,
    cx: &mut VisualTestContext,
) -> (Vec<Entity<Pane>>, Entity<Editor>, Entity<Editor>) {
    cx.update(|window, cx| {
        let center = split_center(workspace, 2, window, cx);
        let left = add_text_editor(workspace, &center[0], left, window, cx);
        let right = add_text_editor(workspace, &center[1], right, window, cx);
        (center, left, right)
    })
}

/// A candidate at `start..end` (buffer offsets) of `editor`, anchored as `collect` anchors it.
fn candidate(
    editor: &Entity<Editor>,
    start: usize,
    end: usize,
    cx: &mut VisualTestContext,
) -> Candidate {
    cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let start_anchor = snapshot.anchor_after(MultiBufferOffset(start));
        let end_anchor = if end == start {
            start_anchor
        } else {
            snapshot.anchor_before(MultiBufferOffset(end))
        };
        Candidate {
            editor: editor.downgrade(),
            range: start_anchor..end_anchor,
            screen: ScreenPoint { x: 0., y: 0. },
        }
    })
}

fn focus(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    cx.run_until_parked();
}

fn run_jump(origin: &Entity<Editor>, target: Candidate, cx: &mut VisualTestContext) {
    let origin = origin.clone();
    cx.update(|window, cx| jump(origin, target, window, cx));
    cx.run_until_parked();
}

/// Every selection of `editor` as (tail, head) buffer offsets.
fn selections(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<(usize, usize)> {
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            editor
                .selections
                .all::<MultiBufferOffset>(&display)
                .iter()
                .map(|selection| (selection.tail().0, selection.head().0))
                .collect()
        })
    })
}

fn offset(editor: &Entity<Editor>, anchor: Anchor, cx: &gpui::App) -> usize {
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    anchor.to_offset(&snapshot).0
}

/// The mark and the ring (newest first) of the buffer under `editor`'s cursor, as offsets.
fn marks(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (Option<usize>, Vec<usize>) {
    cx.update(|_, cx| {
        let mark = mark_for(editor, cx).map(|anchor| offset(editor, anchor, cx));
        let ring = ring_for(editor, cx)
            .into_iter()
            .map(|anchor| offset(editor, anchor, cx))
            .collect();
        (mark, ring)
    })
}

fn mark_mode(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    editor.read_with(cx, |editor, _| editor.selection_mark_mode())
}

fn is_focused(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| editor.focus_handle(cx).is_focused(window))
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| editor.read(cx).buffer().read(cx).snapshot(cx).text())
}

fn scroll(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> gpui::Point<f64> {
    cx.update(|_, cx| editor.update(cx, |editor, cx| editor.scroll_position(cx)))
}

#[gpui::test]
async fn a_jump_in_the_same_pane_moves_the_caret_and_c_x_c_x_returns(cx: &mut TestAppContext) {
    let (workspace, cx) = emacs_setup(cx).await;
    let editor = only_editor(&workspace, "one two three four", cx);
    focus(&editor, cx);
    paint(cx);
    assert_eq!(marks(&editor, cx), (None, vec![]));

    run_jump(&editor, candidate(&editor, 8, 13, cx), cx);
    assert_eq!(selections(&editor, cx), vec![(8, 8)], "a caret, no region");
    assert_eq!(
        marks(&editor, cx),
        (Some(0), vec![]),
        "the first push sets the mark and leaves the ring empty"
    );
    assert!(!mark_mode(&editor, cx), "mark mode is never left on");

    // A second jump puts the first mark on the ring.
    run_jump(&editor, candidate(&editor, 14, 18, cx), cx);
    assert_eq!(selections(&editor, cx), vec![(14, 14)]);
    assert_eq!(marks(&editor, cx), (Some(8), vec![0]));
    assert!(!mark_mode(&editor, cx));

    cx.dispatch_action(ExchangePointAndMark);
    assert_eq!(selections(&editor, cx), vec![(14, 8)], "C-x C-x returns");
}

#[gpui::test]
async fn with_mark_mode_on_a_jump_extends_the_region_and_keeps_the_mark(cx: &mut TestAppContext) {
    let (workspace, cx) = emacs_setup(cx).await;
    let editor = only_editor(&workspace, "one two three four", cx);
    focus(&editor, cx);
    cx.update(|window, cx| {
        move_cursor(&editor, 4, window, cx);
        assert!(push_mark(&editor, None, true, window, cx));
    });
    paint(cx);
    assert!(mark_mode(&editor, cx));
    assert_eq!(marks(&editor, cx), (Some(4), vec![]));

    run_jump(&editor, candidate(&editor, 14, 18, cx), cx);
    assert_eq!(selections(&editor, cx), vec![(4, 14)]);
    assert_eq!(
        marks(&editor, cx),
        (Some(4), vec![]),
        "the mark is unchanged"
    );
    assert!(mark_mode(&editor, cx), "mark mode is only read");

    // Backwards past the mark the region turns around and keeps its tail.
    run_jump(&editor, candidate(&editor, 0, 3, cx), cx);
    assert_eq!(selections(&editor, cx), vec![(4, 0)]);
    assert_eq!(marks(&editor, cx), (Some(4), vec![]));
}

#[gpui::test]
async fn three_cursors_collapse_to_one_at_the_target(cx: &mut TestAppContext) {
    let (workspace, cx) = emacs_setup(cx).await;
    let editor = only_editor(&workspace, "one two three four", cx);
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([
                    MultiBufferOffset(0)..MultiBufferOffset(0),
                    MultiBufferOffset(4)..MultiBufferOffset(4),
                    MultiBufferOffset(8)..MultiBufferOffset(8),
                ]);
            });
        });
    });
    paint(cx);
    assert_eq!(selections(&editor, cx).len(), 3);

    run_jump(&editor, candidate(&editor, 14, 18, cx), cx);
    assert_eq!(selections(&editor, cx), vec![(14, 14)]);
    assert!(!mark_mode(&editor, cx));
}

#[gpui::test]
async fn a_jump_to_another_pane_focuses_it_and_the_origin_keeps_its_primary(
    cx: &mut TestAppContext,
) {
    let (workspace, cx) = emacs_setup(cx).await;
    let (center, left, right) = two_panes(&workspace, "alpha beta gamma", "delta epsilon", cx);
    cx.update(|window, cx| {
        left.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([
                    MultiBufferOffset(0)..MultiBufferOffset(0),
                    MultiBufferOffset(6)..MultiBufferOffset(10),
                ]);
            });
        });
    });
    focus(&left, cx);
    paint(cx);
    assert!(is_focused(&left, cx));

    run_jump(&left, candidate(&right, 6, 13, cx), cx);
    assert_eq!(
        selections(&left, cx),
        vec![(6, 10)],
        "only the primary is left, its region untouched"
    );
    assert_eq!(selections(&right, cx), vec![(6, 6)]);
    assert!(is_focused(&right, cx));
    assert_eq!(
        cx.update(|_, cx| workspace.read(cx).active_pane().clone()),
        center[1]
    );
    assert_eq!(
        marks(&left, cx),
        (Some(10), vec![]),
        "the origin is pushed in its own buffer"
    );
    assert_eq!(marks(&right, cx), (None, vec![]));
    assert!(!mark_mode(&left, cx));
    assert!(!mark_mode(&right, cx));
}

#[gpui::test]
async fn with_one_buffer_in_both_panes_c_x_c_x_in_the_target_returns_to_the_origin(
    cx: &mut TestAppContext,
) {
    let (workspace, cx) = emacs_setup(cx).await;
    let (left, right) = cx.update(|window, cx| {
        let center = split_center(&workspace, 2, window, cx);
        let buffer = MultiBuffer::build_simple("one two three four", cx);
        let left = add_buffer_editor(&workspace, &center[0], buffer.clone(), window, cx);
        let right = add_buffer_editor(&workspace, &center[1], buffer, window, cx);
        (left, right)
    });
    cx.update(|window, cx| move_cursor(&left, 4, window, cx));
    focus(&left, cx);
    paint(cx);

    run_jump(&left, candidate(&right, 14, 18, cx), cx);
    assert!(is_focused(&right, cx));
    assert_eq!(selections(&right, cx), vec![(14, 14)]);
    assert_eq!(selections(&left, cx), vec![(4, 4)]);
    assert_eq!(marks(&right, cx), (Some(4), vec![]), "marks are per buffer");

    cx.dispatch_action(ExchangePointAndMark);
    assert_eq!(selections(&right, cx), vec![(14, 4)]);
    assert_eq!(selections(&left, cx), vec![(4, 4)]);
}

#[gpui::test]
async fn a_line_target_lands_at_column_zero_even_after_an_edit(cx: &mut TestAppContext) {
    let (workspace, cx) = emacs_setup(cx).await;
    let editor = only_editor(&workspace, "first line\n    second line\nthird", cx);
    cx.update(|window, cx| move_cursor(&editor, 3, window, cx));
    paint(cx);
    let panes = visible(&workspace, cx);
    let lines = collect(Mode::Line, "", &panes, editor.entity_id());
    let second = "first line\n".len();
    let target = lines
        .into_iter()
        .find(|candidate| cx.update(|_, cx| offset(&editor, candidate.range.start, cx)) == second)
        .expect("the second line is a candidate");
    assert_eq!(
        target.range.start, target.range.end,
        "a line candidate is empty"
    );

    // Text typed above the target after the labels were drawn moves the anchor with it.
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit(
                    [(MultiBufferOffset(0)..MultiBufferOffset(0), "XX")],
                    None,
                    cx,
                )
            })
        })
    });
    run_jump(&editor, target, cx);
    let head = cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        editor
            .read(cx)
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
    });
    assert_eq!(head, Point::new(1, 0));
    assert_eq!(selections(&editor, cx), vec![(second + 2, second + 2)]);
}

#[gpui::test]
async fn a_jump_scrolls_no_pane_and_edits_no_text(cx: &mut TestAppContext) {
    let (workspace, cx) = emacs_setup(cx).await;
    let tall = (0..200)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (_, left, right) = two_panes(&workspace, &tall, &tall, cx);
    paint(cx);
    cx.update(|window, cx| {
        for editor in [&left, &right] {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(point(0., 20.), window, cx)
            });
        }
    });
    focus(&left, cx);
    paint(cx);
    let before = (scroll(&left, cx), scroll(&right, cx));
    assert_eq!(before.0.y, 20.);
    let texts = (text(&left, cx), text(&right, cx));

    // Far below the visible rows of either pane: nothing may scroll to reveal it.
    let far = tall.find("line 150").unwrap();
    run_jump(&left, candidate(&left, far, far + 4, cx), cx);
    paint(cx);
    assert_eq!(selections(&left, cx), vec![(far, far)]);
    run_jump(&left, candidate(&right, far, far + 4, cx), cx);
    paint(cx);
    assert_eq!(selections(&right, cx), vec![(far, far)]);
    assert!(is_focused(&right, cx));

    assert_eq!((scroll(&left, cx), scroll(&right, cx)), before);
    assert_eq!((text(&left, cx), text(&right, cx)), texts);
}

#[gpui::test]
async fn a_target_whose_pane_closed_changes_nothing(cx: &mut TestAppContext) {
    let (workspace, cx) = emacs_setup(cx).await;
    let (center, left, right) = two_panes(&workspace, "alpha beta", "gamma delta", cx);
    cx.update(|window, cx| {
        left.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([
                    MultiBufferOffset(0)..MultiBufferOffset(0),
                    MultiBufferOffset(6)..MultiBufferOffset(6),
                ]);
            });
        });
    });
    focus(&left, cx);
    paint(cx);
    let target = candidate(&right, 6, 11, cx);

    let right_id = right.entity_id();
    cx.update(|window, cx| {
        center[1].update(cx, |pane, cx| {
            pane.remove_item(right_id, false, true, window, cx)
        });
    });
    drop(right);
    // The workspace holds closed items until its throttled serialization runs (on the test
    // clock), and the window holds the views of its last two frames.
    cx.background_executor
        .advance_clock(workspace::SERIALIZATION_THROTTLE_TIME);
    paint(cx);
    paint(cx);
    assert!(target.editor.upgrade().is_none());
    focus(&left, cx);

    run_jump(&left, target, cx);
    assert_eq!(selections(&left, cx), vec![(0, 0), (6, 6)]);
    assert_eq!(marks(&left, cx), (None, vec![]), "no mark is pushed");
    assert!(is_focused(&left, cx));
    assert!(!mark_mode(&left, cx));
}
