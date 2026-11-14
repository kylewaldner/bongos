//! Tests of the keys that look back and change the replacement: `^` walks to the replacements
//! made without undoing any, `u` undoes them one at a time, `,` replaces and waits, `e` edits the
//! replacement string, and `C-l` recenters. The keys are typed through the real rows of
//! `specific-overrides.json`.

use editor::HighlightKey;
use gpui::TestAppContext;
use zed_actions::minibuffer;

use crate::actions::{QueryReplace, QueryReplaceRegexp};
use crate::commands_tests::{Scene, scene};
use crate::display_tests::{REPLACED, color};

const ASKING: &str = "Query replacing foo with bar: (y/n/!/./,/^/q/?)";
const WALKING: &str = "(previous replacement; y/n to resume)";
const SHOWN: &str = "(replaced; y/n/q to continue)";

async fn asking(text: &str, from: &str, to: &str, cx: &mut TestAppContext) -> Scene {
    let mut scene = scene(text, cx).await;
    scene.select(&[0..0]);
    scene.replace(QueryReplace, from, to);
    assert!(scene.view_open(), "the decision view opens");
    scene
}

async fn asking_regexp(text: &str, from: &str, to: &str, cx: &mut TestAppContext) -> Scene {
    let mut scene = scene(text, cx).await;
    scene.select(&[0..0]);
    scene.replace(QueryReplaceRegexp, from, to);
    assert!(scene.view_open(), "the decision view opens");
    scene
}

impl Scene {
    fn has_highlights(&mut self) -> bool {
        self.editor.read_with(&self.cx, |editor, _| {
            editor.has_background_highlights(HighlightKey::QueryReplace)
        })
    }
}

#[gpui::test]
async fn caret_walks_back_through_the_replacements_without_undoing(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys("y");
    assert_eq!(scene.text(), "bar bar foo foo\n");
    scene.keys("^");
    assert_eq!(scene.prompt(), WALKING);
    assert_eq!(scene.selections(), vec![7..7]);
    scene.keys("^");
    assert_eq!(scene.prompt(), WALKING);
    assert_eq!(scene.selections(), vec![3..3]);
    // Nothing was undone.
    assert_eq!(scene.text(), "bar bar foo foo\n");
    // Nothing older: the status says so and the view stays where it was.
    scene.keys("^");
    assert_eq!(scene.status(), "No previous replacement");
    assert_eq!(scene.prompt(), WALKING);
    assert_eq!(scene.selections(), vec![3..3]);
    assert!(scene.view_open());
    // `y` goes back to the match the session was on and replaces it.
    scene.keys("y");
    assert_eq!(scene.text(), "bar bar bar foo\n");
    assert_eq!(scene.prompt(), ASKING);
    assert_eq!(scene.selections(), vec![15..15]);
    assert_eq!(scene.status(), "4/4");
}

#[gpui::test]
async fn n_after_the_walk_skips_the_match_the_session_was_on(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys("^");
    assert_eq!(scene.selections(), vec![3..3]);
    scene.keys("n");
    assert_eq!(scene.text(), "bar foo foo foo\n");
    assert_eq!(scene.selections(), vec![11..11]);
    assert_eq!(scene.status(), "3/4");
    scene.keys("q");
    assert_eq!(scene.messages(), vec!["Replaced 1, skipped 1"]);
}

#[gpui::test]
async fn q_after_the_walk_exits_at_the_shown_replacement(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys("y");
    scene.keys("^");
    scene.keys("q");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "bar bar foo foo\n");
    assert_eq!(scene.selections(), vec![7..7]);
    assert_eq!(scene.messages(), vec!["Replaced 2 occurrences"]);
}

#[gpui::test]
async fn caret_with_nothing_replaced_says_so_and_stays(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo\n", "foo", "bar", cx).await;
    scene.keys("^");
    assert_eq!(scene.status(), "No previous replacement");
    assert_eq!(scene.prompt(), ASKING);
    assert_eq!(scene.selections(), vec![3..3]);
    // The match is still asked about.
    scene.keys("y");
    assert_eq!(scene.text(), "bar foo\n");
}

