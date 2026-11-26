//! The inbox badge: a small status bar item at the right corner that shows how many items are
//! unread. It renders nothing while `inbox.enabled` is false or nothing is unread, never raises
//! a notification, toast or modal, and opens the inbox with [`Open`] when clicked.

use crate::{Inbox, InboxEvent, Open};
use gpui::{
    App, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, Subscription,
    Window, div,
};
use ui::{Button, ButtonCommon as _, Clickable as _, LabelSize, Tooltip, prelude::*};
use workspace::{ItemHandle, StatusItemView};

/// The most the badge counts to; more shows as `99+`.
pub const MAX_SHOWN: usize = 99;

/// The text of the badge for `count` unread items: the number, or `99+` above 99.
pub fn badge_label(count: usize) -> String {
    if count > MAX_SHOWN {
        format!("{MAX_SHOWN}+")
    } else {
        count.to_string()
    }
}

/// The status bar item. One per workspace; every one shows the same global unread count.
pub struct InboxBadge {
    _subscriptions: Vec<Subscription>,
}

impl InboxBadge {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(inbox) = Inbox::global(cx) {
            subscriptions.push(cx.subscribe(&inbox, |_, _, _: &InboxEvent, cx| cx.notify()));
        }
        Self {
            _subscriptions: subscriptions,
        }
    }

    /// The unread count the badge shows, or `None` when it shows nothing (the inbox is off, not
    /// scanned yet, or nothing is unread).
    pub fn unread(&self, cx: &App) -> Option<usize> {
        let inbox = Inbox::global(cx)?;
        let inbox = inbox.read(cx);
        if !inbox.is_enabled() {
            return None;
        }
        let count = inbox.snapshot()?.unread_count();
        (count > 0).then_some(count)
    }

    /// The text the badge shows, or `None` when it shows nothing.
    pub fn text(&self, cx: &App) -> Option<String> {
        self.unread(cx).map(badge_label)
    }
}

impl Render for InboxBadge {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(text) = self.text(cx) else {
            return div().hidden().into_any_element();
        };
        div()
            .flex_shrink_0()
            .debug_selector(|| "inbox-badge".to_string())
            .child(
                Button::new("inbox-badge", text)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::for_action_title("Open the inbox", &Open))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(Open), cx)),
            )
            .into_any_element()
    }
}

impl StatusItemView for InboxBadge {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // The item appears only while `inbox.enabled` is true and something is unread.
        None
    }
}
