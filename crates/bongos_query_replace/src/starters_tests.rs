//! Tests of starting a replace whose strings and range are handed over.

use bongos_isearch_model::CaseMode;
use gpui::TestAppContext;

use crate::commands_tests::{Scene, scene};
use crate::starters::{ScopedReplace, start_in_scope};

/// Replaces every whole-word match of `from` in the whole buffer.
fn replace_whole_words(from: &str, regexp: bool, to: &str, scene: &mut Scene) {
    let editor = scene.editor.clone();
    let (from, to) = (from.to_string(), to.to_string());
    scene.cx.update(|window, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let scope = snapshot.anchor_before(multi_buffer::MultiBufferOffset(0))
            ..snapshot.anchor_after(snapshot.len());
        start_in_scope(
            &editor,
            ScopedReplace {
                from,
                regexp,
                case: CaseMode::Smart,
                whole_word: true,
                scope,
                to,
                interactive: false,
            },
            window,
            cx,
        );
    });
    scene.cx.run_until_parked();
}

#[gpui::test]
async fn a_whole_word_regexp_is_wrapped_in_word_bounds_and_skips_a_substring(
    cx: &mut TestAppContext,
) {
    let mut scene = scene("cat catalog cut a.b a.bc axb\n", cx).await;

    replace_whole_words("c.t", true, "dog", &mut scene);
    assert_eq!(scene.text(), "dog catalog dog a.b a.bc axb\n");

    // A literal string stays literal: its `.` matches only a dot, `a.bc` is not a whole word,
    // and the backslash of its replacement is kept.
    replace_whole_words("a.b", false, r"x\y", &mut scene);
    assert_eq!(scene.text(), "dog catalog dog x\\y a.bc axb\n");
}

#[gpui::test]
async fn a_whole_word_with_punctuation_edges_matches_as_project_search_does(
    cx: &mut TestAppContext,
) {
    // Project search's rule for a literal: an edge that is a word character must not continue a
    // word, and an edge that is not one bounds nothing, so `a@x` matches and `@xy` does not.
    // The replacement stays literal.
    let mut literal = scene("@x a@x (@x) @xy\n", cx).await;
    replace_whole_words("@x", false, "$1", &mut literal);
    assert_eq!(literal.text(), "$1 a$1 ($1) @xy\n");

    // Its rule for a regexp: a word bound only on an edge that is a word character.
    let mut regexp = scene("@x a@x (@x) @xy\n", cx).await;
    replace_whole_words("@x", true, "Z", &mut regexp);
    assert_eq!(regexp.text(), "Z aZ (Z) @xy\n");
}
