//! Tests of the decision view: each key, the questions it asks and the counter, the keys that wait
//! for a find, the point rules, the end messages and one undo step for a whole session. The keys
//! are typed through the real rows of `specific-overrides.json`.

use bongos_query_replace_model::text::help;
use editor::Editor;
use gpui::{Action as _, AppContext as _, Entity, TestAppContext};
use zed_actions::minibuffer;

use crate::actions::{QueryReplace, QueryReplaceRegexp, Yes};
use crate::commands_tests::{Scene, scene};
use crate::view::QueryReplaceView;

impl Scene {
    pub(crate) fn view(&self) -> Option<Entity<QueryReplaceView>> {
        self.workspace.read_with(&self.cx, |workspace, cx| {
            workspace.active_modal::<QueryReplaceView>(cx)
        })
    }

    pub(crate) fn view_open(&self) -> bool {
        self.view().is_some()
    }

    pub(crate) fn prompt(&self) -> String {
        let view = self.view().expect("the view is open");
        view.read_with(&self.cx, |view, _| view.prompt().to_string())
    }

    pub(crate) fn status(&self) -> String {
        let view = self.view().expect("the view is open");
        view.read_with(&self.cx, |view, _| view.status().to_string())
    }

    pub(crate) fn help_lines(&self) -> Vec<String> {
        let view = self.view().expect("the view is open");
        view.read_with(&self.cx, |view, _| view.help_lines().to_vec())
    }

    pub(crate) fn keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
        self.cx.run_until_parked();
    }
}

async fn asking(text: &str, from: &str, to: &str, cx: &mut TestAppContext) -> Scene {
    let mut scene = scene(text, cx).await;
    scene.select(&[0..0]);
    scene.replace(QueryReplace, from, to);
    assert!(scene.view_open(), "the decision view opens");
    scene
}

#[gpui::test]
async fn the_view_asks_with_the_replacement_and_the_counter(cx: &mut TestAppContext) {
    let scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    assert_eq!(
        scene.prompt(),
        "Query replacing foo with bar: (y/n/!/./,/^/q/?)"
    );
    assert_eq!(scene.status(), "1/3");
    assert!(scene.help_lines().is_empty());
}

#[gpui::test]
async fn y_and_space_replace_and_move_on(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.selections(), vec![7..7]);
    assert_eq!(scene.status(), "2/3");
    scene.keys("space");
    assert_eq!(scene.text(), "bar bar foo\n");
    assert_eq!(scene.selections(), vec![11..11]);
    assert_eq!(scene.status(), "3/3");
    assert!(scene.view_open());
}

#[gpui::test]
async fn n_and_backspace_skip_and_move_on(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("n");
    assert_eq!(scene.text(), "foo foo foo\n");
    assert_eq!(scene.selections(), vec![7..7]);
    scene.keys("backspace");
    assert_eq!(scene.selections(), vec![11..11]);
    assert_eq!(scene.status(), "3/3");
    // The last match is answered: the session runs out of matches, after the last one skipped.
    scene.keys("n");
    assert!(!scene.view_open());
    assert_eq!(scene.messages(), vec!["Replaced 0, skipped 3"]);
    assert_eq!(scene.selections(), vec![11..11]);
}

#[gpui::test]
async fn dot_replaces_and_exits_at_the_end_of_the_replacement(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "quux", cx).await;
    scene.keys("n");
    scene.keys(".");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "foo quux foo\n");
    assert_eq!(scene.selections(), vec![8..8]);
    assert_eq!(scene.messages(), vec!["Replaced 1, skipped 1"]);
}

async fn exits_at_the_end_of_the_match(key: &str, cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys(key);
    assert!(!scene.view_open(), "{key}");
    // The first match is replaced, the cursor stays at the end of the second one.
    assert_eq!(scene.text(), "bar foo foo\n", "{key}");
    assert_eq!(scene.selections(), vec![7..7], "{key}");
    assert_eq!(scene.messages(), vec!["Replaced 1 occurrence"], "{key}");
}

#[gpui::test]
async fn q_exits_at_the_end_of_the_match(cx: &mut TestAppContext) {
    exits_at_the_end_of_the_match("q", cx).await;
}

#[gpui::test]
async fn enter_exits_at_the_end_of_the_match(cx: &mut TestAppContext) {
    exits_at_the_end_of_the_match("enter", cx).await;
}

#[gpui::test]
async fn escape_exits_at_the_end_of_the_match(cx: &mut TestAppContext) {
    exits_at_the_end_of_the_match("escape", cx).await;
}

#[gpui::test]
async fn q_without_replacing_leaves_the_text_alone(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo\n", "foo", "bar", cx).await;
    scene.keys("q");
    assert_eq!(scene.text(), "foo foo\n");
    assert_eq!(scene.selections(), vec![3..3]);
    assert_eq!(scene.messages(), vec!["Replaced 0 occurrences"]);
    // Nothing was changed, so there is nothing to undo, and the editor has its keys back.
    assert!(!scene.view_open());
}

#[gpui::test]
async fn ctrl_g_exits_and_keeps_the_replacements(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys("y");
    scene.keys("ctrl-g");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "bar bar foo\n");
    assert_eq!(scene.selections(), vec![11..11]);
    assert_eq!(scene.messages(), vec!["Replaced 2 occurrences"]);
}

