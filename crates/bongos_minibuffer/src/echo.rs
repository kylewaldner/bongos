//! The echo area: transient text in the strip while no prompt is open, as in Emacs. A message
//! fades after `minibuffer.message_timeout`; an error stays until the next keystroke. Neither
//! takes focus. The last 100 are kept per window and `minibuffer::ShowMessages` lists them. A
//! prompt in the strip hides the echo area, and a message that arrives meanwhile waits until the
//! prompt closes. Everything here is inert until `messages` is routed (see
//! `minibuffer.enabled_for`): the message hook answers `false` and toasts stay notifications.

use std::collections::VecDeque;
use std::rc::Rc;

use ::settings::Settings as _;
use gpui::{App, Context, SharedString, Task, Window};
use workspace::{
    MinibufferMessageKind, MinibufferRouting, MinibufferToastHook, Toast, Workspace,
    routes_to_minibuffer,
};

use crate::MinibufferSettings;
use crate::session::{Candidate, Session, open};
use crate::strip::{self, Minibuffer, minibuffer_for_window};

/// The name that routes the echo area and the toast hook (`minibuffer.enabled_for`), and the host
/// of the message list.
pub const MESSAGES: &str = "messages";

/// How many messages a window keeps.
pub const LOG_CAPACITY: usize = 100;

/// Shows `text` in the echo area of `window`; it fades after `minibuffer.message_timeout`.
/// Without an echo area (`messages` not routed) it goes to the log.
pub fn message(window: &mut Window, cx: &mut App, text: impl Into<SharedString>) {
    let text = text.into();
    MinibufferRouting::message(window, cx, &text, MinibufferMessageKind::Message);
}

/// Shows `text` in the echo area of `window`, where it stays until the next keystroke.
pub fn error(window: &mut Window, cx: &mut App, text: impl Into<SharedString>) {
    let text = text.into();
    MinibufferRouting::message(window, cx, &text, MinibufferMessageKind::Error);
}

/// One line of the echo area or of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EchoLine {
    pub text: SharedString,
    pub kind: MinibufferMessageKind,
}

/// What a strip remembers of its echo area.
#[derive(Default)]
pub(crate) struct Echo {
    /// The line to show, once no prompt is in the strip.
    pub(crate) current: Option<EchoLine>,
    /// Whether `current` has been on show (its timer runs, an error waits for a keystroke).
    shown: bool,
    /// Whether the next keystroke clears the current error.
    armed: bool,
    /// Whether a prompt is in the strip.
    pub(crate) hosted: bool,
    log: VecDeque<EchoLine>,
    fade: Option<Task<()>>,
}

impl Minibuffer {
    /// The echo area's line, if one is showing or waiting for a prompt to close.
    pub fn echo_line(&self) -> Option<&EchoLine> {
        self.echo.current.as_ref()
    }

    /// The last messages, newest first.
    pub fn messages(&self) -> Vec<EchoLine> {
        self.echo.log.iter().rev().cloned().collect()
    }

    /// Whether a line is on screen right now: there is one and no prompt hides it.
    pub fn echo_visible(&self) -> bool {
        self.echo.current.is_some() && !self.echo.hosted
    }

    /// Records `text` and makes it the echo area's line, now or when the prompt closes.
    pub(crate) fn say(
        &mut self,
        text: SharedString,
        kind: MinibufferMessageKind,
        cx: &mut Context<Self>,
    ) {
        let line = EchoLine { text, kind };
        let echo = &mut self.echo;
        echo.log.push_back(line.clone());
        while echo.log.len() > LOG_CAPACITY {
            echo.log.pop_front();
        }
        echo.current = Some(line);
        echo.shown = false;
        echo.armed = false;
        echo.fade = None;
        if !echo.hosted {
            self.start_showing(cx);
        }
        cx.notify();
    }

    /// Called when the prompt in the strip opens or closes.
    pub(crate) fn set_hosted(&mut self, hosted: bool, cx: &mut Context<Self>) {
        self.echo.hosted = hosted;
        if !hosted && self.echo.current.is_some() && !self.echo.shown {
            self.start_showing(cx);
        }
    }

