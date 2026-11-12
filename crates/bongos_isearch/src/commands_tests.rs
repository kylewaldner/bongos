//! Tests of the commands of a session that build or change the search: the yanks, M-e, the
//! toggles, the jumps to the first and last match, and recenter. The keys are the real rows of
//! `specific-overrides.json`.

use std::ops::Range;

use editor::Editor;
use gpui::{ClipboardItem, Entity, KeyBinding, TestAppContext, VisualTestContext};

use crate::actions::Forward;
use crate::seams::set_kill_source;
use crate::session_tests::{
    cursor, input, keys, prompt, selections, session_open, set_selections, start, status, text,
    type_slowly, workspace_text,
};

/// Two `alpha beta` pairs, a `fooBar` between two `foo`s.
const WORDS: &str = "pad alpha beta alpha beta\n";
const SYMBOLS: &str = "foo fooBar foo\n";

#[gpui::test]
async fn isearch_c_w_yanks_the_word_at_point_and_ret_selects_it(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(WORDS, true, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![4..4], cx);
    start(Forward, &workspace, cx);
    keys("ctrl-w", cx);
    assert_eq!(input(&workspace, cx), "alpha");
    assert_eq!(cursor(&editor, cx), 9);
    keys("enter", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(selections(&editor, cx), vec![4..9]);
    assert_eq!(text(&editor, cx), WORDS);
}

#[gpui::test]
async fn isearch_c_w_twice_extends_by_two_words(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(WORDS, true, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![4..4], cx);
    start(Forward, &workspace, cx);
    keys("ctrl-w ctrl-w", cx);
    assert_eq!(input(&workspace, cx), "alpha beta");
    assert_eq!(cursor(&editor, cx), 14);
    assert_eq!(status(&workspace, cx), "1/2");
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![4..14]);
}

#[gpui::test]
async fn isearch_yanks_a_character_and_the_rest_of_the_line(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("ab cd ef\nx\n", true, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![3..3], cx);
    start(Forward, &workspace, cx);
    keys("ctrl-alt-y", cx);
    assert_eq!(input(&workspace, cx), "c");
    assert_eq!(cursor(&editor, cx), 4);
    keys("alt-s ctrl-e", cx);
    assert_eq!(input(&workspace, cx), "cd ef");
    assert_eq!(cursor(&editor, cx), 8);
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![3..8]);
}

#[gpui::test]
async fn isearch_yank_symbol_matches_whole_symbols_and_shows_the_prompt(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(SYMBOLS, true, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![1..1], cx);
    start(Forward, &workspace, cx);
    keys("alt-s .", cx);
    assert_eq!(input(&workspace, cx), "foo");
    assert_eq!(prompt(&workspace, cx), "Symbol I-search: ");
    // `fooBar` is skipped: the next whole `foo` is the last one.
    assert_eq!(cursor(&editor, cx), 14);
    // The symbol stays on later edits.
    keys("backspace", cx);
    type_slowly("o", cx);
    assert_eq!(prompt(&workspace, cx), "Symbol I-search: ");
    // M-r leaves symbol mode for regexp.
    keys("alt-r", cx);
    assert_eq!(prompt(&workspace, cx), "Regexp I-search: ");
    keys("ctrl-g ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
}

#[gpui::test]
async fn isearch_c_y_appends_the_clipboard_or_the_installed_kill_source(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("foobar xx\nx\ny\n", true, cx).await;
    let cx = &mut cx;
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("bar".to_string())));
    start(Forward, &workspace, cx);
    type_slowly("foo", cx);
    keys("ctrl-y", cx);
    assert_eq!(input(&workspace, cx), "foobar");
    assert_eq!(cursor(&editor, cx), 6);

    // A newline in the clipboard is a newline of the string.
    keys("ctrl-g ctrl-g", cx);
    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("x\ny".to_string())));
    start(Forward, &workspace, cx);
    keys("ctrl-y", cx);
    assert_eq!(input(&workspace, cx), "x\ny");
    assert_eq!(cursor(&editor, cx), 13);
    keys("ctrl-g ctrl-g", cx);

    cx.update(|_, cx| set_kill_source(std::rc::Rc::new(|_| Some("xx".to_string())), cx));
    start(Forward, &workspace, cx);
    keys("ctrl-y", cx);
    assert_eq!(input(&workspace, cx), "xx");
    assert_eq!(cursor(&editor, cx), 9);
    keys("ctrl-g ctrl-g", cx);
}