#[gpui::test]
async fn question_mark_shows_the_help_until_the_next_key(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("?");
    assert_eq!(scene.help_lines(), help());
    assert!(scene.view_open());
    // It stays through nothing else but a key: this one answers and takes the list away.
    scene.keys("n");
    assert!(scene.help_lines().is_empty());
    assert_eq!(scene.selections(), vec![7..7]);
    assert_eq!(scene.text(), "foo foo foo\n");
    // A key that nothing is bound to takes it away too, and changes nothing.
    scene.keys("?");
    scene.keys("x");
    assert!(scene.help_lines().is_empty());
    assert_eq!(scene.selections(), vec![7..7]);
    assert!(scene.view_open());
}

#[gpui::test]
async fn rename_carefully_is_one_undo_step(cx: &mut TestAppContext) {
    let original = "fetchUser(a);\nfetchUser(b);\nfetchUser(c);\nfetchUser(d);\n";
    let mut scene = asking(original, "fetchUser", "loadUser", cx).await;
    scene.keys("y");
    scene.keys("n");
    scene.keys("y");
    scene.keys("y");
    assert!(!scene.view_open());
    assert_eq!(
        scene.text(),
        "loadUser(a);\nfetchUser(b);\nloadUser(c);\nloadUser(d);\n"
    );
    assert_eq!(scene.messages(), vec!["Replaced 3, skipped 1"]);
    // The cursor is at the end of the last replacement.
    let end = scene.text().find("(d)").unwrap();
    assert_eq!(scene.selections(), vec![end..end]);
    scene.undo();
    assert_eq!(scene.text(), original);
    assert_eq!(scene.selections(), vec![0..0]);
}

#[gpui::test]
async fn running_out_of_matches_leaves_the_cursor_after_the_last_replacement(
    cx: &mut TestAppContext,
) {
    let mut scene = asking("foo bar foo baz\n", "foo", "quux", cx).await;
    scene.keys("y");
    scene.keys("y");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "quux bar quux baz\n");
    assert_eq!(scene.selections(), vec![13..13]);
    assert_eq!(scene.messages(), vec!["Replaced 2 occurrences"]);
}

#[gpui::test]
async fn keys_pressed_during_a_find_wait_for_the_match(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo foo\n", "foo", "bar", cx).await;
    let press = |scene: &mut Scene, action: Box<dyn gpui::Action>| {
        scene
            .cx
            .update(|window, cx| window.dispatch_action(action, cx));
    };
    press(&mut scene, Yes.boxed_clone());
    // The first replacement is made and the next match is being found.
    assert_eq!(scene.text(), "bar foo foo foo\n");
    assert_eq!(scene.status(), "…");
    press(&mut scene, Yes.boxed_clone());
    press(&mut scene, crate::actions::No.boxed_clone());
    press(&mut scene, Yes.boxed_clone());
    // Nothing acted on the stale match.
    assert_eq!(scene.text(), "bar foo foo foo\n");
    assert!(scene.view_open());
    scene.cx.run_until_parked();
    assert_eq!(scene.text(), "bar bar foo bar\n");
    assert!(!scene.view_open());
    assert_eq!(scene.messages(), vec!["Replaced 3, skipped 1"]);
}

#[gpui::test]
async fn zero_width_matches_are_asked_about_once_each(cx: &mut TestAppContext) {
    let mut scene = scene("a\nb\n", cx).await;
    scene.select(&[0..0]);
    scene.replace(QueryReplaceRegexp, "^", "> ");
    assert!(scene.view_open());
    assert_eq!(
        scene.prompt(),
        "Query replacing regexp ^ with > : (y/n/!/./,/^/q/?)"
    );
    scene.keys("y");
    scene.keys("y");
    scene.keys("y");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "> a\n> b\n> ");
    assert_eq!(scene.messages(), vec!["Replaced 3 occurrences"]);
    scene.undo();
    assert_eq!(scene.text(), "a\nb\n");
}

#[gpui::test]
async fn a_search_with_no_match_opens_no_view(cx: &mut TestAppContext) {
    let mut scene = scene("alpha beta\n", cx).await;
    scene.select(&[0..0]);
    scene.replace(QueryReplace, "zzz", "y");
    assert!(!scene.view_open());
    assert_eq!(scene.messages(), vec!["Nothing to replace"]);
}

#[gpui::test]
async fn closing_the_view_otherwise_exits(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.cx.dispatch_action(minibuffer::Cancel);
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.messages().len(), 1);
}

#[gpui::test]
async fn a_region_within_a_line_seeds_the_string(cx: &mut TestAppContext) {
    let mut scene = scene("x foo y foo\n", cx).await;
    scene.select(&[2..5]);
    scene.cx.dispatch_action(QueryReplace);
    // The string is the region, so Enter takes it as it is.
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.type_answer("bar");
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.cx.run_until_parked();
    assert!(scene.view_open());
    // The search starts at the start of the region, so the selected occurrence is the first match.
    assert_eq!(scene.selections(), vec![5..5]);
    assert_eq!(scene.status(), "1/2");
}

#[gpui::test]
async fn a_single_line_editor_does_not_start_a_replace(cx: &mut TestAppContext) {
    let mut scene = scene("foo\n", cx).await;
    let line = scene.workspace.update_in(&mut scene.cx, |_, window, cx| {
        cx.new(|cx| Editor::single_line(window, cx))
    });
    scene.cx.update(|window, cx| {
        window.focus(&gpui::Focusable::focus_handle(line.read(cx), cx), cx);
    });
    scene.cx.dispatch_action(QueryReplace);
    assert!(!scene.prompt_open());
    assert!(!scene.view_open());
    assert_eq!(scene.messages(), Vec::<String>::new());
}