#[gpui::test]
async fn u_undoes_one_replacement_at_a_time_and_asks_again(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys("y");
    assert_eq!(scene.text(), "bar bar foo\n");
    scene.keys("u");
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.selections(), vec![7..7]);
    assert_eq!(
        scene.prompt(),
        "Query replacing foo with bar: (y/n/!/./,/^/q/?)"
    );
    assert_eq!(scene.status(), "2/3");
    scene.keys("u");
    assert_eq!(scene.text(), "foo foo foo\n");
    assert_eq!(scene.selections(), vec![3..3]);
    assert_eq!(scene.status(), "1/3");
    // The stack is empty.
    scene.keys("u");
    assert_eq!(scene.status(), "Nothing to undo");
    assert_eq!(scene.text(), "foo foo foo\n");
    assert!(scene.view_open());
    // The undone matches are neither replaced nor skipped.
    scene.keys("q");
    assert_eq!(scene.messages(), vec!["Replaced 0 occurrences"]);
}

#[gpui::test]
async fn an_undone_match_is_decided_again(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("n");
    scene.keys("y");
    scene.keys("u");
    assert_eq!(scene.text(), "foo foo foo\n");
    assert_eq!(scene.status(), "2/3");
    scene.keys("y");
    assert_eq!(scene.text(), "foo bar foo\n");
    assert_eq!(scene.status(), "3/3");
    scene.keys("q");
    assert_eq!(scene.messages(), vec!["Replaced 1, skipped 1"]);
}

#[gpui::test]
async fn u_after_the_walk_undoes_the_newest_replacement(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys("y");
    scene.keys("^");
    scene.keys("u");
    assert_eq!(scene.text(), "bar foo foo foo\n");
    assert_eq!(scene.selections(), vec![7..7]);
    assert_eq!(scene.prompt(), ASKING);
}

#[gpui::test]
async fn the_counter_of_the_replacement_goes_down_with_u(cx: &mut TestAppContext) {
    let mut scene = asking_regexp("foo foo foo\n", "foo", "<\\#>", cx).await;
    assert_eq!(
        scene.prompt(),
        "Query replacing regexp foo with <0>: (y/n/!/./,/^/q/?)"
    );
    scene.keys("y");
    scene.keys("y");
    assert_eq!(scene.text(), "<0> <1> foo\n");
    assert_eq!(
        scene.prompt(),
        "Query replacing regexp foo with <2>: (y/n/!/./,/^/q/?)"
    );
    scene.keys("u");
    assert_eq!(scene.text(), "<0> foo foo\n");
    assert_eq!(
        scene.prompt(),
        "Query replacing regexp foo with <1>: (y/n/!/./,/^/q/?)"
    );
    scene.keys("y");
    assert_eq!(scene.text(), "<0> <1> foo\n");
}

#[gpui::test]
async fn y_u_y_q_then_one_undo_restores_the_whole_text(cx: &mut TestAppContext) {
    let original = "foo foo foo\n";
    let mut scene = asking(original, "foo", "bar", cx).await;
    for key in ["y", "u", "y", "q"] {
        scene.keys(key);
    }
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.messages(), vec!["Replaced 1 occurrence"]);
    scene.undo();
    assert_eq!(scene.text(), original);
    assert_eq!(scene.selections(), vec![0..0]);
}

#[gpui::test]
async fn replacements_made_after_an_undo_merge_into_the_oldest_surviving_one(
    cx: &mut TestAppContext,
) {
    let original = "foo foo foo foo\n";
    let mut scene = asking(original, "foo", "bar", cx).await;
    for key in ["y", "y", "u", "u", "y", "y", "y", "q"] {
        scene.keys(key);
    }
    assert_eq!(scene.text(), "bar bar bar foo\n");
    scene.undo();
    assert_eq!(scene.text(), original);
    assert_eq!(scene.selections(), vec![0..0]);
}

const GROUPS: &str = "val a: String?\nval b: String?\nval c: String?\n";
const PATTERN: &str = "val (\\w+): String\\?";
const TEMPLATE: &str = "var \\1: String? = null";

#[gpui::test]
async fn regexp_with_groups_steps_back_once_and_stops(cx: &mut TestAppContext) {
    let mut scene = asking_regexp(GROUPS, PATTERN, TEMPLATE, cx).await;
    assert_eq!(
        scene.prompt(),
        "Query replacing regexp val (\\w+): String\\? with var a: String? = null: \
         (y/n/!/./,/^/q/?)"
    );
    scene.keys("y");
    scene.keys("y");
    assert_eq!(
        scene.text(),
        "var a: String? = null\nvar b: String? = null\nval c: String?\n"
    );
    scene.keys("^");
    // Point is at the end of the second replacement.
    let end = scene.text().find("\nval c").unwrap();
    assert_eq!(scene.selections(), vec![end..end]);
    scene.keys("q");
    assert!(!scene.view_open());
    assert_eq!(scene.selections(), vec![end..end]);
    assert_eq!(scene.messages(), vec!["Replaced 2 occurrences"]);
}

