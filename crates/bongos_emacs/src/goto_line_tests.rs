use std::time::Duration;

use bongos_minibuffer::SessionView;
use editor::{Editor, MultiBufferOffset, ToPoint as _};
use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle};
use project::Project;
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace};

use crate::actions::GotoLine;
use crate::goto_line::GotoLinePreview;

const LINES: usize = 100;

fn buffer_text() -> String {
    (1..=LINES)
        .map(|ix| format!("line {ix}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A workspace with an editor on 100 lines, the caret on line 10, focused. `routed` puts the
/// prompt in the minibuffer strip; otherwise it is centered.
async fn workspace_editor(
    routed: bool,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    if routed {
        cx.update(|cx| {
            bongos_minibuffer::init(cx);
            cx.default_global::<MinibufferRouting>()
                .set_routes(vec!["goto_line".to_string()]);
        });
    }
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let text = buffer_text();
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(&text, None, false, cx)
        });
        buffer.update(cx, |buffer, _| buffer.set_group_interval(Duration::ZERO));
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor.update(cx, |editor, cx| {
            // "line 1\n" .. "line 9\n" is 63 bytes: line 10 starts there.
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges([MultiBufferOffset(63)..MultiBufferOffset(63)])
            })
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

fn session(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |workspace, cx| workspace.active_modal::<SessionView>(cx))
}

fn status(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let view = session(workspace, cx).expect("the prompt is open");
    view.read_with(cx, |view, _| view.status().to_string())
}

/// The caret's 0-based row.
fn caret_row(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> u32 {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.newest_display(&display).head().row().0
    })
}

fn selections(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.all_display(&display).len()
    })
}

/// The 0-based rows the preview highlights.
fn highlighted(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<u32> {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .highlighted_rows::<GotoLinePreview>(cx)
            .map(|(range, _)| {
                let point = range.start.to_point(&snapshot);
                display
                    .point_to_display_point(point, text::Bias::Left)
                    .row()
                    .0
            })
            .collect()
    })
}

fn scroll_top(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> f64 {
    editor.update(cx, |editor, cx| editor.scroll_position(cx).y)
}

/// Presses a prompt key by the action it is bound to: the keymap is not loaded here, so C-g
/// and Escape (both `menu::Cancel`) and RET (`menu::Confirm`) reach the prompt as their actions.
fn press(key: &str, cx: &mut VisualTestContext) {
    let name = match key {
        "cancel" => "menu::Cancel",
        _ => "menu::Confirm",
    };
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn open_prompt(workspace: &Entity<Workspace>, routed: bool, cx: &mut VisualTestContext) {
    cx.dispatch_action(GotoLine::default());
    cx.run_until_parked();
    assert!(session(workspace, cx).is_some(), "the prompt is open");
    let in_strip = workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    });
    assert_eq!(in_strip, routed, "a routed prompt is drawn in the strip");
}

async fn types_a_line_previews_it_and_enter_goes_there(routed: bool, cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor(routed, cx).await;
    let cx = &mut cx;
    let caret = caret_row(&editor, cx);
    let top = scroll_top(&editor, cx);

    open_prompt(&workspace, routed, cx);
    cx.simulate_keystrokes("4");
    cx.run_until_parked();
    cx.simulate_keystrokes("2");
    cx.run_until_parked();
    assert_eq!(highlighted(&editor, cx), vec![41]);
    assert_eq!(caret_row(&editor, cx), 9);
    let now = caret_row(&editor, cx);
    assert_eq!(now, caret, "the caret did not move");
    assert_eq!(status(&workspace, cx), "");
    let _ = top;

    press("confirm", cx);
    assert!(session(&workspace, cx).is_none());
    assert_eq!(caret_row(&editor, cx), 41);
    assert_eq!(selections(&editor, cx), 1);
    assert!(highlighted(&editor, cx).is_empty());
}

