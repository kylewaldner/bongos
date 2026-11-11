use std::cell::Cell;
use std::rc::Rc;

use editor::Editor;
use editor::actions::ExpandAllDiffHunks;
use editor::test::editor_test_context::EditorTestContext;
use gpui::{
    AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, KeyBinding, Render, TestAppContext, VisualTestContext, WindowHandle,
    actions,
};
use project::Project;
use workspace::{AppState, ModalView, MultiWorkspace, Workspace};

use crate::actions::KeyboardQuit;
use crate::apply_tests::init_test;
use crate::quit;

actions!(quit_test, [Behind]);

fn counter(cx: &mut EditorTestContext) -> Rc<Cell<usize>> {
    let count = Rc::new(Cell::new(0));
    let hook = count.clone();
    cx.update(|_, cx| quit::register(cx, move |_, _| hook.set(hook.get() + 1)));
    count
}

fn bind_quit(cx: &mut EditorTestContext) {
    cx.update(|_, cx| cx.bind_keys([KeyBinding::new("ctrl-g", KeyboardQuit, Some("Editor"))]));
}

#[gpui::test]
async fn quit_clears_regions_on_three_carets_then_collapses(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("«oneˇ» «twoˇ» «threeˇ»");
    cx.dispatch_action(KeyboardQuit);
    cx.assert_editor_state("oneˇ twoˇ threeˇ");
    cx.dispatch_action(KeyboardQuit);
    cx.assert_editor_state("oneˇ two three");
    assert_eq!(cx.buffer_text(), "one two three");
}

#[gpui::test]
async fn quit_ends_mark_mode_and_never_edits(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("oneˇ two");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(KeyboardQuit);
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
    assert_eq!(cx.buffer_text(), "one two");
}

#[gpui::test]
async fn quit_with_mark_mode_on_and_a_region_ends_both_in_one_press(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("«oneˇ» two");
    cx.update_editor(|editor, _, cx| editor.set_selection_mark_mode(true, cx));
    cx.dispatch_action(KeyboardQuit);
    cx.update_editor(|editor, _, _| assert!(!editor.selection_mark_mode()));
    cx.assert_editor_state("oneˇ two");
}

#[gpui::test]
async fn quit_in_a_single_line_editor_keeps_its_selection(cx: &mut TestAppContext) {
    init_test(cx);
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text("one two", window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    editor.update_in(cx, |editor, window, cx| {
        editor.select_all(&editor::actions::SelectAll, window, cx)
    });
    cx.dispatch_action(KeyboardQuit);
    editor.update(cx, |editor, _| {
        assert!(
            editor
                .selections
                .disjoint_anchors()
                .iter()
                .any(|selection| selection.start != selection.end)
        );
    });
}

#[gpui::test]
async fn quit_runs_a_hook_once_per_keystroke(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    bind_quit(&mut cx);
    let count = counter(&mut cx);
    cx.set_state("«oneˇ» two");
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(count.get(), 1);
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(count.get(), 2);
    cx.dispatch_action(KeyboardQuit);
    cx.dispatch_action(KeyboardQuit);
    assert_eq!(count.get(), 4);
    assert_eq!(cx.buffer_text(), "one two");
}

#[gpui::test]
async fn quit_collapses_expanded_hunks_then_hides_blame(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("one\ntwo\nthreeˇ\n");
    cx.set_head_text("one\nthree\n");
    cx.update_editor(|editor, window, cx| {
        editor.expand_all_diff_hunks(&ExpandAllDiffHunks, window, cx)
    });
    cx.run_until_parked();
    cx.update_editor(|editor, _, cx| assert!(editor.collapse_expanded_diff_hunks(cx)));
    cx.update_editor(|editor, window, cx| {
        editor.expand_all_diff_hunks(&ExpandAllDiffHunks, window, cx)
    });
    cx.run_until_parked();
    cx.dispatch_action(KeyboardQuit);
    cx.update_editor(|editor, _, cx| assert!(!editor.collapse_expanded_diff_hunks(cx)));

    cx.update_editor(|editor, window, cx| {
        editor.toggle_git_blame(&git::Blame, window, cx);
        assert!(editor.show_git_blame_gutter());
    });
    cx.dispatch_action(KeyboardQuit);
    cx.update_editor(|editor, _, _| assert!(!editor.show_git_blame_gutter()));
    assert_eq!(cx.buffer_text(), "one\ntwo\nthree\n");
}

#[gpui::test]
async fn quit_with_nothing_to_cancel_propagates(cx: &mut TestAppContext) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    let behind = Rc::new(Cell::new(0));
    let seen = behind.clone();
    cx.update_editor(|editor, _, _| {
        editor
            .register_action(move |_: &Behind, _, _| seen.set(seen.get() + 1))
            .detach();
    });
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-g", Behind, Some("Editor")),
            KeyBinding::new("ctrl-g", KeyboardQuit, Some("Editor")),
        ])
    });
    cx.set_state("oneˇ two");
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(behind.get(), 1);

    // With a region to clear the key stops at the quit.
    cx.set_state("«oneˇ» two");
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(behind.get(), 1);
    cx.assert_editor_state("oneˇ two");
}

