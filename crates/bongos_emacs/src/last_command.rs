//! The last command run in each window, so a command can tell whether it repeats (recenter
//! cycles, consecutive kills join).

use collections::HashMap;
use gpui::{App, Global, Window, WindowId};

#[derive(Default)]
struct LastCommand(HashMap<WindowId, Option<&'static str>>);

impl Global for LastCommand {}

/// Records the action of every keystroke (none for a key without one). The observer runs after
/// the action, so a handler still sees the previous command.
pub(crate) fn init(cx: &mut App) {
    cx.observe_keystrokes(|event, window, cx| {
        record(
            window,
            event.action.as_ref().map(|action| action.name()),
            cx,
        );
    })
    .detach();
}

/// Stores `command` as the window's last command. Each `emacs` handler records itself, so a
/// dispatch by name (palette, tests) counts too.
pub fn record(window: &Window, command: Option<&'static str>, cx: &mut App) {
    let id = window.window_handle().window_id();
    cx.default_global::<LastCommand>().0.insert(id, command);
}

/// The action name of the window's last command, or `None` when the last key had no action or
/// nothing ran yet.
pub fn last_command(window: &Window, cx: &App) -> Option<&'static str> {
    let id = window.window_handle().window_id();
    cx.try_global::<LastCommand>()
        .and_then(|last| last.0.get(&id).copied().flatten())
}