#[gpui::test]
async fn isearch_c_j_matches_across_a_line_end(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("ab\ncd\nbc\n", true, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("b", cx);
    keys("ctrl-j", cx);
    type_slowly("c", cx);
    assert_eq!(input(&workspace, cx), "b\nc");
    assert_eq!(cursor(&editor, cx), 4);
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![1..4]);
}

/// Binds the keys the input editor needs to be edited, which the shipped base keymap provides
/// and the overrides do not.
fn bind_input_keys(cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("left", editor::actions::MoveLeft, Some("Editor")),
            KeyBinding::new("backspace", editor::actions::Backspace, Some("Editor")),
        ])
    });
}

#[gpui::test]
async fn isearch_m_e_edits_the_string_and_ret_resumes_the_search(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("abd acd abd\n", true, cx).await;
    let cx = &mut cx;
    bind_input_keys(cx);
    start(Forward, &workspace, cx);
    type_slowly("abd", cx);
    assert_eq!(cursor(&editor, cx), 3);

    keys("alt-e", cx);
    assert_eq!(prompt(&workspace, cx), "Edit I-search string: ");
    // Mid-string edits are not searched: `abd` becomes `acd`.
    keys("left backspace", cx);
    type_slowly("c", cx);
    assert_eq!(cursor(&editor, cx), 3, "nothing is searched while editing");
    assert_eq!(input(&workspace, cx), "abd");

    // C-s, C-r and RET resume; the others of the session's keys act as in any input.
    keys("enter", cx);
    assert!(session_open(&workspace, cx), "RET resumes the search");
    assert_eq!(input(&workspace, cx), "acd");
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    assert_eq!(cursor(&editor, cx), 7);
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![4..7]);
    assert_eq!(text(&editor, cx), "abd acd abd\n");
}

#[gpui::test]
async fn isearch_m_e_then_c_s_searches_the_edited_string_forward(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("abd acd abd\n", true, cx).await;
    let cx = &mut cx;
    bind_input_keys(cx);
    start(Forward, &workspace, cx);
    type_slowly("abd", cx);
    keys("alt-e left backspace", cx);
    type_slowly("c", cx);
    keys("ctrl-s", cx);
    assert!(session_open(&workspace, cx));
    assert_eq!(input(&workspace, cx), "acd");
    assert_eq!(cursor(&editor, cx), 7);
    keys("ctrl-g", cx);
}

#[gpui::test]
async fn isearch_m_e_then_c_g_restores_the_string_and_resumes(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("abd acd abd\n", true, cx).await;
    let cx = &mut cx;
    bind_input_keys(cx);
    start(Forward, &workspace, cx);
    type_slowly("abd", cx);
    keys("alt-e left backspace", cx);
    type_slowly("c", cx);
    keys("ctrl-g", cx);
    assert!(session_open(&workspace, cx), "C-g ends the edit only");
    assert_eq!(input(&workspace, cx), "abd");
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    assert_eq!(cursor(&editor, cx), 3);
    // The search goes on with the old string.
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 11);
    keys("ctrl-g ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(cursor(&editor, cx), 0);
}

