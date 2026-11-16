//! The prefix argument of each window: the argument being built, the handoff to the command now
//! running, and the registry of what each action does with it. The pure model is
//! `bongos_emacs_model::prefix_arg`.

use bongos_emacs_model::prefix_arg::PrefixArg;
use collections::HashMap;
use editor::Editor;
use gpui::{Action, App, Context, Entity, Focusable as _, Global, WeakEntity, Window, WindowId};

/// The most an inserting command repeats: a huge count would hang the editor.
pub const MAX_INSERT: i32 = 65_536;

/// What an action does with a pending prefix argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Meaning {
    /// The action's handler takes the argument: it is handed off and the key goes through.
    Reads,
    /// The action has no use for the argument: it is cleared and the command runs once.
    Ignored,
    /// The key is stopped and the adapter is called once with the argument.
    Adapter(fn(PrefixArg, &Entity<Editor>, &mut Window, &mut App)),
    /// The key is stopped and the action is rebuilt by name with `{"count": N}`.
    CountField,
    /// The key is stopped and the named action runs once instead.
    Redirect(&'static str),
}

#[derive(Default)]
struct Meanings(HashMap<String, Meaning>);

impl Global for Meanings {}

/// Declares what the action called `name` (`Action::name_for_type`) does with an argument. A name
/// declared twice keeps the last declaration, and a different one is logged.
pub fn declare(cx: &mut App, name: &str, meaning: Meaning) {
    let previous = cx
        .default_global::<Meanings>()
        .0
        .insert(name.to_string(), meaning);
    if let Some(previous) = previous
        && previous != meaning
    {
        log::warn!("prefix argument meaning of {name} was declared twice; keeping the last");
    }
}

/// What `name` does with an argument, or `None` when nothing declared it.
pub fn meaning(cx: &App, name: &str) -> Option<Meaning> {
    cx.try_global::<Meanings>()
        .and_then(|meanings| meanings.0.get(name).copied())
}

/// An action that can carry a prefix argument as its count.
pub trait CountArg: Action + Clone {
    /// Whether the action has a `count` field that reads the argument.
    const READS: bool = false;
    /// Whether the count is not bounded by the text, so it is clamped to `MAX_INSERT`.
    const INSERTS: bool = false;

    fn count(&self) -> Option<i32> {
        None
    }

    fn with_count(&self, _count: i32) -> Option<Self> {
        None
    }
}

/// The action with the handed-off argument as its count, or `None` when there is no argument or
/// the action already has an explicit count (which wins).
pub(crate) fn counted<A: CountArg>(action: &A, handed: Option<PrefixArg>) -> Option<A> {
    let arg = handed?;
    if action.count().is_some() {
        return None;
    }
    let value = arg.value();
    let value = if A::INSERTS {
        value.min(MAX_INSERT)
    } else {
        value
    };
    action.with_count(value)
}

struct WindowArg {
    arg: Option<PrefixArg>,
    /// The full editor the argument was started in.
    origin: WeakEntity<Editor>,
    handoff: Option<PrefixArg>,
    /// Counts hand-offs, so a deferred drop only clears the one it was scheduled for.
    generation: u64,
}

#[derive(Default)]
struct PrefixArgs(HashMap<WindowId, WindowArg>);

impl Global for PrefixArgs {}

fn args_mut(cx: &mut App) -> Option<&mut PrefixArgs> {
    cx.has_global::<PrefixArgs>()
        .then(|| cx.global_mut::<PrefixArgs>())
}

fn window_id(window: &Window) -> WindowId {
    window.window_handle().window_id()
}

fn with_state<R>(window: &Window, cx: &mut App, f: impl FnOnce(&mut WindowArg) -> R) -> Option<R> {
    let id = window_id(window);
    args_mut(cx).and_then(|args| args.0.get_mut(&id)).map(f)
}

/// The argument being built in `window`.
pub fn pending(window: &Window, cx: &App) -> Option<PrefixArg> {
    cx.try_global::<PrefixArgs>()
        .and_then(|args| args.0.get(&window_id(window)))
        .and_then(|state| state.arg)
}

/// The argument handed to the command now running, cleared on read.
pub fn take(window: &Window, cx: &mut App) -> Option<PrefixArg> {
    with_state(window, cx, |state| state.handoff.take()).flatten()
}

/// Clears the argument and the handoff.
pub fn cancel(window: &Window, cx: &mut App) {
    let id = window_id(window);
    if let Some(args) = args_mut(cx) {
        args.0.remove(&id);
    }
}

/// Moves the pending argument into the handoff, to be read by the command about to be
/// dispatched, and schedules a drop of a handoff nobody took, so an argument is consumed by
/// exactly one command even when its action has no handler in the context.
pub(crate) fn hand_off(window: &Window, cx: &mut App) {
    let Some(generation) = with_state(window, cx, |state| {
        state.handoff = state.arg.take();
        state.generation += 1;
        state.generation
    }) else {
        return;
    };
    window.defer(cx, move |window, cx| {
        let id = window_id(window);
        let Some(args) = args_mut(cx) else {
            return;
        };
        if let Some(state) = args.0.get_mut(&id) {
            if state.generation == generation {
                state.handoff = None;
            }
            if state.arg.is_none() && state.handoff.is_none() {
                args.0.remove(&id);
            }
        }
    });
}

/// Clears an argument that belongs to `editor` when its focus is lost, so it never applies in a
/// picker or the minibuffer.
pub(crate) fn init_editor(
    editor: &mut Editor,
    window: Option<&mut Window>,
    cx: &mut Context<Editor>,
) {
    let Some(window) = window else {
        return;
    };
    let focus = editor.focus_handle(cx);
    let this = cx.entity().downgrade();
    cx.on_blur(&focus, window, move |_, window, cx| {
        let id = window_id(window);
        let owned = cx
            .try_global::<PrefixArgs>()
            .and_then(|args| args.0.get(&id))
            .is_some_and(|state| state.origin == this);
        if owned {
            cancel(window, cx);
        }
    })
    .detach();
}

/// Starts `arg` in `window` as if typed in `editor`.
#[cfg(test)]
pub(crate) fn start(arg: PrefixArg, editor: &Entity<Editor>, window: &Window, cx: &mut App) {
    let args = cx.default_global::<PrefixArgs>();
    let generation = args
        .0
        .get(&window_id(window))
        .map_or(0, |state| state.generation);
    args.0.insert(
        window_id(window),
        WindowArg {
            arg: Some(arg),
            origin: editor.downgrade(),
            handoff: None,
            generation,
        },
    );
}
