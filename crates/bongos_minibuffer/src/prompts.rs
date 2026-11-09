//! The small prompts built on the session machinery: a string read, a read-key (one key answers,
//! as `read-char-choice`), the yes/no question that is a read-key over `y` and `n`, and the strict
//! `yes or no` question that is a string read accepting only the words. None of them is opened
//! unless a host asks, and a host that is not routed to the minibuffer (see
//! `minibuffer.enabled_for`) gets them centered, as any other prompt.

use bongos_minibuffer_model::answer::parse_strict;
use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, KeyDownEvent, SharedString,
    WeakEntity, Window, prelude::*,
};
use ui::prelude::*;
use workspace::{DismissDecision, ModalView, Workspace};

use crate::session::{Outcome, Session, open};

/// The history that the strict `yes or no` question keeps its answers in.
const STRICT_HISTORY: &str = "yes-or-no";

impl Session {
    /// A string read: a session with free input and no candidates, so the list stays empty and
    /// says nothing about matches. It shares the keys and the input row of every session, so
    /// RET answers the text typed (or the [default](Session::default) when it is empty) and
    /// M-p / M-n walk the [history](Session::history).
    pub fn string(host: &'static str, prompt: impl Into<SharedString>) -> Self {
        Session::completing_read(host, prompt).free_input(true)
    }
}

/// A prompt that one key answers, as the host declares them: `y n ! q`.
pub struct ReadKey {
    host: &'static str,
    prompt: SharedString,
    keys: Vec<(char, SharedString)>,
    default: Option<char>,
    on_answer: Option<Box<dyn FnOnce(char, &mut Window, &mut App)>>,
    on_cancel: Option<Box<dyn FnOnce(&mut Window, &mut App)>>,
}

impl ReadKey {
    pub fn new(host: &'static str, prompt: impl Into<SharedString>) -> Self {
        Self {
            host,
            prompt: prompt.into(),
            keys: Vec::new(),
            default: None,
            on_answer: None,
            on_cancel: None,
        }
    }

    /// Declares a key that answers, with the label shown after it.
    pub fn key(mut self, key: char, label: impl Into<SharedString>) -> Self {
        self.keys.push((key, label.into()));
        self
    }

    /// The declared key that RET answers, and C-g too (so the prompt never needs a cancel).
    pub fn default(mut self, key: char) -> Self {
        self.default = Some(key);
        self
    }

