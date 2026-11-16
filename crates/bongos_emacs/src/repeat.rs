//! Repeating an action that does not read the prefix argument: `C-u N` runs it N times (the sign
//! means nothing to a command with no count, and zero runs nothing).
//!
//! The repeats run in slices of `REPEATS_PER_SLICE`, synchronously; between slices the runner
//! yields so the window draws. In an editor each slice is one transaction, opened and closed
//! inside the slice, so no transaction is ever open across a frame. Each later slice is merged
//! into the first one, so the whole run is one undo step, and redo puts the cursor where the run
//! ended. A run of `REPEATS_PER_SLICE` or fewer is one slice and finishes before `start` returns.
//!
//! While a run is active in a window its keys are held (see `intercept`) and replayed in order
//! once it ends; C-g stops it after the repeat in progress, keeping what was done. A run also
//! ends after a repeat that moves focus out of its editor, when the editor is released, and
//! after a slice that changed neither the text nor the selections.

use std::future::poll_fn;
use std::task::Poll;
use std::time::Instant;

use bongos_emacs_model::prefix_arg::PrefixArg;
use collections::HashMap;
use editor::Editor;
use gpui::{
    Action, App, Entity, FocusHandle, Focusable as _, Global, Keystroke, WeakEntity, Window,
    WindowId,
};
use language::TransactionId;

use crate::prefix_arg;

/// The repeats run between two frames.
pub(crate) const REPEATS_PER_SLICE: usize = 50;

/// Actions that walk the undo history themselves, which an open transaction forbids: they repeat
/// with no transaction around them.
const UNDO_HISTORY_ACTIONS: [&str; 2] = ["editor::Undo", "editor::Redo"];

/// The run active in a window, as the key interceptor and the quit hook see it.
#[derive(Default)]
struct Run {
    /// Set by C-g: the run ends after the repeat in progress.
    stop: bool,
    /// The keys typed during the run, in order.
    held: Vec<Keystroke>,
    /// Repeats done so far.
    done: usize,
    /// Slices run so far.
    slices: usize,
}

#[derive(Default)]
struct Runs(HashMap<WindowId, Run>);

impl Global for Runs {}

fn window_id(window: &Window) -> WindowId {
    window.window_handle().window_id()
}

fn run_mut<'a>(window: &Window, cx: &'a mut App) -> Option<&'a mut Run> {
    let id = window_id(window);
    cx.has_global::<Runs>()
        .then(|| cx.global_mut::<Runs>())
        .and_then(|runs| runs.0.get_mut(&id))
}

/// Whether a repeat runs in `window`.
pub(crate) fn is_running(window: &Window, cx: &App) -> bool {
    cx.try_global::<Runs>()
        .is_some_and(|runs| runs.0.contains_key(&window_id(window)))
}

/// Holds `keystroke`, typed while a repeat runs in `window`, until the run ends.
pub(crate) fn hold(window: &Window, cx: &mut App, keystroke: Keystroke) {
    if let Some(run) = run_mut(window, cx) {
        run.held.push(keystroke);
    }
}

/// Stops the repeat running in `window`, if any, after the repeat in progress.
pub(crate) fn stop(window: &Window, cx: &mut App) {
    if let Some(run) = run_mut(window, cx) {
        run.stop = true;
    }
}

/// The repeats done and the slices run by the repeat active in `window`.
#[cfg(test)]
pub(crate) fn progress(window: &Window, cx: &App) -> Option<(usize, usize)> {
    cx.try_global::<Runs>()
        .and_then(|runs| runs.0.get(&window_id(window)))
        .map(|run| (run.done, run.slices))
}

fn stopped(window: &Window, cx: &App) -> bool {
    cx.try_global::<Runs>()
        .and_then(|runs| runs.0.get(&window_id(window)))
        .is_none_or(|run| run.stop)
}

/// Whether the run goes on after a slice.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Done,
}

struct Runner {
    action: Box<dyn Action>,
    /// Where the repeats are dispatched; the run ends when focus leaves it.
    focus: FocusHandle,
    /// The editor the run edits, when it started in one.
    origin: Option<WeakEntity<Editor>>,
    /// Whether each slice is a transaction.
    transact: bool,
    total: usize,
    done: usize,
    /// The transaction later slices are merged into.
    destination: Option<TransactionId>,
    /// The argument's echo, which the progress echo starts with.
    echo: String,
}

