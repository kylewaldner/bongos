//! The mode line (design D11): "prompt 7/12 · pending · auto-submit on" (or "sent 14:02" for a
//! sent block). The center shows it as a status bar item while the active item is an editor with
//! the mode; the Chats panel's session header shows it through the pane provider.

use editor::Editor;
use gpui::{App, Context, Entity, IntoElement, Render, SharedString, Subscription, Window, div};
use settings::Settings as _;
use ui::{Label, LabelSize, prelude::*};
use workspace::{ItemHandle, StatusItemView};

use crate::PromptsSettings;
use crate::mode::{PromptsMode, mode_of};

impl PromptsMode {
    /// The mode line text. Auto-submit is the global value until a chat is attached.
    pub fn mode_line(&self, cx: &App) -> String {
        let total = self.index.block_count();
        let (number, state) = match &self.current {
            Some(block) => (
                block.index + 1,
                match block.marker.and_then(|marker| marker.sent) {
                    Some(sent) => format!("sent {}", sent.format("%H:%M")),
                    None => "pending".to_string(),
                },
            ),
            None => (1, "pending".to_string()),
        };
        let auto_submit = if PromptsSettings::get_global(cx).auto_submit {
            "on"
        } else {
            "off"
        };
        format!("prompt {number}/{total} · {state} · auto-submit {auto_submit}")
    }
}

/// The mode line of `editor`, or `None` when it does not have the mode.
pub fn mode_line(editor: &Editor, cx: &App) -> Option<SharedString> {
    let mode = mode_of(editor)?;
    let text = mode.borrow().mode_line(cx);
    Some(text.into())
}

/// The status bar item of the center: empty unless the active item is an editor with the mode.
#[derive(Default)]
pub struct PromptsModeLine {
    editor: Option<Entity<Editor>>,
    _observation: Option<Subscription>,
}

impl PromptsModeLine {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the item shows, or `None` when it shows nothing.
    pub fn text(&self, cx: &App) -> Option<SharedString> {
        mode_line(self.editor.as_ref()?.read(cx), cx)
    }
}

impl Render for PromptsModeLine {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(text) = self.text(cx) else {
            return div().hidden().into_any_element();
        };
        div()
            .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
            .into_any_element()
    }
}

impl StatusItemView for PromptsModeLine {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor = active_pane_item.and_then(|item| item.act_as::<Editor>(cx));
        // The editor notifies on every selection change and when the mode attaches or detaches.
        self._observation = self
            .editor
            .as_ref()
            .map(|editor| cx.observe(editor, |_, _, cx| cx.notify()));
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // The item appears only while `prompts.enabled` is true and a prompts file is active.
        None
    }
}
