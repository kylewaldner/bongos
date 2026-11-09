//! The minibuffer: a strip at the bottom of the window that hosts prompts, as in Emacs. This
//! step installs the `minibuffer` settings, the routing global that `workspace` and `picker`
//! read, and the `minibuffer` actions, hidden while `minibuffer.enabled_for` is empty. With no
//! session, every action says "No active minibuffer".

mod settings;

use ::settings::{Settings as _, SettingsStore};
use command_palette_hooks::CommandPaletteFilter;
use gpui::{App, BorrowAppContext as _};
use workspace::{MinibufferMessageKind, MinibufferRouting};

pub use settings::MinibufferSettings;

/// The namespace of the minibuffer actions, hidden while `minibuffer.enabled_for` is empty.
pub const NAMESPACE: &str = "minibuffer";

/// What the actions answer while no session is open.
pub const NO_ACTIVE_MINIBUFFER: &str = "No active minibuffer";

/// Registers the `minibuffer` settings, installs the [`MinibufferRouting`] global and keeps it
/// in step with the settings, hides the `minibuffer` actions while `enabled_for` is empty, and
/// registers the global action handlers. Call after `command_palette::init` (which sets up the
/// command palette filter).
pub fn init(cx: &mut App) {
    MinibufferSettings::register(cx);
    cx.set_global(MinibufferRouting::default());
    sync(cx);
    cx.observe_global::<SettingsStore>(sync).detach();
    register_actions(cx);
}

/// Mirrors the settings into the routing global and shows or hides the action namespace.
fn sync(cx: &mut App) {
    let settings = MinibufferSettings::get_global(cx).clone();
    let enabled = !settings.enabled_for.is_empty();
    cx.update_global::<MinibufferRouting, _>(|routing, _| {
        routing.set_routes(settings.enabled_for);
        routing.set_display(settings.max_rows, settings.show_count);
    });
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_namespace(NAMESPACE);
        } else {
            filter.hide_namespace(NAMESPACE);
        }
    });
}

/// Says there is no session, in the echo area of the active window or else in the log.
fn no_active_minibuffer(cx: &mut App) {
    let Some(window) = cx.active_window() else {
        MinibufferRouting::log_message(NO_ACTIVE_MINIBUFFER, MinibufferMessageKind::Message);
        return;
    };
    window
        .update(cx, |_, window, cx| {
            MinibufferRouting::message(
                window,
                cx,
                NO_ACTIVE_MINIBUFFER,
                MinibufferMessageKind::Message,
            );
        })
        .ok();
}

fn register_actions(cx: &mut App) {
    use zed_actions::minibuffer::*;

    macro_rules! register {
        ($($action:ty),* $(,)?) => {
            $(cx.on_action(|_: &$action, cx| no_active_minibuffer(cx));)*
        };
    }
    register!(
        Cancel,
        Confirm,
        ConfirmInput,
        SelectNext,
        SelectPrev,
        PageDown,
        PageUp,
        CompleteCommonPrefix,
        HistoryPrev,
        HistoryNext,
        Actions,
        Open,
        Focus,
        ShowMessages,
    );
}

#[cfg(test)]
mod settings_tests;
