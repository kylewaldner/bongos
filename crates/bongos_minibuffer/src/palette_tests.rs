//! Tests of the command palette (M-x) in the strip: it is hosted there only while
//! `command_palette` is in `minibuffer.enabled_for`, a command runs from it with focus back where
//! it was, a cancel refocuses the origin, and Up at the first row walks the query history.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use command_palette::CommandPalette;
use editor::Editor;
use gpui::{
    AppContext as _, BorrowAppContext as _, Entity, FocusHandle, Focusable as _, TestAppContext,
    VisualTestContext, WindowHandle, actions,
};
use picker::{Picker, PickerDelegate as _};
use project::Project;
use settings::SettingsStore;
use workspace::dock::{DockPosition, test::TestPanel};
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::{command_palette::Toggle, minibuffer};

actions!(palette_tests, [Zebrafy]);

type Ran = Rc<RefCell<usize>>;

fn init(cx: &mut TestAppContext, enabled_for: &[&str]) -> (Arc<AppState>, Ran) {
    let ran = Ran::default();
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette::init(cx);
        editor::init(cx);
        crate::init(cx);
        let counted = ran.clone();
        cx.on_action(move |_: &Zebrafy, _| *counted.borrow_mut() += 1);
        app_state
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(enabled_for.iter().map(|name| name.to_string()).collect());
            });
        });
    });
    (app_state, ran)
}

async fn window(
    cx: &mut TestAppContext,
    enabled_for: &[&str],
) -> (Entity<Workspace>, VisualTestContext, Ran) {
    let (app_state, ran) = init(cx, enabled_for);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    (workspace, cx, ran)
}

fn add_editor(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Editor> {
    let editor = workspace.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text("hello\nworld", window, cx);
            editor
        });
        let pane = workspace.active_pane().clone();
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(editor.clone()), true, true, None, window, cx)
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    editor
}

fn focused(handle: &FocusHandle, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| handle.contains_focused(window, cx))
}

fn open_palette(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Picker<command_palette::CommandPaletteDelegate>> {
    cx.dispatch_action(Toggle);
    cx.run_until_parked();
    let palette = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<CommandPalette>(cx)
        })
        .expect("the palette is open");
    palette.read_with(cx, |palette, _| palette.picker().clone())
}

fn in_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

fn palette_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<CommandPalette>(cx).is_some()
    })
}

#[gpui::test]
async fn palette_routed_opens_in_the_strip_not_as_a_centered_modal(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = window(cx, &["command_palette"]).await;
    let cx = &mut cx;
    add_editor(&workspace, cx);

    let picker = open_palette(&workspace, cx);
    assert!(palette_open(&workspace, cx));
    assert!(
        in_strip(&workspace, cx),
        "the palette is hosted by the strip"
    );
    assert!(cx.debug_bounds("minibuffer-input").is_some());
    picker.read_with(cx, |picker, cx| {
        assert!(picker.delegate.match_count() > 0);
        assert_eq!(
            picker.delegate.prompt_label(cx).as_deref(),
            Some("M-x "),
            "the prompt is M-x"
        );
        assert!(
            picker
                .delegate
                .match_text(0)
                .is_some_and(|text| !text.is_empty())
        );
    });
}

#[gpui::test]
async fn palette_typing_three_letters_and_ret_runs_the_command_and_returns_focus(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, ran) = window(cx, &["command_palette"]).await;
    let cx = &mut cx;
    let editor = add_editor(&workspace, cx);

    let picker = open_palette(&workspace, cx);
    cx.simulate_input("zeb");
    cx.run_until_parked();
    picker.read_with(cx, |picker, _| {
        assert_eq!(
            picker.delegate.match_text(0).as_deref(),
            Some("palette tests: zebrafy")
        );
    });

    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(*ran.borrow(), 1, "the command ran once");
    assert!(!palette_open(&workspace, cx));
    let handle = editor.read_with(cx, |editor, cx| editor.focus_handle(cx));
    assert!(focused(&handle, cx));
}

