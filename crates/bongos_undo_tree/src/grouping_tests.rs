//! Tests of the Emacs undo grouping: a typing run is one undo step, up to 20 characters, and a
//! command, a cursor move or another editor's input ends it.

use std::sync::Arc;
use std::time::Duration;

use editor::actions::{Backspace, MoveRight, Redo, Undo};
use editor::{Editor, SelectionEffects};
use gpui::{
    AppContext as _, BorrowAppContext as _, Entity, EntityInputHandler as _, Focusable as _,
    KeyBinding, TestAppContext, VisualTestContext,
};
use language::Point;
use project::Project;
use settings::{SettingsStore, UndoGroupingContent};
use workspace::AppState;

/// Settings, theme, editor, the undo tree and (when `prompts`) the prompts mode, with the
/// grouping set before any buffer exists; returns the app state with its fake file system.
fn init(cx: &mut TestAppContext, grouping: UndoGroupingContent, prompts: bool) -> Arc<AppState> {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        // As in the app: the prompts mode first, so only the guard hook puts the grouping ahead
        // of the guard's finalizing.
        if prompts {
            bongos_prompts::init(cx);
        }
        crate::init(cx);
        cx.bind_keys([KeyBinding::new("ctrl-f", MoveRight, Some("Editor"))]);
        app_state
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.undo_tree.get_or_insert_default().grouping = Some(grouping);
                content.prompts.get_or_insert_default().enabled = Some(prompts);
            });
        });
    });
    app_state
}

struct Setup {
    project: Entity<Project>,
    buffer: Entity<language::Buffer>,
}

async fn open_buffer(
    cx: &mut TestAppContext,
    grouping: UndoGroupingContent,
    prompts: bool,
) -> Setup {
    let app_state = init(cx, grouping, prompts);
    let name = if prompts { "prompts.txt" } else { "notes.txt" };
    app_state
        .fs
        .as_fake()
        .insert_tree("/dir", serde_json::json!({ (name): "" }))
        .await;
    let project = Project::test(app_state.fs.clone(), ["/dir".as_ref()], cx).await;
    if prompts {
        bongos_prompts::register_language(
            &project.read_with(cx, |project, _| project.languages().clone()),
        );
    }
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(format!("/dir/{name}"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    Setup { project, buffer }
}

fn focus(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
}

async fn open_editor(
    cx: &mut TestAppContext,
    grouping: UndoGroupingContent,
    prompts: bool,
) -> (Entity<Editor>, &mut VisualTestContext) {
    let Setup { project, buffer } = open_buffer(cx, grouping, prompts).await;
    let (editor, cx) =
        cx.add_window_view(|window, cx| Editor::for_buffer(buffer, Some(project), window, cx));
    focus(&editor, cx);
    (editor, cx)
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        editor.selections.newest::<Point>(&snapshot).head()
    })
}

fn put(editor: &Entity<Editor>, column: u32, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges([Point::new(0, column)..Point::new(0, column)])
        })
    });
    cx.run_until_parked();
}

fn undo(cx: &mut VisualTestContext) {
    cx.dispatch_action(Undo);
    cx.run_until_parked();
}

fn redo(cx: &mut VisualTestContext) {
    cx.dispatch_action(Redo);
    cx.run_until_parked();
}

/// Undoes until the buffer is empty and returns how many undo steps that took.
fn undo_steps(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    let mut steps = 0;
    while !text(editor, cx).is_empty() {
        assert!(steps < 100, "undo does not empty the buffer");
        undo(cx);
        steps += 1;
    }
    steps
}

#[gpui::test]
async fn emacs_typing_is_one_undo_step(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, UndoGroupingContent::Emacs, false).await;
    cx.simulate_input("hello");
    assert_eq!(text(&editor, cx), "hello");
    assert_eq!(undo_steps(&editor, cx), 1);
}

#[gpui::test]
async fn a_command_key_or_a_cursor_move_ends_the_run(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, UndoGroupingContent::Emacs, false).await;
    cx.simulate_input("ab");
    // C-f at the end of the buffer moves nothing: the keystroke alone ends the run.
    cx.simulate_keystrokes("ctrl-f");
    cx.simulate_input("cd");
    // A click in the text moves the caret.
    put(&editor, 2, cx);
    cx.simulate_input("ef");
    assert_eq!(text(&editor, cx), "abefcd");

    for expected in ["abcd", "ab", ""] {
        undo(cx);
        assert_eq!(text(&editor, cx), expected);
    }
}

