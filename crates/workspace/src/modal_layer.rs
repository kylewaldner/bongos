use gpui::{
    AnyView, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable as _, Global, ManagedView,
    MouseButton, Subscription,
};
use std::rc::Rc;
use ui::prelude::*;

use crate::Toast;

#[derive(Debug)]
pub enum DismissDecision {
    Dismiss(bool),
    Pending,
}

pub trait ModalView: ManagedView {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(true)
    }

    fn fade_out_background(&self) -> bool {
        false
    }

    fn render_bare(&self) -> bool {
        false
    }

    /// The name this modal is routed by, matched against `minibuffer.enabled_for`; `None` for
    /// modals that never move to the minibuffer.
    fn minibuffer_name(&self) -> Option<&'static str> {
        None
    }
}

/// How a message sent to the echo area is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinibufferMessageKind {
    /// Fades after `minibuffer.message_timeout`.
    Message,
    /// Stays until the next keystroke.
    Error,
}

/// Where a message sent through [`MinibufferRouting::message`] ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinibufferMessageOutcome {
    /// The echo area showed it.
    Echoed,
    /// There is no echo area; it went to the log.
    Logged,
}

/// Shows a message in the echo area of the window; returns whether it did.
pub type MinibufferMessageHook =
    Rc<dyn Fn(&mut Window, &mut App, &str, MinibufferMessageKind) -> bool>;

/// Shows a toast in the echo area of the workspace; returns whether it did.
pub type MinibufferToastHook = Rc<dyn Fn(&Toast, &mut Context<crate::Workspace>) -> bool>;

/// The settings of the minibuffer that `workspace` and `picker` read, and the hooks the
/// minibuffer crate installs. `bongos_minibuffer::init` installs it and mirrors the settings into
/// it. Nothing in it does anything while `routes` is empty and no hook is installed.
pub struct MinibufferRouting {
    routes: Vec<String>,
    max_rows: usize,
    show_count: bool,
    toast_hook: Option<MinibufferToastHook>,
    message_hook: Option<MinibufferMessageHook>,
}

impl Global for MinibufferRouting {}

impl Default for MinibufferRouting {
    fn default() -> Self {
        Self {
            routes: Vec::new(),
            max_rows: 15,
            show_count: true,
            toast_hook: None,
            message_hook: None,
        }
    }
}

impl MinibufferRouting {
    /// The names routed to the minibuffer (`minibuffer.enabled_for`).
    pub fn routes(&self) -> &[String] {
        &self.routes
    }

    /// Replaces the routed names.
    pub fn set_routes(&mut self, routes: Vec<String>) {
        self.routes = routes;
    }

    /// Whether `name` is routed to the minibuffer.
    pub fn is_routed(&self, name: &str) -> bool {
        self.routes.iter().any(|route| route == name)
    }

    /// The most candidate rows to show at once.
    pub fn max_rows(&self) -> usize {
        self.max_rows
    }

    /// Whether to show the match count in front of the prompt.
    pub fn show_count(&self) -> bool {
        self.show_count
    }

    /// Sets the row limit and whether the count is shown.
    pub fn set_display(&mut self, max_rows: usize, show_count: bool) {
        self.max_rows = max_rows;
        self.show_count = show_count;
    }

    /// The hook that sends toasts to the echo area, if one is installed.
    pub fn toast_hook(&self) -> Option<MinibufferToastHook> {
        self.toast_hook.clone()
    }

    /// Installs (or removes) the hook that sends toasts to the echo area.
    pub fn set_toast_hook(&mut self, hook: Option<MinibufferToastHook>) {
        self.toast_hook = hook;
    }

    /// Installs (or removes) the hook that shows messages in the echo area.
    pub fn set_message_hook(&mut self, hook: Option<MinibufferMessageHook>) {
        self.message_hook = hook;
    }

    /// Sends `text` to the echo area of `window`. Without an echo area (no hook installed, or
    /// the hook declines) it goes to the log instead.
    pub fn message(
        window: &mut Window,
        cx: &mut App,
        text: &str,
        kind: MinibufferMessageKind,
    ) -> MinibufferMessageOutcome {
        let hook = cx
            .try_global::<MinibufferRouting>()
            .and_then(|routing| routing.message_hook.clone());
        if let Some(hook) = hook
            && hook(window, cx, text, kind)
        {
            return MinibufferMessageOutcome::Echoed;
        }
        Self::log_message(text, kind);
        MinibufferMessageOutcome::Logged
    }

    /// The fallback of [`Self::message`] while no echo area exists.
    pub fn log_message(text: &str, kind: MinibufferMessageKind) {
        match kind {
            MinibufferMessageKind::Message => log::info!("minibuffer: {text}"),
            MinibufferMessageKind::Error => log::error!("minibuffer: {text}"),
        }
    }
}

/// Whether the prompt called `name` (see [`ModalView::minibuffer_name`]) is routed to the
/// minibuffer. False before the minibuffer is initialized and while `minibuffer.enabled_for` is
/// empty.
pub fn routes_to_minibuffer(name: &str, cx: &App) -> bool {
    cx.try_global::<MinibufferRouting>()
        .is_some_and(|routing| routing.is_routed(name))
}

trait ModalViewHandle {
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> DismissDecision;
    fn view(&self) -> AnyView;
    fn fade_out_background(&self, cx: &mut App) -> bool;
    fn render_bare(&self, cx: &mut App) -> bool;
}

