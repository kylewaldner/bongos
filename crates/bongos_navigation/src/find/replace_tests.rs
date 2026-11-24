//! Tests of replace mode: the prompt mark, RET asking for the replacement and opening the decision
//! view on the exported results, the decision keys across files, `C-c C-r` with its count
//! question, the edits left unsaved with one undo step per buffer, and an export closed before
//! its search ends.

use std::cell::RefCell;
use std::rc::Rc;

use bongos_minibuffer::{ReadKeyView, SessionView};
use bongos_query_replace::session::batch_running;
use bongos_query_replace::view::QueryReplaceView;
use gpui::{BorrowAppContext as _, Entity, KeyBinding, TestAppContext, VisualTestContext};
use language::Buffer;
use search::ProjectSearchView;
use workspace::{MinibufferRouting, SaveIntent, Workspace};
use zed_actions::minibuffer;

use crate::actions::{ReplaceAll, ToggleReplace};
use crate::find::picker::KEY_CONTEXT;
use crate::find::picker::tests::{dispatch, find, setup, type_text};

const A: &str = "foo foo\n";
const B: &str = "foo\nfoo\n";

type Messages = Rc<RefCell<Vec<String>>>;

/// A workspace holding `a.txt` and `b.txt`, the replace keys bound, and the echo area recorded.
async fn scene(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext, Messages) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", A), ("b.txt", B)]).await;
    let messages = Messages::default();
    cx.update(|_, cx| {
        let context = format!("Minibuffer > {KEY_CONTEXT} > Editor");
        cx.bind_keys([
            KeyBinding::new("alt-%", ToggleReplace, Some(&context)),
            KeyBinding::new("ctrl-c ctrl-r", ReplaceAll, Some(&context)),
        ]);
        let messages = messages.clone();
        cx.update_global::<MinibufferRouting, _>(|routing, _| {
            routing.set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
        });
    });
    (workspace, cx, messages)
}

fn session_prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    cx.read(|cx| {
        let view = workspace
            .read(cx)
            .active_modal::<SessionView>(cx)
            .expect("a session is open");
        view.read(cx).prompt_text().to_string()
    })
}

fn decision_view(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<QueryReplaceView>> {
    cx.read(|cx| workspace.read(cx).active_modal::<QueryReplaceView>(cx))
}

fn question(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    cx.read(|cx| {
        let view = workspace
            .read(cx)
            .active_modal::<ReadKeyView>(cx)
            .expect("a question is asked");
        view.read(cx).prompt_text().to_string()
    })
}

fn search_views(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<Entity<ProjectSearchView>> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .items_of_type::<ProjectSearchView>(cx)
            .collect()
    })
}

fn buffer(workspace: &Entity<Workspace>, name: &str, cx: &mut VisualTestContext) -> Entity<Buffer> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .project()
            .read(cx)
            .opened_buffers(cx)
            .into_iter()
            .find(|buffer| {
                buffer
                    .read(cx)
                    .file()
                    .is_some_and(|file| file.path().as_unix_str() == name)
            })
            .expect("the buffer is open")
    })
}

fn text(workspace: &Entity<Workspace>, name: &str, cx: &mut VisualTestContext) -> String {
    let buffer = buffer(workspace, name, cx);
    cx.read(|cx| buffer.read(cx).text())
}

/// Opens the picker on `foo` in replace mode.
fn replace_mode(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    find(workspace, false, cx);
    type_text("foo", cx);
    dispatch("navigation::ToggleReplace", cx);
}

/// Answers the replacement prompt with `to`.
fn answer(to: &str, workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    assert_eq!(session_prompt(workspace, cx), "Replace foo with: ");
    type_text(to, cx);
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
}

/// Replace mode, RET, and `bar` as the replacement: the decision view is open.
fn asking(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    replace_mode(workspace, cx);
    dispatch("menu::Confirm", cx);
    answer("bar", workspace, cx);
    assert!(
        decision_view(workspace, cx).is_some(),
        "the decision view opens"
    );
}

/// `C-c C-r` in replace mode with `bar` as the replacement: the count question is open.
fn asking_all(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    replace_mode(workspace, cx);
    cx.simulate_keystrokes("ctrl-c ctrl-r");
    cx.run_until_parked();
    answer("bar", workspace, cx);
    assert_eq!(question(workspace, cx), "Replace 4 matches in 2 files? ");
}

fn results_editor(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<editor::Editor> {
    let views = search_views(workspace, cx);
    assert_eq!(views.len(), 1, "one export");
    cx.read(|cx| views[0].read(cx).results_editor().clone())
}

#[gpui::test]
async fn m_percent_marks_the_prompt_replace(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = scene(cx).await;
    let cx = &mut cx;
    find(&workspace, false, cx);
    type_text("foo", cx);
    assert_eq!(session_prompt(&workspace, cx), "Find [Aa]: ");

    cx.simulate_keystrokes("alt-%");
    cx.run_until_parked();
    assert_eq!(session_prompt(&workspace, cx), "Find [Aa][replace]: ");
    dispatch("navigation::ToggleWholeWord", cx);
    assert_eq!(session_prompt(&workspace, cx), "Find [Aa][w][replace]: ");

    cx.simulate_keystrokes("alt-%");
    cx.run_until_parked();
    assert_eq!(session_prompt(&workspace, cx), "Find [Aa][w]: ");
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn ret_asks_the_replacement_exports_and_asks_at_the_first_match(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = scene(cx).await;
    let cx = &mut cx;
    replace_mode(&workspace, cx);
    dispatch("menu::Confirm", cx);
    answer("bar", &workspace, cx);

    let editor = results_editor(&workspace, cx);
    let view = decision_view(&workspace, cx).expect("the decision view opens");
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).prompt(),
            "Query replacing foo with bar: (y/n/!/./,/^/q/?)"
        );
        assert_eq!(view.read(cx).status(), "1/4");
    });
    // The cursor is at the first match of the results (`foo` at the start of `a.txt`), and
    // nothing is replaced yet.
    let cursor = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let selection = editor
            .selections
            .newest::<editor::MultiBufferOffset>(&display);
        (selection.start.0, selection.end.0)
    });
    assert_eq!(cursor, (3, 3));
    assert_eq!(text(&workspace, "a.txt", cx), A);
    assert_eq!(text(&workspace, "b.txt", cx), B);
}

