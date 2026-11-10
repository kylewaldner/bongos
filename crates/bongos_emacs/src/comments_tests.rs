use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use editor::Editor;
use editor::actions::Undo;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{Action as _, Focusable as _, TestAppContext, VisualTestContext};
use language::{BlockCommentConfig, Language, LanguageConfig};
use workspace::MinibufferRouting;

use crate::actions::{CommentDwim, DuplicateAndComment};
use crate::apply_tests::init_test;

/// A language with `//` line comments and `/* */` block comments.
fn slashes() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "Slashes".into(),
            line_comments: vec!["// ".into()],
            block_comment: Some(BlockCommentConfig {
                start: "/* ".into(),
                end: " */".into(),
                prefix: "".into(),
                tab_size: 0,
            }),
            ..Default::default()
        },
        None,
    ))
}

/// A language with `#` line comments only.
fn hashes() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "Hashes".into(),
            line_comments: vec!["# ".into()],
            ..Default::default()
        },
        None,
    ))
}

/// A language with block comments only.
fn blocks_only() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "Blocks".into(),
            block_comment: Some(BlockCommentConfig {
                start: "<!-- ".into(),
                end: " -->".into(),
                prefix: "".into(),
                tab_size: 0,
            }),
            ..Default::default()
        },
        None,
    ))
}

/// A language with no comment syntax.
fn bare() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "Bare".into(),
            ..Default::default()
        },
        None,
    ))
}

async fn test_context(language: Arc<Language>, cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, cx| {
        buffer.set_group_interval(Duration::ZERO);
        buffer.set_language(Some(language), cx);
    });
    cx
}

#[gpui::test]
async fn comment_dwim_with_no_region_toggles_the_line(cx: &mut TestAppContext) {
    let mut cx = test_context(slashes(), cx).await;

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(CommentDwim);
    cx.assert_editor_state("one\n// twˇo\nthree");
    cx.dispatch_action(CommentDwim);
    cx.assert_editor_state("one\ntwˇo\nthree");
}

#[gpui::test]
async fn comment_dwim_on_whole_lines_line_comments_and_toggles_back(cx: &mut TestAppContext) {
    let mut cx = test_context(slashes(), cx).await;

    cx.set_state("one\n«two\nthreeˇ»\nfour");
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "one\n// two\n// three\nfour");
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "one\ntwo\nthree\nfour");

    // A region that starts in the indent and ends right after a newline still covers lines.
    cx.set_state("one\n«  two\nthree\nˇ»four");
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "one\n//   two\n// three\nfour");
}

#[gpui::test]
async fn comment_dwim_on_a_partial_region_block_comments_and_toggles_back(cx: &mut TestAppContext) {
    let mut cx = test_context(slashes(), cx).await;

    cx.set_state("one «twoˇ» three");
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "one /* two */ three");
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "one two three");

    // A region that ends before the last word of its last line is partial too.
    cx.set_state("«one\ntwˇ»o");
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "/* one\ntw */o");
}

#[gpui::test]
async fn comment_dwim_falls_back_to_line_comments_without_a_block_comment(cx: &mut TestAppContext) {
    let mut cx = test_context(hashes(), cx).await;

    cx.set_state("one «twoˇ» three\nfour");
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "# one two three\nfour");
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "one two three\nfour");
}

#[gpui::test]
async fn comment_dwim_is_one_undo_step_and_ends_mark_mode(cx: &mut TestAppContext) {
    let mut cx = test_context(slashes(), cx).await;

    cx.set_state("one\n«two\nthreeˇ»");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(CommentDwim);
    assert_eq!(cx.buffer_text(), "one\n// two\n// three");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
    cx.dispatch_action(Undo);
    assert_eq!(cx.buffer_text(), "one\ntwo\nthree");
}

#[gpui::test]
async fn comment_dwim_in_a_read_only_editor_changes_nothing(cx: &mut TestAppContext) {
    let mut cx = test_context(slashes(), cx).await;

    cx.set_state("oneˇ\ntwo");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    cx.dispatch_action(CommentDwim);
    cx.dispatch_action(DuplicateAndComment);
    cx.assert_editor_state("oneˇ\ntwo");
}

