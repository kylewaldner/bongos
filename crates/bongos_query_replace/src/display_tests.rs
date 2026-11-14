//! Tests of what a replace shows in the editor: the highlights of the matches, of the replacement
//! just made and of the region, their removal at the end, and the folds and collapsed excerpts a
//! match opens and closes again.

use std::ops::Range;

use editor::{Editor, HighlightKey};
use gpui::{AppContext as _, Entity, Focusable as _, Hsla, TestAppContext, UpdateGlobal as _};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, MultiBufferOffset, PathKey};
use settings::{SettingsStore, ThemeColorsContent, ThemeStyleContent};
use zed_actions::minibuffer;

use crate::actions::QueryReplace;
use crate::commands_tests::{Scene, scene};

pub(crate) const ACTIVE: &str = "#ff0000ff";
pub(crate) const OTHER: &str = "#00ff00ff";
pub(crate) const REPLACED: &str = "#0000ffff";
pub(crate) const REGION: &str = "#ffff00ff";

pub(crate) fn color(hex: &str) -> Hsla {
    gpui::Rgba::try_from(hex).expect("a color").into()
}

impl Scene {
    /// Gives the three highlights and the region their own colors, so they can be told apart.
    pub(crate) fn color_the_highlights(&mut self) {
        self.cx.update(|_, cx| {
            SettingsStore::update_global(cx, |settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.theme.experimental_theme_overrides = Some(ThemeStyleContent {
                        colors: ThemeColorsContent {
                            search_active_match_background: Some(ACTIVE.to_string()),
                            search_match_background: Some(OTHER.to_string()),
                            editor_document_highlight_read_background: Some(REPLACED.to_string()),
                            editor_highlighted_line_background: Some(REGION.to_string()),
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                });
            });
        });
        self.cx.run_until_parked();
    }

    /// The highlights as (row, column, color), in order.
    pub(crate) fn highlights(&mut self) -> Vec<(u32, u32, Hsla)> {
        self.editor.update_in(&mut self.cx, |editor, window, cx| {
            editor
                .all_text_background_highlights(window, cx)
                .into_iter()
                .map(|(range, color)| (range.start.row().0, range.start.column(), color))
                .collect()
        })
    }

    fn highlighted(&mut self, key: HighlightKey) -> bool {
        self.editor
            .read_with(&self.cx, |editor, _| editor.has_background_highlights(key))
    }

    fn fold_count(&mut self) -> usize {
        self.editor.update(&mut self.cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let len = display.buffer_snapshot().len();
            display.folds_in_range(MultiBufferOffset(0)..len).count()
        })
    }

    fn fold(&mut self, range: Range<usize>) {
        self.editor.update_in(&mut self.cx, |editor, window, cx| {
            editor.fold_ranges(
                vec![MultiBufferOffset(range.start)..MultiBufferOffset(range.end)],
                false,
                window,
                cx,
            )
        });
        self.cx.run_until_parked();
    }
}

#[gpui::test]
async fn the_matches_the_current_one_and_the_replacement_have_three_colors(
    cx: &mut TestAppContext,
) {
    let mut scene = scene("foo foo foo foo\n", cx).await;
    scene.select(&[0..0]);
    scene.replace(QueryReplace, "foo", "bar");
    scene.color_the_highlights();
    assert!(scene.highlighted(HighlightKey::QueryReplace));
    assert!(!scene.highlighted(HighlightKey::QueryReplaceRegion));
    assert_eq!(
        scene.highlights(),
        vec![
            (0, 0, color(ACTIVE)),
            (0, 4, color(OTHER)),
            (0, 8, color(OTHER)),
            (0, 12, color(OTHER))
        ]
    );

    // The replacement just made stays until the next decision; matches go on from the current one.
    scene.keys("y");
    assert_eq!(scene.text(), "bar foo foo foo\n");
    assert_eq!(
        scene.highlights(),
        vec![
            (0, 0, color(REPLACED)),
            (0, 4, color(ACTIVE)),
            (0, 8, color(OTHER)),
            (0, 12, color(OTHER))
        ]
    );
    scene.keys("n");
    assert_eq!(
        scene.highlights(),
        vec![(0, 8, color(ACTIVE)), (0, 12, color(OTHER))],
        "the replacement is gone with the next decision, and the skipped match with it"
    );
}

#[gpui::test]
async fn highlights_never_leave_the_scope_and_a_multi_line_region_stays_visible(
    cx: &mut TestAppContext,
) {
    let mut scene = scene("foo\nfoo\nfoo\nfoo\n", cx).await;
    // The second and third lines.
    scene.select(&[4..11]);
    scene.replace(QueryReplace, "foo", "x");
    scene.color_the_highlights();
    assert!(scene.highlighted(HighlightKey::QueryReplaceRegion));
    let highlights = scene.highlights();
    let matches: Vec<_> = highlights
        .iter()
        .filter(|(_, _, hsla)| *hsla != color(REGION))
        .map(|(row, column, _)| (*row, *column))
        .collect();
    assert_eq!(
        matches,
        vec![(1, 0), (2, 0)],
        "nothing on the first or last line"
    );
    assert!(
        highlights.iter().any(|(_, _, hsla)| *hsla == color(REGION)),
        "the region is shown"
    );

    scene.keys("y y");
    assert!(!scene.highlighted(HighlightKey::QueryReplace));
    assert!(!scene.highlighted(HighlightKey::QueryReplaceRegion));
}

