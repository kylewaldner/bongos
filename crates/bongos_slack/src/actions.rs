//! The `slack` actions, hidden from the command palette while `slack.enabled` is false and shown
//! while it is true. They are unbound in every keymap. `slack: connect` and `slack: disconnect`
//! have handlers (see `token_prompt`); while `slack.enabled` is false they only say Slack is off.

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{Action, App, Context, Window};
use settings::{Settings as _, SettingsStore};
use workspace::Workspace;

use crate::SlackSettings;
use crate::token_prompt::{self, show_toast};

/// Actions in the `slack` namespace.
pub mod slack {
    use gpui::actions;

    actions!(
        slack,
        [
            /// Asks for a Slack token, checks it and stores it.
            Connect,
            /// Removes the stored Slack token.
            Disconnect,
            /// Shows whether Slack is connected, as whom and with which scopes.
            Status,
            /// Reads the latest messages of a channel.
            ReadChannel,
            /// Reads a thread.
            ReadThread,
            /// Loads the messages before the oldest one shown.
            LoadOlderMessages,
            /// Posts a message to a channel, after confirming it.
            Post,
            /// Replies in a thread, after confirming it.
            Reply,
            /// Lets runs and chat agents post to a channel without asking.
            AllowChannel,
            /// Makes runs and chat agents ask again before posting to a channel.
            DisallowChannel,
            /// Shows the posts waiting for an answer.
            ReviewPendingPosts,
        ]
    );
}

/// Every `slack` action, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    vec![
        TypeId::of::<slack::Connect>(),
        TypeId::of::<slack::Disconnect>(),
        TypeId::of::<slack::Status>(),
        TypeId::of::<slack::ReadChannel>(),
        TypeId::of::<slack::ReadThread>(),
        TypeId::of::<slack::LoadOlderMessages>(),
        TypeId::of::<slack::Post>(),
        TypeId::of::<slack::Reply>(),
        TypeId::of::<slack::AllowChannel>(),
        TypeId::of::<slack::DisallowChannel>(),
        TypeId::of::<slack::ReviewPendingPosts>(),
    ]
}

/// Hides the `slack` actions from the command palette while `slack.enabled` is false and shows
/// them while it is true. Call after `command_palette_hooks::init` and [`crate::init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = SlackSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// The toast a command shows while `slack.enabled` is false.
pub(crate) const OFF_MESSAGE: &str = "Slack is off (slack.enabled)";

/// Registers the handler of `A` that runs `handler` while `slack.enabled` is true and shows the
/// off toast while it is false.
fn register_direct<A: Action>(
    workspace: &mut Workspace,
    handler: fn(&mut Workspace, &mut Window, &mut Context<Workspace>),
) {
    workspace.register_action(move |workspace, _: &A, window: &mut Window, cx| {
        if !SlackSettings::get_global(cx).enabled {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
            return;
        }
        handler(workspace, window, cx);
    });
}

/// Registers the handlers of the `slack` actions on `workspace`.
pub fn register(workspace: &mut Workspace) {
    register_direct::<slack::Connect>(workspace, token_prompt::connect);
    register_direct::<slack::Disconnect>(workspace, token_prompt::disconnect);
}
