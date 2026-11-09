//! The `prompts` actions, hidden from the command palette while `prompts.enabled` is false, and
//! their handlers. While it is false each handler only says that the prompts mode is off. The
//! navigation, new and copy commands live in [`crate::commands`]; until the later steps give them
//! behavior, the others do nothing while on. No key is bound to any of them.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{Action, App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Toast;
use workspace::Workspace;
use workspace::notifications::NotificationId;

use editor::Editor;

use crate::{LANGUAGE_NAME, PromptsSettings, commands};

/// Actions in the `prompts` namespace.
pub mod prompts {
    use gpui::actions;

    actions!(
        prompts,
        [
            /// Sends the prompt at point to the chat.
            Send,
            /// Sends the prompt at point without marking it as sent.
            SendWithoutMarking,
            /// Sends the selection, or else the whole buffer, to the chat.
            SendRegion,
            /// Adds a new prompt.
            New,
            /// Moves to the next prompt that was not sent.
            NextPending,
            /// Copies the prompt at point.
            Copy,
            /// Picks the file the chat reads its prompts from.
            UseFile,
            /// Goes back to the default prompts file of the chat.
            UseDefault,
            /// Turns the automatic submit of the chat on or off.
            ToggleAutoSubmit,
            /// Opens the prompts of the chat.
            Open,
            /// Turns the prompts mode on for the current file.
            Mode,
        ]
    );

    /// Moves to the previous prompt, or back by `count` prompts.
    #[derive(PartialEq, Clone, Default, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
    #[action(namespace = prompts)]
    #[serde(deny_unknown_fields)]
    pub struct Previous {
        /// How many prompts to move; 1 when absent. Feature 09's prefix argument fills it.
        #[serde(default)]
        pub count: Option<u32>,
    }

    /// Moves to the next prompt, or forward by `count` prompts.
    #[derive(PartialEq, Clone, Default, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
    #[action(namespace = prompts)]
    #[serde(deny_unknown_fields)]
    pub struct Next {
        /// How many prompts to move; 1 when absent. Feature 09's prefix argument fills it.
        #[serde(default)]
        pub count: Option<u32>,
    }

    #[cfg(debug_assertions)]
    actions!(
        prompts,
        [
            /// Debug builds only: writes the text of the prompts view to `prompts-view.txt` in
            /// the logs directory.
            DumpView,
        ]
    );
}

/// Every `prompts` action, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    #[allow(unused_mut)]
    let mut types = vec![
        TypeId::of::<prompts::Send>(),
        TypeId::of::<prompts::SendWithoutMarking>(),
        TypeId::of::<prompts::SendRegion>(),
        TypeId::of::<prompts::New>(),
        TypeId::of::<prompts::Previous>(),
        TypeId::of::<prompts::Next>(),
        TypeId::of::<prompts::NextPending>(),
        TypeId::of::<prompts::Copy>(),
        TypeId::of::<prompts::UseFile>(),
        TypeId::of::<prompts::UseDefault>(),
        TypeId::of::<prompts::ToggleAutoSubmit>(),
        TypeId::of::<prompts::Open>(),
        TypeId::of::<prompts::Mode>(),
    ];
    #[cfg(debug_assertions)]
    types.push(TypeId::of::<prompts::DumpView>());
    types
}

/// Hides the `prompts` actions from the command palette while `prompts.enabled` is false and
/// shows them while it is true. Call after `command_palette_hooks::init` and [`crate::init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = PromptsSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// A toast of the prompts commands.
pub struct PromptsToast;

/// The toast a command shows while `prompts.enabled` is false.
pub(crate) const OFF_MESSAGE: &str = "Prompts mode is off (prompts.enabled)";

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<PromptsToast>(), message),
        cx,
    );
}

