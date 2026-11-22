//! A running jump: reads the query and the label keys of one window, shows the labels and reports
//! the chosen candidate. While a jump runs, every key pressed in its window goes to it and
//! nowhere else: no buffer, binding or panel sees it, and none is replayed afterwards.
//!
//! A jump ends with its candidate, with no candidates for the query, or cancelled: by C-g,
//! Escape or any key that is not part of a jump, by focus leaving where it was, by the window
//! losing activation, by a scroll or an edit in any editor that takes part, or by a new jump in
//! the same window. A cancel changes nothing: no text, selection, scroll position or focus. Each
//! window runs at most one jump; jumps in different windows are independent.

use std::collections::HashMap;
use std::time::Duration;

use bongos_avy_model::{Effect, HOME_ROW, Input, Mode as KeyMode, Session as Keys};
use editor::{Editor, EditorEvent, EditorSettings};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Focusable as _, Global, Keystroke,
    KeystrokeEvent, Subscription, Task, WeakEntity, Window, WindowId,
};
use settings::Settings as _;
use workspace::Workspace;

use crate::overlay;
use crate::visible::{self, Candidate, Mode};

/// How a jump ended, when it did not end by a cancel.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// A label was typed: jump to its candidate.
    Jump(Candidate),
    /// The query (the string; empty in line mode) has no candidate on screen.
    NoMatches(String),
}

type OnDone = Box<dyn FnOnce(Outcome, &mut Window, &mut App)>;

/// A jump waiting for keys.
struct Running {
    /// Tells this jump from a later one in the same window, for callbacks that outlive it.
    generation: u64,
    keys: Keys,
    mode: Mode,
    origin: EntityId,
    workspace: WeakEntity<Workspace>,
    /// Every editor that may carry labels; each is cleared when the jump ends.
    editors: Vec<WeakEntity<Editor>>,
    /// The candidates of the shown labels, with their full labels, nearest first.
    labelled: Vec<(Candidate, String)>,
    timer: Option<Task<()>>,
    subscriptions: Vec<Subscription>,
    on_done: OnDone,
}

#[derive(Default)]
struct Sessions {
    running: HashMap<WindowId, Running>,
    next_generation: u64,
}

impl Global for Sessions {}

struct Initialized;

impl Global for Initialized {}

/// The keys a lone modifier press produces; they are not part of a jump.
const MODIFIER_KEYS: [&str; 5] = ["shift", "control", "alt", "platform", "function"];

/// Lets C-g cancel the jump of its window. Calling it again does nothing.
pub fn init(cx: &mut App) {
    if cx.has_global::<Initialized>() {
        return;
    }
    cx.set_global(Initialized);
    bongos_emacs::quit::register(cx, |window, cx| {
        cancel(window, cx);
    });
}

/// Starts a jump of `mode` from the primary cursor of `origin` over the editors visible in
/// `workspace`'s window, replacing a jump already running there. Does nothing when no editor
/// is visible. `on_done` runs once, after the labels are gone and the keys are released, when a
/// label is typed or when the query has no candidate; it never runs for a cancel. It always runs
/// after this call returns, even when the outcome is immediate (line mode with no line to jump
/// to), so it is never called while `workspace` is being updated.
///
/// Call it outside any update of `origin` or of the other visible editors: it reads their
/// layout.
pub fn start(
    mode: Mode,
    origin: Entity<Editor>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    on_done: impl FnOnce(Outcome, &mut Window, &mut App) + 'static,
) {
    let id = window.window_handle().window_id();
    if let Some(previous) = take_running(id, cx) {
        end(previous, cx);
    }
    let panes = visible::panes(workspace, window, cx);
    if panes.is_empty() {
        return;
    }
    let generation = {
        let sessions = cx.default_global::<Sessions>();
        sessions.next_generation += 1;
        sessions.next_generation
    };

    let mut subscriptions = Vec::new();
    subscriptions.push(cx.intercept_keystrokes(move |event, window, cx| {
        on_key(id, generation, event, window, cx);
    }));
    let focused = window
        .focused(cx)
        .unwrap_or_else(|| origin.focus_handle(cx));
    subscriptions.push(window.on_focus_out(&focused, cx, move |_, _, cx| {
        cancel_if(id, generation, cx);
    }));
    subscriptions.push(cx.observe_window_activation(window, move |_, window, cx| {
        if !window.is_window_active() {
            cancel_if(id, generation, cx);
        }
    }));
    let editors: Vec<WeakEntity<Editor>> = panes.iter().map(|pane| pane.editor.clone()).collect();
    for editor in editors.iter().filter_map(WeakEntity::upgrade) {
        subscriptions.push(watch(&editor, id, generation, cx));
    }

    let key_mode = match mode {
        Mode::CharTimer => KeyMode::CharTimer,
        Mode::Word => KeyMode::Word,
        Mode::Line => KeyMode::Line,
    };
    let mut running = Running {
        generation,
        keys: Keys::new(key_mode, HOME_ROW.to_vec()),
        mode,
        origin: origin.entity_id(),
        workspace: cx.weak_entity(),
        editors,
        labelled: Vec::new(),
        timer: None,
        subscriptions,
        on_done: Box::new(on_done),
    };
    let mut fresh = Vec::new();
    let effect = running.keys.start(|query| {
        fresh = visible::collect(mode, query, &panes, running.origin);
        fresh.len()
    });
    apply(id, running, effect, fresh, true, window, cx);
}