/// Runs `action` as many times as `arg` says, on `origin` (or on the focused element when there
/// is no editor), and consumes the argument. A count beyond one slice goes on after this
/// returns.
pub(crate) fn start(
    window: &mut Window,
    cx: &mut App,
    action: &dyn Action,
    arg: PrefixArg,
    origin: Option<&Entity<Editor>>,
) {
    prefix_arg::cancel(window, cx);
    let total = arg.value().unsigned_abs() as usize;
    if total == 0 || is_running(window, cx) {
        return;
    }
    let focus = match origin {
        Some(editor) => editor.focus_handle(cx),
        None => match window.focused(cx) {
            Some(focus) => focus,
            None => return,
        },
    };
    cx.default_global::<Runs>()
        .0
        .insert(window_id(window), Run::default());
    let mut runner = Runner {
        action: action.boxed_clone(),
        focus,
        origin: origin.map(Entity::downgrade),
        transact: !UNDO_HISTORY_ACTIONS.contains(&action.name()),
        total,
        done: 0,
        destination: None,
        echo: arg.echo(),
    };
    if runner.slice(window, cx) == Flow::Done {
        finish(window, cx, false);
        return;
    }
    runner.show_progress(window, cx);
    window
        .spawn(cx, async move |cx| {
            loop {
                yield_now().await;
                let flow = cx.update(|window, cx| {
                    let flow = runner.slice(window, cx);
                    if flow == Flow::Continue {
                        runner.show_progress(window, cx);
                    }
                    flow
                });
                if flow.ok() != Some(Flow::Continue) {
                    break;
                }
            }
            cx.update(|window, cx| finish(window, cx, true)).ok();
        })
        .detach();
}

/// Ends the run of `window`: clears its progress echo (`echoed`) and replays the keys held
/// during it, in order.
fn finish(window: &mut Window, cx: &mut App, echoed: bool) {
    let id = window_id(window);
    let run = cx
        .has_global::<Runs>()
        .then(|| cx.global_mut::<Runs>().0.remove(&id))
        .flatten();
    if echoed {
        bongos_minibuffer::set_prefix_echo(window, cx, None);
    }
    for keystroke in run.map(|run| run.held).unwrap_or_default() {
        window.dispatch_keystroke(keystroke, cx);
    }
}

impl Runner {
    /// Runs one slice: up to `REPEATS_PER_SLICE` repeats, as one transaction merged into the
    /// run's first one. Nothing stays open when it returns.
    fn slice(&mut self, window: &mut Window, cx: &mut App) -> Flow {
        let editor = match &self.origin {
            Some(origin) => match origin.upgrade() {
                Some(editor) => Some(editor),
                None => return Flow::Done,
            },
            None => None,
        };
        if stopped(window, cx) || !self.focus.is_focused(window) {
            return Flow::Done;
        }
        let before = editor.as_ref().map(|editor| progress_mark(editor, cx));
        let transaction = editor.as_ref().filter(|_| self.transact);
        if let Some(editor) = transaction {
            editor.update(cx, |editor, cx| {
                editor.finalize_last_transaction(cx);
                editor.start_transaction_at(Instant::now(), window, cx);
            });
        }
        let mut flow = Flow::Continue;
        let mut ran = 0;
        while ran < REPEATS_PER_SLICE && self.done < self.total {
            if stopped(window, cx) {
                flow = Flow::Done;
                break;
            }
            self.focus.dispatch_action(self.action.as_ref(), window, cx);
            self.done += 1;
            ran += 1;
            if !self.focus.is_focused(window) {
                flow = Flow::Done;
                break;
            }
        }
        if let Some(editor) = transaction {
            self.close(editor, cx);
        }
        let done = self.done;
        if let Some(run) = run_mut(window, cx) {
            run.done = done;
            run.slices += 1;
        }
        if self.done >= self.total {
            flow = Flow::Done;
        }
        if let (Some(editor), Some(before)) = (&editor, before)
            && progress_mark(editor, cx) == before
        {
            flow = Flow::Done;
        }
        flow
    }

    /// Closes the slice's transaction and merges it into the run's first one. When that one is
    /// gone (an undo moved it to the redo stack, which this slice's edits then cleared), this
    /// slice becomes the one later slices merge into, so no edit is left without an undo step.
    fn close(&mut self, editor: &Entity<Editor>, cx: &mut App) {
        let destination = self.destination;
        let merged = editor.update(cx, |editor, cx| {
            let slice = editor.end_transaction_at(Instant::now(), cx);
            editor.finalize_last_transaction(cx);
            let slice = slice?;
            let Some(destination) = destination.filter(|destination| {
                *destination != slice && editor.buffer().read(cx).has_transaction(*destination, cx)
            }) else {
                return Some(slice);
            };
            editor.buffer().update(cx, |buffer, cx| {
                buffer.merge_transactions(slice, destination, cx)
            });
            // Redo puts the cursor where the run ended, not where its first slice did.
            let end = editor.selections.disjoint_anchors_arc();
            editor.modify_transaction_selection_history(destination, |(_, after)| {
                *after = Some(end);
            });
            None
        });
        if let Some(slice) = merged {
            self.destination = Some(slice);
        }
    }

    fn show_progress(&self, window: &mut Window, cx: &mut App) {
        let text = format!(
            "{}: {}/{}, C-g stops",
            self.echo, self.done, self.total
        );
        bongos_minibuffer::set_prefix_echo(window, cx, Some(text));
    }
}

/// What a slice that made progress changes: the text or the selections.
fn progress_mark(
    editor: &Entity<Editor>,
    cx: &App,
) -> (usize, std::sync::Arc<[text::Selection<multi_buffer::Anchor>]>) {
    let editor = editor.read(cx);
    let edits = editor.buffer().read(cx).snapshot(cx).edit_count();
    (edits, editor.selections.disjoint_anchors_arc())
}

/// Lets the other tasks of the main thread run (and the window draw) before going on.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}