#[gpui::test]
async fn isearch_m_c_toggles_case_sensitivity(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("Foo foo FOO\n", true, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("foo", cx);
    assert_eq!(cursor(&editor, cx), 3);
    assert_eq!(status(&workspace, cx), "1/3");
    keys("alt-c", cx);
    assert_eq!(prompt(&workspace, cx), "case-sensitive I-search: ");
    assert_eq!(cursor(&editor, cx), 7);
    assert_eq!(status(&workspace, cx), "1/1");
    keys("alt-c", cx);
    assert_eq!(prompt(&workspace, cx), "case-insensitive I-search: ");
    // The search goes on from the start of the step it toggles, so it stays on this match.
    assert_eq!(cursor(&editor, cx), 7);
    assert_eq!(status(&workspace, cx), "2/3");
    keys("ctrl-g ctrl-g", cx);
}

#[gpui::test]
async fn isearch_m_r_toggles_regexp(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("abc a.c\n", true, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("a.c", cx);
    assert_eq!(cursor(&editor, cx), 7);
    assert_eq!(status(&workspace, cx), "1/1");
    keys("alt-r", cx);
    assert_eq!(prompt(&workspace, cx), "Regexp I-search: ");
    assert_eq!(status(&workspace, cx), "2/2");
    keys("alt-r", cx);
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    assert_eq!(status(&workspace, cx), "1/1");
    keys("ctrl-g ctrl-g", cx);
}

#[gpui::test]
async fn isearch_m_s_space_toggles_lax_whitespace(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("foo(x, bar\n", true, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("foo bar", cx);
    assert_eq!(cursor(&editor, cx), 10);
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    keys("alt-s space", cx);
    assert_eq!(prompt(&workspace, cx), "Failing strict-space I-search: ");
    keys("alt-s space", cx);
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    assert_eq!(cursor(&editor, cx), 10);
    keys("ctrl-g ctrl-g", cx);
}

#[gpui::test]
async fn isearch_jumps_to_the_first_and_last_match(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("a x\nb x\nc x\nd x\n", true, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    keys("ctrl-s ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 11);
    keys("alt-s alt-<", cx);
    assert_eq!(cursor(&editor, cx), 3);
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    assert_eq!(status(&workspace, cx), "1/4");
    keys("alt-s alt->", cx);
    assert_eq!(cursor(&editor, cx), 14);
    assert_eq!(prompt(&workspace, cx), "I-search backward: ");
    assert_eq!(status(&workspace, cx), "4/4");
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![Range { start: 15, end: 14 }]);
}

/// The row of the cursor inside the window.
fn window_row(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> u32 {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let row = editor.selections.newest_display(&display).head().row().0;
        row - editor.scroll_position(cx).y as u32
    })
}

#[gpui::test]
async fn isearch_c_l_cycles_middle_top_bottom_and_resets(cx: &mut TestAppContext) {
    let lines = (0..100)
        .map(|ix| format!("line {ix}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (workspace, editor, mut cx) = workspace_text(&lines, true, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("line 50", cx);
    let (rows, margin) = editor.update(cx, |editor, _| {
        (
            editor.visible_row_count().expect("the window has rows"),
            editor.vertical_scroll_margin() as u32,
        )
    });
    let margin = margin.min(rows.saturating_sub(1) / 2);
    let (middle, top, bottom) = (rows / 2, margin, rows - 1 - margin);

    let mut seen = Vec::new();
    for _ in 0..4 {
        keys("ctrl-l", cx);
        seen.push(window_row(&editor, cx));
    }
    assert_eq!(seen, [middle, top, bottom, middle]);
    let end = lines.find("line 50").unwrap() + "line 50".len();
    assert_eq!(cursor(&editor, cx), end, "the cursor stays on the match");

    // Any other command starts the cycle over at the middle.
    keys("ctrl-l", cx);
    assert_eq!(window_row(&editor, cx), top);
    keys("ctrl-s", cx);
    keys("ctrl-l", cx);
    assert_eq!(window_row(&editor, cx), middle);
    keys("ctrl-g ctrl-g", cx);
}
