use std::sync::Arc;

use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use language::{Language, LanguageConfig};

use crate::actions::{ContractRegion, ExpandRegion};
use crate::apply_tests::init_test;

const TEXT: &str = "fn f() {\n    let x = foo(ˇbar, baz);\n}\n";

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let language = Arc::new(Language::new(
        LanguageConfig::default(),
        Some(tree_sitter_rust::LANGUAGE.into()),
    ));
    cx.update_buffer(|buffer, cx| buffer.set_language(Some(language), cx));
    cx.set_state(TEXT);
    cx.run_until_parked();
    cx
}

fn expand(cx: &mut EditorTestContext, count: Option<i32>) {
    cx.dispatch_action(ExpandRegion { count });
}

fn contract(cx: &mut EditorTestContext, count: Option<i32>) {
    cx.dispatch_action(ContractRegion { count });
}

fn selected(cx: &mut EditorTestContext) -> String {
    cx.update_editor(|editor, window, cx| {
        let _ = window;
        let snapshot = editor.display_snapshot(cx);
        let range = editor.selections.newest_adjusted(&snapshot).range();
        snapshot.buffer_snapshot().text_for_range(range).collect()
    })
}

#[gpui::test]
async fn expand_twice_then_contract_returns_to_first_expansion(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    expand(&mut cx, None);
    let first = selected(&mut cx);
    assert_eq!(first, "bar");
    expand(&mut cx, None);
    let second = selected(&mut cx);
    assert_eq!(second, "(bar, baz)");
    contract(&mut cx, None);
    assert_eq!(selected(&mut cx), first);
}

#[gpui::test]
async fn count_expands_that_many_levels(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    expand(&mut cx, Some(2));
    assert_eq!(selected(&mut cx), "(bar, baz)");
    contract(&mut cx, Some(2));
    assert_eq!(selected(&mut cx), "");
}

#[gpui::test]
async fn negative_count_goes_the_other_way(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    expand(&mut cx, Some(-1));
    assert_eq!(selected(&mut cx), "");
    expand(&mut cx, Some(2));
    contract(&mut cx, Some(-1));
    assert_eq!(selected(&mut cx), "foo(bar, baz)");
}

#[gpui::test]
async fn contract_without_history_does_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    contract(&mut cx, None);
    cx.assert_editor_state(TEXT);
}