#[gpui::test]
async fn regexp_with_groups_undoes_and_expands_the_groups_again(cx: &mut TestAppContext) {
    let mut scene = asking_regexp(GROUPS, PATTERN, TEMPLATE, cx).await;
    scene.keys("y");
    scene.keys("y");
    scene.keys("u");
    assert_eq!(
        scene.text(),
        "var a: String? = null\nval b: String?\nval c: String?\n"
    );
    assert_eq!(
        scene.prompt(),
        "Query replacing regexp val (\\w+): String\\? with var b: String? = null: \
         (y/n/!/./,/^/q/?)"
    );
    scene.keys("y");
    scene.keys("y");
    assert_eq!(
        scene.text(),
        "var a: String? = null\nvar b: String? = null\nvar c: String? = null\n"
    );
    scene.undo();
    assert_eq!(scene.text(), GROUPS);
}

#[gpui::test]
async fn a_match_changed_outside_the_session_leaves_nothing_for_u_to_undo(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    // The second match is edited behind the session's back, so `y` cannot replace it.
    scene.editor.update(&mut scene.cx, |editor, cx| {
        editor.edit(
            [(
                multi_buffer::MultiBufferOffset(4)..multi_buffer::MultiBufferOffset(7),
                "fox",
            )],
            cx,
        )
    });
    scene.cx.run_until_parked();
    scene.keys("y");
    assert_eq!(scene.text(), "bar fox foo\n");
    // The entry of the match that was not replaced undoes nothing.
    scene.keys("u");
    assert_eq!(scene.text(), "bar fox foo\n");
    // The next one undoes the replacement that was made.
    scene.keys("u");
    assert_eq!(scene.text(), "foo fox foo\n");
    assert_eq!(scene.selections(), vec![3..3]);
}

#[gpui::test]
async fn comma_replaces_and_waits_until_the_next_key(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.color_the_highlights();
    scene.keys(",");
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.prompt(), SHOWN);
    assert_eq!(scene.selections(), vec![3..3]);
    assert!(scene.highlights().contains(&(0, 0, color(REPLACED))));
    assert!(scene.view_open());
    // `y` searches on without replacing again.
    scene.keys("y");
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.prompt(), ASKING);
    assert_eq!(scene.selections(), vec![7..7]);
    assert!(!scene.highlights().contains(&(0, 0, color(REPLACED))));
    scene.keys("q");
    assert_eq!(scene.messages(), vec!["Replaced 1 occurrence"]);
}

#[gpui::test]
async fn n_after_comma_also_searches_on(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys(",");
    scene.keys("n");
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.selections(), vec![7..7]);
    assert_eq!(scene.status(), "2/3");
}

#[gpui::test]
async fn u_after_comma_undoes_it_and_q_after_comma_exits(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys(",");
    scene.keys("u");
    assert_eq!(scene.text(), "foo foo foo\n");
    assert_eq!(scene.prompt(), ASKING);
    assert_eq!(scene.selections(), vec![3..3]);
    scene.keys(",");
    scene.keys("q");
    assert!(!scene.view_open());
    assert_eq!(scene.text(), "bar foo foo\n");
    assert_eq!(scene.selections(), vec![3..3]);
    assert_eq!(scene.messages(), vec!["Replaced 1 occurrence"]);
}

#[gpui::test]
async fn the_walk_after_comma_shows_the_older_replacement_and_y_resumes(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    scene.keys(",");
    assert_eq!(scene.text(), "bar bar foo foo\n");
    scene.keys("^");
    assert_eq!(scene.prompt(), WALKING);
    assert_eq!(scene.selections(), vec![3..3]);
    scene.keys("y");
    // Back after the `,` result: the search goes on from there.
    assert_eq!(scene.text(), "bar bar foo foo\n");
    assert_eq!(scene.prompt(), ASKING);
    assert_eq!(scene.selections(), vec![11..11]);
}