#[gpui::test]
async fn a_run_holds_twenty_characters(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, UndoGroupingContent::Emacs, false).await;
    cx.simulate_input("abcdefghijklmnopqrstuvwxy");
    undo(cx);
    assert_eq!(text(&editor, cx), "abcdefghijklmnopqrst");
    assert_eq!(undo_steps(&editor, cx), 1);
}

#[gpui::test]
async fn a_commands_edit_after_typing_is_its_own_step(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, UndoGroupingContent::Emacs, false).await;
    cx.simulate_input("hello");
    cx.dispatch_action(Backspace);
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "hell");
    undo(cx);
    assert_eq!(text(&editor, cx), "hello");
    assert_eq!(undo_steps(&editor, cx), 1);
}

#[gpui::test]
async fn another_editors_input_starts_a_new_run(cx: &mut TestAppContext) {
    let Setup { project, buffer } = open_buffer(cx, UndoGroupingContent::Emacs, false).await;
    let (first, cx) = cx.add_window_view({
        let (project, buffer) = (project.clone(), buffer.clone());
        |window, cx| Editor::for_buffer(buffer, Some(project), window, cx)
    });
    let second =
        cx.update(|window, cx| cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx)));
    focus(&first, cx);
    cx.simulate_input("he");
    // The second editor is not on screen, so its input goes through its input handler.
    for ch in ['X', 'Y'] {
        second.update_in(cx, |editor, window, cx| {
            editor.replace_text_in_range(None, &ch.to_string(), window, cx)
        });
        cx.run_until_parked();
    }
    cx.simulate_input("llo");
    assert_eq!(undo_steps(&first, cx), 3);
}

#[gpui::test]
async fn redo_of_a_run_puts_the_caret_at_its_end(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, UndoGroupingContent::Emacs, false).await;
    cx.simulate_input("hello");
    undo(cx);
    assert_eq!(text(&editor, cx), "");
    assert_eq!(head(&editor, cx), Point::new(0, 0));
    redo(cx);
    assert_eq!(text(&editor, cx), "hello");
    assert_eq!(head(&editor, cx), Point::new(0, 5));
}

#[gpui::test]
async fn a_run_is_one_step_with_the_prompt_guard_active(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, UndoGroupingContent::Emacs, true).await;
    assert!(editor.read_with(cx, |editor, _| bongos_prompts::is_attached(editor)));
    cx.simulate_input("hello");
    assert_eq!(text(&editor, cx), "hello");
    assert_eq!(undo_steps(&editor, cx), 1);
}

/// Opens a disk-loaded buffer with a non-zero interval (as in the app, where the buffer observer
/// does not see it), shows it in a full editor and returns the interval afterwards.
async fn interval_after_showing(
    cx: &mut TestAppContext,
    grouping: UndoGroupingContent,
) -> Duration {
    let Setup { project, buffer } = open_buffer(cx, grouping, false).await;
    let interval = Duration::from_millis(300);
    buffer.update(cx, |buffer, _| buffer.set_group_interval(interval));
    let (_editor, cx) = cx.add_window_view({
        let buffer = buffer.clone();
        |window, cx| Editor::for_buffer(buffer, Some(project), window, cx)
    });
    cx.run_until_parked();
    buffer.read_with(cx, |buffer, _| buffer.transaction_group_interval())
}

#[gpui::test]
async fn emacs_zeroes_the_interval_of_a_disk_loaded_buffer(cx: &mut TestAppContext) {
    let interval = interval_after_showing(cx, UndoGroupingContent::Emacs).await;
    assert_eq!(interval, Duration::ZERO);
}

#[gpui::test]
async fn zed_keeps_the_interval_of_a_disk_loaded_buffer(cx: &mut TestAppContext) {
    let interval = interval_after_showing(cx, UndoGroupingContent::Zed).await;
    assert_eq!(interval, Duration::from_millis(300));
}

#[gpui::test]
async fn zed_keeps_the_time_grouping(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, UndoGroupingContent::Zed, false).await;
    // The interval is long enough for the run of typing, so only the time grouping can merge it,
    // and the 20-character limit of the Emacs grouping does not apply.
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.update(cx, |buffer, _| {
            buffer.set_group_interval(Duration::from_secs(3600))
        });
    });
    cx.simulate_input("abcdefghijklmnopqrstuvwxy");
    assert_eq!(undo_steps(&editor, cx), 1);
}
