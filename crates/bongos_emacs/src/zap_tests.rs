use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bongos_minibuffer::ReadKeyView;
use editor::Editor;
use editor::actions::Undo;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{
    AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use multi_buffer::MultiBufferOffset;
use project::Project;
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace};

use crate::actions::{ZapBackToChar, ZapBackUpToChar, ZapToChar, ZapUpToChar};
use crate::apply_tests::{capture_kills, init_test};
use crate::kill_sink::KillDirection;

fn clipboard_text(cx: &mut EditorTestContext) -> Option<String> {
    cx.update(|_, cx| cx.read_from_clipboard())
        .and_then(|item| item.text())
}

fn echo(cx: &mut EditorTestContext) -> Rc<RefCell<Vec<String>>> {
    let messages = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    messages
}

async fn test_context(cx: &mut TestAppContext) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, _| buffer.set_group_interval(Duration::ZERO));
    cx
}

fn zap(count: Option<i32>, char: char) -> ZapToChar {
    ZapToChar {
        count,
        char: Some(char),
    }
}

#[gpui::test]
async fn zap_to_char_kills_through_the_next_character_and_the_clipboard_holds_it(
    cx: &mut TestAppContext,
) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("foo ˇbar(baz) qux)");
    cx.dispatch_action(zap(None, ')'));
    cx.assert_editor_state("foo ˇ qux)");
    assert_eq!(kills.borrow()[0].pieces, vec!["bar(baz)".to_string()]);
    assert_eq!(kills.borrow()[0].command, "emacs::ZapToChar");
    assert_eq!(kills.borrow()[0].direction, KillDirection::Forward);
}

#[gpui::test]
async fn zap_to_char_puts_the_killed_text_on_the_clipboard(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("foo ˇbar(baz) qux");
    cx.dispatch_action(zap(None, ')'));
    cx.assert_editor_state("foo ˇ qux");
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("bar(baz)"));

    cx.dispatch_action(Undo);
    cx.assert_editor_state("foo ˇbar(baz) qux");
}

#[gpui::test]
async fn zap_with_a_count_reaches_the_nth_occurrence(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;

    cx.set_state("ˇa,b,c,d");
    cx.dispatch_action(zap(Some(2), ','));
    cx.assert_editor_state("ˇc,d");
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("a,b,"));

    // A negative count goes the other way.
    cx.set_state("a,b,cˇ,d");
    cx.dispatch_action(zap(Some(-2), ','));
    cx.assert_editor_state("aˇ,d");
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some(",b,c"));
}

#[gpui::test]
async fn zap_up_to_char_stops_before_the_character(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("ˇfoo(bar)");
    cx.dispatch_action(ZapUpToChar {
        count: None,
        char: Some('('),
    });
    cx.assert_editor_state("ˇ(bar)");
    assert_eq!(kills.borrow()[0].pieces, vec!["foo".to_string()]);

    cx.set_state("ˇa;b;c");
    cx.dispatch_action(ZapUpToChar {
        count: Some(2),
        char: Some(';'),
    });
    cx.assert_editor_state("ˇ;c");
    assert_eq!(kills.borrow()[1].pieces, vec!["a;b".to_string()]);
}

#[gpui::test]
async fn the_backward_forms_kill_toward_the_start(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("foo(bar) bazˇ");
    cx.dispatch_action(ZapBackToChar {
        count: None,
        char: Some(')'),
    });
    cx.assert_editor_state("foo(barˇ");
    assert_eq!(kills.borrow()[0].pieces, vec![") baz".to_string()]);
    assert_eq!(kills.borrow()[0].direction, KillDirection::Backward);

    cx.set_state("foo(bar) bazˇ");
    cx.dispatch_action(ZapBackUpToChar {
        count: None,
        char: Some(')'),
    });
    cx.assert_editor_state("foo(bar)ˇ");
    assert_eq!(kills.borrow()[1].pieces, vec![" baz".to_string()]);

    cx.set_state("a,b,c,dˇ");
    cx.dispatch_action(ZapBackToChar {
        count: Some(2),
        char: Some(','),
    });
    cx.assert_editor_state("a,bˇ");
}

