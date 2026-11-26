//! The item in a modal over the whole window, for the sidebar layout: 640 px wide (never wider
//! than the window less 40 px), a 2 px accent line along its top edge, the elevated surface with
//! the border outline, rounded corners and a shadow, over a dim scrim. `inbox: next` and
//! `inbox: previous` (`n` and `p`) show the next or previous item, which is then read; Esc, or a
//! click outside, closes it.

use crate::item::{self, ItemDetail};
use crate::view::InboxView;
use crate::{Next, OpenFile, Previous};
use gpui::{
    App, BoxShadow, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, MouseButton, Render, ScrollHandle, Subscription, Window, black, div, point, px,
};
use ui::prelude::*;
use workspace::ModalView;

/// The width of the modal.
const WIDTH: f32 = 640.;
/// The gap kept between the modal and the window's edges.
const MARGIN: f32 = 40.;
/// The dim layer behind the modal, black at 20% (`#00000033`).
const SCRIM_OPACITY: f32 = 0.2;
/// The modal's drop shadow, black at 40% (`#00000066`).
const SHADOW_OPACITY: f32 = 0.4;
/// The modal's corner radius: the base radius 6 px plus 4.
const RADIUS: f32 = 10.;

pub struct ItemModal {
    view: Entity<InboxView>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    _subscription: Subscription,
}

impl ItemModal {
    pub fn new(view: Entity<InboxView>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&view, |_, _, cx| cx.notify());
        Self {
            view,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    /// The item the modal shows.
    pub fn detail(&self, cx: &App) -> Option<ItemDetail> {
        self.view.read(cx).detail(cx)
    }

    /// Closes the modal and puts the focus back on the list; a bare modal has no wrapper that
    /// the layer could restore it from.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view.update(cx, |view, cx| view.focus_list(window, cx));
        cx.emit(DismissEvent);
    }

    fn step(&mut self, offset: isize, cx: &mut Context<Self>) {
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.view.update(cx, |view, cx| view.step_item(offset, cx));
    }
}

impl EventEmitter<DismissEvent> for ItemModal {}

impl Focusable for ItemModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for ItemModal {
    fn render_bare(&self) -> bool {
        true
    }
}

impl Render for ItemModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let detail = self.view.read(cx).detail(cx);
        let position = self.view.read(cx).position();
        let colors = cx.theme().colors().clone();
        let width = WIDTH.min((f32::from(window.viewport_size().width) - 2. * MARGIN).max(0.));
        let content = item::render_item::<Self>(
            "inbox-modal-item",
            detail.as_ref(),
            false,
            &self.scroll,
            |this, window, cx| {
                this.view.update(cx, |view, cx| view.open_file(window, cx));
            },
            cx,
        );
        let panel = v_flex()
            .id("inbox-modal")
            .key_context("InboxItemModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Next, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &Previous, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| {
                this.view.update(cx, |view, cx| view.open_file(window, cx));
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| this.close(window, cx)))
            .relative()
            .occlude()
            .w(px(width))
            .max_h(relative(0.9))
            .overflow_hidden()
            .bg(colors.elevated_surface_background)
            .border_1()
            .border_color(colors.border)
            .rounded(px(RADIUS))
            .shadow(vec![BoxShadow {
                color: black().opacity(SHADOW_OPACITY).into(),
                offset: point(px(0.), px(12.)),
                blur_radius: px(32.),
                spread_radius: px(0.),
                inset: false,
            }])
            .font_ui(cx)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(item::render_item_bar(position, cx))
            .child(content)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(2.))
                    .bg(colors.text_accent),
            );
        div()
            .id("inbox-modal-scrim")
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(black().opacity(SCRIM_OPACITY))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close(window, cx)),
            )
            .child(panel)
    }
}
