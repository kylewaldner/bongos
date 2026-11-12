//! Tests of what the session shows in the editor: the highlights of the visible matches, the folds
//! and collapsed excerpts a match opens, and the offsets following an edit of the text.

use std::ops::Range;

use editor::{Editor, HighlightKey, SelectionEffects};
use gpui::{AppContext as _, Entity, Hsla, TestAppContext, UpdateGlobal as _, VisualTestContext};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, MultiBufferOffset, PathKey};
use settings::{SettingsStore, ThemeColorsContent, ThemeStyleContent};

use crate::actions::{Backward, Forward};
use crate::session_tests::{
    cursor, input, keys, selections, session_open, set_selections, start, status, text,
    type_slowly, workspace_text, workspace_with,
};

/// Four lines with an `x` each, at 2, 6, 10 and 14.
const XS: &str = "a x\nb x\nc x\nd x\n";

/// The fallback theme paints the active match like the others; this makes it red, so the two can
/// be told apart.
fn color_the_active_match(cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |settings, cx| {
            settings.update_user_settings(cx, |settings| {
                settings.theme.experimental_theme_overrides = Some(ThemeStyleContent {
                    colors: ThemeColorsContent {
                        search_active_match_background: Some("#ff0000ff".to_string()),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            });
        });
    });
    cx.run_until_parked();
}

/// The rows and colors of every highlight, in order.
fn highlights(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<(u32, Hsla)> {
    editor.update_in(cx, |editor, window, cx| {
        editor
            .all_text_background_highlights(window, cx)
            .into_iter()
            .map(|(range, color)| (range.start.row().0, color))
            .collect()
    })
}

fn has_highlights(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    editor.read_with(cx, |editor, _| {
        editor.has_background_highlights(HighlightKey::Isearch)
    })
}

/// The rows whose highlight is not the color of the rest. Fails unless the highlights are of two
/// colors at most, so the active color is the odd one out.
fn active_rows(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<u32> {
    let highlights = highlights(editor, cx);
    let mut colors: Vec<Hsla> = Vec::new();
    for (_, color) in &highlights {
        if !colors.contains(color) {
            colors.push(*color);
        }
    }
    assert!(colors.len() <= 2, "two colors at most: {colors:?}");
    if colors.len() < 2 {
        return Vec::new();
    }
    let count = |color: &Hsla| highlights.iter().filter(|(_, c)| c == color).count();
    let active = if count(&colors[0]) < count(&colors[1]) {
        colors[0]
    } else {
        colors[1]
    };
    highlights
        .iter()
        .filter(|(_, color)| *color == active)
        .map(|(row, _)| *row)
        .collect()
}

fn fold_count(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let len = display.buffer_snapshot().len();
        display.folds_in_range(MultiBufferOffset(0)..len).count()
    })
}

fn fold(editor: &Entity<Editor>, range: Range<usize>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.fold_ranges(
            vec![MultiBufferOffset(range.start)..MultiBufferOffset(range.end)],
            false,
            window,
            cx,
        )
    });
    cx.run_until_parked();
}

