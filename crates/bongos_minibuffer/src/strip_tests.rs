//! Tests of the strip and the bottom placement: untouched modals stay centered, a routed modal
//! is drawn in the strip, the strip comes and goes with `minibuffer.enabled_for`, and the
//! `Cancel` and `Focus` actions act on the prompt in the strip.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, TestAppContext, VisualTestContext,
    WindowHandle, px,
};
use project::Project;
use settings::SettingsStore;
use ui::prelude::*;
use workspace::{
    AppState, MinibufferMessageKind, MinibufferRouting, ModalView, MultiWorkspace, Workspace,
};

use crate::{NO_ACTIVE_MINIBUFFER, minibuffer_for_window};

/// A prompt that is routed by `name` (or never, when `None`).
struct TestModal {
    focus_handle: FocusHandle,
    name: Option<&'static str>,
}

impl TestModal {
    fn named(name: &'static str) -> impl FnOnce(&mut Window, &mut Context<Self>) -> Self {
        move |_, cx| Self {
            focus_handle: cx.focus_handle(),
            name: Some(name),
        }
    }

    fn unnamed(_: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            name: None,
        }
    }
}

impl EventEmitter<DismissEvent> for TestModal {}

impl Focusable for TestModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for TestModal {
    fn minibuffer_name(&self) -> Option<&'static str> {
        self.name
    }
}

impl Render for TestModal {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .debug_selector(|| "test-modal".to_string())
            .w(px(200.))
            .h(px(20.))
    }
}

fn init(cx: &mut TestAppContext, enabled_for: &[&str]) -> Arc<AppState> {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        crate::init(cx);
        app_state
    });
    set_enabled_for(cx, enabled_for);
    app_state
}

fn set_enabled_for(cx: &mut TestAppContext, names: &[&str]) {
    cx.update(|cx| write_enabled_for(cx, names));
}

fn write_enabled_for(cx: &mut App, names: &[&str]) {
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |content| {
            content.minibuffer.get_or_insert_default().enabled_for =
                Some(names.iter().map(|name| name.to_string()).collect());
        });
    });
}

async fn add_window(
    cx: &mut TestAppContext,
    app_state: &Arc<AppState>,
) -> WindowHandle<MultiWorkspace> {
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
}

fn home(window: WindowHandle<MultiWorkspace>, cx: &mut TestAppContext) -> Entity<Workspace> {
    window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap()
}

fn bottom_item_id(
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Option<gpui::EntityId> {
    workspace.read_with(cx, |workspace, _| {
        workspace.bottom_item().map(|item| item.entity_id())
    })
}

/// A window of one workspace, drawn, with focus on the workspace.
async fn visual_window(
    cx: &mut TestAppContext,
    enabled_for: &[&str],
) -> (Entity<Workspace>, VisualTestContext) {
    let app_state = init(cx, enabled_for);
    let window = add_window(cx, &app_state).await;
    let workspace = home(window, cx);
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    (workspace, cx)
}

fn open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, name: Option<&'static str>) {
    workspace.update_in(cx, |workspace, window, cx| match name {
        Some(name) => workspace.toggle_modal(window, cx, TestModal::named(name)),
        None => workspace.toggle_modal(window, cx, TestModal::unnamed),
    });
    cx.run_until_parked();
}

