use std::time::Duration;

use bongos_minibuffer::SessionView;
use editor::actions::Undo;
use editor::{Editor, MultiBufferOffset};
use gpui::{
    AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace};

use crate::actions::StringRectangle;
use crate::mark::set_mark;

/// Four rows: a full one, a short one, one ending inside the rectangle and a full one.
const BLOCK: &str = "abcdefgh\na\nabcde\nabcdefgh";

/// A workspace with an editor on `text`, the mark at byte `mark` and the caret at byte `point`,
/// and the prompt routed to the minibuffer strip.
async fn workspace_editor(
    text: &str,
    mark: usize,
    point: usize,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        bongos_minibuffer::init(cx);
        cx.default_global::<MinibufferRouting>()
            .set_routes(vec!["string_rectangle".to_string()]);
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
                s.select_ranges([MultiBufferOffset(point)..MultiBufferOffset(point)])
            })
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let at = snapshot.anchor_before(MultiBufferOffset(mark));
        assert!(set_mark(&editor, at, cx));
    });
    (workspace, editor, cx)
}

fn session(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx)
    })
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.update(cx, |editor, cx| editor.text(cx))
}

/// Presses RET (`menu::Confirm`) or C-g (`menu::Cancel`) by the action the key is bound to.
fn press(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

/// Opens the prompt, which must be there, and types `typed` (nothing for an empty answer).
fn open_and_type(workspace: &Entity<Workspace>, typed: &str, cx: &mut VisualTestContext) {
    cx.dispatch_action(StringRectangle);
    cx.run_until_parked();
    assert!(session(workspace, cx).is_some(), "the prompt is open");
    if !typed.is_empty() {
        cx.simulate_input(typed);
        cx.run_until_parked();
    }
}

fn prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let view = session(workspace, cx).expect("the prompt is open");
    view.read_with(cx, |view, _| view.prompt_text().to_string())
}

#[gpui::test]
async fn confirm_replaces_every_row_and_is_one_undo_step(cx: &mut TestAppContext) {
    // Mark at row 0 column 2, caret at row 3 column 5: the rectangle is columns 2 to 5.
    let (workspace, editor, mut cx) = workspace_editor(BLOCK, 2, 22, cx).await;
    let cx = &mut cx;
    assert_eq!(prompt_after_open(&workspace, cx), "String rectangle: ");
    cx.simulate_input("XY");
    press("menu::Confirm", cx);
    assert!(session(&workspace, cx).is_none());
    assert_eq!(text(&editor, cx), "abXYfgh\na XY\nabXY\nabXYfgh");
    cx.dispatch_action(Undo);
    assert_eq!(text(&editor, cx), BLOCK);
}

fn prompt_after_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    open_and_type(workspace, "", cx);
    prompt(workspace, cx)
}

#[gpui::test]
async fn short_rows_are_padded(cx: &mut TestAppContext) {
    // Columns 3 to 5: the second row ("a") and the third ("abc") end before the rectangle.
    let (workspace, editor, mut cx) = workspace_editor("abcdef\na\nabc\nabcdef", 3, 18, cx).await;
    let cx = &mut cx;
    open_and_type(&workspace, "-", cx);
    press("menu::Confirm", cx);
    assert_eq!(text(&editor, cx), "abc-f\na  -\nabc-\nabc-f");
}

#[gpui::test]
async fn zero_width_inserts_at_the_column(cx: &mut TestAppContext) {
    // Mark and caret both at column 2: the rectangle has no width.
    let (workspace, editor, mut cx) = workspace_editor("abcd\nabcd\nabcd", 2, 12, cx).await;
    let cx = &mut cx;
    open_and_type(&workspace, "--", cx);
    press("menu::Confirm", cx);
    assert_eq!(text(&editor, cx), "ab--cd\nab--cd\nab--cd");
}

#[gpui::test]
async fn cancel_changes_nothing(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor(BLOCK, 2, 22, cx).await;
    let cx = &mut cx;
    open_and_type(&workspace, "XY", cx);
    press("menu::Cancel", cx);
    assert!(session(&workspace, cx).is_none());
    assert_eq!(text(&editor, cx), BLOCK);
}

#[gpui::test]
async fn the_second_prompt_offers_the_first_string_and_an_empty_confirm_uses_it(
    cx: &mut TestAppContext,
) {
    let (workspace, editor, mut cx) = workspace_editor(BLOCK, 2, 22, cx).await;
    let cx = &mut cx;
    open_and_type(&workspace, "X", cx);
    press("menu::Confirm", cx);
    assert_eq!(text(&editor, cx), "abXfgh\na X\nabX\nabXfgh");

    // Set the corners again over the new text: columns 2 to 5.
    cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let at = snapshot.anchor_before(MultiBufferOffset(2));
        assert!(set_mark(&editor, at, cx));
    });
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([MultiBufferOffset(20)..MultiBufferOffset(20)])
        })
    });
    open_and_type(&workspace, "", cx);
    assert_eq!(prompt(&workspace, cx), "String rectangle (default X): ");
    press("menu::Confirm", cx);
    assert_eq!(text(&editor, cx), "abXh\na X\nabX\nabXh");
}
