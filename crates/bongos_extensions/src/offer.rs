//! The offer line: a status bar item that says "<Name> support is available" with Install, Not
//! now and Don't offer again while the active item is an editor on a file of a language the
//! editor does not have but the curated list does. It reads only local state; opening a file
//! never clones, resolves or builds anything.

use std::collections::HashSet;
use std::path::PathBuf;

use bongos_extensions_model::ListEntry;
use editor::Editor;
use fs::Fs;
use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window,
};
use ui::{
    Button, ButtonCommon as _, ButtonSize, Clickable as _, Color, Label, LabelCommon as _,
    LabelSize, h_flex,
};
use workspace::{ItemHandle, StatusItemView};

use crate::installer::Installer;
use crate::{InstallLanguage, LanguageExtensionsSettings};
use settings::Settings as _;

/// The status bar item of the offer line.
#[derive(Default)]
pub struct LanguageOffer {
    editor: Option<Entity<Editor>>,
    /// List entry ids hidden by "Not now" until restart, in this item only.
    dismissed: HashSet<String>,
    _observation: Option<Subscription>,
    _installer_observation: Option<Subscription>,
}

impl LanguageOffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The list entry on offer for the active editor, or `None` when the line is hidden.
    pub fn offered(&self, cx: &mut App) -> Option<ListEntry> {
        let settings = LanguageExtensionsSettings::get_global(cx);
        if !settings.enabled || !settings.offer {
            return None;
        }
        let never_offer = settings.never_offer.clone();
        let editor = self.editor.as_ref()?.read(cx);
        let buffer = editor.buffer().read(cx).as_singleton()?;
        let buffer = buffer.read(cx);
        let path: PathBuf = buffer.file()?.path().as_std_path().to_path_buf();
        if buffer.language().is_some() {
            return None;
        }
        let registry = editor.project()?.read(cx).languages().clone();
        if registry.language_for_file_path(&path).is_some() {
            return None;
        }
        let installer = Installer::global(cx);
        let installer = installer.read(cx);
        let entry = installer.list().entry_for_path(&path)?;
        if never_offer.contains(&entry.id)
            || self.dismissed.contains(&entry.id)
            || installer.is_installed_or_installing(entry, cx)
        {
            return None;
        }
        Some(entry.clone())
    }

    /// The text of the line, or `None` when it is hidden.
    pub fn text(&self, cx: &mut App) -> Option<SharedString> {
        let entry = self.offered(cx)?;
        Some(format!("{} support is available", entry.name).into())
    }

    /// Installs the language through the `InstallLanguage` action.
    pub fn install(&mut self, entry: &ListEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.dismissed.insert(entry.id.clone());
        window.dispatch_action(
            Box::new(InstallLanguage {
                name: Some(entry.name.clone()),
            }),
            cx,
        );
        cx.notify();
    }

    /// Hides the line for the entry until restart.
    pub fn not_now(&mut self, entry: &ListEntry, cx: &mut Context<Self>) {
        self.dismissed.insert(entry.id.clone());
        cx.notify();
    }

    /// Adds the entry id to `language_extensions.never_offer` in the user settings.
    pub fn never_offer(&mut self, entry: &ListEntry, cx: &mut Context<Self>) {
        self.dismissed.insert(entry.id.clone());
        let id = entry.id.clone();
        settings::update_settings_file(<dyn Fs>::global(cx), cx, move |content, _| {
            let never_offer = content
                .extension
                .language_extensions
                .get_or_insert_default()
                .never_offer
                .get_or_insert_default();
            if !never_offer.contains(&id) {
                never_offer.push(id);
            }
        });
        cx.notify();
    }
}

impl Render for LanguageOffer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entry) = self.offered(cx) else {
            return h_flex().hidden().into_any_element();
        };
        let text = format!("{} support is available", entry.name);
        h_flex()
            .gap_1()
            .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
            .child({
                let entry = entry.clone();
                Button::new("language-offer-install", "Install")
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.install(&entry, window, cx);
                    }))
            })
            .child({
                let entry = entry.clone();
                Button::new("language-offer-not-now", "Not now")
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener(move |this, _, _, cx| this.not_now(&entry, cx)))
            })
            .child({
                Button::new("language-offer-never", "Don't offer again")
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener(move |this, _, _, cx| this.never_offer(&entry, cx)))
            })
            .into_any_element()
    }
}

impl StatusItemView for LanguageOffer {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor = active_pane_item.and_then(|item| item.act_as::<Editor>(cx));
        self._observation = self
            .editor
            .as_ref()
            .map(|editor| cx.observe(editor, |_, _, cx| cx.notify()));
        let installer = Installer::global(cx);
        self._installer_observation = Some(cx.observe(&installer, |_, _, cx| cx.notify()));
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // The item appears only while `language_extensions.enabled` and `.offer` are true.
        None
    }
}

#[cfg(test)]
#[path = "offer_tests.rs"]
mod tests;