/// Cancels the jump numbered `generation` of window `id` when `editor` scrolls or is edited.
fn watch(editor: &Entity<Editor>, id: WindowId, generation: u64, cx: &mut App) -> Subscription {
    cx.subscribe(editor, move |_, event: &EditorEvent, cx| {
        if matches!(
            event,
            EditorEvent::ScrollPositionChanged { .. } | EditorEvent::BufferEdited
        ) {
            cancel_if(id, generation, cx);
        }
    })
}

/// Whether a jump runs in `window`.
pub fn is_running(window: &Window, cx: &App) -> bool {
    cx.try_global::<Sessions>().is_some_and(|sessions| {
        sessions
            .running
            .contains_key(&window.window_handle().window_id())
    })
}

/// Cancels the jump of `window`, if one runs; returns whether one did.
pub fn cancel(window: &Window, cx: &mut App) -> bool {
    match take_running(window.window_handle().window_id(), cx) {
        Some(running) => {
            end(running, cx);
            true
        }
        None => false,
    }
}

fn on_key(
    id: WindowId,
    generation: u64,
    event: &KeystrokeEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let keystroke = &event.keystroke;
    if keystroke.key.is_empty()
        || MODIFIER_KEYS.contains(&keystroke.key.as_str())
        || window.window_handle().window_id() != id
    {
        return;
    }
    let Some(running) = take_if(id, generation, cx) else {
        return;
    };
    cx.stop_propagation();
    feed(id, running, input_for(keystroke), window, cx);
}

/// What a key means to a jump.
fn input_for(keystroke: &Keystroke) -> Input {
    let modifiers = &keystroke.modifiers;
    let plain = !modifiers.control && !modifiers.alt && !modifiers.platform && !modifiers.function;
    match keystroke.key.as_str() {
        "escape" => Input::Cancel,
        "g" if modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.function
            && !modifiers.shift =>
        {
            Input::Cancel
        }
        "enter" if plain => Input::Enter,
        "backspace" if plain => Input::Backspace,
        _ if plain => typed_char(keystroke).map_or(Input::Other, Input::Text),
        _ => Input::Other,
    }
}

/// The printable character a key without command modifiers types, if any.
fn typed_char(keystroke: &Keystroke) -> Option<char> {
    let single = |text: &str| {
        let mut chars = text.chars();
        let first = chars.next()?;
        chars.next().is_none().then_some(first)
    };
    let typed = match keystroke.key_char.as_deref() {
        Some(text) => single(text)?,
        None if keystroke.key == "space" => ' ',
        None => {
            let key = single(&keystroke.key)?;
            if keystroke.modifiers.shift {
                key.to_uppercase().next()?
            } else {
                key
            }
        }
    };
    (!typed.is_control()).then_some(typed)
}

/// Feeds `input` to a jump taken out of the registry, then acts on what it says.
fn feed(id: WindowId, mut running: Running, input: Input, window: &mut Window, cx: &mut App) {
    let (mode, origin, workspace) = (running.mode, running.origin, running.workspace.clone());
    let mut fresh = Vec::new();
    let effect = running.keys.step(input, |query| {
        fresh = candidates(mode, query, origin, &workspace, window, cx);
        fresh.len()
    });
    apply(id, running, effect, fresh, false, window, cx);
}

