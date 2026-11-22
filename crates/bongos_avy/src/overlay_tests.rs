use std::ops::Range;

use bongos_avy_model::ScreenPoint;
use bongos_window::test_support::split_center;
use editor::{Editor, MultiBufferOffset, ToOffset as _};
use gpui::{Entity, TestAppContext, VisualTestContext};
use theme::ActiveTheme as _;

use crate::overlay::{AVY_OVERLAY_KEY, clear, show};
use crate::test_support::{add_text_editor, paint, setup};
use crate::visible::Candidate;

/// A candidate at buffer offset `offset` of `editor`, drawn at window x `x` on the first row.
fn candidate(
    editor: &Entity<Editor>,
    offset: usize,
    x: f32,
    cx: &mut VisualTestContext,
) -> Candidate {
    cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let anchor = snapshot.anchor_before(MultiBufferOffset(offset));
        Candidate {
            editor: editor.downgrade(),
            range: anchor..anchor,
            screen: ScreenPoint { x, y: 0. },
        }
    })
}

fn labeled(candidate: Candidate, label: &str) -> (Candidate, String) {
    (candidate, label.to_string())
}

/// What `editor` shows now, in paint order: (label text, target offset, covered offsets).
fn shown(
    editor: &Entity<Editor>,
    cx: &mut VisualTestContext,
) -> Vec<(String, usize, Option<Range<usize>>)> {
    cx.update(|_, cx| {
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .navigation_overlays(AVY_OVERLAY_KEY)
            .into_iter()
            .map(|overlay| {
                assert_eq!(overlay.target_range.start, overlay.target_range.end);
                (
                    overlay.label.text.to_string(),
                    overlay.target_range.start.to_offset(&snapshot).0,
                    overlay.covered_text_range.map(|range| {
                        range.start.to_offset(&snapshot).0..range.end.to_offset(&snapshot).0
                    }),
                )
            })
            .collect()
    })
}

/// How many ranges of `editor` are faded out.
fn fades(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor
                .all_text_highlights(window, cx)
                .into_iter()
                .filter(|(style, _)| style.fade_out == Some(1.0))
                .map(|(_, ranges)| ranges.len())
                .sum()
        })
    })
}

