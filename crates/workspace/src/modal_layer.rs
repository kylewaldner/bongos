use gpui::{
    AnyView, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable as _, Global,
    ManagedView, MouseButton, Subscription, WeakEntity,
};
use std::rc::Rc;
use ui::prelude::*;

use crate::{DockPosition, Pane, Toast, Workspace};

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

/// Where focus was when a prompt opened in the minibuffer strip: the focused handle, the pane it
/// was in (center or dock), that pane's active item and the dock it was in. Hiding the prompt
/// puts focus back from it even when the pane, the item or the dock changed meanwhile (see
/// [`Workspace::restore_modal_origin`]). Only `Bottom` modals record one, and only while
/// something is routed to the minibuffer.
#[derive(Clone)]
pub struct ModalOrigin {
    pub focus: FocusHandle,
    /// `None` when focus was in no pane (a dock panel without panes, the workspace itself).
    pub pane: Option<WeakEntity<Pane>>,
    pub item: Option<EntityId>,
    pub dock: Option<DockPosition>,
    pub(crate) workspace: WeakEntity<Workspace>,
}

impl ModalOrigin {
    /// Puts focus back through the workspace's fallbacks. The workspace must not be in an
    /// update; inside one, use [`Workspace::restore_modal_origin`].
    fn restore(self, window: &mut Window, cx: &mut App) {
        match self.workspace.upgrade() {
            Some(workspace) => workspace.update(cx, |workspace, cx| {
                workspace.restore_modal_origin(self, window, cx)
            }),
            None => window.focus(&self.focus, cx),
        }
    }
}

/// Where an active modal is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    /// By the layer, over the window.
    Centered,
    /// By the minibuffer strip, which asks for it through [`ModalLayer::bottom_modal`].
    Bottom,
}

trait ModalViewHandle {
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> DismissDecision;
    fn view(&self) -> AnyView;
    fn fade_out_background(&self, cx: &mut App) -> bool;
    fn render_bare(&self, cx: &mut App) -> bool;
    fn focus(&self, window: &mut Window, cx: &mut App);
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

    fn focus(&self, window: &mut Window, cx: &mut App) {
        let focus_handle = self.focus_handle(cx);
        window.focus(&focus_handle, cx);
    }
}

pub struct ActiveModal {
    modal: Box<dyn ModalViewHandle>,
    _subscriptions: [Subscription; 2],
    previous_focus_handle: Option<FocusHandle>,
    focus_handle: FocusHandle,
    placement: Placement,
    /// Recorded for `Bottom` modals only; it takes the place of `previous_focus_handle`.
    origin: Option<ModalOrigin>,
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
        self.toggle_modal_with_origin(None, window, cx, build_view)
    }

    /// [`Self::toggle_modal`], with the origin the workspace recorded for the new modal, which
    /// is kept only when the modal is placed in the minibuffer strip.
    pub(crate) fn toggle_modal_with_origin<V, B>(
        &mut self,
        origin: Option<ModalOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
        build_view: B,
    ) where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<V>) -> V,
    {
        if let Some(active_modal) = &self.active_modal {
            let should_close = active_modal.modal.view().downcast::<V>().is_ok();
            // The workspace hides a modal with an origin itself before it gets here, so this
            // one has none; should one slip through, its handle is all that can be restored
            // while the workspace is being updated.
            let (did_close, origin) = self.hide_modal_keeping_origin(window, cx);
            if let Some(origin) = origin {
                window.focus(&origin.focus, cx);
            }
            if should_close || !did_close {
                return;
            }
        }
        let new_modal = cx.new(|cx| build_view(window, cx));
        self.show_modal(new_modal, origin, window, cx);
        cx.emit(ModalOpenedEvent);
    }

    /// Shows a modal and sets up subscriptions for dismiss events and focus tracking.
    /// The modal is automatically focused after being shown.
    fn show_modal<V>(
        &mut self,
        new_modal: Entity<V>,
        origin: Option<ModalOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        V: ModalView,
    {
        let focus_handle = cx.focus_handle();
        let placement = match new_modal.read(cx).minibuffer_name() {
            Some(name) if routes_to_minibuffer(name, cx) => Placement::Bottom,
            _ => Placement::Centered,
        };
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
            placement,
            origin: origin.filter(|_| placement == Placement::Bottom),
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
        let (hidden, origin) = self.hide_modal_keeping_origin(window, cx);
        if let Some(origin) = origin {
            origin.restore(window, cx);
        }
        hidden
    }

    /// The origin the active modal restores when it hides, if it recorded one.
    pub fn modal_origin(&self) -> Option<ModalOrigin> {
        self.active_modal.as_ref()?.origin.clone()
    }

    /// Whether the active modal has an origin to restore when it hides.
    pub(crate) fn has_origin(&self) -> bool {
        self.active_modal
            .as_ref()
            .is_some_and(|active_modal| active_modal.origin.is_some())
    }

    /// [`Self::hide_modal`], except that the origin of a `Bottom` modal is handed back to the
    /// caller to restore instead of restored, so that a caller inside an update of the workspace
    /// can restore it. It is handed back only when focus is still inside the modal: a click
    /// outside the strip cancels it and leaves focus where the click put it.
    pub(crate) fn hide_modal_keeping_origin(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (bool, Option<ModalOrigin>) {
        let Some(active_modal) = self.active_modal.as_mut() else {
            self.dismiss_on_focus_lost = false;
            return (false, None);
        };

        match active_modal.modal.on_before_dismiss(window, cx) {
            DismissDecision::Dismiss(should_dismiss) => {
                if !should_dismiss {
                    self.dismiss_on_focus_lost = !should_dismiss;
                    return (false, None);
                }
            }
            DismissDecision::Pending => {
                self.dismiss_on_focus_lost = false;
                return (false, None);
            }
        }

        let mut restore = None;
        if let Some(active_modal) = self.active_modal.take() {
            if active_modal.focus_handle.contains_focused(window, cx) {
                if let Some(origin) = active_modal.origin {
                    restore = Some(origin);
                } else if let Some(previous_focus) = active_modal.previous_focus_handle {
                    previous_focus.focus(window, cx);
                }
            }
            cx.notify();
        }
        self.dismiss_on_focus_lost = false;
        (true, restore)
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

    /// The active modal and the layer's focus handle when it is placed in the minibuffer strip,
    /// which draws it inside an element that tracks the handle (so that [`Self::hide_modal`]
    /// can tell whether focus is inside the modal). `None` for a centered modal.
    pub fn bottom_modal(&self) -> Option<(AnyView, FocusHandle)> {
        let active_modal = self.active_modal.as_ref()?;
        (active_modal.placement == Placement::Bottom)
            .then(|| (active_modal.modal.view(), active_modal.focus_handle.clone()))
    }

    /// Hides the active modal if it is placed in the minibuffer strip. Returns whether it did.
    pub fn hide_bottom_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.bottom_modal().is_some() && self.hide_modal(window, cx)
    }

    /// Moves focus into the active modal if it is placed in the minibuffer strip. Returns
    /// whether there was one.
    pub fn focus_bottom_modal(&self, window: &mut Window, cx: &mut App) -> bool {
        match &self.active_modal {
            Some(active_modal) if active_modal.placement == Placement::Bottom => {
                active_modal.modal.focus(window, cx);
                true
            }
            _ => false,
        }
    }
}

impl Render for ModalLayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(active_modal) = &self.active_modal else {
            return div().into_any_element();
        };

        if active_modal.placement == Placement::Bottom {
            return div().into_any_element();
        }

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
