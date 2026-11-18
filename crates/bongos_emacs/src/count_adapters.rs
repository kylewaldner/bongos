//! Count adapters: what the Zed commands without a count do with a prefix argument. Each does
//! all N steps in one go, so one selection change or one undo step covers the whole count.

use bongos_emacs_model::prefix_arg::PrefixArg;
use editor::actions::{
    Backspace, Delete, MoveDownByLines, MoveLeft, MovePageDown, MovePageUp, MoveRight,
    MoveUpByLines, Newline, SelectDown, SelectDownByLines, SelectLeft, SelectPageDown,
    SelectPageUp, SelectRight, SelectUp, SelectUpByLines, Undo,
};
use editor::display_map::{DisplayPoint, DisplayRow, DisplaySnapshot};
use editor::scroll::ScrollAmount;
use editor::{Editor, SelectionEffects, movement};
use gpui::{Action, App, Entity, Window};
use text::{Bias, SelectionGoal};

use crate::prefix_arg::{self, MAX_INSERT, Meaning, declare};
use crate::rectangle_mode;

/// The most undo steps one count takes: beyond this the history has long run out.
const MAX_UNDO: u32 = 10_000;

type Adapter = fn(PrefixArg, &Entity<Editor>, &mut Window, &mut App);

/// Declares every adapter.
pub(crate) fn init(cx: &mut App) {
    let adapters: [(&str, Adapter); 16] = [
        (MoveRight::name_for_type(), |a, e, w, c| {
            horizontal(a, true, false, e, w, c)
        }),
        (MoveLeft::name_for_type(), |a, e, w, c| {
            horizontal(a, false, false, e, w, c)
        }),
        (SelectRight::name_for_type(), |a, e, w, c| {
            horizontal(a, true, true, e, w, c)
        }),
        (SelectLeft::name_for_type(), |a, e, w, c| {
            horizontal(a, false, true, e, w, c)
        }),
        ("editor::MoveDown", |a, e, w, c| {
            vertical(a, true, false, e, w, c)
        }),
        ("editor::MoveUp", |a, e, w, c| {
            vertical(a, false, false, e, w, c)
        }),
        (SelectDown::name_for_type(), |a, e, w, c| {
            vertical(a, true, true, e, w, c)
        }),
        (SelectUp::name_for_type(), |a, e, w, c| {
            vertical(a, false, true, e, w, c)
        }),
        (MovePageDown::name_for_type(), |a, e, w, c| {
            page(a, true, false, e, w, c)
        }),
        (MovePageUp::name_for_type(), |a, e, w, c| {
            page(a, false, false, e, w, c)
        }),
        (SelectPageDown::name_for_type(), |a, e, w, c| {
            page(a, true, true, e, w, c)
        }),
        (SelectPageUp::name_for_type(), |a, e, w, c| {
            page(a, false, true, e, w, c)
        }),
        (Delete::name_for_type(), |a, e, w, c| {
            delete_chars(a, true, e, w, c)
        }),
        (Backspace::name_for_type(), |a, e, w, c| {
            delete_chars(a, false, e, w, c)
        }),
        (Newline::name_for_type(), newlines),
        (Undo::name_for_type(), undo),
    ];
    for (name, adapter) in adapters {
        declare(cx, name, Meaning::Adapter(adapter));
    }
}

/// An adapter runs without dispatching its action, so the rectangle mode's motion listener
/// never sees it: collapse the column selection to the region here, as that listener does.
fn leave_rectangle(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    if rectangle_mode::is_on(editor.read(cx)) {
        rectangle_mode::to_region(editor, window, cx);
    }
}

/// `point` moved `times` characters, stopping where it stops moving.
fn walk(map: &DisplaySnapshot, mut point: DisplayPoint, forward: bool, times: u32) -> DisplayPoint {
    for _ in 0..times {
        let next = if forward {
            movement::right(map, point)
        } else {
            movement::left(map, point)
        };
        if next == point {
            break;
        }
        point = next;
    }
    point
}

/// Moves (or extends) every selection `value` characters; a negative count goes the other way.
/// A plain move from a region first goes to its end, which counts as one character.
fn horizontal(
    arg: PrefixArg,
    forward: bool,
    select: bool,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let value = arg.value();
    if value == 0 {
        return;
    }
    leave_rectangle(editor, window, cx);
    let forward = forward == (value > 0);
    let times = value.unsigned_abs();
    editor.update(cx, |editor, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            if select {
                s.move_heads_with(&mut |map, head, _| {
                    (walk(map, head, forward, times), SelectionGoal::None)
                });
            } else {
                s.move_with(&mut |map, selection| {
                    let (start, rest) = if selection.is_empty() {
                        (selection.head(), times)
                    } else if forward {
                        (selection.end, times - 1)
                    } else {
                        (selection.start, times - 1)
                    };
                    selection.collapse_to(walk(map, start, forward, rest), SelectionGoal::None);
                });
            }
        });
    });
}

