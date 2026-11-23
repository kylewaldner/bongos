//! Emacs-style undo grouping. With `undo_tree.grouping` set to `emacs`, a new buffer does not group
//! edits by time (its group interval is zero), and a run of typing is merged into one undo step
//! here instead: up to 20 characters typed one after the other in one editor, with no command, no
//! cursor move and no edit by anyone else in between. Every other command is one step of its own.
//! With `zed` nothing here acts, and the buffer's 300 ms time grouping stays.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_prompts::BeforeFinalize;
use editor::{Addon, Anchor, Editor, EditorEvent};
use gpui::{App, Context, Global, Window};
use language::{Buffer, TransactionId};
use settings::Settings as _;
use text::Selection;

use crate::settings::{UndoGrouping, UndoTreeSettings};

/// A typing run holds at most this many characters; the next one starts a new run.
pub const RUN_LIMIT: usize = 20;

type Selections = Arc<[Selection<Anchor>]>;

/// How many keystrokes with an action ran in the app. A command ends every typing run.
#[derive(Default)]
struct KeystrokeEpoch(u64);

impl Global for KeystrokeEpoch {}

/// The typing run of one editor.
struct Run {
    /// The run's first transaction; later keystrokes are merged into it.
    first: TransactionId,
    /// The characters the run holds.
    chars: usize,
    /// The buffer version right after the run's last keystroke.
    after: clock::Global,
    /// The selections right after the run's last keystroke.
    end: Selections,
    /// The keystroke epoch when the run's last keystroke was typed.
    epoch: u64,
}

#[derive(Default)]
struct State {
    /// Whether text was input in the transaction that has begun and not ended yet.
    input: bool,
    /// The characters input in that transaction.
    typed: usize,
    /// The transaction handled last, so that it is handled once.
    handled: Option<TransactionId>,
    run: Option<Run>,
}

/// The addon that carries an editor's typing run.
#[derive(Default)]
struct GroupingAddon {
    state: Rc<RefCell<State>>,
}

impl Addon for GroupingAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn emacs_grouping(cx: &App) -> bool {
    UndoTreeSettings::try_get(cx).is_some_and(|settings| settings.grouping == UndoGrouping::Emacs)
}

/// Turns the grouping on for new buffers and editors.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(
        |buffer: &mut Buffer, _: Option<&mut Window>, cx: &mut Context<Buffer>| {
            if emacs_grouping(cx) {
                buffer.set_group_interval(Duration::ZERO);
            }
        },
    )
    .detach();

    cx.observe_keystrokes(|event, _, cx| {
        if event.action.is_some() {
            cx.default_global::<KeystrokeEpoch>().0 += 1;
        }
    })
    .detach();

    cx.observe_new(
        |editor: &mut Editor, _: Option<&mut Window>, cx: &mut Context<Editor>| {
            if !editor.mode().is_full() {
                return;
            }
            editor.register_addon(GroupingAddon::default());
            let this = cx.entity();
            cx.subscribe(&this, |editor, _, event: &EditorEvent, cx| {
                on_event(editor, event, cx)
            })
            .detach();
        },
    )
    .detach();

    // The prompt guard finalizes every transaction it keeps, which ends its grouping, so the
    // merge must come first.
    cx.set_global(BeforeFinalize(on_edited));
}

fn on_event(editor: &mut Editor, event: &EditorEvent, cx: &mut Context<Editor>) {
    let Some(state) = editor
        .addon::<GroupingAddon>()
        .map(|addon| addon.state.clone())
    else {
        return;
    };
    match event {
        EditorEvent::TransactionBegun { .. } => {
            let mut state = state.borrow_mut();
            state.input = false;
            state.typed = 0;
        }
        EditorEvent::InputHandled { text, .. } => {
            let mut state = state.borrow_mut();
            state.input = true;
            state.typed += text.chars().count();
        }
        EditorEvent::Edited { transaction_id } => on_edited(editor, *transaction_id, cx),
        _ => {}
    }
}

/// `EditorEvent::Edited`: merges the transaction into the typing run when it continues it, else
/// starts a new run (when the transaction is typing) or ends the run.
fn on_edited(editor: &mut Editor, transaction_id: TransactionId, cx: &mut Context<Editor>) {
    let Some(state) = editor
        .addon::<GroupingAddon>()
        .map(|addon| addon.state.clone())
    else {
        return;
    };
    let mut state = state.borrow_mut();
    if state.handled == Some(transaction_id) {
        return;
    }
    state.handled = Some(transaction_id);
    let typed = std::mem::take(&mut state.typed);
    let input = std::mem::take(&mut state.input);
    let previous = state.run.take();
    if !input || !emacs_grouping(cx) {
        return;
    }
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let Some((start_selections, Some(end))) = editor.transaction_selections(transaction_id) else {
        return;
    };
    let (start, version) = {
        let buffer = buffer.read(cx);
        let Some(transaction) = buffer.get_transaction(transaction_id) else {
            return;
        };
        (transaction.start.clone(), buffer.version())
    };
    let epoch = cx.try_global::<KeystrokeEpoch>().map_or(0, |epoch| epoch.0);
    let continued = previous.filter(|run| {
        run.epoch == epoch
            && run.after == start
            && run.chars < RUN_LIMIT
            && same_selections(&run.end, &start_selections)
    });
    let (first, chars) = match continued {
        Some(run) => {
            editor.group_until_transaction(run.first, cx);
            let merged = editor.buffer().read(cx).last_transaction_id(cx) == Some(run.first);
            if merged {
                // Redo puts the caret where the run ended, not where its first keystroke did.
                editor.modify_transaction_selection_history(run.first, |(_, after)| {
                    *after = Some(end.clone());
                });
                (run.first, run.chars + typed)
            } else {
                (transaction_id, typed)
            }
        }
        None => (transaction_id, typed),
    };
    state.run = Some(Run {
        first,
        chars,
        after: version,
        end,
        epoch,
    });
}

/// Whether both hold the same ranges (the ids and goals do not matter).
fn same_selections(a: &Selections, b: &Selections) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(a, b)| a.start == b.start && a.end == b.end)
}
