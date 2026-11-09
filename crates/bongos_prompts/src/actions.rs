//! The `prompts` actions, hidden from the command palette while `prompts.enabled` is false, and
//! their handlers. Until the later steps give them behavior, each handler only says that the
//! prompts editor is off. No key is bound to any of them.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{Action, App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Workspace;
use workspace::notifications::NotificationId;
use workspace::Toast;

use crate::PromptsSettings;

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
            /// Moves to the previous prompt.
            Previous,
            /// Moves to the next prompt.
            Next,
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
pub(crate) fn off_message(command: &str) -> String {
    format!("Prompts are off (prompts.enabled): {command}")
}

pub(crate) fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<PromptsToast>(), message),
        cx,
    );
}

fn register_one<A: Action>(workspace: &mut Workspace, command: &'static str) {
    workspace.register_action(move |workspace, _: &A, _window: &mut Window, cx| {
        if !PromptsSettings::get_global(cx).enabled {
            show_toast(workspace, off_message(command), cx);
        }
    });
}

/// Registers the handlers of the `prompts` actions on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_one::<prompts::Send>(workspace, "prompts: send");
    register_one::<prompts::SendWithoutMarking>(workspace, "prompts: send without marking");
    register_one::<prompts::SendRegion>(workspace, "prompts: send region");
    register_one::<prompts::New>(workspace, "prompts: new");
    register_one::<prompts::Previous>(workspace, "prompts: previous");
    register_one::<prompts::Next>(workspace, "prompts: next");
    register_one::<prompts::NextPending>(workspace, "prompts: next pending");
    register_one::<prompts::Copy>(workspace, "prompts: copy");
    register_one::<prompts::UseFile>(workspace, "prompts: use file");
    register_one::<prompts::UseDefault>(workspace, "prompts: use default");
    register_one::<prompts::ToggleAutoSubmit>(workspace, "prompts: toggle auto-submit");
    register_one::<prompts::Open>(workspace, "prompts: open");
    register_one::<prompts::Mode>(workspace, "prompts: mode");
    #[cfg(debug_assertions)]
    register_one::<prompts::DumpView>(workspace, "prompts: dump view");
}
