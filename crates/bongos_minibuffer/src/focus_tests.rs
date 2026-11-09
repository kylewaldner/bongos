//! Tests of focus restore: where focus goes when a prompt in the strip ends, after the pane, the
//! item or the dock it came from changed meanwhile, for chained and replaced prompts, and with
//! real key dispatch through the `Minibuffer` key context.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use editor::Editor;
use gpui::{
    AppContext as _, BorrowAppContext as _, Entity, FocusHandle, Focusable as _, KeyBinding,
    TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use settings::SettingsStore;
use workspace::dock::{DockPosition, test::TestPanel};
use workspace::{AppState, ModalOrigin, MultiWorkspace, Pane, SplitDirection, Workspace};
use zed_actions::minibuffer;

use crate::{Candidate, Outcome, Session, SessionView, Source, open};

const HOST: &str = "test_host";

fn init(cx: &mut TestAppContext, enabled_for: &[&str]) -> Arc<AppState> {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        crate::init(cx);
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
    app_state
}

async fn window(
    cx: &mut TestAppContext,
    enabled_for: &[&str],
) -> (Entity<Workspace>, VisualTestContext) {
    let app_state = init(cx, enabled_for);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    (workspace, cx)
}

/// Adds an editor with two lines to `pane` (the active pane when `None`), focused, with its
/// cursor at the end.
fn add_editor(
    workspace: &Entity<Workspace>,
    pane: Option<&Entity<Pane>>,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let editor = workspace.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text("hello\nworld", window, cx);
            editor
        });
        let pane = pane
            .cloned()
            .unwrap_or_else(|| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(editor.clone()), true, true, None, window, cx)
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor.update(cx, |editor, cx| {
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx)
        });
        editor
    });
    cx.run_until_parked();
    editor
}

fn session(log: &Rc<RefCell<Vec<String>>>) -> Session {
    let confirmed = log.clone();
    let cancelled = log.clone();
    Session::completing_read(HOST, "Pick: ")
        .source(Source::Fixed(vec![Candidate::text("one")]))
        .on_confirm(move |_, _, _| confirmed.borrow_mut().push("confirmed".into()))
        .on_cancel(move |_, _| cancelled.borrow_mut().push("cancelled".into()))
}

fn open_session(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, session: Session) {
    workspace.update_in(cx, |workspace, window, cx| {
        open(workspace, session, window, cx);
    });
    cx.run_until_parked();
}

fn session_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx).is_some()
    })
}

fn origin(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<ModalOrigin> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).modal_origin()
    })
}

fn focused(handle: &FocusHandle, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| handle.contains_focused(window, cx))
}

fn strip_focused(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    let view = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<SessionView>(cx)
        })
        .expect("a session is open");
    let handle = view.read_with(cx, |view, cx| view.focus_handle(cx));
    focused(&handle, cx)
}

/// Puts focus back in the prompt (`minibuffer::Focus`), as after a command that moved it away.
fn focus_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        let layer = workspace.modal_layer().clone();
        assert!(layer.update(cx, |layer, cx| layer.focus_bottom_modal(window, cx)));
    });
    cx.run_until_parked();
}

fn cancel(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
}

fn cursor(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> editor::Anchor {
    editor.read_with(cx, |editor, _| editor.selections.newest_anchor().head())
}

#[gpui::test]
async fn minibuffer_cancel_returns_to_the_same_editor_and_cursor(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    let other = add_editor(&workspace, None, cx);
    let editor = add_editor(&workspace, None, cx);
    let before = cursor(&editor, cx);
    let log = Rc::default();

    open_session(&workspace, cx, session(&log));
    assert!(strip_focused(&workspace, cx));
    let recorded = origin(&workspace, cx).expect("a prompt in the strip records its origin");
    assert_eq!(recorded.item, Some(editor.entity_id()));
    assert_eq!(recorded.dock, None);

    cancel(cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(*log.borrow(), ["cancelled"]);
    assert!(focused(&editor.focus_handle_of(cx), cx));
    assert!(!focused(&other.focus_handle_of(cx), cx));
    assert_eq!(cursor(&editor, cx), before);
}

#[gpui::test]
async fn minibuffer_split_during_the_prompt_returns_to_the_original_side(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    let editor = add_editor(&workspace, None, cx);
    let original = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    open_session(&workspace, cx, session(&Rc::default()));

    let new_pane = workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(original.clone(), SplitDirection::Right, window, cx)
    });
    let clone = add_editor(&workspace, Some(&new_pane), cx);
    focus_strip(&workspace, cx);

    cancel(cx);
    assert!(focused(&editor.focus_handle_of(cx), cx));
    assert!(!focused(&clone.focus_handle_of(cx), cx));
}

