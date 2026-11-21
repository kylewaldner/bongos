//! The pane picker: labels every target window with a letter and waits for the next key. A shown
//! letter picks its window; any other key, a focus change or the frame losing activation cancels.
//! While the picker waits, the key it is waiting for reaches nothing else: no editor, terminal or
//! binding sees it.

use std::collections::HashMap;

use gpui::{
    AnyWeakView, App, Context, Entity, Focusable as _, Global, SharedString, Subscription,
    WeakEntity, Window, WindowId,
};
use workspace::{Pane, Workspace, ZoomIn, dock::Dock};

use crate::targets::{letters, targets};

type OnPick = Box<dyn FnOnce(Entity<Pane>, &mut Window, &mut App)>;

/// What was zoomed before the picker un-zoomed the frame, to zoom again on a cancel.
enum Rezoom {
    Pane(WeakEntity<Pane>),
    Panel(WeakEntity<Dock>, AnyWeakView),
}

impl Rezoom {
    fn apply(self, window: &mut Window, cx: &mut App) {
        match self {
            Rezoom::Pane(pane) => {
                if let Some(pane) = pane.upgrade() {
                    pane.update(cx, |pane, cx| pane.zoom_in(&ZoomIn, window, cx));
                }
            }
            Rezoom::Panel(dock, panel) => {
                if let (Some(dock), Some(panel)) = (dock.upgrade(), panel.upgrade()) {
                    dock.update(cx, |dock, cx| {
                        dock.set_panel_zoomed(&panel, true, window, cx)
                    });
                }
            }
        }
    }
}