fn modal(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Entity<TestModal>> {
    workspace.read_with(cx, |workspace, cx| workspace.active_modal::<TestModal>(cx))
}

fn in_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

fn focused(cx: &mut VisualTestContext) -> Option<FocusHandle> {
    cx.update(|window, cx| window.focused(cx))
}

#[gpui::test]
async fn minibuffer_empty_enabled_for_has_no_strip_and_modals_stay_centered(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = visual_window(cx, &[]).await;
    assert!(workspace.read_with(&cx, |workspace, _| workspace.bottom_item().is_none()));

    // A modal that names itself is still centered while nothing is routed.
    open(&workspace, &mut cx, Some("test"));
    assert!(modal(&workspace, &mut cx).is_some());
    assert!(!in_strip(&workspace, &mut cx));
    let bounds = cx.debug_bounds("test-modal").expect("the modal is drawn");
    let window_size = cx.update(|window, _| window.viewport_size());
    assert!(
        bounds.origin.y < window_size.height / 2.0,
        "the modal is drawn near the top: {bounds:?}"
    );
}

#[gpui::test]
async fn minibuffer_routed_modal_renders_in_the_strip_and_dismisses(cx: &mut TestAppContext) {
    let (workspace, mut cx) = visual_window(cx, &["test"]).await;
    assert!(workspace.read_with(&cx, |workspace, _| workspace.bottom_item().is_some()));
    let before = focused(&mut cx);
    assert!(before.is_some());

    open(&workspace, &mut cx, Some("test"));
    let modal = modal(&workspace, &mut cx).expect("active_modal finds the routed modal");
    assert!(in_strip(&workspace, &mut cx));
    let bounds = cx.debug_bounds("test-modal").expect("the strip draws it");
    let window_size = cx.update(|window, _| window.viewport_size());
    assert!(
        bounds.origin.y > window_size.height / 2.0,
        "the modal is drawn at the bottom: {bounds:?}"
    );
    let modal_focus = modal.read_with(&cx, |modal, _| modal.focus_handle.clone());
    assert!(cx.update(|window, _| modal_focus.is_focused(window)));

    modal.update(&mut cx, |_, cx| cx.emit(DismissEvent));
    cx.run_until_parked();
    assert!(self::modal(&workspace, &mut cx).is_none());
    assert!(cx.debug_bounds("test-modal").is_none());
    assert_eq!(focused(&mut cx), before);
}

#[gpui::test]
async fn minibuffer_unnamed_modal_stays_centered_while_routing_is_on(cx: &mut TestAppContext) {
    let (workspace, mut cx) = visual_window(cx, &["test"]).await;
    open(&workspace, &mut cx, None);
    assert!(modal(&workspace, &mut cx).is_some());
    assert!(!in_strip(&workspace, &mut cx));
    let bounds = cx.debug_bounds("test-modal").expect("the modal is drawn");
    let window_size = cx.update(|window, _| window.viewport_size());
    assert!(bounds.origin.y < window_size.height / 2.0);

    // A named modal that is not in `enabled_for` is centered too. (Toggling the same type
    // closes the first.)
    open(&workspace, &mut cx, Some("other"));
    assert!(modal(&workspace, &mut cx).is_none());
    open(&workspace, &mut cx, Some("other"));
    assert!(!in_strip(&workspace, &mut cx));
    assert!(modal(&workspace, &mut cx).is_some());
}

#[gpui::test]
async fn minibuffer_two_workspaces_of_a_window_share_one_strip_and_windows_have_their_own(
    cx: &mut TestAppContext,
) {
    let app_state = init(cx, &["test"]);
    let first = add_window(cx, &app_state).await;
    let first_home = home(first, cx);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let second_workspace = first
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project, window, cx)
        })
        .unwrap();
    let second = add_window(cx, &app_state).await;
    let second_home = home(second, cx);

    let strip = cx
        .update(|cx| minibuffer_for_window(first.into(), cx))
        .unwrap();
    assert_eq!(bottom_item_id(&first_home, cx), Some(strip.entity_id()));
    assert_eq!(
        bottom_item_id(&second_workspace, cx),
        Some(strip.entity_id())
    );

    let other = cx
        .update(|cx| minibuffer_for_window(second.into(), cx))
        .unwrap();
    assert_ne!(other.entity_id(), strip.entity_id());
    assert_eq!(bottom_item_id(&second_home, cx), Some(other.entity_id()));
}

#[gpui::test]
async fn minibuffer_toggling_enabled_for_adds_and_removes_the_strip(cx: &mut TestAppContext) {
    let app_state = init(cx, &[]);
    let window = add_window(cx, &app_state).await;
    let workspace = home(window, cx);
    assert_eq!(bottom_item_id(&workspace, cx), None);
    assert!(
        cx.update(|cx| minibuffer_for_window(window.into(), cx))
            .is_none()
    );

    set_enabled_for(cx, &["test"]);
    let strip = cx
        .update(|cx| minibuffer_for_window(window.into(), cx))
        .unwrap();
    assert_eq!(bottom_item_id(&workspace, cx), Some(strip.entity_id()));

    // Another workspace of the window gets it too.
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let added = window
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project, window, cx)
        })
        .unwrap();
    assert_eq!(bottom_item_id(&added, cx), Some(strip.entity_id()));

    set_enabled_for(cx, &["test", "other"]);
    assert_eq!(bottom_item_id(&workspace, cx), Some(strip.entity_id()));

    set_enabled_for(cx, &[]);
    assert_eq!(bottom_item_id(&workspace, cx), None);
    assert_eq!(bottom_item_id(&added, cx), None);
    assert!(
        cx.update(|cx| minibuffer_for_window(window.into(), cx))
            .is_none()
    );
}