impl<V: ModalView> ModalViewHandle for Entity<V> {
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> DismissDecision {
        self.update(cx, |this, cx| this.on_before_dismiss(window, cx))
    }

    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn fade_out_background(&self, cx: &mut App) -> bool {
        self.read(cx).fade_out_background()
    }

    fn render_bare(&self, cx: &mut App) -> bool {
        self.read(cx).render_bare()
    }
}

pub struct ActiveModal {
    modal: Box<dyn ModalViewHandle>,
    _subscriptions: [Subscription; 2],
    previous_focus_handle: Option<FocusHandle>,
    focus_handle: FocusHandle,
}

pub struct ModalLayer {
    active_modal: Option<ActiveModal>,
    dismiss_on_focus_lost: bool,
}

pub(crate) struct ModalOpenedEvent;

impl EventEmitter<ModalOpenedEvent> for ModalLayer {}

impl Default for ModalLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl ModalLayer {
    pub fn new() -> Self {
        Self {
            active_modal: None,
            dismiss_on_focus_lost: false,
        }
    }

    /// Toggles a modal of type `V`. If a modal of the same type is currently active,
    /// it will be hidden. If a different modal is active, it will be replaced with the new one.
    /// If no modal is active, the new modal will be shown.
    ///
    /// If closing the current modal fails (e.g., due to `on_before_dismiss` returning
    /// `DismissDecision::Dismiss(false)` or `DismissDecision::Pending`), the new modal
    /// will not be shown.
    pub fn toggle_modal<V, B>(&mut self, window: &mut Window, cx: &mut Context<Self>, build_view: B)
    where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<V>) -> V,
    {
        if let Some(active_modal) = &self.active_modal {
            let should_close = active_modal.modal.view().downcast::<V>().is_ok();
            let did_close = self.hide_modal(window, cx);
            if should_close || !did_close {
                return;
            }
        }
        let new_modal = cx.new(|cx| build_view(window, cx));
        self.show_modal(new_modal, window, cx);
        cx.emit(ModalOpenedEvent);
    }

    /// Shows a modal and sets up subscriptions for dismiss events and focus tracking.
    /// The modal is automatically focused after being shown.
    fn show_modal<V>(&mut self, new_modal: Entity<V>, window: &mut Window, cx: &mut Context<Self>)
    where
        V: ModalView,
    {
        let focus_handle = cx.focus_handle();
        self.active_modal = Some(ActiveModal {
            modal: Box::new(new_modal.clone()),
            _subscriptions: [
                cx.subscribe_in(
                    &new_modal,
                    window,
                    |this, _, _: &DismissEvent, window, cx| {
                        this.hide_modal(window, cx);
                    },
                ),
                cx.on_focus_out(&focus_handle, window, |this, _event, window, cx| {
                    if this.dismiss_on_focus_lost {
                        this.hide_modal(window, cx);
                    }
                }),
            ],
            previous_focus_handle: window.focused(cx),
            focus_handle,
        });
        cx.defer_in(window, move |_, window, cx| {
            window.focus(&new_modal.focus_handle(cx), cx);
        });
        cx.notify();
    }

    /// Attempts to hide the currently active modal.
    ///
    /// The modal's `on_before_dismiss` method is called to determine if dismissal should proceed.
    /// If dismissal is allowed, the modal is removed and focus is restored to the previously
    /// focused element.
    ///
    /// Returns `true` if the modal was successfully hidden, `false` otherwise.
    pub fn hide_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(active_modal) = self.active_modal.as_mut() else {
            self.dismiss_on_focus_lost = false;
            return false;
        };

        match active_modal.modal.on_before_dismiss(window, cx) {
            DismissDecision::Dismiss(should_dismiss) => {
                if !should_dismiss {
                    self.dismiss_on_focus_lost = !should_dismiss;
                    return false;
                }
            }
            DismissDecision::Pending => {
                self.dismiss_on_focus_lost = false;
                return false;
            }
        }

        if let Some(active_modal) = self.active_modal.take() {
            if let Some(previous_focus) = active_modal.previous_focus_handle
                && active_modal.focus_handle.contains_focused(window, cx)
            {
                previous_focus.focus(window, cx);
            }
            cx.notify();
        }
        self.dismiss_on_focus_lost = false;
        true
    }

    /// Returns the currently active modal if it is of type `V`.
    pub fn active_modal<V>(&self) -> Option<Entity<V>>
    where
        V: 'static,
    {
        let active_modal = self.active_modal.as_ref()?;
        active_modal.modal.view().downcast::<V>().ok()
    }

    pub fn has_active_modal(&self) -> bool {
        self.active_modal.is_some()
    }
}

impl Render for ModalLayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(active_modal) = &self.active_modal else {
            return div().into_any_element();
        };

        if active_modal.modal.render_bare(cx) {
            return active_modal.modal.view().into_any_element();
        }

        div()
            .absolute()
            .size_full()
            .inset_0()
            .occlude()
            .when(active_modal.modal.fade_out_background(cx), |this| {
                let mut background = cx.theme().colors().elevated_surface_background;
                background.fade_out(0.2);
                this.bg(background)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.hide_modal(window, cx);
                }),
            )
            .child(
                v_flex()
                    .h(px(0.0))
                    .top_20()
                    .items_center()
                    .track_focus(&active_modal.focus_handle)
                    .child(
                        h_flex()
                            .occlude()
                            .child(active_modal.modal.view())
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            }),
                    ),
            )
            .into_any_element()
    }
}