#[gpui::test]
async fn a_character_that_is_not_there_changes_nothing_and_says_so(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);
    let messages = echo(&mut cx);
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("kept".to_string()));

    cx.set_state("one ˇtwo three");
    cx.dispatch_action(zap(None, 'x'));
    cx.assert_editor_state("one ˇtwo three");
    assert_eq!(*messages.borrow(), vec!["Search failed: \"x\"".to_string()]);
    assert!(kills.borrow().is_empty());
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("kept"));

    // Too few occurrences for the count; and for each backward form.
    cx.set_state("one ˇtwo three");
    cx.dispatch_action(zap(Some(2), 'w'));
    cx.dispatch_action(ZapBackToChar {
        count: None,
        char: Some('z'),
    });
    cx.dispatch_action(ZapUpToChar {
        count: Some(3),
        char: Some('e'),
    });
    cx.assert_editor_state("one ˇtwo three");
    assert_eq!(messages.borrow().len(), 4);
    assert_eq!(messages.borrow()[1], "Search failed: \"w\"");
    assert!(kills.borrow().is_empty());
    assert_eq!(clipboard_text(&mut cx).as_deref(), Some("kept"));

    // No undo step was made either.
    cx.dispatch_action(Undo);
    cx.assert_editor_state("one ˇtwo three");
}

#[gpui::test]
async fn a_missing_character_keeps_the_region(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let messages = echo(&mut cx);

    cx.set_state("one «twoˇ» three");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(zap(None, 'x'));
    cx.assert_editor_state("one «twoˇ» three");
    cx.update_editor(|editor, _, _| assert!(editor.selection_mark_mode()));
    assert_eq!(messages.borrow().len(), 1);
}

#[gpui::test]
async fn zap_clears_the_region_and_ends_mark_mode(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("one «twˇ»o three)");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(zap(None, ')'));
    cx.assert_editor_state("one twˇ");
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
    assert_eq!(kills.borrow()[0].pieces, vec!["o three)".to_string()]);
}

#[gpui::test]
async fn each_caret_zaps_its_own_text_in_one_undo_step(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);

    cx.set_state("aˇ(b) c\ndˇ(e) f");
    cx.dispatch_action(zap(None, ')'));
    cx.assert_editor_state("aˇ c\ndˇ f");
    assert_eq!(
        kills.borrow()[0].pieces,
        vec!["(b)".to_string(), "(e)".to_string()]
    );
    cx.dispatch_action(Undo);
    cx.assert_editor_state("aˇ(b) c\ndˇ(e) f");
}

#[gpui::test]
async fn zap_does_nothing_in_a_read_only_buffer_and_with_a_zero_count(cx: &mut TestAppContext) {
    let mut cx = test_context(cx).await;
    let kills = capture_kills(&mut cx);
    let messages = echo(&mut cx);

    cx.set_state("ˇone)");
    cx.dispatch_action(zap(Some(0), ')'));
    cx.assert_editor_state("ˇone)");
    cx.update_editor(|editor, _, _| editor.set_read_only(true));
    cx.dispatch_action(zap(None, ')'));
    cx.assert_editor_state("ˇone)");
    assert!(kills.borrow().is_empty());
    assert!(messages.borrow().is_empty());
}

fn key_view(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<ReadKeyView>(cx).is_some()
    })
}

/// A workspace with an editor on `text`, caret at `caret`, focused.
async fn workspace_editor(
    text: &str,
    caret: usize,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(text, None, false, cx)
        });
        buffer.update(cx, |buffer, _| buffer.set_group_interval(Duration::ZERO));
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges([MultiBufferOffset(caret)..MultiBufferOffset(caret)])
            })
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

#[gpui::test]
async fn zap_without_a_character_asks_for_one_and_any_character_answers(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("foo bar(baz) Z qux", 4, cx).await;
    let cx = &mut cx;
    let text = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
        editor.update(cx, |editor, cx| editor.text(cx))
    };

    cx.dispatch_action(ZapToChar::default());
    cx.run_until_parked();
    assert!(key_view(&workspace, cx), "the prompt is open");
    assert_eq!(text(&editor, cx), "foo bar(baz) Z qux");
    cx.simulate_keystrokes(")");
    cx.run_until_parked();
    assert!(!key_view(&workspace, cx));
    assert_eq!(text(&editor, cx), "foo  Z qux");
    assert_eq!(
        cx.update(|_, cx| cx.read_from_clipboard())
            .and_then(|item| item.text())
            .as_deref(),
        Some("bar(baz)")
    );

    // A capital letter, with a count, up to it.
    cx.dispatch_action(ZapUpToChar {
        count: Some(1),
        char: None,
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("shift-z");
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "foo Z qux");

    // C-g cancels and kills nothing.
    cx.dispatch_action(ZapBackToChar::default());
    cx.run_until_parked();
    assert!(key_view(&workspace, cx));
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert!(!key_view(&workspace, cx));
    assert_eq!(text(&editor, cx), "foo Z qux");
}