#[gpui::test]
async fn e_edits_the_replacement_and_enter_resumes_on_the_same_match(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("y");
    // The settings are left alone: changing them drops the minibuffer routing of the test.
    let before = scene.highlights();
    assert!(!before.is_empty());
    scene.keys("e");
    // The prompt is open in place of the view, and the session goes on.
    assert!(!scene.view_open());
    assert!(scene.prompt_open());
    assert_eq!(scene.messages(), Vec::<String>::new());
    assert!(
        scene.has_highlights(),
        "the highlights stay under the prompt"
    );
    assert_eq!(scene.highlights(), before);
    assert_eq!(scene.selections(), vec![7..7]);
    // The current replacement is the selected initial input.
    scene.type_answer("baz");
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.cx.run_until_parked();
    assert!(scene.view_open());
    assert!(!scene.prompt_open());
    assert_eq!(
        scene.prompt(),
        "Query replacing foo with baz: (y/n/!/./,/^/q/?)"
    );
    assert_eq!(scene.status(), "2/3");
    assert_eq!(scene.selections(), vec![7..7]);
    assert_eq!(scene.highlights(), before);
    scene.keys("y");
    scene.keys("y");
    assert_eq!(scene.text(), "bar baz baz\n");
    assert_eq!(scene.messages(), vec!["Replaced 3 occurrences"]);
}

#[gpui::test]
async fn ctrl_g_in_the_edit_prompt_resumes_with_the_old_replacement(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys("e");
    scene.type_answer("zzz");
    scene.keys("ctrl-g");
    assert!(scene.view_open());
    assert!(!scene.prompt_open());
    assert_eq!(scene.messages(), Vec::<String>::new());
    assert_eq!(scene.prompt(), ASKING);
    assert_eq!(scene.selections(), vec![3..3]);
    assert!(scene.has_highlights());
    scene.keys("y");
    assert_eq!(scene.text(), "bar foo foo\n");
}

#[gpui::test]
async fn e_keeps_the_same_checks_as_the_replacement_prompt(cx: &mut TestAppContext) {
    let mut scene = asking_regexp("foo foo\n", "(o+)", "<\\1>", cx).await;
    scene.keys("e");
    scene.type_answer("\\2");
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.cx.run_until_parked();
    // `\2` names no group: the prompt stays open.
    assert!(scene.prompt_open());
    assert!(!scene.view_open());
    scene.keys("ctrl-g");
    assert!(scene.view_open());
}

#[gpui::test]
async fn e_after_comma_changes_what_the_next_matches_become(cx: &mut TestAppContext) {
    let mut scene = asking("foo foo foo\n", "foo", "bar", cx).await;
    scene.keys(",");
    scene.keys("e");
    scene.type_answer("baz");
    scene.cx.dispatch_action(minibuffer::Confirm);
    scene.cx.run_until_parked();
    assert!(scene.view_open());
    assert_eq!(scene.prompt(), SHOWN);
    scene.keys("y");
    assert_eq!(
        scene.prompt(),
        "Query replacing foo with baz: (y/n/!/./,/^/q/?)"
    );
    scene.keys("y");
    assert_eq!(scene.text(), "bar baz foo\n");
}

#[gpui::test]
async fn ctrl_l_cycles_the_match_through_middle_top_and_bottom(cx: &mut TestAppContext) {
    let text = "line\n".repeat(100) + "foo\n" + &"line\n".repeat(50);
    let mut scene = asking(&text, "foo", "bar", cx).await;
    let row = 100;
    // The window rows, the margin the scroll keeps, and the row of the match inside the window.
    let window = |scene: &mut Scene| -> (u32, u32, u32) {
        scene.editor.update(&mut scene.cx, |editor, cx| {
            let rows = editor.visible_row_count().expect("the editor has a size");
            let top = editor.scroll_position(cx).y as u32;
            let margin = (editor.vertical_scroll_margin() as u32).min(rows.saturating_sub(1) / 2);
            (rows, margin, row - top)
        })
    };
    let (rows, margin, _) = window(&mut scene);
    let mut seen = Vec::new();
    for _ in 0..4 {
        scene.keys("ctrl-l");
        seen.push(window(&mut scene).2);
    }
    assert_eq!(seen, [rows / 2, margin, rows - 1 - margin, rows / 2]);
    // Nothing was decided, and the match is still asked about.
    assert!(scene.view_open());
    assert_eq!(scene.text(), text);
    assert_eq!(scene.selections(), vec![503..503]);
    assert_eq!(scene.prompt(), ASKING);
}