#[gpui::test]
async fn minibuffer_disabling_hides_a_prompt_in_the_strip(cx: &mut TestAppContext) {
    let (workspace, mut cx) = visual_window(cx, &["test"]).await;
    open(&workspace, &mut cx, Some("test"));
    assert!(in_strip(&workspace, &mut cx));

    cx.update(|_, cx| write_enabled_for(cx, &[]));
    cx.run_until_parked();
    assert!(modal(&workspace, &mut cx).is_none());
}

#[gpui::test]
async fn minibuffer_strip_redraws_when_the_modal_layer_changes(cx: &mut TestAppContext) {
    let (workspace, mut cx) = visual_window(cx, &["test"]).await;
    let strip = workspace
        .read_with(&cx, |workspace, _| workspace.bottom_item())
        .unwrap();
    let redraws = Rc::new(Cell::new(0));
    let _subscription = cx.update({
        let redraws = redraws.clone();
        move |_, cx| {
            cx.observe(
                &strip.downcast::<crate::Minibuffer>().unwrap(),
                move |_, _| redraws.set(redraws.get() + 1),
            )
        }
    });

    open(&workspace, &mut cx, Some("test"));
    let after_open = redraws.get();
    assert!(after_open > 0, "opening a prompt redraws the strip");
    assert!(cx.debug_bounds("test-modal").is_some());

    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.hide_modal(window, cx);
    });
    cx.run_until_parked();
    assert!(redraws.get() > after_open, "closing it redraws the strip");
    assert!(cx.debug_bounds("test-modal").is_none());
}

#[gpui::test]
async fn minibuffer_cancel_hides_a_routed_session_and_restores_focus(cx: &mut TestAppContext) {
    let (workspace, mut cx) = visual_window(cx, &["test"]).await;
    let before = focused(&mut cx);
    open(&workspace, &mut cx, Some("test"));
    assert!(in_strip(&workspace, &mut cx));

    cx.dispatch_action(zed_actions::minibuffer::Cancel);
    cx.run_until_parked();
    assert!(modal(&workspace, &mut cx).is_none());
    assert_eq!(focused(&mut cx), before);
}

#[gpui::test]
async fn minibuffer_cancel_without_a_session_says_there_is_none(cx: &mut TestAppContext) {
    let (workspace, mut cx) = visual_window(cx, &["test"]).await;
    let messages = Rc::new(Cell::new(0));
    cx.update({
        let messages = messages.clone();
        move |_, cx| {
            cx.global_mut::<MinibufferRouting>()
                .set_message_hook(Some(Rc::new(move |_, _, text, kind| {
                    assert_eq!(text, NO_ACTIVE_MINIBUFFER);
                    assert_eq!(kind, MinibufferMessageKind::Message);
                    messages.set(messages.get() + 1);
                    true
                })));
        }
    });
    // A centered modal is not a session either.
    open(&workspace, &mut cx, None);

    cx.dispatch_action(zed_actions::minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(messages.get(), 1);
    assert!(
        modal(&workspace, &mut cx).is_some(),
        "the centered modal stays"
    );

    cx.dispatch_action(zed_actions::minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(messages.get(), 2);
}

#[gpui::test]
async fn minibuffer_focus_focuses_the_hosted_view(cx: &mut TestAppContext) {
    let (workspace, mut cx) = visual_window(cx, &["test"]).await;
    open(&workspace, &mut cx, Some("test"));
    let modal = modal(&workspace, &mut cx).unwrap();
    let modal_focus = modal.read_with(&cx, |modal, _| modal.focus_handle.clone());

    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    assert!(!cx.update(|window, _| modal_focus.is_focused(window)));

    cx.dispatch_action(zed_actions::minibuffer::Focus);
    cx.run_until_parked();
    assert!(cx.update(|window, _| modal_focus.is_focused(window)));
}

#[gpui::test]
async fn minibuffer_closed_window_leaves_no_strip_behind(cx: &mut TestAppContext) {
    let app_state = init(cx, &["test"]);
    let kept = add_window(cx, &app_state).await;
    let closed = add_window(cx, &app_state).await;
    assert!(
        cx.update(|cx| minibuffer_for_window(closed.into(), cx))
            .is_some()
    );

    closed
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert!(
        cx.update(|cx| minibuffer_for_window(closed.into(), cx))
            .is_none()
    );
    assert!(
        cx.update(|cx| minibuffer_for_window(kept.into(), cx))
            .is_some()
    );
}
