//! Tests of starting a replace whose strings and range are handed over.

use bongos_isearch_model::CaseMode;
use gpui::TestAppContext;

use crate::commands_tests::scene;
use crate::starters::{ScopedReplace, start_in_scope, whole_word_pattern};

#[gpui::test]
async fn a_whole_word_regexp_is_wrapped_in_word_bounds_and_skips_a_substring(
    cx: &mut TestAppContext,
) {
    assert_eq!(whole_word_pattern("c.t"), r"\b(?:c.t)\b");
    let mut scene = scene("cat catalog cut a.b a.bc axb\n", cx).await;
    let editor = scene.editor.clone();
    let replace = |from: &str, regexp: bool, to: &str, scene: &mut crate::commands_tests::Scene| {
        let editor = editor.clone();
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
    };

    replace("c.t", true, "dog", &mut scene);
    assert_eq!(scene.text(), "dog catalog dog a.b a.bc axb\n");

    // A literal string is escaped: its `.` matches only a dot, and `a.bc` is not a whole word.
    replace("a.b", false, r"x\y", &mut scene);
    assert_eq!(scene.text(), "dog catalog dog x\\y a.bc axb\n");
}
