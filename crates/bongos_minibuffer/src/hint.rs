//! The pending-key hint and the prefix echo. While `key_hint` is routed (see
//! `minibuffer.enabled_for`) the strip observes pending input as which-key does: after
//! `minibuffer.key_hint_delay` it shows the pending keys in Emacs notation (`C-x 4 -`) and, when
//! `which_key.enabled` is true, the keys that can follow, in a list above the strip. It clears
//! when the sequence completes or is cancelled. `set_prefix_echo` shows text at once, for
//! prefix arguments (`C-u 4 -`). A prompt in the strip hides both. Everything here is inert
//! until `key_hint` is routed, and while it is routed the which-key modal stays closed.

use bongos_minibuffer_model::{Keystroke as ModelKeystroke, emacs_notation};
use gpui::{AnyElement, App, Context, SharedString, Task, Window, deferred};
use ui::prelude::*;
use workspace::{Workspace, routes_to_minibuffer};

use ::settings::Settings as _;

use crate::MinibufferSettings;
use crate::strip::{Minibuffer, minibuffer_for_window};

/// The name that routes the hint and the prefix echo (`minibuffer.enabled_for`).
pub const KEY_HINT: &str = which_key::KEY_HINT;

/// What a strip shows of pending input.
#[derive(Default)]
pub(crate) struct Hint {
    /// Text set through [`set_prefix_echo`].
    prefix: Option<SharedString>,
    /// The pending keys in Emacs notation, once the delay passed.
    keys: Option<SharedString>,
    /// The continuations of the pending keys: (keys, action), when which-key is enabled.
    rows: Vec<(SharedString, SharedString)>,
    delay: Option<Task<()>>,
}

/// Shows `text` in the strip of `window` at once, as the echo of a prefix argument (`C-u 4 -`);
/// `None` clears it. The caller decides when to clear it. Does nothing unless `key_hint` is
/// routed.
pub fn set_prefix_echo(window: &mut Window, cx: &mut App, text: Option<String>) {
    let Some(strip) = minibuffer_for_window(window.window_handle(), cx) else {
        return;
    };
    let text = text
        .filter(|_| routes_to_minibuffer(KEY_HINT, cx))
        .map(SharedString::from);
    strip.update(cx, |strip, cx| {
        strip.hint.prefix = text;
        cx.notify();
    });
}

impl Minibuffer {
    /// The text of the hint: the pending keys (after the delay) following any prefix echo, or
    /// the prefix echo alone.
    pub fn hint_text(&self) -> Option<SharedString> {
        let hint = &self.hint;
        match (&hint.prefix, &hint.keys) {
            (Some(prefix), Some(keys)) => {
                let prefix = prefix.trim_end_matches('-').trim_end();
                Some(format!("{prefix} {keys}").into())
            }
            (Some(text), None) | (None, Some(text)) => Some(text.clone()),
            (None, None) => None,
        }
    }

    /// The keys that can follow the pending keys, with their actions.
    pub fn hint_rows(&self) -> &[(SharedString, SharedString)] {
        &self.hint.rows
    }

    /// Whether the hint is on screen right now: there is one and no prompt hides it.
    pub fn hint_visible(&self) -> bool {
        self.hint_text().is_some() && !self.echo.hosted
    }

    /// The list of continuations, drawn above the strip.
    pub(crate) fn hint_list(&self, cx: &App) -> Option<AnyElement> {
        if self.hint.rows.is_empty() {
            return None;
        }
        let max_rows = MinibufferSettings::get_global(cx).max_rows;
        let colors = cx.theme().colors();
        let rows = self.hint.rows.iter().take(max_rows).map(|(keys, action)| {
            let color = if action.starts_with('+') {
                Color::Success
            } else {
                Color::Default
            };
            h_flex()
                .px_1()
                .gap_2()
                .child(Label::new(keys.clone()).color(Color::Accent).single_line())
                .child(Label::new(action.clone()).color(color).single_line())
        });
        Some(
            deferred(
                v_flex()
                    .id("minibuffer-hint-list")
                    .debug_selector(|| "hint-list".into())
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .w_full()
                    .occlude()
                    .bg(colors.elevated_surface_background)
                    .border_t_1()
                    .border_color(colors.border)
                    .children(rows),
            )
            .into_any_element(),
        )
    }

    /// Pending input changed: show or clear the hint.
    fn pending_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.pending_input_keystrokes().is_none() || !routes_to_minibuffer(KEY_HINT, cx) {
            self.hint.delay = None;
            if self.hint.keys.take().is_some() {
                self.hint.rows.clear();
                cx.notify();
            }
            return;
        }
        if self.hint.keys.is_some() {
            self.show_hint(window, cx);
            return;
        }
        let delay = MinibufferSettings::get_global(cx).key_hint_delay;
        self.hint.delay = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update_in(cx, |this, window, cx| this.show_hint(window, cx))
                .ok();
        }));
    }

    /// Shows the keys pending now, if any, and what can follow them.
    fn show_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = window.pending_input_keystrokes() else {
            return;
        };
        if !routes_to_minibuffer(KEY_HINT, cx) {
            return;
        }
        let keys: Vec<_> = pending.iter().map(model_keystroke).collect();
        self.hint.keys = Some(format!("{} -", emacs_notation(&keys)).into());
        let which_key =
            which_key::WhichKeySettings::try_get(cx).is_some_and(|which_key| which_key.enabled);
        self.hint.rows = if which_key {
            which_key::continuations(pending, window, cx)
        } else {
            Vec::new()
        };
        cx.notify();
    }
}

fn model_keystroke(keystroke: &gpui::Keystroke) -> ModelKeystroke {
    ModelKeystroke {
        key: keystroke.key.clone(),
        control: keystroke.modifiers.control,
        alt: keystroke.modifiers.alt,
        shift: keystroke.modifiers.shift,
        platform: keystroke.modifiers.platform,
    }
}

/// Observes pending input in every workspace, as which-key does.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|_: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        cx.observe_pending_input(window, |_, window, cx| {
            if let Some(strip) = minibuffer_for_window(window.window_handle(), cx) {
                strip.update(cx, |strip, cx| strip.pending_changed(window, cx));
            }
        })
        .detach();
    })
    .detach();
}