    /// The current line goes on show: a message starts fading, an error starts waiting for a
    /// keystroke.
    fn start_showing(&mut self, cx: &mut Context<Self>) {
        let Some(line) = &self.echo.current else {
            return;
        };
        self.echo.shown = true;
        match line.kind {
            MinibufferMessageKind::Message => {
                let timeout = MinibufferSettings::get_global(cx).message_timeout;
                self.echo.fade = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(timeout).await;
                    this.update(cx, |this, cx| this.clear_echo(cx)).ok();
                }));
            }
            MinibufferMessageKind::Error => {
                // The keystroke that caused the error is still being handled and would clear it
                // at once, so the error only listens for keystrokes after that one.
                let this = cx.weak_entity();
                cx.defer(move |cx| {
                    this.update(cx, |this, _| {
                        let error = matches!(
                            this.echo.current,
                            Some(EchoLine {
                                kind: MinibufferMessageKind::Error,
                                ..
                            })
                        );
                        this.echo.armed = error && this.echo.shown;
                    })
                    .ok();
                });
            }
        }
    }

    fn clear_echo(&mut self, cx: &mut Context<Self>) {
        let echo = &mut self.echo;
        echo.current = None;
        echo.shown = false;
        echo.armed = false;
        echo.fade = None;
        cx.notify();
    }

    fn keystroke(&mut self, cx: &mut Context<Self>) {
        if self.echo.armed {
            self.clear_echo(cx);
        }
    }
}

/// Installs the message hook, the toast hook and the keystroke observer.
pub(crate) fn init(cx: &mut App) {
    let message_hook: workspace::MinibufferMessageHook =
        Rc::new(|window, cx, text, kind| show(window, cx, text, kind));
    let toast_hook: MinibufferToastHook = Rc::new(show_toast);
    cx.global_mut::<MinibufferRouting>()
        .set_message_hook(Some(message_hook));
    cx.global_mut::<MinibufferRouting>()
        .set_toast_hook(Some(toast_hook));
    cx.observe_keystrokes(|_, window, cx| {
        if let Some(strip) = minibuffer_for_window(window.window_handle(), cx) {
            strip.update(cx, |strip, cx| strip.keystroke(cx));
        }
    })
    .detach();
}

fn show(window: &mut Window, cx: &mut App, text: &str, kind: MinibufferMessageKind) -> bool {
    if !routes_to_minibuffer(MESSAGES, cx) {
        return false;
    }
    let Some(strip) = minibuffer_for_window(window.window_handle(), cx) else {
        return false;
    };
    let text = SharedString::from(text.to_string());
    strip.update(cx, |strip, cx| strip.say(text, kind, cx));
    true
}

/// Shows a toast with no click action in the echo area of its workspace's window.
fn show_toast(toast: &Toast, workspace: &Workspace, cx: &mut Context<Workspace>) -> bool {
    if !routes_to_minibuffer(MESSAGES, cx) {
        return false;
    }
    let Some(strip) = workspace
        .bottom_item()
        .and_then(|item| item.downcast::<Minibuffer>().ok())
    else {
        return false;
    };
    let text = SharedString::from(toast.text().to_string());
    strip.update(cx, |strip, cx| {
        strip.say(text, MinibufferMessageKind::Message, cx)
    });
    true
}

/// Lists the messages of the active window in a native session, newest first. `false` when
/// there is no window with an echo area.
pub(crate) fn show_messages(cx: &mut App) -> bool {
    let Some(window) = cx.active_window() else {
        return false;
    };
    window
        .update(cx, |_, window, cx| {
            let Some(workspace) = strip::active_workspace(window, cx) else {
                return false;
            };
            let Some(strip) = minibuffer_for_window(window.window_handle(), cx) else {
                return false;
            };
            let candidates = strip
                .read(cx)
                .messages()
                .into_iter()
                .map(|line| {
                    let candidate = Candidate::text(line.text);
                    match line.kind {
                        MinibufferMessageKind::Message => candidate,
                        MinibufferMessageKind::Error => candidate.kind("error"),
                    }
                })
                .collect();
            let session = Session::completing_read(MESSAGES, "Messages: ")
                .source(crate::session::Source::Fixed(candidates));
            workspace.update(cx, |workspace, cx| {
                open(workspace, session, window, cx);
            });
            true
        })
        .unwrap_or(false)
}
