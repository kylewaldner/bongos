//! Recenter (C-l) and Reposition (M-r). Both walk the model's cycle (middle, top, bottom,
//! middle) while the same command repeats in the same editor and the carets have not moved;
//! any other command, or any caret move, starts the cycle over at the middle.

use bongos_emacs_model::recenter::Cycle;
use editor::display_map::{DisplayPoint, DisplayRow};
use editor::{Editor, EditorEvent, SelectionEffects};
use gpui::{Action, App, Context, EntityId, Global, Window};
use multi_buffer::Anchor;
use text::{Bias, Selection};

use crate::actions::{Recenter, Reposition};
use crate::last_command;

/// The cycle position the last recenter or reposition took, and the carets it left behind. A
/// selection change that leaves other carets than these resets the cycle.
#[derive(Default)]
struct CycleState(Option<Stored>);

struct Stored {
    editor: EntityId,
    cycle: Cycle,
    selections: Vec<Selection<Anchor>>,
}

impl Global for CycleState {}

/// Makes any caret move other than the commands' own reset the cycle.
pub(crate) fn init_editor(cx: &mut Context<Editor>) {
    cx.subscribe_self(|editor, event: &EditorEvent, cx| {
        if !matches!(event, EditorEvent::SelectionsChanged { .. }) {
            return;
        }
        let id = cx.entity_id();
        let moved = cx.try_global::<CycleState>().is_some_and(|state| {
            state.0.as_ref().is_some_and(|stored| {
                stored.editor == id && stored.selections.as_slice() != editor.selections.disjoint_anchors()
            })
        });
        if moved {
            cx.default_global::<CycleState>().0 = None;
        }
    })
    .detach();
}

/// The position this press takes: the next one when the same command ran just before in this
/// editor and nothing moved the carets since, else the middle.
fn position(name: &'static str, editor: &Editor, id: EntityId, window: &Window, cx: &App) -> Cycle {
    let repeats = last_command::last_command(window, cx) == Some(name);
    let previous = cx
        .try_global::<CycleState>()
        .and_then(|state| state.0.as_ref())
        .filter(|stored| {
            repeats
                && stored.editor == id
                && stored.selections.as_slice() == editor.selections.disjoint_anchors()
        })
        .map(|stored| stored.cycle);
    Cycle::after(previous)
}

fn store(editor: &Editor, id: EntityId, cycle: Cycle, cx: &mut App) {
    cx.default_global::<CycleState>().0 = Some(Stored {
        editor: id,
        cycle,
        selections: editor.selections.disjoint_anchors().to_vec(),
    });
}

/// The margin the scroll keeps above and below the caret, never more than fits.
fn margin(editor: &Editor, rows: u32) -> u32 {
    (editor.vertical_scroll_margin() as u32).min(rows.saturating_sub(1) / 2)
}

/// The window row (0 is the top) the cycle position stands for.
fn cycle_row(cycle: Cycle, rows: u32, margin: u32) -> u32 {
    match cycle {
        Cycle::Middle => rows / 2,
        Cycle::Top => margin,
        Cycle::Bottom => rows.saturating_sub(1 + margin),
    }
}

pub(crate) fn recenter_command(
    action: &Recenter,
    editor: &gpui::Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let name = action.name();
    let id = editor.entity_id();
    let count = action.count;
    let cycle = position(name, editor.read(cx), id, window, cx);
    editor.update(cx, |editor, cx| {
        let Some(rows) = editor.visible_row_count().filter(|rows| *rows > 0) else {
            return;
        };
        let margin = margin(editor, rows);
        let display = editor.display_snapshot(cx);
        let heads: Vec<_> = editor
            .selections
            .all_display(&display)
            .iter()
            .map(|selection| selection.head())
            .collect();
        let head = editor.selections.newest_display(&display).head();
        let row = match count {
            None => cycle_row(cycle, rows, margin),
            Some(count) if count >= 0 => (count as u32).clamp(margin, rows - 1 - margin),
            Some(count) => rows
                .saturating_sub(count.unsigned_abs())
                .clamp(margin, rows - 1 - margin),
        };
        let top = head.row().0.saturating_sub(row);
        // The region goes; collapsing the selections emits a change that the stored carets
        // below already account for.
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_display_ranges(heads.iter().map(|head| *head..*head));
        });
        editor.set_selection_mark_mode(false, cx);
        editor.set_scroll_position(gpui::point(editor.scroll_position(cx).x, top as f64), window, cx);
        if count.is_none() {
            store(editor, id, cycle, cx);
        } else {
            cx.default_global::<CycleState>().0 = None;
        }
    });
}

pub(crate) fn reposition_command(
    action: &Reposition,
    editor: &gpui::Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let name = action.name();
    let id = editor.entity_id();
    let cycle = position(name, editor.read(cx), id, window, cx);
    editor.update(cx, |editor, cx| {
        let Some(rows) = editor.visible_row_count().filter(|rows| *rows > 0) else {
            return;
        };
        let margin = margin(editor, rows);
        let display = editor.display_snapshot(cx);
        let top = editor.scroll_position(cx).y.max(0.0) as u32;
        let last = display.max_point().row().0;
        let row = (top + cycle_row(cycle, rows, margin)).min(last);
        let point = display.clip_point(DisplayPoint::new(DisplayRow(row), 0), Bias::Left);
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_display_ranges([point..point]);
        });
        editor.set_selection_mark_mode(false, cx);
        store(editor, id, cycle, cx);
    });
}
