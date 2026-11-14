//! The decision view of an interactive replace: one line in the minibuffer strip with the
//! question about the current match and the counter after it. `?` shows the list of keys above the
//! line until the next key. The view is a modal that the session keeps focused; the keys are the
//! `QueryReplace` rows of the keymap, which the session's workspace actions handle.

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, KeyDownEvent, SharedString,
    Window, prelude::*,
};
use ui::prelude::*;
use workspace::{DismissDecision, ModalView};

/// The name the view is routed by (`minibuffer.enabled_for`), the host of the prompts too.
pub const HOST: &str = crate::prompts::HOST;
/// The key context of the view.
pub const KEY_CONTEXT: &str = "QueryReplace";

type OnDismiss = Box<dyn FnOnce(&mut Window, &mut App)>;

pub struct QueryReplaceView {
    focus_handle: FocusHandle,
    prompt: SharedString,
    status: SharedString,
    help: Vec<String>,
    on_dismiss: Option<OnDismiss>,
}

impl QueryReplaceView {
    pub(crate) fn new(
        prompt: impl Into<SharedString>,
        on_dismiss: OnDismiss,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            prompt: prompt.into(),
            status: SharedString::default(),
            help: Vec::new(),
            on_dismiss: Some(on_dismiss),
        }
    }

    /// The question on the line.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// The counter, or `…` while the next match is being found.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The help lines on show, empty when none.
    pub fn help_lines(&self) -> &[String] {
        &self.help
    }

    pub(crate) fn set_prompt(&mut self, prompt: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.prompt = prompt.into();
        cx.notify();
    }

    pub(crate) fn set_status(&mut self, status: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = status.into();
        cx.notify();
    }

    pub(crate) fn set_help(&mut self, help: Vec<String>, cx: &mut Context<Self>) {
        self.help = help;
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // A key no row binds does nothing here and does not reach the editor behind. Keys with
        // modifiers go on.
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        if !self.help.is_empty() {
            self.help.clear();
            cx.notify();
        }
        cx.stop_propagation();
    }
}

impl EventEmitter<DismissEvent> for QueryReplaceView {}

impl Focusable for QueryReplaceView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for QueryReplaceView {
    fn minibuffer_name(&self) -> Option<&'static str> {
        Some(HOST)
    }

    /// Always lets the view go; the session ends as `Exit`.
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        if let Some(on_dismiss) = self.on_dismiss.take() {
            on_dismiss(window, cx);
        }
        DismissDecision::Dismiss(true)
    }
}

impl Render for QueryReplaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let routed = workspace::routes_to_minibuffer(HOST, cx);
        v_flex()
            .id("query-replace-view")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)))
            .px_2()
            .when(!routed, |this| this.py_1().min_w(rems(24.)))
            .children(
                self.help
                    .iter()
                    .map(|line| Label::new(line.clone()).single_line().buffer_font(cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(Label::new(self.prompt.clone()).single_line())
                    .child(Label::new(self.status.clone()).color(Color::Muted)),
            )
    }
}
