//! The `chat` actions, and hiding them from the command palette while `chats.enabled` is false.
//! The actions carry no data: each opens the pickers and prompts it needs. Their handlers come
//! with the panel and the runtime; until then every one is hidden.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
use gpui::App;
use settings::{Settings as _, SettingsStore};

use crate::ChatsSettings;

/// Actions in the `chat` namespace.
pub mod chat {
    use gpui::actions;

    actions!(
        chat,
        [
            /// Starts a chat on the focused task, asking for the session type and agent.
            New,
            /// Starts a chat, asking also for its name, model and layout.
            NewWithOptions,
            /// Starts a plan chat on the focused task.
            NewPlan,
            /// Starts an implement chat on the focused task.
            NewImplement,
            /// Starts a review chat on the focused task.
            NewReview,
            /// Starts a thinking chat on the focused task.
            NewThinking,
            /// Starts a free chat on the focused task.
            NewFree,
            /// Shows or hides the Chats panel.
            TogglePanel,
            /// Picks an open chat of the focused task and shows it, resuming it if it is closed.
            Open,
            /// Picks an open chat of any task and shows it.
            OpenAny,
            /// Shows the next open chat of the task.
            Next,
            /// Shows the previous open chat of the task.
            Previous,
            /// Ends the chat's process and keeps it in the open list.
            Close,
            /// Moves the chat to the archive.
            Archive,
            /// Picks an archived chat and moves it back to the open list.
            Restore,
            /// Lists the archived chats of the task.
            ShowArchive,
            /// Renames the chat.
            Rename,
            /// Deletes the chat's record. Asks first.
            Delete,
            /// Forks the chat into a new chat on the same task.
            Fork,
            /// Forks the chat into a new chat on another task.
            ForkToTask,
            /// Forks the chat onto another task, asking for the session type.
            ForkToTaskAs,
            /// Shows or hides the prompts pane of the chat.
            TogglePromptEditor,
            /// Focuses the chat's terminal.
            FocusTerminal,
            /// Focuses the chat's prompts pane.
            FocusPrompts,
            /// Shows the artifacts the chat was given.
            ShowArtifacts,
            /// Picks a session type and opens its files.
            EditSessionType,
            /// Sends the selection, or else the whole buffer, of a chat's prompts file to the chat.
            SendRegion,
            /// Filters the chat list.
            Filter,
            /// Switches the chat list between the focused task and all tasks.
            ToggleAllTasks,
        ]
    );

    #[cfg(debug_assertions)]
    actions!(
        chat,
        [
            /// Debug builds only: writes the text of the view the Chats panel shows to
            /// `chat-view.txt` in the logs directory.
            DumpView,
        ]
    );
}

/// Every `chat` action, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    #[allow(unused_mut)]
    let mut types = vec![
        TypeId::of::<chat::New>(),
        TypeId::of::<chat::NewWithOptions>(),
        TypeId::of::<chat::NewPlan>(),
        TypeId::of::<chat::NewImplement>(),
        TypeId::of::<chat::NewReview>(),
        TypeId::of::<chat::NewThinking>(),
        TypeId::of::<chat::NewFree>(),
        TypeId::of::<chat::TogglePanel>(),
        TypeId::of::<chat::Open>(),
        TypeId::of::<chat::OpenAny>(),
        TypeId::of::<chat::Next>(),
        TypeId::of::<chat::Previous>(),
        TypeId::of::<chat::Close>(),
        TypeId::of::<chat::Archive>(),
        TypeId::of::<chat::Restore>(),
        TypeId::of::<chat::ShowArchive>(),
        TypeId::of::<chat::Rename>(),
        TypeId::of::<chat::Delete>(),
        TypeId::of::<chat::Fork>(),
        TypeId::of::<chat::ForkToTask>(),
        TypeId::of::<chat::ForkToTaskAs>(),
        TypeId::of::<chat::TogglePromptEditor>(),
        TypeId::of::<chat::FocusTerminal>(),
        TypeId::of::<chat::FocusPrompts>(),
        TypeId::of::<chat::ShowArtifacts>(),
        TypeId::of::<chat::EditSessionType>(),
        TypeId::of::<chat::SendRegion>(),
        TypeId::of::<chat::Filter>(),
        TypeId::of::<chat::ToggleAllTasks>(),
    ];
    #[cfg(debug_assertions)]
    types.push(TypeId::of::<chat::DumpView>());
    types
}

/// Hides the `chat` actions from the command palette while `chats.enabled` is false and shows
/// them while it is true. Call after `command_palette_hooks::init`.
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = ChatsSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}