/// Registers `handler` for `A`, behind the off toast while `prompts.enabled` is false.
fn register_gated<A: Action>(
    workspace: &mut Workspace,
    handler: fn(&mut Workspace, &mut Window, &mut Context<Workspace>),
) {
    workspace.register_action(move |workspace, _: &A, window: &mut Window, cx| {
        if PromptsSettings::get_global(cx).enabled {
            handler(workspace, window, cx);
        } else {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
        }
    });
}

/// Registers `handler` for `A`, which reads the action, behind the off toast.
fn register_gated_with<A: Action>(
    workspace: &mut Workspace,
    handler: fn(&mut Workspace, &A, &mut Window, &mut Context<Workspace>),
) {
    workspace.register_action(move |workspace, action: &A, window: &mut Window, cx| {
        if PromptsSettings::get_global(cx).enabled {
            handler(workspace, action, window, cx);
        } else {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
        }
    });
}

/// Registers an action that later steps give behavior to: only the off toast for now.
fn register_one<A: Action>(workspace: &mut Workspace) {
    register_gated::<A>(workspace, |_, _, _| {});
}

/// Registers the handlers of the `prompts` actions on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_one::<prompts::Send>(workspace);
    register_one::<prompts::SendWithoutMarking>(workspace);
    register_one::<prompts::SendRegion>(workspace);
    register_gated::<prompts::New>(workspace, commands::new);
    register_gated_with::<prompts::Previous>(workspace, commands::previous);
    register_gated_with::<prompts::Next>(workspace, commands::next);
    register_gated::<prompts::NextPending>(workspace, commands::next_pending);
    register_gated::<prompts::Copy>(workspace, commands::copy);
    register_one::<prompts::UseFile>(workspace);
    register_one::<prompts::UseDefault>(workspace);
    register_one::<prompts::ToggleAutoSubmit>(workspace);
    register_one::<prompts::Open>(workspace);
    register_gated::<prompts::Mode>(workspace, prompts_mode);
    #[cfg(debug_assertions)]
    register_gated::<prompts::DumpView>(workspace, dump_view);
}

/// `prompts: mode`: gives the active file the `Prompts` language, the call the language selector
/// makes, so the mode attaches and holds while the buffer lives.
fn prompts_mode(workspace: &mut Workspace, _window: &mut Window, cx: &mut Context<Workspace>) {
    let buffer = workspace
        .active_item_as::<Editor>(cx)
        .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
    let Some(buffer) = buffer else {
        show_toast(workspace, "No file to put in prompts mode".to_string(), cx);
        return;
    };
    let project = workspace.project().clone();
    let languages = project.read(cx).languages().clone();
    cx.spawn(async move |workspace, cx| {
        let language = languages.language_for_name(LANGUAGE_NAME).await;
        match language {
            Ok(language) => project.update(cx, |project, cx| {
                project.set_language_for_buffer(&buffer, language, cx)
            }),
            Err(error) => {
                log::warn!("prompts: no Prompts language: {error}");
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, "The Prompts language is missing".to_string(), cx)
                    })
                    .ok();
            }
        }
    })
    .detach();
}

/// `prompts: dump view` (debug builds): writes the state of the prompts editor focused last, else
/// of the active one, to `prompts-view.txt` in the logs directory.
#[cfg(debug_assertions)]
fn dump_view(workspace: &mut Workspace, _window: &mut Window, cx: &mut Context<Workspace>) {
    let editor = crate::mode::last_focused(cx).or_else(|| {
        workspace
            .active_item_as::<Editor>(cx)
            .filter(|editor| crate::mode::is_attached(editor.read(cx)))
    });
    let Some(text) = editor.and_then(|editor| crate::dump::dump_text(&editor, cx)) else {
        show_toast(workspace, "No prompts view to dump".to_string(), cx);
        return;
    };
    let path = crate::dump::dump_path();
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, text));
    let message = match written {
        Ok(()) => format!("Wrote {}", path.display()),
        Err(error) => format!("Could not write {}: {error}", path.display()),
    };
    show_toast(workspace, message, cx);
}
