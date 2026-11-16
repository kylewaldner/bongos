//! The `settings: set keybindings` command: a completing read over the base keymaps that writes
//! `base_keymap` and nothing else.

use bongos_minibuffer::{Candidate, Outcome, Session, Source};
use gpui::{App, Context, Window};
use settings::{BaseKeymap, BaseKeymapContent, Settings as _, update_settings_file};
use workspace::Workspace;
use zed_actions::settings::SetKeybindings;

/// The name the prompt is routed under (`minibuffer.enabled_for`).
pub(crate) const MINIBUFFER_NAME: &str = "set_keybindings";

/// The candidates, as the text that is narrowed, the label shown after it and the value written.
pub(crate) const CHOICES: [(&str, &str, BaseKeymapContent); 4] = [
    ("emacs", "Emacs", BaseKeymapContent::Emacs),
    ("vim", "Vim", BaseKeymapContent::Vim),
    ("vscode", "VS Code", BaseKeymapContent::VSCode),
    ("None", "None", BaseKeymapContent::None),
];

/// The candidate text of a base keymap.
pub(crate) fn candidate_text(keymap: BaseKeymap) -> &'static str {
    let content: BaseKeymapContent = keymap.into();
    CHOICES
        .iter()
        .find(|(_, _, value)| *value == content)
        .map_or(CHOICES[0].0, |(text, _, _)| text)
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(set_keybindings);
    })
    .detach();
}

fn set_keybindings(
    workspace: &mut Workspace,
    _: &SetKeybindings,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let candidates = CHOICES
        .iter()
        .map(|(text, label, value)| Candidate::new(*text, *value).secondary(*label))
        .collect();
    let current = candidate_text(*BaseKeymap::get_global(cx));
    let fs = workspace.app_state().fs.clone();
    let session = Session::completing_read(MINIBUFFER_NAME, "Keybindings: ")
        .source(Source::Fixed(candidates))
        .default(current)
        .on_confirm(move |outcome, _window, cx| {
            let Outcome::Candidate(value) = outcome else {
                return;
            };
            let Some(content) = value.downcast_ref::<BaseKeymapContent>().copied() else {
                return;
            };
            update_settings_file(fs, cx, move |settings, _| {
                settings.base_keymap = Some(content);
            });
        });
    bongos_minibuffer::open(workspace, session, window, cx);
}
