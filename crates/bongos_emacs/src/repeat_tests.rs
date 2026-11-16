//! Tests of the repeat runner: an action with no count typed after a prefix argument, run as
//! many times as it says, in slices, as one undo step, with the keys typed meanwhile held.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use bongos_minibuffer::minibuffer_for_window;
use editor::Editor;
use editor::actions::{Redo, Undo};
use gpui::{
    AppContext as _, Entity, KeyBinding, Keystroke, Subscription, TestAppContext,
    VisualTestContext, actions,
};
use language::{Buffer, BufferEvent};
use workspace::Workspace;

use crate::actions::UniversalArgument;
use crate::prefix_arg;
use crate::prefix_arg_tests::route_echo;
use crate::quit_tests::workspace_window_with;
use crate::repeat::{self, REPEATS_PER_SLICE};

actions!(repeat_tests, [InsertX, Weave, Mark, Leave, Nothing]);

thread_local! {
    /// The number of `x` in the editor each time `Mark` ran.
    static MARKS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// The runs of `Leave`.
    static LEAVES: Cell<usize> = const { Cell::new(0) };
    /// The runs of `Nothing`.
    static NOTHINGS: Cell<usize> = const { Cell::new(0) };
}

/// The run of `Leave` that moves focus out of the editor.
const LEAVE_AT: usize = 3;

fn setup(cx: &mut gpui::App) {
    route_echo(cx);
    let editor = Some("Editor");
    cx.bind_keys([
        KeyBinding::new("ctrl-u", UniversalArgument, editor),
        KeyBinding::new("ctrl-k", InsertX, editor),
        KeyBinding::new("ctrl-w", Weave, editor),
        KeyBinding::new("ctrl-p", Mark, editor),
        KeyBinding::new("ctrl-l", Leave, editor),
        KeyBinding::new("ctrl-n", Nothing, editor),
    ]);
    cx.observe_new(|editor: &mut Editor, _, cx| {
        let weak = cx.entity().downgrade();
        let insert = weak.clone();
        editor
            .register_action(move |_: &InsertX, window, cx| {
                insert
                    .update(cx, |editor, cx| editor.insert("x", window, cx))
                    .ok();
            })
            .detach();
        let weave = weak.clone();
        editor
            .register_action(move |_: &Weave, window, cx| {
                weave
                    .update(cx, |editor, cx| {
                        editor.insert("x", window, cx);
                        editor.move_right(&editor::actions::MoveRight, window, cx);
                    })
                    .ok();
            })
            .detach();
        let mark = weak.clone();
        editor
            .register_action(move |_: &Mark, _, cx| {
                if let Some(editor) = mark.upgrade() {
                    let xs = editor.read(cx).text(cx).matches('x').count();
                    MARKS.with(|marks| marks.borrow_mut().push(xs));
                }
            })
            .detach();
        let leave = weak;
        editor
            .register_action(move |_: &Leave, window, cx| {
                leave
                    .update(cx, |editor, cx| editor.insert("x", window, cx))
                    .ok();
                LEAVES.with(|leaves| leaves.set(leaves.get() + 1));
                if LEAVES.with(Cell::get) == LEAVE_AT {
                    window.blur();
                }
            })
            .detach();
        editor
            .register_action(|_: &Nothing, _, _| {
                NOTHINGS.with(|nothings| nothings.set(nothings.get() + 1));
            })
            .detach();
    })
    .detach();
}

/// A workspace window with the test keys bound and "abc" in the focused editor, the cursor at
/// its start.
async fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let (workspace, editor, mut cx) = workspace_window_with(cx, setup).await;
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.set_text("abc", window, cx);
        editor.move_to_beginning(&editor::actions::MoveToBeginning, window, cx);
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

/// Types `keys` without running the other tasks, so a run longer than one slice stays active.
fn type_keys(cx: &mut VisualTestContext, keys: &str) {
    for key in keys.split(' ') {
        let keystroke = Keystroke::parse(key).unwrap();
        cx.update(|window, cx| window.dispatch_keystroke(keystroke, cx));
    }
}

/// The repeats done and slices run by the active run, `None` when no run is active.
fn progress(cx: &mut VisualTestContext) -> Option<(usize, usize)> {
    cx.update(|window, cx| repeat::progress(window, cx))
}