/// The buffer text and the display text of `editor`.
fn texts(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (String, String) {
    cx.update(|_, cx| editor.update(cx, |editor, cx| (editor.text(cx), editor.display_text(cx))))
}

const LEFT: &str = "alpha beta\nxy";
const RIGHT: &str = "gamma";

/// Two editors side by side, [`LEFT`] and [`RIGHT`].
async fn two_editors(
    cx: &mut TestAppContext,
) -> (Entity<Editor>, Entity<Editor>, &mut VisualTestContext) {
    let (workspace, cx) = setup(cx).await;
    let (left, right) = cx.update(|window, cx| {
        let center = split_center(&workspace, 2, window, cx);
        let left = add_text_editor(&workspace, &center[0], LEFT, window, cx);
        let right = add_text_editor(&workspace, &center[1], RIGHT, window, cx);
        (left, right)
    });
    paint(cx);
    (left, right, cx)
}

#[gpui::test]
async fn labels_go_on_their_editors_with_text_and_covered_cells(cx: &mut TestAppContext) {
    let (left, right, cx) = two_editors(cx).await;
    let labels = vec![
        labeled(candidate(&left, 0, 10., cx), "a"),
        labeled(candidate(&left, 6, 20., cx), "sd"),
        // One cell before the end of the line: a two-key label covers only what exists.
        labeled(candidate(&left, 9, 30., cx), "jk"),
        // The end of the buffer: nothing under the label.
        labeled(candidate(&left, 13, 40., cx), "lm"),
        labeled(candidate(&right, 3, 50., cx), "fgh"),
        labeled(candidate(&right, 0, 60., cx), "q"),
    ];
    cx.update(|_, cx| show(&labels, "", cx));

    // Farthest (last given) first.
    assert_eq!(
        shown(&left, cx),
        vec![
            ("lm".to_string(), 13, None),
            ("jk".to_string(), 9, Some(9..10)),
            ("sd".to_string(), 6, Some(6..8)),
            ("a".to_string(), 0, Some(0..1)),
        ]
    );
    assert_eq!(
        shown(&right, cx),
        vec![
            ("q".to_string(), 0, Some(0..1)),
            ("fgh".to_string(), 3, Some(3..5)),
        ]
    );
    // One fade per covered range: the label at the end of the buffer covers nothing.
    assert_eq!(fades(&left, cx), 3);
    assert_eq!(fades(&right, cx), 2);

    cx.update(|_, cx| {
        let colors = cx.theme().colors().editor_background;
        let cursor = cx.theme().players().local().cursor;
        for editor in [&left, &right] {
            for overlay in editor.read(cx).navigation_overlays(AVY_OVERLAY_KEY) {
                assert_eq!(overlay.label.text_color, colors);
                assert_eq!(overlay.label.background, Some(cursor));
                assert_eq!(overlay.label.scale_factor, 1.0);
                assert_eq!(overlay.label.x_offset, gpui::px(0.));
            }
        }
    });
}

#[gpui::test]
async fn a_typed_prefix_leaves_only_matching_labels_with_their_remaining_key(
    cx: &mut TestAppContext,
) {
    let (left, right, cx) = two_editors(cx).await;
    let labels = vec![
        labeled(candidate(&left, 0, 10., cx), "as"),
        labeled(candidate(&left, 6, 20., cx), "ad"),
        labeled(candidate(&right, 0, 30., cx), "sa"),
        labeled(candidate(&right, 3, 40., cx), "sd"),
    ];
    cx.update(|_, cx| show(&labels, "", cx));
    assert_eq!(shown(&left, cx).len(), 2);
    assert_eq!(shown(&right, cx).len(), 2);

    cx.update(|_, cx| show(&labels, "a", cx));
    assert_eq!(
        shown(&left, cx),
        vec![
            // The covered range keeps the full label's width, two cells.
            ("d".to_string(), 6, Some(6..8)),
            ("s".to_string(), 0, Some(0..2)),
        ]
    );
    // Nothing on the right editor starts with "a": its earlier labels and fade are gone.
    assert_eq!(shown(&right, cx), vec![]);
    assert_eq!(fades(&right, cx), 0);

    cx.update(|_, cx| show(&labels, "sd", cx));
    assert_eq!(shown(&left, cx), vec![]);
    assert_eq!(shown(&right, cx), vec![]);
}

#[gpui::test]
async fn clear_removes_every_label_and_fade_and_leaves_the_text(cx: &mut TestAppContext) {
    let (left, right, cx) = two_editors(cx).await;
    let before = (texts(&left, cx), texts(&right, cx));
    let labels = vec![
        labeled(candidate(&left, 0, 10., cx), "a"),
        labeled(candidate(&left, 6, 20., cx), "sd"),
        labeled(candidate(&right, 3, 30., cx), "fg"),
    ];
    cx.update(|_, cx| show(&labels, "", cx));
    paint(cx);
    assert_eq!(shown(&left, cx).len(), 2);
    assert_eq!(shown(&right, cx).len(), 1);
    assert_eq!(fades(&left, cx), 2);
    assert_eq!(fades(&right, cx), 1);
    assert_eq!((texts(&left, cx), texts(&right, cx)), before);

    let editors = [left.downgrade(), right.downgrade()];
    cx.update(|_, cx| clear(&editors, cx));
    paint(cx);
    assert_eq!(shown(&left, cx), vec![]);
    assert_eq!(shown(&right, cx), vec![]);
    assert_eq!(fades(&left, cx), 0);
    assert_eq!(fades(&right, cx), 0);
    assert_eq!((texts(&left, cx), texts(&right, cx)), before);

    // Nothing to clear and nothing to show: neither call does anything.
    cx.update(|_, cx| clear(&editors, cx));
    cx.update(|_, cx| show(&[], "", cx));
    assert_eq!(shown(&left, cx), vec![]);
    assert_eq!(fades(&right, cx), 0);
}

#[gpui::test]
async fn clearing_with_a_closed_editor_clears_the_rest(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let (center, left, right) = cx.update(|window, cx| {
        let center = split_center(&workspace, 2, window, cx);
        let left = add_text_editor(&workspace, &center[0], LEFT, window, cx);
        let right = add_text_editor(&workspace, &center[1], RIGHT, window, cx);
        (center, left, right)
    });
    paint(cx);
    let labels = vec![
        labeled(candidate(&left, 6, 10., cx), "a"),
        labeled(candidate(&right, 0, 20., cx), "s"),
    ];
    cx.update(|_, cx| show(&labels, "", cx));
    assert_eq!(shown(&left, cx).len(), 1);
    assert_eq!(shown(&right, cx).len(), 1);
    let editors = [left.downgrade(), right.downgrade()];

    // Close the right pane's editor, as closing the pane does.
    let right_id = right.entity_id();
    cx.update(|window, cx| {
        center[1].update(cx, |pane, cx| {
            pane.remove_item(right_id, false, true, window, cx)
        });
    });
    drop(right);
    cx.background_executor
        .advance_clock(workspace::SERIALIZATION_THROTTLE_TIME);
    paint(cx);
    paint(cx);
    assert!(editors[1].upgrade().is_none());

    cx.update(|_, cx| clear(&editors, cx));
    assert_eq!(shown(&left, cx), vec![]);
    assert_eq!(fades(&left, cx), 0);

    // Showing with the closed editor's candidates among them skips them.
    cx.update(|_, cx| show(&labels, "", cx));
    assert_eq!(shown(&left, cx).len(), 1);
}

#[gpui::test]
async fn overlays_are_set_farthest_first(cx: &mut TestAppContext) {
    let (left, _right, cx) = two_editors(cx).await;
    // Nearest first, as ranked: the candidates are scattered so that neither the buffer order
    // nor the label order is the paint order by accident.
    let labels = vec![
        labeled(candidate(&left, 6, 10., cx), "a"),
        labeled(candidate(&left, 0, 20., cx), "s"),
        labeled(candidate(&left, 11, 30., cx), "d"),
        labeled(candidate(&left, 4, 40., cx), "f"),
    ];
    cx.update(|_, cx| show(&labels, "", cx));
    let order: Vec<(String, usize)> = shown(&left, cx)
        .into_iter()
        .map(|(text, target, _)| (text, target))
        .collect();
    assert_eq!(
        order,
        vec![
            ("f".to_string(), 4),
            ("d".to_string(), 11),
            ("s".to_string(), 0),
            ("a".to_string(), 6),
        ]
    );
}
