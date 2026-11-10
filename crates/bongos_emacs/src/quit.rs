//! The C-g hooks: later features register what a quit must also cancel (a pending prefix,
//! jump labels). `keyboard-quit` runs them once per keystroke.

use std::rc::Rc;

use gpui::{App, Global, Window};

type Hook = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Default)]
struct QuitHooks(Vec<Hook>);

impl Global for QuitHooks {}

/// Set while one keystroke's quit handlers run, so the hooks run once even when several
/// bindings of the key reach a handler.
#[derive(Default)]
struct QuitState {
    hooks_ran: bool,
}

impl Global for QuitState {}

/// Adds a hook that every C-g runs before cancelling anything else.
pub fn register(cx: &mut App, hook: impl Fn(&mut Window, &mut App) + 'static) {
    cx.default_global::<QuitHooks>().0.push(Rc::new(hook));
}

/// Runs every registered hook unless they already ran for this keystroke (or this dispatch).
/// The guard clears once the current update finishes, before the next keystroke.
pub fn run_hooks_once(window: &mut Window, cx: &mut App) {
    if cx.default_global::<QuitState>().hooks_ran {
        return;
    }
    cx.default_global::<QuitState>().hooks_ran = true;
    cx.defer(|cx| cx.default_global::<QuitState>().hooks_ran = false);
    let hooks = cx
        .try_global::<QuitHooks>()
        .map(|hooks| hooks.0.clone())
        .unwrap_or_default();
    for hook in hooks {
        hook(window, cx);
    }
}