#[gpui::test]
async fn minibuffer_closing_the_origin_pane_lands_in_the_neighbour(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    let left_editor = add_editor(&workspace, None, cx);
    let left = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    let right = workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(left.clone(), SplitDirection::Right, window, cx)
    });
    let right_editor = add_editor(&workspace, Some(&right), cx);
    open_session(&workspace, cx, session(&Rc::default()));
    assert_eq!(
        origin(&workspace, cx).and_then(|origin| origin.pane),
        Some(right.downgrade())
    );

    right.update_in(cx, |pane, window, cx| {
        pane.remove_item(right_editor.entity_id(), false, true, window, cx)
    });
    cx.run_until_parked();
    drop(right);
    assert_eq!(
        workspace.read_with(cx, |workspace, _| workspace.panes().len()),
        1
    );
    focus_strip(&workspace, cx);

    cancel(cx);
    assert!(focused(&left_editor.focus_handle_of(cx), cx));
}

#[gpui::test]
async fn minibuffer_closing_the_origin_item_lands_in_the_active_item(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    let first = add_editor(&workspace, None, cx);
    let second = add_editor(&workspace, None, cx);
    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    open_session(&workspace, cx, session(&Rc::default()));

    pane.update_in(cx, |pane, window, cx| {
        pane.remove_item(second.entity_id(), false, false, window, cx)
    });
    cx.run_until_parked();
    focus_strip(&workspace, cx);

    cancel(cx);
    assert!(focused(&first.focus_handle_of(cx), cx));
}

#[gpui::test]
async fn minibuffer_restores_a_dock_origin_and_the_center_once_the_dock_closed(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    let editor = add_editor(&workspace, None, cx);
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        let panel = cx.new(|cx| TestPanel::new(DockPosition::Bottom, 100, cx));
        workspace.add_panel(panel.clone(), window, cx);
        workspace.toggle_panel_focus::<TestPanel>(window, cx);
        panel
    });
    cx.run_until_parked();
    let terminal = panel.read_with(cx, |panel, cx| panel.focus_handle(cx));
    assert!(focused(&terminal, cx));

    // The dock is still open: the panel gets focus back.
    open_session(&workspace, cx, session(&Rc::default()));
    assert_eq!(
        origin(&workspace, cx).and_then(|origin| origin.dock),
        Some(DockPosition::Bottom)
    );
    cancel(cx);
    assert!(focused(&terminal, cx));

    // The dock closed during the prompt: the center pane does.
    open_session(&workspace, cx, session(&Rc::default()));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace
            .bottom_dock()
            .update(cx, |dock, cx| dock.set_open(false, window, cx))
    });
    cx.run_until_parked();
    focus_strip(&workspace, cx);
    cancel(cx);
    assert!(focused(&editor.focus_handle_of(cx), cx));
    assert!(!focused(&terminal, cx));
}

#[gpui::test]
async fn minibuffer_chained_prompts_keep_the_first_origin(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    let editor = add_editor(&workspace, None, cx);
    let editor_handle = editor.focus_handle_of(cx);
    let log: Rc<RefCell<Vec<String>>> = Rc::default();

    // The first prompt's handler runs with focus back in the editor and opens the second.
    let weak = workspace.downgrade();
    let first_log = log.clone();
    let second = session(&log);
    let first = Session::completing_read(HOST, "First: ")
        .source(Source::Fixed(vec![Candidate::text("one")]))
        .on_confirm(move |outcome, window, cx| {
            assert!(matches!(outcome, Outcome::Candidate(_)));
            let restored = editor_handle.contains_focused(window, cx);
            first_log
                .borrow_mut()
                .push(format!("first confirmed, editor focused: {restored}"));
            weak.update(cx, |workspace, cx| open(workspace, second, window, cx))
                .unwrap();
        });
    open_session(&workspace, cx, first);
    let first_origin = origin(&workspace, cx).unwrap();

    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["first confirmed, editor focused: true"]);
    assert!(session_open(&workspace, cx), "the second prompt opened");
    assert!(strip_focused(&workspace, cx));
    let second_origin = origin(&workspace, cx).unwrap();
    assert_eq!(second_origin.focus, first_origin.focus);
    assert_eq!(second_origin.pane, first_origin.pane);
    assert_eq!(second_origin.item, first_origin.item);

    cancel(cx);
    assert!(focused(&editor.focus_handle_of(cx), cx));
}