/// Runs tasks until the active run has run its next slice.
fn step(cx: &mut VisualTestContext) {
    let before = progress(cx);
    assert!(before.is_some(), "no run is active");
    while progress(cx) == before {
        assert!(cx.executor().tick(), "the run never ran its next slice");
    }
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn xs(count: usize) -> String {
    "x".repeat(count)
}

fn buffer(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Entity<Buffer> {
    editor.read_with(cx, |editor, cx| {
        editor.buffer().read(cx).as_singleton().unwrap()
    })
}

/// Counts the buffer's `Edited` events.
fn count_edits(
    buffer: &Entity<Buffer>,
    cx: &mut VisualTestContext,
) -> (Rc<Cell<usize>>, Subscription) {
    let edits = Rc::new(Cell::new(0));
    let seen = edits.clone();
    let subscription = cx.update(|_, cx| {
        cx.subscribe(buffer, move |_, event: &BufferEvent, _| {
            if matches!(event, BufferEvent::Edited { .. }) {
                seen.set(seen.get() + 1);
            }
        })
    });
    (edits, subscription)
}

/// Whether no transaction is open on the buffer: only then does starting one return its id.
fn no_open_transaction(buffer: &Entity<Buffer>, cx: &mut VisualTestContext) -> bool {
    buffer.update(cx, |buffer, cx| {
        let started = buffer.start_transaction();
        buffer.end_transaction(cx);
        started.is_some()
    })
}

fn undo(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| editor.undo(&Undo, window, cx));
}

fn redo(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| editor.redo(&Redo, window, cx));
}

/// The echo as shown now, without running the other tasks.
fn echo_now(cx: &mut VisualTestContext) -> Option<String> {
    let strip = cx
        .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip");
    strip.read_with(cx, |strip, _| {
        strip.hint_text().map(|text| text.to_string())
    })
}

#[gpui::test]
async fn three_repeats_are_one_edit_and_one_undo_step(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    let buffer = buffer(&editor, &mut cx);
    let (edits, _subscription) = count_edits(&buffer, &mut cx);
    type_keys(&mut cx, "ctrl-u 3 ctrl-k");
    assert_eq!(progress(&mut cx), None);
    assert_eq!(text(&editor, &mut cx), "xxxabc");
    assert_eq!(edits.get(), 1);
    assert!(no_open_transaction(&buffer, &mut cx));
    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), "abc");
}

#[gpui::test]
async fn a_long_run_is_sliced_and_one_undo_step(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    let buffer = buffer(&editor, &mut cx);
    let (edits, _subscription) = count_edits(&buffer, &mut cx);
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-k");
    assert_eq!(progress(&mut cx), Some((REPEATS_PER_SLICE, 1)));
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(50)));
    assert_eq!(edits.get(), 1);
    assert!(no_open_transaction(&buffer, &mut cx));
    step(&mut cx);
    assert_eq!(progress(&mut cx), Some((100, 2)));
    assert_eq!(edits.get(), 2);
    assert!(no_open_transaction(&buffer, &mut cx));
    step(&mut cx);
    assert_eq!(progress(&mut cx), None);
    assert_eq!(edits.get(), 3);
    assert!(no_open_transaction(&buffer, &mut cx));
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(120)));

    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), "abc");
    redo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(120)));
}

#[gpui::test]
async fn redo_puts_the_cursor_where_the_run_ended(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    let original = "abc".repeat(50);
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.set_text(original.clone(), window, cx);
        editor.move_to_beginning(&editor::actions::MoveToBeginning, window, cx);
    });
    // Each run of `Weave` puts an `x` before the next character and steps over it, so the
    // cursor ends past where any slice's edits sit.
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-w");
    cx.run_until_parked();
    let woven: String = original[..120].chars().map(|c| format!("x{c}")).collect();
    assert_eq!(
        text(&editor, &mut cx),
        format!("{woven}{}", &original[120..])
    );
    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), original);
    redo(&editor, &mut cx);
    cx.simulate_keystrokes("|");
    assert_eq!(
        text(&editor, &mut cx),
        format!("{woven}|{}", &original[120..])
    );
}

#[gpui::test]
async fn an_undo_between_slices_makes_the_next_slice_its_own_step(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-k");
    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), "abc");
    step(&mut cx);
    step(&mut cx);
    assert_eq!(progress(&mut cx), None);
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(70)));
    // The slices after the undo are one step of their own, and every edit stays undoable.
    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), "abc");
    redo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(70)));
}

#[gpui::test]
async fn a_transaction_pushed_between_slices_stays_its_own_step(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    let buffer = buffer(&editor, &mut cx);
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-k");
    buffer.update(&mut cx, |buffer, cx| {
        buffer.start_transaction();
        let end = buffer.len();
        buffer.edit([(end..end, "!")], None, cx);
        let id = buffer.end_transaction(cx).unwrap();
        let transaction = buffer.forget_transaction(id).unwrap();
        buffer.push_transaction(transaction, Instant::now());
    });
    step(&mut cx);
    step(&mut cx);
    assert_eq!(progress(&mut cx), None);
    assert_eq!(text(&editor, &mut cx), format!("{}abc!", xs(120)));
    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(120)));
    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), "abc");
}

