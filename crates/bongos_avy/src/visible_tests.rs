use std::sync::Arc;

use bongos_window::test_support::{add_terminal, split_center};
use editor::{
    Editor, MultiBuffer, MultiBufferOffset, ToOffset as _,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, DisplayRow},
};
use gpui::{Entity, EntityId, IntoElement as _, TestAppContext, VisualTestContext, div, point};
use language::{Point, language_settings::SoftWrap};
use workspace::{Workspace, dock::DockPosition};

use crate::test_support::{
    add_buffer_editor, add_dock_editor, add_project_panel, add_search_results, add_text_editor,
    editor_ids, move_cursor, paint, setup, visible,
};
use crate::visible::{Candidate, Mode, PaneText, collect, panes as visible_panes};

/// Each candidate as (editor, buffer offset of its start), in the order given.
fn starts(found: &[Candidate], cx: &mut VisualTestContext) -> Vec<(EntityId, usize)> {
    cx.update(|_, cx| {
        found
            .iter()
            .map(|candidate| {
                let editor = candidate.editor.upgrade().expect("the editor is open");
                let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
                (
                    editor.entity_id(),
                    candidate.range.start.to_offset(&snapshot).0,
                )
            })
            .collect()
    })
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

#[gpui::test]
async fn two_panes_give_candidates_and_the_nearer_ranks_first(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let (center, left, right) = cx.update(|window, cx| {
        let center = split_center(&workspace, 2, window, cx);
        let left = add_text_editor(&workspace, &center[0], "alpha beta\ngamma beta", window, cx);
        let right = add_text_editor(&workspace, &center[1], "beta delta", window, cx);
        (center, left, right)
    });
    paint(cx);
    let panes = visible(&workspace, cx);
    assert_eq!(
        editor_ids(&panes),
        vec![left.entity_id(), right.entity_id()]
    );

    let found = collect(Mode::CharTimer, "beta", &panes, left.entity_id());
    assert_eq!(
        starts(&found, cx),
        vec![
            (left.entity_id(), 6),
            (left.entity_id(), 17),
            (right.entity_id(), 0),
        ]
    );
    assert!(found[2].screen.x > found[0].screen.x);
    assert_eq!(found[0].screen.y, found[2].screen.y);

    // From the right pane the order turns around: its own match comes first.
    let from_right = collect(Mode::CharTimer, "beta", &panes, right.entity_id());
    assert_eq!(starts(&from_right, cx)[0], (left.entity_id(), 6));
    assert_eq!(
        from_right.len(),
        2,
        "the match under the right cursor is dropped"
    );

    // Candidates hold only weak handles: once the editor is closed they no longer reach it.
    let right_id = right.entity_id();
    cx.update(|window, cx| {
        center[1].update(cx, |pane, cx| {
            // Closing the emptied pane also clears its toolbar's hold on the item.
            pane.remove_item(right_id, false, true, window, cx)
        });
    });
    drop(right);
    drop(panes);
    // The workspace holds closed items until its throttled serialization runs (on the test
    // clock), and the window holds the views of its last two frames.
    cx.background_executor
        .advance_clock(workspace::SERIALIZATION_THROTTLE_TIME);
    paint(cx);
    paint(cx);
    assert!(found[2].editor.upgrade().is_none());
    assert!(found[0].editor.upgrade().is_some());
}

#[gpui::test]
async fn soft_wrap_gives_a_slice_per_row_and_line_starts_only_at_column_zero(
    cx: &mut TestAppContext,
) {
    let (workspace, cx) = setup(cx).await;
    let first = "wrap me ".repeat(60);
    let text = format!("{first}\nsecond line");
    let editor = only_editor(&workspace, &text, cx);
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx)
        });
        move_cursor(&editor, text.len(), window, cx);
    });
    paint(cx);
    let panes = visible(&workspace, cx);
    assert_eq!(editor_ids(&panes), vec![editor.entity_id()]);

    let first_rows: Vec<_> = panes[0]
        .rows
        .iter()
        .filter(|row| {
            row.segments
                .first()
                .is_some_and(|segment| segment.start.0 < first.len())
        })
        .collect();
    assert!(
        first_rows.len() >= 2,
        "the first line wraps, got {} rows",
        first_rows.len()
    );
    let joined: String = first_rows
        .iter()
        .flat_map(|row| row.segments.iter().map(|segment| segment.text.as_str()))
        .collect();
    assert_eq!(joined, first);
    assert_eq!(first_rows[0].line_start, Some(MultiBufferOffset(0)));
    assert!(first_rows[1..].iter().all(|row| row.line_start.is_none()));

    let lines = collect(Mode::Line, "", &panes, editor.entity_id());
    let mut found = starts(&lines, cx);
    found.sort();
    assert_eq!(
        found,
        vec![
            (editor.entity_id(), 0),
            (editor.entity_id(), first.len() + 1)
        ]
    );
    assert_eq!(lines[0].screen.x, lines[1].screen.x);
    // A word that starts on a continuation row is still a word start there.
    let words = collect(Mode::Word, "w", &panes, editor.entity_id());
    assert_eq!(words.len(), 60);
}