#[gpui::test]
async fn duplicate_and_comment_leaves_a_live_copy_below(cx: &mut TestAppContext) {
    let mut cx = test_context(slashes(), cx).await;

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(DuplicateAndComment);
    cx.assert_editor_state("one\n// two\ntwˇo\nthree");

    // The last line has no newline to copy.
    cx.set_state("one\ntˇwo");
    cx.dispatch_action(DuplicateAndComment);
    cx.assert_editor_state("one\n// two\ntˇwo");

    // An empty line is copied too; Zed's toggle puts a bare marker on the original.
    cx.set_state("one\nˇ\nthree");
    cx.dispatch_action(DuplicateAndComment);
    assert_eq!(cx.buffer_text(), "one\n// \n\nthree");
}

#[gpui::test]
async fn duplicate_and_comment_copies_every_line_the_region_touches(cx: &mut TestAppContext) {
    let mut cx = test_context(slashes(), cx).await;

    cx.set_state("one\ntw«o\nthrˇ»ee\nfour");
    cx.dispatch_action(DuplicateAndComment);
    cx.assert_editor_state("one\n// two\n// three\ntwo\nthrˇee\nfour");
}

#[gpui::test]
async fn duplicate_and_comment_works_at_every_caret(cx: &mut TestAppContext) {
    let mut cx = test_context(hashes(), cx).await;

    cx.set_state("oˇne\ntwo\nthrˇee\nˇfour");
    cx.dispatch_action(DuplicateAndComment);
    cx.assert_editor_state("# one\noˇne\ntwo\n# three\nthrˇee\n# four\nˇfour");

    // Two carets on one line copy it once.
    cx.set_state("oˇnˇe\ntwo");
    cx.dispatch_action(DuplicateAndComment);
    assert_eq!(cx.buffer_text(), "# one\none\ntwo");
}

#[gpui::test]
async fn duplicate_and_comment_is_one_undo_step(cx: &mut TestAppContext) {
    let mut cx = test_context(slashes(), cx).await;

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(DuplicateAndComment);
    assert_eq!(cx.buffer_text(), "one\n// two\ntwo\nthree");
    cx.dispatch_action(Undo);
    cx.assert_editor_state("one\ntwˇo\nthree");
}

#[gpui::test]
async fn duplicate_and_comment_blocks_the_originals_without_a_line_comment(
    cx: &mut TestAppContext,
) {
    let mut cx = test_context(blocks_only(), cx).await;

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(DuplicateAndComment);
    cx.assert_editor_state("one\n<!-- two -->\ntwˇo\nthree");

    cx.set_state("«one\ntwˇ»o\nthree");
    cx.dispatch_action(DuplicateAndComment);
    cx.assert_editor_state("<!-- one\ntwo -->\none\ntwˇo\nthree");
}

#[gpui::test]
async fn duplicate_and_comment_without_comment_syntax_says_so(cx: &mut TestAppContext) {
    let mut cx = test_context(bare(), cx).await;
    let messages = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });

    cx.set_state("one\ntwˇo\nthree");
    cx.dispatch_action(CommentDwim);
    cx.dispatch_action(DuplicateAndComment);
    cx.assert_editor_state("one\ntwˇo\nthree");
    assert_eq!(
        *messages.borrow(),
        vec!["No comment syntax for this language".to_string()]
    );
}

/// A single-line editor has no lines to comment, so both commands pass the key on.
#[gpui::test]
async fn a_single_line_editor_propagates_the_key(cx: &mut TestAppContext) {
    init_test(cx);
    let passed = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let count = passed.clone();
        cx.on_action(move |_: &CommentDwim, _| count.set(count.get() + 1));
        let count = passed.clone();
        cx.on_action(move |_: &DuplicateAndComment, _| count.set(count.get() + 1));
    });
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text("one two", window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));

    cx.update(|window, cx| {
        window.dispatch_action(CommentDwim.boxed_clone(), cx);
        window.dispatch_action(DuplicateAndComment.boxed_clone(), cx);
    });
    assert_eq!(passed.get(), 2);
    assert_eq!(editor.update(cx, |editor, cx| editor.text(cx)), "one two");
}