#[gpui::test]
async fn minibuffer_replacing_a_session_restores_and_records_the_same_origin(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    let editor = add_editor(&workspace, None, cx);
    let first_log = Rc::default();
    let second_log = Rc::default();

    open_session(&workspace, cx, session(&first_log));
    let first_origin = origin(&workspace, cx).unwrap();
    open_session(&workspace, cx, session(&second_log));
    assert_eq!(*first_log.borrow(), ["cancelled"]);
    assert!(strip_focused(&workspace, cx));
    let second_origin = origin(&workspace, cx).unwrap();
    assert_eq!(second_origin.focus, first_origin.focus);
    assert_eq!(second_origin.item, Some(editor.entity_id()));

    cancel(cx);
    assert_eq!(*second_log.borrow(), ["cancelled"]);
    assert!(focused(&editor.focus_handle_of(cx), cx));
}

#[gpui::test]
async fn minibuffer_cancel_after_focus_left_the_strip_leaves_it_there(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    let editor = add_editor(&workspace, None, cx);
    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    let right = workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(pane, SplitDirection::Right, window, cx)
    });
    let other = add_editor(&workspace, Some(&right), cx);
    let editor_handle = editor.focus_handle_of(cx);
    cx.update(|window, cx| window.focus(&editor_handle, cx));
    cx.run_until_parked();
    assert!(focused(&editor.focus_handle_of(cx), cx));
    open_session(&workspace, cx, session(&Rc::default()));

    // A click on the other editor focuses it, then cancels the prompt (the picker dispatches
    // `minibuffer::Cancel` on a mouse-down outside the strip).
    let other_handle = other.focus_handle_of(cx);
    cx.update(|window, cx| window.focus(&other_handle, cx));
    cx.run_until_parked();
    cancel(cx);
    assert!(!session_open(&workspace, cx));
    assert!(focused(&other_handle, cx));
}

#[gpui::test]
async fn minibuffer_records_no_origin_for_a_centered_prompt(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["another_host"]).await;
    let cx = &mut cx;
    let editor = add_editor(&workspace, None, cx);
    open_session(&workspace, cx, session(&Rc::default()));
    assert!(session_open(&workspace, cx));
    assert!(origin(&workspace, cx).is_none());
    workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
    cx.run_until_parked();
    assert!(focused(&editor.focus_handle_of(cx), cx));
}

#[gpui::test]
async fn minibuffer_keys_dispatch_through_the_minibuffer_context(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[HOST]).await;
    let cx = &mut cx;
    cx.update(|_, cx| {
        cx.bind_keys([KeyBinding::new(
            "ctrl-g",
            minibuffer::Cancel,
            Some("Minibuffer > TestHost > Editor"),
        )])
    });
    let editor = add_editor(&workspace, None, cx);
    let log = Rc::default();
    open_session(&workspace, cx, session(&log).key_context("TestHost"));

    let stack: Vec<String> = cx.update(|window, _| {
        window
            .context_stack()
            .iter()
            .filter_map(|context| context.primary().map(|entry| entry.key.to_string()))
            .collect()
    });
    let position = |name: &str| {
        stack
            .iter()
            .position(|entry| entry == name)
            .unwrap_or_else(|| panic!("{name} is not in {stack:?}"))
    };
    assert!(position("Workspace") < position("Minibuffer"));
    assert!(position("Minibuffer") < position("TestHost"));
    assert!(position("TestHost") < position("Picker"));
    assert!(position("Picker") < position("Editor"));

    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert_eq!(*log.borrow(), ["cancelled"]);
    assert!(focused(&editor.focus_handle_of(cx), cx));
}

trait FocusHandleOf {
    fn focus_handle_of(&self, cx: &mut VisualTestContext) -> FocusHandle;
}

impl FocusHandleOf for Entity<Editor> {
    fn focus_handle_of(&self, cx: &mut VisualTestContext) -> FocusHandle {
        self.read_with(cx, |editor, cx| editor.focus_handle(cx))
    }
}