#[gpui::test]
async fn a_fold_inside_a_row_labels_both_sides_and_nothing_under_it(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let text = "left (inner stuff) right\nnext";
    let editor = only_editor(&workspace, text, cx);
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.fold_ranges(
                vec![MultiBufferOffset(6)..MultiBufferOffset(17)],
                false,
                window,
                cx,
            )
        });
    });
    paint(cx);
    let panes = visible(&workspace, cx);
    let first_row: Vec<&str> = panes[0].rows[0]
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    assert_eq!(first_row, vec!["left (", ") right"]);

    let found = collect(Mode::CharTimer, "t", &panes, editor.entity_id());
    let mut offsets: Vec<usize> = starts(&found, cx).into_iter().map(|(_, o)| o).collect();
    offsets.sort();
    assert_eq!(
        offsets,
        vec![3, 23, 28],
        "the t of stuff (13) is folded away"
    );
    let at = |offset: usize, cx: &mut VisualTestContext| {
        let index = starts(&found, cx)
            .iter()
            .position(|(_, o)| *o == offset)
            .unwrap();
        found[index].screen
    };
    assert!(at(23, cx).x > at(3, cx).x);
    assert_eq!(at(23, cx).y, at(3, cx).y);
    assert!(at(28, cx).y > at(3, cx).y);
}

#[gpui::test]
async fn excerpt_headers_and_block_rows_get_no_candidates(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let editor = cx.update(|window, cx| {
        let buffer = MultiBuffer::build_multi(
            [
                ("one\ntwo\nthree", vec![Point::new(0, 0)..Point::new(2, 5)]),
                ("four\nfive", vec![Point::new(0, 0)..Point::new(1, 4)]),
            ],
            cx,
        );
        let pane = workspace.read(cx).active_pane().clone();
        add_buffer_editor(&workspace, &pane, buffer, window, cx)
    });
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            let anchor = editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_after(Point::new(0, 0));
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(1),
                    style: BlockStyle::Fixed,
                    render: Arc::new(|_| div().into_any_element()),
                    priority: 0,
                }],
                None,
                cx,
            );
        });
    });
    paint(cx);
    let panes = visible(&workspace, cx);
    assert_eq!(editor_ids(&panes), vec![editor.entity_id()]);

    let block_rows: Vec<DisplayRow> = cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            (0..=snapshot.max_point().row().0)
                .map(DisplayRow)
                .filter(|row| snapshot.is_block_line(*row))
                .collect()
        })
    });
    assert!(
        block_rows.len() >= 3,
        "two excerpt headers and a block, got {block_rows:?}"
    );
    let rows = &panes[0].rows;
    assert!(
        rows.iter()
            .all(|row| !block_rows.contains(&row.display_row)),
        "no kept row is a block row"
    );
    let texts: Vec<&str> = rows
        .iter()
        .flat_map(|row| row.segments.iter().map(|segment| segment.text.as_str()))
        .collect();
    assert_eq!(texts, vec!["one", "two", "three", "four", "five"]);
    assert_eq!(
        rows.iter().filter(|row| row.line_start.is_some()).count(),
        5
    );
    // Every labelled row lies outside the header and block rows, so so do the candidates.
    let found = collect(Mode::Line, "", &panes, editor.entity_id());
    assert_eq!(
        found.len(),
        4,
        "five line starts minus the one under the cursor"
    );
}

#[gpui::test]
async fn partially_visible_rows_and_unpainted_editors_get_no_candidates(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let text = (0..60)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let editor = only_editor(&workspace, &text, cx);
    paint(cx);
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.set_scroll_position(point(0., 0.5), window, cx)
        });
    });
    paint(cx);
    let panes = visible(&workspace, cx);
    let rows = &panes[0].rows;
    assert_eq!(rows[0].display_row, DisplayRow(1), "row 0 is cut in half");
    let lines = collect(Mode::Line, "", &panes, editor.entity_id());
    let offsets: Vec<usize> = starts(&lines, cx).into_iter().map(|(_, o)| o).collect();
    assert!(!offsets.contains(&0));
    assert!(offsets.contains(&"line 0\n".len()));
    let bottom = rows.last().unwrap().display_row;
    assert!(bottom.0 < 59, "the window does not show all 60 lines");

    // An editor that was never painted has no layout yet, so nothing is collected for it.
    let (late, unpainted) = cx.update(|window, cx| {
        let pane = workspace.read(cx).active_pane().clone();
        let late = add_text_editor(&workspace, &pane, "late text", window, cx);
        let found = workspace.update(cx, |workspace, cx| visible_panes(workspace, window, cx));
        (late, found)
    });
    assert!(unpainted.is_empty());
    assert!(collect(Mode::Line, "", &unpainted, late.entity_id()).is_empty());
}

