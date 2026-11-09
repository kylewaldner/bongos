//! The item of the Chats panel's prompts pane (design D22). Installed once at init and kept for
//! the process; it reads `prompts.enabled` on each call. The editor it builds gets the mode the way
//! every editor does (it attaches only while the setting is on), so while off the pane holds a
//! plain editor with no addon, and the header shows no mode line.

use bongos_chat::PromptPaneProvider;
use editor::Editor;
use gpui::{App, AppContext as _, Entity, SharedString, Window};
use language::Buffer;
use project::Project;
use settings::Settings as _;
use workspace::ItemHandle;

use crate::{PromptsSettings, mode_line};

/// The prompts pane provider of the prompts mode.
pub struct PromptsPaneProvider;

impl PromptPaneProvider for PromptsPaneProvider {
    fn build_item(
        &self,
        buffer: Entity<Buffer>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Box<dyn ItemHandle> {
        Box::new(cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx)))
    }

    fn mode_line(&self, item: &dyn ItemHandle, cx: &App) -> Option<SharedString> {
        if !PromptsSettings::get_global(cx).enabled {
            return None;
        }
        let editor = item.act_as::<Editor>(cx)?;
        mode_line::mode_line(editor.read(cx), cx)
    }
}

/// Installs the provider, whatever `prompts.enabled` says.
pub(crate) fn install(cx: &mut App) {
    bongos_chat::set_prompt_pane_provider(PromptsPaneProvider, cx);
}