fn edit(editor: &Entity<Editor>, range: Range<usize>, with: &str, cx: &mut VisualTestContext) {
    editor.update(cx, |editor, cx| {
        editor.buffer().update(cx, |buffer, cx| {
            buffer.edit(
                [(
                    MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                    with,
                )],
                None,
                cx,
            )
        })
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn isearch_highlights_every_visible_match_and_the_current_one_apart(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, true, cx).await;
    let cx = &mut cx;
    color_the_active_match(cx);
    assert!(!has_highlights(&editor, cx));

    start(Forward, &workspace, cx);
    assert!(!has_highlights(&editor, cx), "nothing to search for yet");
    type_slowly("x", cx);
    let all = highlights(&editor, cx);
    assert_eq!(
        all.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert_eq!(active_rows(&editor, cx), vec![0], "only the current match");

    keys("ctrl-s", cx);
    assert_eq!(active_rows(&editor, cx), vec![1]);
    keys("ctrl-s ctrl-s", cx);
    assert_eq!(active_rows(&editor, cx), vec![3]);
    keys("ctrl-r", cx);
    assert_eq!(active_rows(&editor, cx), vec![2]);

    // A string that fails has no current match, and an empty one no highlights.
    type_slowly("q", cx);
    assert_eq!(highlights(&editor, cx), vec![]);
    keys("backspace backspace", cx);
    assert_eq!(input(&workspace, cx), "");
    assert!(!has_highlights(&editor, cx));
    assert_eq!(text(&editor, cx), XS);
}

fn tall_text() -> String {
    (0..2000)
        .map(|row| match row {
            5 | 1500 => format!("// line {row} needle"),
            _ => format!("// line {row}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[gpui::test]
async fn isearch_highlights_again_after_a_scroll(cx: &mut TestAppContext) {
    let content = tall_text();
    let (workspace, editor, mut cx) = workspace_text(&content, true, cx).await;
    let cx = &mut cx;
    color_the_active_match(cx);

    start(Forward, &workspace, cx);
    type_slowly("needle", cx);
    let before = highlights(&editor, cx);
    assert_eq!(
        before.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
        vec![5],
        "the match a screen away is not highlighted yet"
    );

    editor.update_in(cx, |editor, window, cx| {
        editor.set_scroll_position(gpui::point(0., 1490.), window, cx)
    });
    cx.run_until_parked();
    let after = highlights(&editor, cx);
    assert_eq!(
        after.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
        vec![1500],
        "the match in view is highlighted"
    );
    assert_ne!(
        after[0].1, before[0].1,
        "in the ordinary color: not current"
    );

    keys("ctrl-g", cx);
    assert!(!has_highlights(&editor, cx));
}

#[gpui::test]
async fn isearch_clears_the_highlights_on_every_exit(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, true, cx).await;
    let cx = &mut cx;
    for exit in ["enter", "ctrl-enter", "ctrl-g", "escape", "ctrl-e"] {
        set_selections(&editor, vec![0..0], cx);
        start(Forward, &workspace, cx);
        type_slowly("x", cx);
        assert!(has_highlights(&editor, cx), "{exit}");
        keys(exit, cx);
        assert!(!session_open(&workspace, cx), "{exit}");
        assert!(!has_highlights(&editor, cx), "{exit}: cleared");
    }

    // A click outside the session.
    set_selections(&editor, vec![0..0], cx);
    start(Backward, &workspace, cx);
    type_slowly("x", cx);
    assert!(has_highlights(&editor, cx));
    workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
    cx.run_until_parked();
    assert!(!session_open(&workspace, cx));
    assert!(!has_highlights(&editor, cx));
    assert_eq!(text(&editor, cx), XS);
}

/// The braces and the lines between them are the fold; `mark` is in it and after it.
const FOLDED: &str = "start\n{\n  mark inside\n}\nmark outside\n";

fn fold_range() -> Range<usize> {
    FOLDED.find('{').unwrap()..FOLDED.find('}').unwrap() + 1
}

#[gpui::test]
async fn isearch_a_match_in_a_fold_opens_it_and_c_g_closes_it(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(FOLDED, true, cx).await;
    let cx = &mut cx;
    fold(&editor, fold_range(), cx);
    assert_eq!(fold_count(&editor, cx), 1);

    start(Forward, &workspace, cx);
    type_slowly("mark", cx);
    assert_eq!(fold_count(&editor, cx), 0, "the match opened the fold");
    assert_eq!(cursor(&editor, cx), FOLDED.find("mark").unwrap() + 4);

    keys("ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(fold_count(&editor, cx), 1, "C-g folds it again");
    assert_eq!(cursor(&editor, cx), 0);
    assert_eq!(text(&editor, cx), FOLDED);
}

#[gpui::test]
async fn isearch_moving_on_folds_it_again_and_back_opens_it(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(FOLDED, true, cx).await;
    let cx = &mut cx;
    fold(&editor, fold_range(), cx);

    start(Forward, &workspace, cx);
    type_slowly("mark", cx);
    assert_eq!(fold_count(&editor, cx), 0);
    keys("ctrl-s", cx);
    assert_eq!(
        cursor(&editor, cx),
        FOLDED.rfind("mark").unwrap() + 4,
        "the match after the fold"
    );
    assert_eq!(
        fold_count(&editor, cx),
        1,
        "the fold the search left closed"
    );
    keys("ctrl-r", cx);
    assert_eq!(fold_count(&editor, cx), 0, "back in it, it opens again");

    keys("ctrl-g", cx);
    assert_eq!(fold_count(&editor, cx), 1);
    assert_eq!(cursor(&editor, cx), 0);
}

#[gpui::test]
async fn isearch_ret_keeps_the_fold_around_the_match_open(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(FOLDED, true, cx).await;
    let cx = &mut cx;
    fold(&editor, fold_range(), cx);

    start(Forward, &workspace, cx);
    type_slowly("mark", cx);
    keys("ctrl-s ctrl-r enter", cx);
    assert!(!session_open(&workspace, cx));
    let mark = FOLDED.find("mark").unwrap();
    assert_eq!(
        selections(&editor, cx),
        vec![mark + 4..mark],
        "going backward, the point is at the start"
    );
    assert_eq!(fold_count(&editor, cx), 0, "the fold stays open");
    assert_eq!(text(&editor, cx), FOLDED);
}

#[gpui::test]
async fn isearch_a_match_in_a_collapsed_excerpt_opens_it_and_c_g_collapses_it(
    cx: &mut TestAppContext,
) {
    let (workspace, editor, mut cx) = workspace_with(true, cx, |project, window, cx| {
        let first = cx.new(|cx| Buffer::local("one\ntwo\n", cx));
        let second = cx.new(|cx| Buffer::local("three\nfind beta\nfour\n", cx));
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            multibuffer.set_excerpts_for_path(
                PathKey::sorted(0),
                first,
                [Point::new(0, 0)..Point::new(1, 3)],
                0,
                cx,
            );
            multibuffer.set_excerpts_for_path(
                PathKey::sorted(1),
                second,
                [Point::new(1, 0)..Point::new(1, 9)],
                0,
                cx,
            );
            multibuffer
        });
        cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx))
    })
    .await;
    let cx = &mut cx;
    let content = text(&editor, cx);
    let beta = content.find("beta").expect("the second buffer's excerpt");
    let second = editor.read_with(cx, |editor, cx| {
        assert_eq!(editor.buffer().read(cx).all_buffers().len(), 2);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        snapshot
            .range_to_buffer_ranges(MultiBufferOffset(beta)..MultiBufferOffset(beta + 4))
            .into_iter()
            .map(|(buffer, _, _)| buffer.remote_id())
            .next()
            .unwrap()
    });
    editor.update(cx, |editor, cx| editor.fold_buffer(second, cx));
    cx.run_until_parked();
    let folded = |cx: &mut VisualTestContext| {
        editor.read_with(cx, |editor, cx| editor.is_buffer_folded(second, cx))
    };
    assert!(folded(cx));

    start(Forward, &workspace, cx);
    type_slowly("beta", cx);
    assert_eq!(status(&workspace, cx), "1/1");
    assert!(!folded(cx), "the match opened its buffer");
    assert_eq!(cursor(&editor, cx), beta + 4);

    keys("ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
    assert!(folded(cx), "C-g collapses it again");
    assert_eq!(text(&editor, cx), content);

    // RET keeps it open.
    start(Forward, &workspace, cx);
    type_slowly("beta", cx);
    keys("enter", cx);
    assert!(!folded(cx));
}

#[gpui::test]
async fn isearch_an_edit_before_the_match_keeps_the_current_match_on_its_text(
    cx: &mut TestAppContext,
) {
    let (workspace, editor, mut cx) = workspace_text(XS, true, cx).await;
    let cx = &mut cx;
    color_the_active_match(cx);

    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    assert_eq!(cursor(&editor, cx), 3);
    assert_eq!(status(&workspace, cx), "1/4");

    edit(&editor, 0..0, "zzzz", cx);
    let edited = format!("zzzz{XS}");
    assert_eq!(text(&editor, cx), edited);
    assert_eq!(
        cursor(&editor, cx),
        7,
        "still after the first x, which is where the match is now"
    );
    assert_eq!(status(&workspace, cx), "1/4", "counted again");
    assert_eq!(active_rows(&editor, cx), vec![0]);

    // The next match is the second x, not the first again: the model's offsets moved with it.
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 11);
    assert_eq!(status(&workspace, cx), "2/4");
    assert_eq!(active_rows(&editor, cx), vec![1]);

    // An edit that takes the match away leaves the search failing rather than stuck.
    edit(&editor, 0..text(&editor, cx).len(), "nothing here", cx);
    assert!(session_open(&workspace, cx));
    keys("enter", cx);
    assert!(!session_open(&workspace, cx));
}

#[gpui::test]
async fn isearch_does_not_highlight_the_occurrences_of_a_search_selection(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, true, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    assert!(has_highlights(&editor, cx));

    let select = |from_search: bool, cx: &mut VisualTestContext| {
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(
                SelectionEffects::default().from_search(from_search),
                window,
                cx,
                |s| s.select_ranges([MultiBufferOffset(2)..MultiBufferOffset(3)]),
            )
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
    };
    let selected_text = |cx: &mut VisualTestContext| {
        editor.read_with(cx, |editor, _| {
            editor.has_background_highlights(HighlightKey::SelectedTextHighlight)
        })
    };

    select(true, cx);
    assert!(
        !selected_text(cx),
        "the other occurrences of a match are not lit up during the session"
    );
    // The same selection made by hand does light them up, so the test above means something.
    select(false, cx);
    assert!(selected_text(cx), "the control: a manual selection");
}
