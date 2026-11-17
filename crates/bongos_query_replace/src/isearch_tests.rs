//! Tests of the hand-off from an incremental search: the search's string and settings are
//! carried, only the replacement is asked, the string lands on the FROM history, the replace
//! starts at the match, and the mark ring holds the search's origin once.

use bongos_emacs::actions::PopMark;
use bongos_isearch::actions::{
    Forward, QueryReplace, QueryReplaceRegexp, RepeatForward, ToggleCaseFold, ToggleRegexp,
    YankSymbol,
};
use bongos_minibuffer::HistoryStore;
use gpui::TestAppContext;
use zed_actions::minibuffer;

use crate::commands_tests::{Scene, scene_with};
use crate::prompts::{FROM_HISTORY, REGEXP_FROM_HISTORY, test_prompt};

async fn searching(text: &str, cursor: usize, search: &str, cx: &mut TestAppContext) -> Scene {
    let mut scene = scene_with(text, bongos_isearch::init, cx).await;
    scene.select(&[cursor..cursor]);
    scene.cx.dispatch_action(Forward);
    scene.cx.run_until_parked();
    assert!(scene.prompt_open(), "the search opens");
    for character in search.chars() {
        scene.cx.simulate_input(&character.to_string());
        scene.cx.run_until_parked();
    }
    scene
}

impl Scene {
    fn prompt_line(&mut self) -> String {
        let id = self.workspace.entity_id();
        self.cx
            .update(|_, cx| test_prompt(id, cx))
            .expect("a replace prompt is open")
            .1
    }

    fn ring(&mut self, name: &str) -> Vec<String> {
        self.cx
            .update(|_, cx| HistoryStore::global(cx).ring(name).entries().to_vec())
    }

    /// Answers the replacement prompt and the first `answers` questions with yes.
    fn finish(&mut self, to: &str, answers: usize) {
        self.type_answer(to);
        self.cx.dispatch_action(minibuffer::Confirm);
        self.cx.run_until_parked();
        for _ in 0..answers {
            self.cx.simulate_keystrokes("y");
            self.cx.run_until_parked();
        }
    }
}

#[gpui::test]
async fn only_the_replacement_is_asked_and_the_string_joins_the_ring(cx: &mut TestAppContext) {
    let mut scene = searching("x foo foo\n", 0, "foo", cx).await;
    scene.cx.dispatch_action(QueryReplace);
    scene.cx.run_until_parked();
    assert!(scene.prompt_open());
    assert_eq!(scene.prompt_line(), "Query replace foo with: ");
    assert_eq!(
        scene.ring(FROM_HISTORY).first().map(String::as_str),
        Some("foo")
    );
    scene.finish("bar", 2);
    assert_eq!(scene.text(), "x bar bar\n");
    assert_eq!(scene.messages(), vec!["Replaced 2 occurrences"]);
}

#[gpui::test]
async fn the_replace_starts_at_the_match_and_the_mark_holds_the_origin_once(
    cx: &mut TestAppContext,
) {
    let mut scene = searching("foo foo foo foo\n", 0, "foo", cx).await;
    scene.cx.dispatch_action(RepeatForward);
    scene.cx.run_until_parked();
    scene.cx.dispatch_action(QueryReplace);
    scene.cx.run_until_parked();
    scene.finish("bar", 3);
    assert_eq!(scene.text(), "foo bar bar bar\n");
    assert_eq!(scene.mark(), Some(0));
    scene.cx.dispatch_action(PopMark);
    assert_eq!(scene.selections(), vec![0..0]);
}

#[gpui::test]
async fn the_regexp_action_carries_a_regexp(cx: &mut TestAppContext) {
    let mut scene = searching("fo foo foooo\n", 0, "(f)o+", cx).await;
    scene.cx.dispatch_action(QueryReplaceRegexp);
    scene.cx.run_until_parked();
    assert_eq!(scene.prompt_line(), "Query replace regexp (f)o+ with: ");
    assert_eq!(
        scene.ring(REGEXP_FROM_HISTORY).first().map(String::as_str),
        Some("(f)o+")
    );
    scene.finish(r"\1x", 3);
    assert_eq!(scene.text(), "fx fx fx\n");
}