#[gpui::test]
async fn palette_cancel_refocuses_a_dock_origin(cx: &mut TestAppContext) {
    let (workspace, mut cx, ran) = window(cx, &["command_palette"]).await;
    let cx = &mut cx;
    add_editor(&workspace, cx);
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        let panel = cx.new(|cx| TestPanel::new(DockPosition::Bottom, 100, cx));
        workspace.add_panel(panel.clone(), window, cx);
        workspace.toggle_panel_focus::<TestPanel>(window, cx);
        panel
    });
    cx.run_until_parked();
    let terminal = panel.read_with(cx, |panel, cx| panel.focus_handle(cx));
    assert!(focused(&terminal, cx));

    open_palette(&workspace, cx);
    assert!(in_strip(&workspace, cx));
    assert!(!focused(&terminal, cx));

    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert!(!palette_open(&workspace, cx));
    assert!(focused(&terminal, cx), "focus is back in the dock panel");
    assert_eq!(*ran.borrow(), 0);
}

#[gpui::test]
async fn palette_up_at_the_first_row_walks_the_query_history(cx: &mut TestAppContext) {
    let (workspace, mut cx, ran) = window(cx, &["command_palette"]).await;
    let cx = &mut cx;
    add_editor(&workspace, cx);

    // Running a command records its query.
    open_palette(&workspace, cx);
    cx.simulate_input("zeb");
    cx.run_until_parked();
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(*ran.borrow(), 1);

    let picker = open_palette(&workspace, cx);
    picker.read_with(cx, |picker, cx| assert_eq!(picker.query(cx), ""));
    cx.dispatch_action(minibuffer::SelectPrev);
    cx.run_until_parked();
    picker.read_with(cx, |picker, cx| assert_eq!(picker.query(cx), "zeb"));
}

/// With nothing routed (or another prompt routed), the palette is a centered modal that runs a
/// command on `menu::Confirm`, as before.
async fn unrouted(cx: &mut TestAppContext, enabled_for: &[&str]) {
    let (workspace, mut cx, ran) = window(cx, enabled_for).await;
    let cx = &mut cx;
    let editor = add_editor(&workspace, cx);

    let picker = open_palette(&workspace, cx);
    assert!(palette_open(&workspace, cx), "a modal exists");
    assert!(!in_strip(&workspace, cx), "nothing is in the strip");
    assert!(cx.debug_bounds("minibuffer-input").is_none());

    cx.simulate_input("zeb");
    cx.run_until_parked();
    picker.read_with(cx, |picker, _| {
        assert_eq!(
            picker.delegate.match_text(0).as_deref(),
            Some("palette tests: zebrafy")
        );
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(*ran.borrow(), 1);
    assert!(!palette_open(&workspace, cx));
    let handle = editor.read_with(cx, |editor, cx| editor.focus_handle(cx));
    assert!(focused(&handle, cx));
}

#[gpui::test]
async fn palette_unrouted_is_a_centered_modal_as_before(cx: &mut TestAppContext) {
    unrouted(cx, &[]).await;
}

#[gpui::test]
async fn palette_not_in_enabled_for_stays_centered_while_others_are_routed(
    cx: &mut TestAppContext,
) {
    unrouted(cx, &["something_else"]).await;
}

#[gpui::test]
async fn open_with_no_session_opens_the_palette_in_the_strip(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = window(cx, &["command_palette"]).await;
    let cx = &mut cx;
    add_editor(&workspace, cx);
    assert!(!palette_open(&workspace, cx));

    cx.dispatch_action(minibuffer::Open);
    cx.run_until_parked();
    assert!(palette_open(&workspace, cx));
    assert!(in_strip(&workspace, cx), "the palette is hosted by the strip");
}

#[gpui::test]
async fn open_while_the_palette_is_open_keeps_one_palette_and_says_so(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = window(cx, &["command_palette", "messages"]).await;
    let cx = &mut cx;
    add_editor(&workspace, cx);
    open_palette(&workspace, cx);

    cx.dispatch_action(minibuffer::Open);
    cx.run_until_parked();
    assert!(palette_open(&workspace, cx), "the palette is still open");
    assert!(in_strip(&workspace, cx));
    let strip = cx
        .update(|window, cx| crate::minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip");
    let said = strip.read_with(cx, |strip, _| strip.messages());
    assert!(
        said.iter()
            .any(|line| line.text.as_ref() == crate::MINIBUFFER_IN_MINIBUFFER),
        "the strip says a command used the minibuffer while in it"
    );
}