#[gpui::test]
async fn y_n_and_q_act_on_each_match_across_files(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = scene(cx).await;
    let cx = &mut cx;
    asking(&workspace, cx);

    dispatch("query_replace::Yes", cx);
    assert_eq!(text(&workspace, "a.txt", cx), "bar foo\n");
    dispatch("query_replace::No", cx);
    assert_eq!(text(&workspace, "a.txt", cx), "bar foo\n");
    // The next match is in the other file.
    dispatch("query_replace::Yes", cx);
    assert_eq!(text(&workspace, "b.txt", cx), "bar\nfoo\n");
    dispatch("query_replace::Exit", cx);

    assert!(
        decision_view(&workspace, cx).is_none(),
        "q ends the session"
    );
    assert_eq!(text(&workspace, "a.txt", cx), "bar foo\n");
    assert_eq!(text(&workspace, "b.txt", cx), "bar\nfoo\n");
}

#[gpui::test]
async fn ctrl_c_ctrl_r_asks_the_count_and_replaces_all_on_yes(cx: &mut TestAppContext) {
    let (workspace, mut cx, messages) = scene(cx).await;
    let cx = &mut cx;
    asking_all(&workspace, cx);
    let editor = results_editor(&workspace, cx);

    cx.simulate_keystrokes("y");
    cx.run_until_parked();

    assert_eq!(text(&workspace, "a.txt", cx), "bar bar\n");
    assert_eq!(text(&workspace, "b.txt", cx), "bar\nbar\n");
    assert!(
        !cx.read(|cx| batch_running(editor.read(cx))),
        "the batch marker is cleared at the end"
    );
    assert_eq!(
        messages.borrow().last().map(String::as_str),
        Some("Replaced 4 occurrences")
    );
}

#[gpui::test]
async fn no_to_the_count_replaces_nothing(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = scene(cx).await;
    let cx = &mut cx;
    asking_all(&workspace, cx);
    let editor = results_editor(&workspace, cx);

    cx.simulate_keystrokes("n");
    cx.run_until_parked();

    assert_eq!(text(&workspace, "a.txt", cx), A);
    assert_eq!(text(&workspace, "b.txt", cx), B);
    assert!(!cx.read(|cx| batch_running(editor.read(cx))));
    for name in ["a.txt", "b.txt"] {
        let buffer = buffer(&workspace, name, cx);
        assert!(
            !cx.read(|cx| buffer.read(cx).is_dirty()),
            "{name} is untouched"
        );
    }
}

#[gpui::test]
async fn replace_all_leaves_buffers_unsaved_and_one_undo_restores_each(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = scene(cx).await;
    let cx = &mut cx;
    asking_all(&workspace, cx);
    cx.simulate_keystrokes("y");
    cx.run_until_parked();

    for (name, before, after) in [("a.txt", A, "bar bar\n"), ("b.txt", B, "bar\nbar\n")] {
        let buffer = buffer(&workspace, name, cx);
        cx.read(|cx| {
            assert_eq!(buffer.read(cx).text(), after);
            assert!(
                buffer.read(cx).is_dirty(),
                "{name} is modified and not saved"
            );
        });
        cx.update(|_, cx| buffer.update(cx, |buffer, cx| buffer.undo(cx)));
        cx.read(|cx| assert_eq!(buffer.read(cx).text(), before, "one undo restores {name}"));
    }
}

#[gpui::test]
async fn an_export_closed_before_its_search_ends_replaces_nothing(cx: &mut TestAppContext) {
    let (workspace, mut cx, messages) = scene(cx).await;
    let cx = &mut cx;
    replace_mode(&workspace, cx);
    dispatch("menu::Confirm", cx);
    assert_eq!(session_prompt(&workspace, cx), "Replace foo with: ");
    type_text("bar", cx);

    // Confirm without letting the export's search run, then close its item at once.
    cx.update(|window, cx| window.dispatch_action(Box::new(minibuffer::Confirm), cx));
    let views = search_views(&workspace, cx);
    assert_eq!(views.len(), 1, "the export item is open");
    let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
    let id = views[0].entity_id();
    drop(views);
    pane.update_in(cx, |pane, window, cx| {
        pane.close_item_by_id(id, SaveIntent::Skip, window, cx)
            .detach()
    });
    cx.run_until_parked();

    assert!(search_views(&workspace, cx).is_empty());
    assert!(decision_view(&workspace, cx).is_none(), "no replace starts");
    assert!(
        cx.read(|cx| workspace.read(cx).active_modal::<ReadKeyView>(cx))
            .is_none()
    );
    assert!(messages.borrow().is_empty(), "{:?}", messages.borrow());
    // The buffers the search opened, if any, are as they were on disk.
    cx.read(|cx| {
        for buffer in workspace.read(cx).project().read(cx).opened_buffers(cx) {
            let buffer = buffer.read(cx);
            let name = buffer
                .file()
                .map(|file| file.path().as_unix_str().to_string());
            let before = match name.as_deref() {
                Some("a.txt") => A,
                Some("b.txt") => B,
                _ => continue,
            };
            assert_eq!(buffer.text(), before);
            assert!(!buffer.is_dirty());
        }
    });
}