#[gpui::test]
async fn keys_typed_during_a_run_are_held_and_run_in_order(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-k");
    type_keys(&mut cx, "ctrl-p ctrl-k ctrl-p");
    assert!(MARKS.with(|marks| marks.borrow().is_empty()));
    cx.run_until_parked();
    assert_eq!(progress(&mut cx), None);
    assert_eq!(MARKS.with(|marks| marks.borrow().clone()), vec![120, 121]);
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(121)));
}

#[gpui::test]
async fn c_g_stops_the_run_and_keeps_what_was_done(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-k");
    type_keys(&mut cx, "ctrl-p ctrl-g");
    cx.run_until_parked();
    assert_eq!(progress(&mut cx), None);
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(50)));
    // The key held before the C-g ran once the run stopped.
    assert_eq!(MARKS.with(|marks| marks.borrow().clone()), vec![50]);
    assert_eq!(echo_now(&mut cx), None);
    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), "abc");
}

#[gpui::test]
async fn a_repeat_that_moves_focus_ends_the_run(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    type_keys(&mut cx, "ctrl-u 1 0 ctrl-l");
    cx.run_until_parked();
    assert_eq!(progress(&mut cx), None);
    assert_eq!(LEAVES.with(Cell::get), LEAVE_AT);
    assert_eq!(text(&editor, &mut cx), format!("{}abc", xs(LEAVE_AT)));
    undo(&editor, &mut cx);
    assert_eq!(text(&editor, &mut cx), "abc");
}

#[gpui::test]
async fn releasing_the_editor_mid_run_leaves_nothing_open(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = window(cx).await;
    let buffer = buffer(&editor, &mut cx);
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-k");
    let id = editor.entity_id();
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.active_pane().update(cx, |pane, cx| {
            pane.remove_item(id, false, false, window, cx);
        });
    });
    drop(editor);
    cx.run_until_parked();
    assert_eq!(progress(&mut cx), None);
    assert!(no_open_transaction(&buffer, &mut cx));
    assert_eq!(
        buffer.read_with(&cx, |buffer, _| buffer.text()),
        format!("{}abc", xs(50))
    );
    // A later edit and its undo work on the same buffer.
    buffer.update(&mut cx, |buffer, cx| {
        buffer.edit([(0..0, "q")], None, cx);
        assert!(buffer.text().starts_with('q'));
        buffer.undo(cx);
    });
    assert_eq!(
        buffer.read_with(&cx, |buffer, _| buffer.text()),
        format!("{}abc", xs(50))
    );
}

#[gpui::test]
async fn a_slice_without_progress_ends_the_run(cx: &mut TestAppContext) {
    let (_workspace, _editor, mut cx) = window(cx).await;
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-n");
    cx.run_until_parked();
    assert_eq!(progress(&mut cx), None);
    assert_eq!(NOTHINGS.with(Cell::get), REPEATS_PER_SLICE);
}

#[gpui::test]
async fn a_zero_count_runs_nothing(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    type_keys(&mut cx, "ctrl-u 0 ctrl-k");
    cx.run_until_parked();
    assert_eq!(text(&editor, &mut cx), "abc");
    assert_eq!(
        cx.update(|window, cx| prefix_arg::pending(window, cx)),
        None
    );
}

#[gpui::test]
async fn a_negative_count_runs_its_size(cx: &mut TestAppContext) {
    let (_workspace, editor, mut cx) = window(cx).await;
    type_keys(&mut cx, "ctrl-u - 3 ctrl-k");
    cx.run_until_parked();
    assert_eq!(text(&editor, &mut cx), "xxxabc");
}

#[gpui::test]
async fn the_echo_shows_progress_and_clears(cx: &mut TestAppContext) {
    let (_workspace, _editor, mut cx) = window(cx).await;
    type_keys(&mut cx, "ctrl-u 1 2 0 ctrl-k");
    assert_eq!(
        echo_now(&mut cx).as_deref(),
        Some("C-u 120: 50/120, C-g stops")
    );
    step(&mut cx);
    assert_eq!(
        echo_now(&mut cx).as_deref(),
        Some("C-u 120: 100/120, C-g stops")
    );
    step(&mut cx);
    assert_eq!(progress(&mut cx), None);
    assert_eq!(echo_now(&mut cx), None);
}
