//! The minibuffer: a strip at the bottom of the window that hosts prompts, as in Emacs. It
//! installs the `minibuffer` settings, the routing global that `workspace` and `picker` read,
//! the strip (one line below the status bar of every workspace, while `minibuffer.enabled_for`
//! is not empty) and the `minibuffer` actions, hidden while `enabled_for` is empty. `Cancel`
//! hides a prompt in the strip and `Focus` focuses it; with no prompt, every action says "No
//! active minibuffer".

mod history_store;
mod session;
mod session_view;
mod settings;
mod strip;

use ::settings::{Settings as _, SettingsStore};
use command_palette_hooks::CommandPaletteFilter;
use gpui::{App, BorrowAppContext as _, Context, Window};
use workspace::{MinibufferMessageKind, MinibufferRouting, ModalLayer};

pub use history_store::{HISTORY_NAMESPACE, HistoryStore};
pub use session::{
    Candidate, DynamicSource, Outcome, Preview, Session, SessionHandle, Source, Value, open,
};
pub use session_view::SessionView;
pub use settings::MinibufferSettings;
pub use strip::{Minibuffer, minibuffer_for_window};

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
    strip::init(cx);
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
    strip::sync_windows(cx);
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

/// Runs `act` on the modal layer of the active window's workspace, which is `true` when it found
/// a prompt in the strip to act on.
fn act_on_session(
    cx: &mut App,
    act: impl FnOnce(&mut ModalLayer, &mut Window, &mut Context<ModalLayer>) -> bool,
) -> bool {
    let Some(window) = cx.active_window() else {
        return false;
    };
    window
        .update(cx, |_, window, cx| {
            let Some(workspace) = strip::active_workspace(window, cx) else {
                return false;
            };
            let layer = workspace.read(cx).modal_layer().clone();
            layer.update(cx, |layer, cx| act(layer, window, cx))
        })
        .unwrap_or(false)
}

fn register_actions(cx: &mut App) {
    use zed_actions::minibuffer::*;

    // Global handlers run while the window is being updated, so each one defers its work until
    // the window can be updated again.
    macro_rules! register {
        ($($action:ty => $handle:expr),* $(,)?) => {
            $(cx.on_action(|_: &$action, cx| {
                cx.defer(|cx| {
                    let handled: fn(&mut App) -> bool = $handle;
                    if !handled(cx) {
                        no_active_minibuffer(cx);
                    }
                });
            });)*
        };
    }
    register!(
        Cancel => |cx| act_on_session(cx, |layer, window, cx| layer.hide_bottom_modal(window, cx)),
        Focus => |cx| act_on_session(cx, |layer, window, cx| layer.focus_bottom_modal(window, cx)),
        Confirm => |_| false,
        ConfirmInput => |_| false,
        SelectNext => |_| false,
        SelectPrev => |_| false,
        PageDown => |_| false,
        PageUp => |_| false,
        CompleteCommonPrefix => |_| false,
        HistoryPrev => |_| false,
        HistoryNext => |_| false,
        Actions => |_| false,
        Open => |_| false,
        ShowMessages => |_| false,
    );
}

#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod strip_tests;