#[gpui::test]
async fn a_regexp_search_keeps_its_flag_with_the_text_action(cx: &mut TestAppContext) {
    let mut scene = searching("a1 b22\n", 0, "", cx).await;
    scene.cx.dispatch_action(ToggleRegexp);
    for character in "[0-9]+".chars() {
        scene.cx.simulate_input(&character.to_string());
        scene.cx.run_until_parked();
    }
    scene.cx.dispatch_action(QueryReplace);
    scene.cx.run_until_parked();
    assert_eq!(scene.prompt_line(), "Query replace regexp [0-9]+ with: ");
    scene.finish("N", 2);
    assert_eq!(scene.text(), "aN bN\n");
}

#[gpui::test]
async fn a_symbol_search_replaces_whole_symbols_only(cx: &mut TestAppContext) {
    let mut scene = searching("foo foobar foo\n", 1, "", cx).await;
    scene.cx.dispatch_action(YankSymbol);
    scene.cx.run_until_parked();
    scene.cx.dispatch_action(QueryReplace);
    scene.cx.run_until_parked();
    assert_eq!(scene.prompt_line(), "Query replace foo with: ");
    // The search found the next whole symbol, the last `foo`, and skipped `foobar`.
    scene.finish("x", 1);
    assert_eq!(scene.text(), "foo foobar x\n");
}

#[gpui::test]
async fn smart_case_is_carried(cx: &mut TestAppContext) {
    let mut scene = searching("foo Foo\n", 0, "Foo", cx).await;
    scene.cx.dispatch_action(QueryReplace);
    scene.cx.run_until_parked();
    assert_eq!(scene.prompt_line(), "Query replace Foo with: ");
    scene.finish("x", 1);
    assert_eq!(scene.text(), "foo x\n");
}

#[gpui::test]
async fn case_sensitive_is_carried(cx: &mut TestAppContext) {
    let mut scene = searching("Foo foo Foo\n", 0, "foo", cx).await;
    scene.cx.dispatch_action(ToggleCaseFold);
    scene.cx.run_until_parked();
    scene.cx.dispatch_action(QueryReplace);
    scene.cx.run_until_parked();
    assert_eq!(
        scene.prompt_line(),
        "Query replace (case-sensitive) foo with: "
    );
    scene.finish("x", 1);
    assert_eq!(scene.text(), "Foo x Foo\n");
    assert_eq!(scene.messages(), vec!["Replaced 1 occurrence"]);
}

#[gpui::test]
async fn case_insensitive_is_carried(cx: &mut TestAppContext) {
    let mut scene = searching("Foo foo\n", 0, "Foo", cx).await;
    scene.cx.dispatch_action(ToggleCaseFold);
    scene.cx.run_until_parked();
    scene.cx.dispatch_action(QueryReplace);
    scene.cx.run_until_parked();
    assert_eq!(
        scene.prompt_line(),
        "Query replace (case-insensitive) Foo with: "
    );
    scene.finish("x", 2);
    assert_eq!(scene.messages(), vec!["Replaced 2 occurrences"]);
}

#[gpui::test]
async fn m_c_at_the_replacement_prompt_changes_the_search(cx: &mut TestAppContext) {
    let mut scene = searching("Foo foo Foo\n", 0, "foo", cx).await;
    scene.cx.dispatch_action(QueryReplace);
    scene.cx.run_until_parked();
    scene.cx.simulate_keystrokes("alt-c");
    scene.cx.run_until_parked();
    assert_eq!(
        scene.prompt_line(),
        "Query replace (case-sensitive) foo with: "
    );
    scene.finish("x", 1);
    assert_eq!(scene.text(), "Foo x Foo\n");
    assert_eq!(scene.messages(), vec!["Replaced 1 occurrence"]);
}