#[gpui::test]
async fn a_region_within_one_line_seeds_the_string_and_shows_no_region(cx: &mut TestAppContext) {
    let mut scene = scene("a foo b foo\nfoo\n", cx).await;
    scene.select(&[2..5]);
    scene.cx.dispatch_action(QueryReplace);
    // The seeded string is accepted as it is.
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.type_answer("x");
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.cx.run_until_parked();
    scene.color_the_highlights();
    assert!(!scene.highlighted(HighlightKey::QueryReplaceRegion));
    assert_eq!(
        scene.highlights(),
        vec![
            (0, 2, color(ACTIVE)),
            (0, 8, color(OTHER)),
            (1, 0, color(OTHER))
        ],
        "the whole text is searched, from the selected occurrence"
    );
}

async fn way_out_clears_the_highlights(key: &str, cx: &mut TestAppContext) {
    let mut scene = scene("foo\nfoo\nfoo\nfoo\n", cx).await;
    scene.select(&[4..11]);
    scene.replace(QueryReplace, "foo", "x");
    assert!(scene.highlighted(HighlightKey::QueryReplace), "{key}");
    assert!(scene.highlighted(HighlightKey::QueryReplaceRegion), "{key}");
    scene.keys(key);
    assert!(!scene.highlighted(HighlightKey::QueryReplace), "{key}");
    assert!(
        !scene.highlighted(HighlightKey::QueryReplaceRegion),
        "{key}"
    );
}

#[gpui::test]
async fn q_clears_the_highlights(cx: &mut TestAppContext) {
    way_out_clears_the_highlights("q", cx).await;
}

#[gpui::test]
async fn enter_clears_the_highlights(cx: &mut TestAppContext) {
    way_out_clears_the_highlights("enter", cx).await;
}

#[gpui::test]
async fn c_g_clears_the_highlights(cx: &mut TestAppContext) {
    way_out_clears_the_highlights("ctrl-g", cx).await;
}

#[gpui::test]
async fn running_out_of_matches_clears_the_highlights(cx: &mut TestAppContext) {
    let mut scene = scene("foo\nfoo\nfoo\nfoo\n", cx).await;
    scene.select(&[4..11]);
    scene.replace(QueryReplace, "foo", "x");
    scene.keys("y y");
    assert!(!scene.highlighted(HighlightKey::QueryReplace));
    assert!(!scene.highlighted(HighlightKey::QueryReplaceRegion));
}

#[gpui::test]
async fn a_match_in_a_fold_opens_it_and_moving_on_folds_it_again(cx: &mut TestAppContext) {
    let mut scene = scene("foo\nhidden foo\nfoo\nfoo\n", cx).await;
    scene.select(&[0..0]);
    // The second line.
    scene.fold(4..14);
    assert_eq!(scene.fold_count(), 1);
    scene.replace(QueryReplace, "foo", "x");
    assert_eq!(scene.fold_count(), 1, "the first match is outside the fold");
    scene.keys("n");
    assert_eq!(scene.fold_count(), 0, "the match in the fold opened it");
    scene.keys("n");
    assert_eq!(scene.fold_count(), 1, "moving on folded it again");
}

#[gpui::test]
async fn ending_on_a_match_in_a_fold_leaves_it_open(cx: &mut TestAppContext) {
    let mut scene = scene("foo\nhidden foo\nfoo\nfoo\n", cx).await;
    scene.select(&[0..0]);
    scene.fold(4..14);
    scene.replace(QueryReplace, "foo", "x");
    scene.keys("n");
    assert_eq!(scene.fold_count(), 0);
    scene.keys("q");
    assert_eq!(
        scene.fold_count(),
        0,
        "the fold around the cursor stays open"
    );
}

#[gpui::test]
async fn a_match_in_a_collapsed_excerpt_opens_it(cx: &mut TestAppContext) {
    let mut scene = scene("", cx).await;
    let editor = scene
        .workspace
        .update_in(&mut scene.cx, |workspace, window, cx| {
            let project = workspace.project().clone();
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
            let editor =
                cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project), window, cx));
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            window.focus(&editor.focus_handle(cx), cx);
            editor
        });
    scene.editor = editor.clone();
    scene.cx.run_until_parked();
    let content = scene.text();
    // A region over both excerpts bounds the replace.
    scene.select(&[0..content.len()]);
    let beta = content.find("beta").expect("the second excerpt");
    let second = editor.read_with(&scene.cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        snapshot
            .range_to_buffer_ranges(MultiBufferOffset(beta)..MultiBufferOffset(beta + 4))
            .into_iter()
            .map(|(buffer, _, _)| buffer.remote_id())
            .next()
            .expect("a buffer")
    });
    editor.update(&mut scene.cx, |editor, cx| editor.fold_buffer(second, cx));
    scene.cx.run_until_parked();
    let folded = |scene: &mut Scene| {
        let editor: Entity<Editor> = scene.editor.clone();
        editor.read_with(&scene.cx, |editor, cx| editor.is_buffer_folded(second, cx))
    };
    assert!(folded(&mut scene));

    scene.replace(QueryReplace, "beta", "gamma");
    assert!(!folded(&mut scene), "the match opened its excerpt");
    scene.keys("q");
    assert!(!folded(&mut scene), "it stays open at the end");
}