/// Moves (or extends) every selection `value` lines through the by-lines actions, which are
/// built by name because their field is private.
fn vertical(
    arg: PrefixArg,
    down: bool,
    select: bool,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let value = arg.value();
    if value == 0 {
        return;
    }
    let down = down == (value > 0);
    let name = match (down, select) {
        (true, false) => MoveDownByLines::name_for_type(),
        (false, false) => MoveUpByLines::name_for_type(),
        (true, true) => SelectDownByLines::name_for_type(),
        (false, true) => SelectUpByLines::name_for_type(),
    };
    let params = gpui::private::serde_json::json!({ "lines": value.unsigned_abs() });
    let Ok(action) = cx.build_action(name, Some(params)) else {
        return;
    };
    leave_rectangle(editor, window, cx);
    let any = action.as_any();
    editor.update(cx, |editor, cx| {
        if let Some(action) = any.downcast_ref::<MoveDownByLines>() {
            editor.move_down_by_lines(action, window, cx);
        } else if let Some(action) = any.downcast_ref::<MoveUpByLines>() {
            editor.move_up_by_lines(action, window, cx);
        } else if let Some(action) = any.downcast_ref::<SelectDownByLines>() {
            editor.select_down_by_lines(action, window, cx);
        } else if let Some(action) = any.downcast_ref::<SelectUpByLines>() {
            editor.select_up_by_lines(action, window, cx);
        }
    });
}

/// Scrolls the window `value` lines, then moves a selection that left the window onto the
/// nearest visible row (extending it for the select variants). A selection still in view stays.
fn page(
    arg: PrefixArg,
    down: bool,
    select: bool,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let value = arg.value();
    if value == 0 || editor.read(cx).mode().is_single_line() {
        return;
    }
    leave_rectangle(editor, window, cx);
    let lines = if down { value } else { value.saturating_neg() };
    editor.update(cx, |editor, cx| {
        let Some(visible) = editor.visible_line_count() else {
            return;
        };
        editor.scroll_screen(&ScrollAmount::Line(lines as f32), window, cx);
        let top = editor.scroll_position(cx).y.max(0.0).ceil() as u32;
        let display = editor.display_snapshot(cx);
        let last = display.max_point().row().0;
        let bottom = (((top as f64 + visible).floor() as u32).saturating_sub(1)).max(top);
        let bottom = bottom.min(last);
        let top = top.min(bottom);
        let clamp = |map: &DisplaySnapshot, head: DisplayPoint| -> Option<DisplayPoint> {
            let row = head.row().0.clamp(top, bottom);
            (row != head.row().0).then(|| {
                map.clip_point(
                    DisplayPoint::new(DisplayRow(row), head.column()),
                    Bias::Left,
                )
            })
        };
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            if select {
                s.move_heads_with(&mut |map, head, goal| match clamp(map, head) {
                    Some(point) => (point, SelectionGoal::None),
                    None => (head, goal),
                });
            } else {
                s.move_with(&mut |map, selection| {
                    if let Some(point) = clamp(map, selection.head()) {
                        selection.collapse_to(point, SelectionGoal::None);
                    }
                });
            }
        });
    });
}

/// Deletes `value` characters after (or before) every caret as one edit. A selection with a
/// region is deleted once, whatever the count. A count of 1 is Zed's own command.
fn delete_chars(
    arg: PrefixArg,
    forward: bool,
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let value = arg.value();
    if value == 0 {
        return;
    }
    let forward = forward == (value > 0);
    let times = value.unsigned_abs();
    editor.update(cx, |editor, cx| {
        if editor.read_only(cx) {
            return;
        }
        // Text typed just before the command must not merge into the command's undo step.
        editor.finalize_last_transaction(cx);
        editor.transact(window, cx, |editor, window, cx| {
            let plain = times == 1;
            if !plain {
                editor.change_selections(Default::default(), window, cx, |s| {
                    s.move_with(&mut |map, selection| {
                        if selection.is_empty() {
                            let head = selection.head();
                            let other = walk(map, head, forward, times);
                            if forward {
                                selection.end = other;
                                selection.reversed = true;
                            } else {
                                selection.start = other;
                                selection.reversed = false;
                            }
                            selection.goal = SelectionGoal::None;
                        }
                    });
                });
            }
            if forward {
                editor.delete(&Delete, window, cx);
            } else {
                editor.backspace(&Backspace, window, cx);
            }
        });
        editor.finalize_last_transaction(cx);
    });
}

/// RET: `value` auto-indented newlines in one undo step. 0 or less inserts nothing.
fn newlines(arg: PrefixArg, editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let times = arg.value().min(MAX_INSERT);
    if times <= 0 {
        return;
    }
    if arg.value() > MAX_INSERT {
        prefix_arg::show_limited(window, cx);
    }
    editor.update(cx, |editor, cx| {
        if editor.read_only(cx) {
            return;
        }
        editor.finalize_last_transaction(cx);
        editor.transact(window, cx, |editor, window, cx| {
            for _ in 0..times {
                editor.newline(&Newline, window, cx);
            }
        });
        editor.finalize_last_transaction(cx);
    });
}

/// Undo `value` times, each its own undo step and never inside an outer transaction.
fn undo(arg: PrefixArg, editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let times = arg.value().clamp(0, MAX_UNDO as i32);
    editor.update(cx, |editor, cx| {
        for _ in 0..times {
            editor.undo(&Undo, window, cx);
        }
    });
}