    /// Runs with the key that answered.
    pub fn on_answer(
        mut self,
        handler: impl FnOnce(char, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_answer = Some(Box::new(handler));
        self
    }

    /// Runs when the prompt ends without an answer: C-g with no default, or the prompt being
    /// replaced or closed.
    pub fn on_cancel(mut self, handler: impl FnOnce(&mut Window, &mut App) + 'static) -> Self {
        self.on_cancel = Some(Box::new(handler));
        self
    }
}

/// The view of a read-key: a focusable line that answers from the keys it was given.
pub struct ReadKeyView {
    host: &'static str,
    prompt: SharedString,
    keys: Vec<(char, SharedString)>,
    default: Option<char>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    done: bool,
    on_answer: Option<Box<dyn FnOnce(char, &mut Window, &mut App)>>,
    on_cancel: Option<Box<dyn FnOnce(&mut Window, &mut App)>>,
}

/// Opens `prompt` as the workspace's modal, replacing the one that is open (which ends as
/// cancelled).
pub fn read_key(
    workspace: &mut Workspace,
    prompt: ReadKey,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let ReadKey {
        host,
        prompt,
        keys,
        default,
        on_answer,
        on_cancel,
    } = prompt;
    let weak = cx.entity().downgrade();
    let mut parts = Some((prompt, keys, on_answer, on_cancel));
    workspace.hide_modal(window, cx);
    workspace.toggle_modal(window, cx, |_, cx| {
        let (prompt, keys, on_answer, on_cancel) = parts.take().expect("built once");
        ReadKeyView {
            host,
            prompt,
            keys,
            default,
            focus_handle: cx.focus_handle(),
            workspace: weak,
            done: false,
            on_answer,
            on_cancel,
        }
    });
}

/// A yes/no question answered by `y` or `n` alone. RET and C-g answer `default`.
pub fn yes_or_no(
    workspace: &mut Workspace,
    host: &'static str,
    prompt: impl Into<SharedString>,
    default: bool,
    on_answer: impl FnOnce(bool, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let prompt = ReadKey::new(host, prompt)
        .key('y', "yes")
        .key('n', "no")
        .default(if default { 'y' } else { 'n' })
        .on_answer(move |key, window, cx| on_answer(key == 'y', window, cx));
    read_key(workspace, prompt, window, cx);
}

/// The strict `yes or no` question: a string read that only the whole words `yes` and `no`
/// answer, so `y` and an empty answer leave it open. The answer is `None` when it is cancelled.
pub fn strict_yes_or_no(
    workspace: &mut Workspace,
    host: &'static str,
    prompt: impl Into<SharedString>,
    on_answer: impl FnOnce(Option<bool>, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let on_answer = std::rc::Rc::new(std::cell::RefCell::new(Some(on_answer)));
    let cancelled = on_answer.clone();
    let session = Session::string(host, prompt)
        .history(STRICT_HISTORY)
        .accept(
            |input| parse_strict(input).is_some(),
            "Please answer yes or no.",
        )
        .on_confirm(move |outcome, window, cx| {
            let answer = match outcome {
                Outcome::Input(input) => parse_strict(&input),
                Outcome::Candidate(_) => None,
            };
            if let Some(on_answer) = on_answer.borrow_mut().take() {
                on_answer(answer, window, cx);
            }
        })
        .on_cancel(move |window, cx| {
            if let Some(on_answer) = cancelled.borrow_mut().take() {
                on_answer(None, window, cx);
            }
        });
    open(workspace, session, window, cx);
}

impl ReadKeyView {
    /// The declared keys as the line shows them, `y n ! q`.
    #[cfg(test)]
    pub(crate) fn legend(&self) -> String {
        let keys: Vec<String> = self.keys.iter().map(|(key, _)| key.to_string()).collect();
        keys.join(" ")
    }

    /// Ends the prompt, once: with the key that answered, or without one. The handlers run
    /// deferred, after the prompt has closed and focus is back where it was.
    fn finish(
        &mut self,
        answer: Option<char>,
        hide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let on_answer = self.on_answer.take();
        let on_cancel = self.on_cancel.take();
        let workspace = self.workspace.clone();
        let this = cx.entity();
        window.defer(cx, move |window, cx| {
            if hide {
                workspace
                    .update(cx, |workspace, cx| {
                        if workspace.active_modal::<Self>(cx) == Some(this) {
                            workspace.hide_modal(window, cx);
                        }
                    })
                    .ok();
            }
            match (answer, on_answer, on_cancel) {
                (Some(key), Some(on_answer), _) => on_answer(key, window, cx),
                (None, _, Some(on_cancel)) => on_cancel(window, cx),
                _ => {}
            }
        });
    }

    /// RET and C-g answer the default when there is one; without one C-g cancels.
    fn answer_default(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.default {
            Some(key) => {
                self.finish(Some(key), true, window, cx);
                true
            }
            None => false,
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if (modifiers.control && keystroke.key == "g") || keystroke.key == "escape" {
            if !self.answer_default(window, cx) {
                self.finish(None, true, window, cx);
            }
        } else if modifiers.number_of_modifiers() == 0 && keystroke.key == "enter" {
            self.answer_default(window, cx);
        } else if !modifiers.control && !modifiers.alt && !modifiers.platform && !modifiers.function
        {
            let typed = keystroke.key_char.as_deref().unwrap_or(&keystroke.key);
            let mut chars = typed.chars();
            let key = chars.next().filter(|_| chars.next().is_none());
            let declared = key.filter(|key| self.keys.iter().any(|(declared, _)| declared == key));
            let Some(key) = declared else {
                // An undeclared key is not an answer, and does not reach the editor behind.
                cx.stop_propagation();
                return;
            };
            self.finish(Some(key), true, window, cx);
        } else {
            return;
        }
        cx.stop_propagation();
    }
}

impl EventEmitter<DismissEvent> for ReadKeyView {}

impl Focusable for ReadKeyView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for ReadKeyView {
    fn minibuffer_name(&self) -> Option<&'static str> {
        Some(self.host)
    }

    /// Always lets the prompt go. C-g bound to `minibuffer::Cancel` lands here, so it answers the
    /// default like the key does; with none, the prompt ends as cancelled.
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.finish(self.default, false, window, cx);
        DismissDecision::Dismiss(true)
    }
}

impl Render for ReadKeyView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let routed = workspace::routes_to_minibuffer(self.host, cx);
        h_flex()
            .id("minibuffer-read-key")
            .key_context("ReadKey")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)))
            .gap_2()
            .px_2()
            .when(!routed, |this| this.py_1().min_w(rems(24.)))
            .child(Label::new(self.prompt.clone()))
            .children(self.keys.iter().map(|(key, label)| {
                h_flex()
                    .gap_1()
                    .child(Label::new(key.to_string()).color(Color::Accent))
                    .child(Label::new(label.clone()).color(Color::Muted))
            }))
    }
}