#[gpui::test]
async fn a_zoomed_window_lists_only_the_zoomed_pane(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let (center, editors) = cx.update(|window, cx| {
        let center = split_center(&workspace, 3, window, cx);
        let editors: Vec<_> = center
            .iter()
            .map(|pane| add_text_editor(&workspace, pane, "same text", window, cx))
            .collect();
        (center, editors)
    });
    paint(cx);
    assert_eq!(visible(&workspace, cx).len(), 3);

    // The other panes keep the layout of their last paint, so only the zoom decides.
    cx.update(|window, cx| {
        center[2].update(cx, |pane, cx| pane.zoom_in(&workspace::ZoomIn, window, cx));
    });
    paint(cx);
    assert!(cx.update(|_, cx| workspace.read(cx).zoomed_item().is_some()));
    assert!(editors[0].read_with(cx, |editor, _| editor.visible_text_layout().is_some()));
    let panes = visible(&workspace, cx);
    assert_eq!(editor_ids(&panes), vec![editors[2].entity_id()]);
    let found = collect(Mode::CharTimer, "e", &panes, editors[2].entity_id());
    assert!(!found.is_empty());
    assert!(
        starts(&found, cx)
            .iter()
            .all(|(id, _)| *id == editors[2].entity_id())
    );
}

#[gpui::test]
async fn terminals_the_project_panel_and_search_results_are_skipped(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let (main, results, dock_editor) = cx.update(|window, cx| {
        let center = split_center(&workspace, 3, window, cx);
        let main = add_text_editor(&workspace, &center[0], "main text", window, cx);
        add_terminal(&workspace, &center[1], window, cx);
        let results = add_search_results(&workspace, &center[2], "result text", window, cx);
        add_project_panel(&workspace, DockPosition::Left, window, cx);
        let (_, dock_editor) =
            add_dock_editor(&workspace, DockPosition::Right, "dock text", window, cx);
        (main, results, dock_editor)
    });
    paint(cx);

    // The results editor was painted, so it is left out for not being an editor item itself.
    let results_editor = cx.update(|_, cx| results.read(cx).editor.clone());
    assert!(
        results_editor
            .read_with(cx, |editor, _| editor.visible_text_layout())
            .is_some()
    );
    let panes = visible(&workspace, cx);
    assert_eq!(
        editor_ids(&panes),
        vec![main.entity_id(), dock_editor.entity_id()]
    );
    let found = collect(Mode::CharTimer, "text", &panes, main.entity_id());
    assert_eq!(
        starts(&found, cx),
        vec![(main.entity_id(), 5), (dock_editor.entity_id(), 5)]
    );
}

#[gpui::test]
async fn the_match_under_the_cursor_is_dropped(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let editor = only_editor(&workspace, "abc abc\nabc", cx);
    cx.update(|window, cx| move_cursor(&editor, 4, window, cx));
    paint(cx);
    let panes = visible(&workspace, cx);
    let id = editor.entity_id();
    let sorted = |found: Vec<Candidate>, cx: &mut VisualTestContext| {
        let mut offsets: Vec<usize> = starts(&found, cx).into_iter().map(|(_, o)| o).collect();
        offsets.sort();
        offsets
    };
    assert_eq!(
        sorted(collect(Mode::CharTimer, "abc", &panes, id), cx),
        vec![0, 8]
    );
    assert_eq!(sorted(collect(Mode::Word, "a", &panes, id), cx), vec![0, 8]);
    assert_eq!(sorted(collect(Mode::Line, "", &panes, id), cx), vec![0, 8]);
    // The nearest candidate to the cursor comes first.
    assert_eq!(
        starts(&collect(Mode::CharTimer, "abc", &panes, id), cx)[0],
        (id, 0)
    );

    cx.update(|window, cx| move_cursor(&editor, 0, window, cx));
    paint(cx);
    let panes: Vec<PaneText> = visible(&workspace, cx);
    assert_eq!(sorted(collect(Mode::Line, "", &panes, id), cx), vec![8]);
}

#[gpui::test]
async fn matches_outside_the_visible_columns_get_no_candidates(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let text = format!("zz{}zz", " ".repeat(300));
    let end = text.len() - 2;
    let editor = only_editor(&workspace, &text, cx);
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx)
        });
        move_cursor(&editor, 150, window, cx);
    });
    paint(cx);
    let id = editor.entity_id();
    let offsets = |cx: &mut VisualTestContext| {
        let panes = visible(&workspace, cx);
        let found = collect(Mode::CharTimer, "zz", &panes, id);
        starts(&found, cx)
            .into_iter()
            .map(|(_, offset)| offset)
            .collect::<Vec<_>>()
    };
    // The match past the right edge is laid out but not on screen.
    assert_eq!(offsets(cx), vec![0]);

    // Scrolled to the far end, the first match is off to the left and the last one is in view.
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.set_scroll_position(point(290., 0.), window, cx)
        });
    });
    paint(cx);
    assert_eq!(offsets(cx), vec![end]);
}