async fn cancel_restores_caret_and_scroll(routed: bool, key: &str, cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor(routed, cx).await;
    let cx = &mut cx;
    let top = scroll_top(&editor, cx);

    open_prompt(&workspace, routed, cx);
    cx.simulate_keystrokes("9 0");
    cx.run_until_parked();
    assert_eq!(highlighted(&editor, cx), vec![89]);
    assert!(scroll_top(&editor, cx) != top, "the preview scrolled");

    press(key, cx);
    assert!(session(&workspace, cx).is_none());
    assert!(highlighted(&editor, cx).is_empty());
    assert_eq!(caret_row(&editor, cx), 9);
    assert_eq!(scroll_top(&editor, cx), top);
}

async fn invalid_text_says_no_line(routed: bool, cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor(routed, cx).await;
    let cx = &mut cx;

    open_prompt(&workspace, routed, cx);
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    assert_eq!(status(&workspace, cx), "no line");
    assert!(highlighted(&editor, cx).is_empty());
    cx.dispatch_action(editor::actions::Backspace);
    cx.simulate_keystrokes("3");
    cx.run_until_parked();
    assert_eq!(status(&workspace, cx), "");
    assert_eq!(highlighted(&editor, cx), vec![2]);
    press("cancel", cx);
}

#[gpui::test]
async fn goto_line_previews_and_enter_goes_there(cx: &mut TestAppContext) {
    types_a_line_previews_it_and_enter_goes_there(false, cx).await;
}

#[gpui::test]
async fn goto_line_routed_previews_and_enter_goes_there(cx: &mut TestAppContext) {
    types_a_line_previews_it_and_enter_goes_there(true, cx).await;
}

#[gpui::test]
async fn goto_line_ctrl_g_restores_caret_and_scroll(cx: &mut TestAppContext) {
    cancel_restores_caret_and_scroll(false, "cancel", cx).await;
}

#[gpui::test]
async fn goto_line_routed_ctrl_g_restores_caret_and_scroll(cx: &mut TestAppContext) {
    cancel_restores_caret_and_scroll(true, "cancel", cx).await;
}

#[gpui::test]
async fn goto_line_escape_restores_caret_and_scroll(cx: &mut TestAppContext) {
    cancel_restores_caret_and_scroll(false, "cancel", cx).await;
}

#[gpui::test]
async fn goto_line_routed_escape_restores_caret_and_scroll(cx: &mut TestAppContext) {
    cancel_restores_caret_and_scroll(true, "cancel", cx).await;
}

#[gpui::test]
async fn goto_line_invalid_text_says_no_line(cx: &mut TestAppContext) {
    invalid_text_says_no_line(false, cx).await;
}

#[gpui::test]
async fn goto_line_routed_invalid_text_says_no_line(cx: &mut TestAppContext) {
    invalid_text_says_no_line(true, cx).await;
}

#[gpui::test]
async fn goto_line_with_a_count_jumps_without_a_prompt_and_clamps(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor(false, cx).await;
    let cx = &mut cx;

    cx.dispatch_action(GotoLine { count: Some(42) });
    cx.run_until_parked();
    assert!(session(&workspace, cx).is_none());
    assert_eq!(caret_row(&editor, cx), 41);

    cx.dispatch_action(GotoLine { count: Some(500) });
    cx.run_until_parked();
    assert_eq!(caret_row(&editor, cx), (LINES - 1) as u32);

    cx.dispatch_action(GotoLine { count: Some(-3) });
    cx.run_until_parked();
    assert_eq!(caret_row(&editor, cx), 0);
    assert_eq!(selections(&editor, cx), 1);
}

#[gpui::test]
async fn goto_line_with_a_column_and_a_region(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor(false, cx).await;
    let cx = &mut cx;
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([MultiBufferOffset(0)..MultiBufferOffset(20)])
        })
    });

    open_prompt(&workspace, false, cx);
    cx.simulate_keystrokes("5 : 3");
    cx.run_until_parked();
    press("confirm", cx);
    assert_eq!(caret_row(&editor, cx), 4);
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let selection = editor.selections.newest_display(&display);
        assert!(selection.is_empty());
        assert_eq!(selection.head().column(), 2);
    });
}