/// A pick waiting for its key.
struct Running {
    /// Tells this pick from a later one in the same frame, for callbacks that outlive it.
    generation: u64,
    shown: Vec<(char, WeakEntity<Pane>)>,
    rezoom: Option<Rezoom>,
    on_pick: OnPick,
    subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct Pickers {
    running: HashMap<WindowId, Running>,
    next_generation: u64,
}

impl Global for Pickers {}

/// The keys a lone modifier press produces; they are not a choice.
const MODIFIER_KEYS: [&str; 5] = ["shift", "control", "alt", "platform", "function"];

/// Picks a target window and calls `on_pick` with it. With fewer than two targets nothing
/// happens; with exactly two the other one is picked at once. Otherwise every target shows its
/// letter until the next key: a shown letter (with no modifier) picks its window, any other key
/// cancels and is consumed. A cancel, a focus change or the frame losing activation zooms again
/// what the picker un-zoomed. A second pick in the same frame replaces a running one.
pub fn pick_pane(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    on_pick: impl FnOnce(Entity<Pane>, &mut Window, &mut App) + 'static,
) {
    let id = window.window_handle().window_id();
    let previous = take_running(id, cx);
    let panes = targets(workspace, window, cx);
    if panes.len() < 3 {
        if let Some(previous) = previous {
            end(previous, None, window, cx);
        }
        if panes.len() == 2 {
            let current = workspace.focused_pane(window, cx);
            let other = panes
                .iter()
                .find(|pane| **pane != current)
                .unwrap_or(&panes[1])
                .clone();
            on_pick(other, window, cx);
        }
        return;
    }

    // A replaced pick hands over what it un-zoomed, since the frame is not zoomed any more.
    let mut rezoom = previous.and_then(|previous| {
        clear_badges(&previous.shown, cx);
        previous.rezoom
    });
    if let Some(found) = zoomed(workspace, window, cx) {
        rezoom = Some(found);
    }
    if workspace.zoomed_item().is_some() || rezoom.is_some() {
        workspace.unzoom(window, cx);
    }

    let shown: Vec<(char, WeakEntity<Pane>)> = letters(panes.len())
        .into_iter()
        .zip(&panes)
        .map(|(letter, pane)| (letter, pane.downgrade()))
        .collect();
    for (letter, pane) in &shown {
        if let Some(pane) = pane.upgrade() {
            pane.update(cx, |pane, cx| {
                pane.set_badge(Some(SharedString::from(letter.to_string())), cx)
            });
        }
    }

    let generation = {
        let pickers = cx.default_global::<Pickers>();
        pickers.next_generation += 1;
        pickers.next_generation
    };
    let mut subscriptions = Vec::new();
    subscriptions.push(cx.intercept_keystrokes(move |event, current, cx| {
        let keystroke = &event.keystroke;
        if keystroke.key.is_empty()
            || MODIFIER_KEYS.contains(&keystroke.key.as_str())
            || current.window_handle().window_id() != id
        {
            return;
        }
        let Some(running) = take_if(id, generation, cx) else {
            return;
        };
        cx.stop_propagation();
        let picked = if keystroke.modifiers.modified() {
            None
        } else {
            running
                .shown
                .iter()
                .find(|(letter, _)| keystroke.key == letter.to_string())
                .and_then(|(_, pane)| pane.upgrade())
        };
        end(running, picked, current, cx);
    }));
    if let Some(focused) = window.focused(cx) {
        subscriptions.push(window.on_focus_out(&focused, cx, move |_, window, cx| {
            if let Some(running) = take_if(id, generation, cx) {
                end(running, None, window, cx);
            }
        }));
    }
    subscriptions.push(cx.observe_window_activation(window, move |_, window, cx| {
        if !window.is_window_active()
            && let Some(running) = take_if(id, generation, cx)
        {
            end(running, None, window, cx);
        }
    }));

    cx.default_global::<Pickers>().running.insert(
        id,
        Running {
            generation,
            shown,
            rezoom,
            on_pick: Box::new(on_pick),
            subscriptions,
        },
    );
}

/// Whether a pick waits for its key in `window`.
pub fn is_picking(window: &Window, cx: &App) -> bool {
    cx.try_global::<Pickers>().is_some_and(|pickers| {
        pickers
            .running
            .contains_key(&window.window_handle().window_id())
    })
}

/// The center pane or dock panel that is zoomed, if any.
fn zoomed(workspace: &Workspace, window: &Window, cx: &App) -> Option<Rezoom> {
    if let Some(pane) = workspace
        .center_panes()
        .into_iter()
        .find(|pane| pane.read(cx).is_zoomed())
    {
        return Some(Rezoom::Pane(pane.downgrade()));
    }
    workspace.all_docks().into_iter().find_map(|dock| {
        dock.read(cx)
            .zoomed_panel(window, cx)
            .map(|panel| Rezoom::Panel(dock.downgrade(), panel.to_any().downgrade()))
    })
}

fn take_running(id: WindowId, cx: &mut App) -> Option<Running> {
    if !cx.has_global::<Pickers>() {
        return None;
    }
    cx.global_mut::<Pickers>().running.remove(&id)
}

fn running_mut(id: WindowId, generation: u64, cx: &mut App) -> Option<&mut Running> {
    if !cx.has_global::<Pickers>() {
        return None;
    }
    cx.global_mut::<Pickers>()
        .running
        .get_mut(&id)
        .filter(|running| running.generation == generation)
}

/// Removes the pick of `id` when it is still the one numbered `generation`.
fn take_if(id: WindowId, generation: u64, cx: &mut App) -> Option<Running> {
    running_mut(id, generation, cx)?;
    take_running(id, cx)
}

fn clear_badges(shown: &[(char, WeakEntity<Pane>)], cx: &mut App) {
    for (_, pane) in shown {
        if let Some(pane) = pane.upgrade() {
            pane.update(cx, |pane, cx| pane.set_badge(None, cx));
        }
    }
}

/// Ends a pick that is no longer in the registry: clears its badges, then calls its `on_pick`
/// with `picked`, or zooms again what it un-zoomed when nothing was picked.
fn end(running: Running, picked: Option<Entity<Pane>>, window: &mut Window, cx: &mut App) {
    let Running {
        shown,
        rezoom,
        on_pick,
        subscriptions,
        ..
    } = running;
    clear_badges(&shown, cx);
    // The subscription that called this may still be running.
    cx.defer(move |_| drop(subscriptions));
    match picked {
        Some(pane) => on_pick(pane, window, cx),
        None => {
            if let Some(rezoom) = rezoom {
                // Deferred, since a cancel may run while the workspace is being updated.
                window.defer(cx, move |window, cx| rezoom.apply(window, cx));
            }
        }
    }
}

/// Focuses `pane`'s active item.
pub(crate) fn focus_pane(pane: Entity<Pane>, window: &mut Window, cx: &mut App) {
    window.focus(&pane.focus_handle(cx), cx);
}