struct TestModal {
    focus: FocusHandle,
}

impl EventEmitter<DismissEvent> for TestModal {}
impl ModalView for TestModal {}
impl Focusable for TestModal {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for TestModal {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let _ = cx;
        gpui::div().track_focus(&self.focus)
    }
}

async fn workspace_window(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-g", KeyboardQuit, Some("Editor")),
            KeyBinding::new("ctrl-g", KeyboardQuit, Some("Workspace")),
        ]);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text("one two", window, cx);
            editor
        });
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

#[gpui::test]
async fn one_key_reaching_four_handlers_runs_a_hook_once(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = workspace_window(cx).await;
    let count = Rc::new(Cell::new(0));
    let hook = count.clone();
    cx.update(|_, cx| quit::register(cx, move |_, _| hook.set(hook.get() + 1)));
    // Idle: the editor propagates to Workspace, and the second binding does it again.
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(count.get(), 1);
    cx.simulate_keystrokes("ctrl-g");
    assert_eq!(count.get(), 2);
    cx.dispatch_action(KeyboardQuit);
    cx.dispatch_action(KeyboardQuit);
    assert_eq!(count.get(), 4);
    editor.read_with(&cx, |editor, cx| assert_eq!(editor.text(cx), "one two"));
}

#[gpui::test]
async fn workspace_quit_hides_a_modal_unless_focus_is_inside(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_window(cx).await;
    let behind = Rc::new(Cell::new(0));
    let seen = behind.clone();
    cx.update(|_, cx| {
        cx.bind_keys([KeyBinding::new("ctrl-g", Behind, Some("Workspace"))]);
    });
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace
            .register_action(move |_: &mut Workspace, _: &Behind, _, _| seen.set(seen.get() + 1));
        let _ = (window, cx);
    });

    // No modal: the quit propagates.
    cx.simulate_keystrokes("ctrl-g");
    assert!(behind.get() >= 1);

    // A modal with focus outside it is hidden by the quit.
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |_, cx| TestModal {
            focus: cx.focus_handle(),
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    workspace.update_in(&mut cx, |workspace, window, cx| {
        assert!(workspace.has_active_modal(window, cx));
        window.focus(&editor.focus_handle(cx), cx);
        assert!(!workspace.focus_in_modal(window, cx));
    });
    cx.dispatch_action(KeyboardQuit);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        assert!(!workspace.has_active_modal(window, cx));
    });

    // Focus inside the modal: the quit propagates and the modal stays.
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |_, cx| TestModal {
            focus: cx.focus_handle(),
        });
    });
    cx.run_until_parked();
    workspace.update_in(&mut cx, |workspace, window, cx| {
        let modal = workspace.active_modal::<TestModal>(cx).unwrap();
        window.focus(&modal.focus_handle(cx), cx);
        assert!(workspace.focus_in_modal(window, cx));
    });
    cx.dispatch_action(KeyboardQuit);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        assert!(workspace.has_active_modal(window, cx));
    });
}