/// The candidates on screen now.
fn candidates(
    mode: Mode,
    query: &str,
    origin: EntityId,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Vec<Candidate> {
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    let panes = workspace.update(cx, |workspace, cx| visible::panes(workspace, window, cx));
    visible::collect(mode, query, &panes, origin)
}

/// Carries out `effect` for a jump taken out of the registry: puts it back while it runs, or ends
/// it. `fresh` holds the candidates computed for the step, if any were. `immediate` is set when
/// the caller is `start`, which still holds the workspace: an outcome is then reported deferred.
fn apply(
    id: WindowId,
    mut running: Running,
    effect: Effect,
    fresh: Vec<Candidate>,
    immediate: bool,
    window: &mut Window,
    cx: &mut App,
) {
    match effect {
        Effect::RestartTimer => {
            let timeout = EditorSettings::get_global(cx).avy_timeout;
            running.timer = Some(timer(id, running.generation, timeout, window, cx));
        }
        Effect::ShowLabels => {
            running.timer = None;
            for candidate in &fresh {
                if !running
                    .editors
                    .iter()
                    .any(|editor| editor.entity_id() == candidate.editor.entity_id())
                {
                    running.editors.push(candidate.editor.clone());
                    if let Some(editor) = candidate.editor.upgrade() {
                        running
                            .subscriptions
                            .push(watch(&editor, id, running.generation, cx));
                    }
                }
            }
            running.labelled = fresh
                .into_iter()
                .zip(running.keys.labels().iter().cloned())
                .collect();
            overlay::clear(&running.editors, cx);
            overlay::show(&running.labelled, "", cx);
        }
        Effect::Narrow(prefix) => overlay::show(&running.labelled, &prefix, cx),
        Effect::Echo(_) => {
            // A backspace over the whole query goes back to reading, with no labels.
            if running.keys.labels().is_empty() && !running.labelled.is_empty() {
                running.labelled.clear();
                overlay::clear(&running.editors, cx);
            }
        }
        Effect::None => {}
        Effect::Jump(index) => {
            let chosen = running
                .labelled
                .get(index)
                .map(|(candidate, _)| candidate.clone());
            let on_done = finish(running, cx);
            if let Some(candidate) = chosen {
                on_done(Outcome::Jump(candidate), window, cx);
            }
            return;
        }
        Effect::NoMatches(query) => {
            let on_done = finish(running, cx);
            if immediate {
                window.defer(cx, move |window, cx| {
                    on_done(Outcome::NoMatches(query), window, cx)
                });
            } else {
                on_done(Outcome::NoMatches(query), window, cx);
            }
            return;
        }
        Effect::Cancel => {
            end(running, cx);
            return;
        }
    }
    // Without an echo area the message goes to the log, and the jump goes on.
    bongos_minibuffer::message(window, cx, format!("char: {}", running.keys.query()));
    cx.default_global::<Sessions>().running.insert(id, running);
}

/// Sends `TimerFired` to the jump numbered `generation` after `timeout`, unless it ended.
fn timer(id: WindowId, generation: u64, timeout: Duration, window: &Window, cx: &App) -> Task<()> {
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(timeout).await;
        cx.update_window(handle, |_, window, cx| {
            let Some(mut running) = take_if(id, generation, cx) else {
                return;
            };
            // This task is the one running; let it finish rather than drop it from inside.
            if let Some(task) = running.timer.take() {
                task.detach();
            }
            feed(id, running, Input::TimerFired, window, cx);
        })
        .ok();
    })
}

fn take_running(id: WindowId, cx: &mut App) -> Option<Running> {
    if !cx.has_global::<Sessions>() {
        return None;
    }
    cx.global_mut::<Sessions>().running.remove(&id)
}

/// Removes the jump of `id` when it is still the one numbered `generation`.
fn take_if(id: WindowId, generation: u64, cx: &mut App) -> Option<Running> {
    if !cx.has_global::<Sessions>() {
        return None;
    }
    let sessions = cx.global_mut::<Sessions>();
    if sessions
        .running
        .get(&id)
        .is_some_and(|running| running.generation == generation)
    {
        sessions.running.remove(&id)
    } else {
        None
    }
}

fn cancel_if(id: WindowId, generation: u64, cx: &mut App) {
    if let Some(running) = take_if(id, generation, cx) {
        end(running, cx);
    }
}

/// Ends a jump that is no longer in the registry without calling its `on_done`.
fn end(running: Running, cx: &mut App) {
    drop(finish(running, cx));
}

/// Ends a jump that is no longer in the registry: stops its timer, clears its labels and
/// releases the keys. Returns its `on_done` for the caller to run.
fn finish(running: Running, cx: &mut App) -> OnDone {
    let Running {
        editors,
        timer,
        subscriptions,
        on_done,
        ..
    } = running;
    drop(timer);
    overlay::clear(&editors, cx);
    // The subscription that called this may still be running.
    cx.defer(move |_| drop(subscriptions));
    on_done
}
