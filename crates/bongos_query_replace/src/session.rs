//! A replace running in an editor: the interactive one of `query-replace`, driven from the
//! decision view (see the end of this file), and the batch of `replace-string` and
//! `replace-regexp`: every match of the scope is found on the background executor and replaced
//! on the main thread in chunks of [`crate::engine::SCAN_CHUNK`], one undo entry each, yielding
//! between chunks so the window keeps drawing. At the end the entries are merged into one undo
//! step, the cursor goes to the end of the last replacement and the count goes to the echo area.
//!
//! While a batch runs, its editor has the [`BATCH_KEY_CONTEXT`] key context (through an editor
//! addon), where `C-g` is [`Abort`]: the batch stops before its next chunk and the replacements
//! already made stay.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use bongos_isearch::display::Display;
use bongos_isearch::engine::Cancel;
use bongos_query_replace_model::session::{Effect, End, Found, Key, Phase, Reason, Session};
use bongos_query_replace_model::template::Template;
use bongos_query_replace_model::text::{counter, decision_prompt, end_message, help};
use editor::{Addon, Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    Action, App, AppContext as _, Entity, EntityId, Global, KeyContext, Subscription, WeakEntity,
    Window,
};
use multi_buffer::{Anchor, MultiBufferOffset};
use project::search::SearchQuery;
use workspace::Workspace;

use crate::actions::{Abort, Exit, Help, No, ThisAndExit, Yes};
use crate::display::{self, Shown};
use crate::edits::{Edits, READ_ONLY, Replace, match_anchors};
use crate::engine::{self, Replacer, ScanEnd};
use crate::view::QueryReplaceView;

/// The key context of an editor while a batch replaces in it.
pub const BATCH_KEY_CONTEXT: &str = "QueryReplaceBatch";

/// Marks an editor whose batch is running, and holds the flag that stops it.
struct Batch {
    cancel: Cancel,
}

impl Addon for Batch {
    fn extend_key_context(&self, context: &mut KeyContext, _: &App) {
        context.add(BATCH_KEY_CONTEXT);
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

/// Registers [`Abort`] on every editor: it stops the editor's running batch, and passes the key on
/// when there is none.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        register_key::<Yes>(workspace, Input::Key(Key::Yes));
        register_key::<No>(workspace, Input::Key(Key::No));
        register_key::<ThisAndExit>(workspace, Input::Key(Key::ThisAndExit));
        register_key::<Exit>(workspace, Input::Key(Key::Exit));
        register_key::<Abort>(workspace, Input::Key(Key::Abort));
        register_key::<Help>(workspace, Input::Help);
    })
    .detach();
    cx.observe_new(|editor: &mut Editor, _, cx| {
        let weak = cx.entity().downgrade();
        editor
            .register_action(move |_: &Abort, _, cx| {
                let cancel = weak.upgrade().and_then(|editor| {
                    editor
                        .read(cx)
                        .addon::<Batch>()
                        .map(|batch| batch.cancel.clone())
                });
                match cancel {
                    Some(cancel) => cancel.cancel(),
                    None => cx.propagate(),
                }
            })
            .detach();
    })
    .detach();
}

/// Whether a batch is replacing in `editor` now.
pub fn batch_running(editor: &Editor) -> bool {
    editor.addon::<Batch>().is_some()
}

/// What a batch replaces: every match of `query` in `scope` from `first`, the first match, found
/// on the editor's current text. `preserve_case` and `template` make the replacements.
pub(crate) struct BatchRequest {
    pub query: Arc<SearchQuery>,
    pub scope: Range<Anchor>,
    pub first: Found,
    pub preserve_case: bool,
    pub template: Template,
}

/// Replaces every match of `request` in `editor` without asking, as one undo step.
pub(crate) fn run_batch(
    editor: &Entity<Editor>,
    request: BatchRequest,
    window: &mut Window,
    cx: &mut App,
) {
    let BatchRequest {
        query,
        scope,
        first,
        preserve_case,
        template,
    } = request;
    let mut model = Session::new(preserve_case, template.clone());
    model.on_match(Some(first));
    let from = match model.on_key(Key::All).as_slice() {
        [Effect::StartBatch { from }] => from.start,
        _ => return,
    };

    let cancel = Cancel::new();
    let (mut edits, snapshot) = editor.update(cx, |editor, cx| {
        editor.register_addon(Batch {
            cancel: cancel.clone(),
        });
        cx.notify();
        (
            Edits::start(editor, cx),
            editor.buffer().read(cx).snapshot(cx),
        )
    });

    let (sender, mut chunks) = mpsc::unbounded();
    let scan = cx.background_spawn({
        let cancel = cancel.clone();
        async move {
            // The model session that computes the replacements, a twin of the one that counts.
            let session = Session::new(preserve_case, template);
            let replacer = Replacer {
                session: &session,
                counter: 0,
            };
            engine::scan(
                &snapshot,
                &query,
                MultiBufferOffset(from),
                &scope,
                &cancel,
                Some(replacer),
                |chunk| {
                    sender.unbounded_send(chunk).ok();
                },
            )
        }
    });

    let editor = editor.downgrade();
    window
        .spawn(cx, async move |cx| {
            let mut replaced = 0;
            let mut end = None;
            let mut stopped = false;
            let mut read_only = None;
            while let Some(chunk) = chunks.next().await {
                if cancel.is_canceled() {
                    stopped = true;
                    break;
                }
                let applied = editor.update_in(cx, |editor, window, cx| {
                    edits.apply_chunk(editor, chunk, window, cx)
                });
                match applied {
                    Ok(Ok(applied)) => {
                        replaced += applied.replaced;
                        end = applied.end.or(end);
                        model.on_batch_progress(replaced);
                    }
                    Ok(Err(error)) => {
                        read_only = Some(error);
                        break;
                    }
                    // The editor or its window is gone.
                    Err(_) => {
                        cancel.cancel();
                        return;
                    }
                }
                yield_now().await;
            }
            let interrupted = if stopped || read_only.is_some() {
                cancel.cancel();
                stopped
            } else {
                matches!(scan.await, ScanEnd::Canceled { .. })
            };

            let finished = editor.update_in(cx, |editor, window, cx| {
                editor.unregister_addon::<Batch>();
                // The cursor goes to the end of the last replacement, which also ends the region.
                if let Some(end) = end {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::fit()),
                        window,
                        cx,
                        |s| s.select_anchor_ranges([end..end]),
                    );
                }
                edits.finish(editor, cx);
                cx.notify();
            });
            if finished.is_err() {
                return;
            }
            let message = match read_only {
                Some(error) => error.to_string(),
                None => model
                    .on_batch_end(interrupted)
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::Finish(end) => Some(end_message(end)),
                        _ => None,
                    })
                    .unwrap_or_default(),
            };
            cx.update(|window, cx| bongos_minibuffer::message(window, cx, message))
                .ok();
        })
        .detach();
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

// The interactive replace of `query-replace`.

/// The most matches the counter counts; past it the counter shows `n/10000+`.
const COUNT_CAP: usize = 10_000;

/// What the decision view asks for: the first match and everything the driver needs to go on.
pub(crate) struct InteractiveRequest {
    pub query: Arc<SearchQuery>,
    pub scope: Range<Anchor>,
    pub first: Found,
    pub preserve_case: bool,
    pub template: Template,
    /// The string to replace, as typed.
    pub from: String,
    pub regexp: bool,
    /// Whether the scope is a region the user made, which is kept visible.
    pub region: bool,
}

/// One thing the user asked for in the decision view.
#[derive(Clone, Copy, Debug)]
enum Input {
    Key(Key),
    Help,
}

/// The open interactive replace of a workspace.
struct Interactive {
    editor: WeakEntity<Editor>,
    workspace: WeakEntity<Workspace>,
    view: WeakEntity<QueryReplaceView>,
    model: Session,
    query: Arc<SearchQuery>,
    scope: Range<Anchor>,
    from: String,
    regexp: bool,
    edits: Option<Edits>,
    /// The replacement of the last decision, highlighted until the next one.
    replacement: Option<Range<Anchor>>,
    /// The folds and collapsed excerpts the session opened.
    folds: Display,
    _editor_events: Option<Subscription>,
    /// The match asked about, found and on show.
    current: Option<(Found, Range<Anchor>)>,
    /// Where the cursor goes when the session runs out of matches or ends on a replacement: the end
    /// of the last replacement or of the last match skipped.
    rest: Option<Anchor>,
    /// What was pressed while a find was running, run in order as the match lands.
    queued: VecDeque<Input>,
    find: Option<Cancel>,
    /// The number of matches and whether the count stopped at the cap, once known.
    total: Option<(usize, bool)>,
    count: Cancel,
    done: bool,
}

type Shared = Rc<RefCell<Interactive>>;

/// The open interactive replace of each workspace, by the workspace's entity id.
#[derive(Default)]
struct Sessions(HashMap<EntityId, Shared>);

impl Global for Sessions {}

fn session_of(workspace: EntityId, cx: &mut App) -> Option<Shared> {
    cx.default_global::<Sessions>().0.get(&workspace).cloned()
}

fn forget(state: &Shared, workspace: EntityId, cx: &mut App) {
    let sessions = &mut cx.default_global::<Sessions>().0;
    if sessions
        .get(&workspace)
        .is_some_and(|open| Rc::ptr_eq(open, state))
    {
        sessions.remove(&workspace);
    }
}

/// Whether an interactive replace is open in `workspace`.
pub fn interactive_open(workspace: EntityId, cx: &mut App) -> bool {
    session_of(workspace, cx).is_some()
}

fn register_key<A: Action>(workspace: &mut Workspace, input: Input) {
    workspace.register_action(move |_, _: &A, window, cx| {
        let Some(state) = session_of(cx.entity_id(), cx) else {
            cx.propagate();
            return;
        };
        press(&state, input, window, cx);
    });
}

/// Opens the decision view on the first match of `request`, in the workspace of `editor`.
pub(crate) fn open(
    editor: &Entity<Editor>,
    workspace: &Entity<Workspace>,
    request: InteractiveRequest,
    window: &mut Window,
    cx: &mut App,
) {
    let InteractiveRequest {
        query,
        scope,
        first,
        preserve_case,
        template,
        from,
        regexp,
        region,
    } = request;
    let mut model = Session::new(preserve_case, template);
    model.on_match(Some(first.clone()));
    let edits = editor.update(cx, |editor, cx| Edits::start(editor, cx));
    let workspace_id = workspace.entity_id();
    let state: Shared = Rc::new(RefCell::new(Interactive {
        editor: editor.downgrade(),
        workspace: workspace.downgrade(),
        view: WeakEntity::new_invalid(),
        model,
        query,
        scope,
        from,
        regexp,
        edits: Some(edits),
        replacement: None,
        folds: Display::default(),
        _editor_events: None,
        current: None,
        rest: None,
        queued: VecDeque::new(),
        find: None,
        total: None,
        count: Cancel::new(),
        done: false,
    }));
    let on_dismiss = {
        let state = state.clone();
        Box::new(move |window: &mut Window, cx: &mut App| {
            let counts = state.borrow().model.counts();
            let end = End {
                reason: Reason::Exit,
                counts,
            };
            finish(&state, end, false, window, cx);
        })
    };
    let mut on_dismiss = Some(on_dismiss);
    workspace.update(cx, |workspace, cx| {
        workspace.hide_modal(window, cx);
        workspace.toggle_modal(window, cx, |_, cx| {
            QueryReplaceView::new("", on_dismiss.take().expect("built once"), cx)
        });
    });
    let Some(view) = workspace.read(cx).active_modal::<QueryReplaceView>(cx) else {
        // The modal could not open (the one before it refused to close): nothing was asked.
        let edits = state.borrow_mut().edits.take();
        if let Some(edits) = edits {
            editor.update(cx, |editor, cx| edits.finish(editor, cx));
        }
        return;
    };
    state.borrow_mut().view = view.downgrade();
    let events = window.subscribe(editor, cx, {
        let state = Rc::downgrade(&state);
        move |_, event: &EditorEvent, _, cx| {
            let Some(state) = state.upgrade() else {
                return;
            };
            if matches!(
                event,
                EditorEvent::ScrollPositionChanged { .. } | EditorEvent::BufferEdited
            ) {
                refresh_highlights(&state, cx);
            }
        }
    });
    state.borrow_mut()._editor_events = Some(events);
    if region {
        let scope = state.borrow().scope.clone();
        editor.update(cx, |editor, cx| {
            display::highlight_region(editor, &scope, cx)
        });
    }
    cx.default_global::<Sessions>()
        .0
        .insert(workspace_id, state.clone());
    start_count(&state, first.range.start, window, cx);
    land(&state, first, window, cx);
}

/// Counts the matches of the session in the background, up to [`COUNT_CAP`].
fn start_count(state: &Shared, from: usize, window: &mut Window, cx: &mut App) {
    let (editor, query, scope, cancel) = {
        let state = state.borrow();
        let Some(editor) = state.editor.upgrade() else {
            return;
        };
        (
            editor,
            state.query.clone(),
            state.scope.clone(),
            state.count.clone(),
        )
    };
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let task = cx.background_spawn(async move {
        let mut matches = 0;
        engine::scan(
            &snapshot,
            &query,
            MultiBufferOffset(from),
            &scope,
            &cancel,
            None,
            |chunk| {
                matches += chunk.len();
                if matches >= COUNT_CAP {
                    cancel.cancel();
                }
            },
        );
        matches
    });
    let state = state.clone();
    window
        .spawn(cx, async move |cx| {
            let matches = task.await;
            cx.update(|window, cx| {
                if state.borrow().done {
                    return;
                }
                let capped = matches >= COUNT_CAP;
                state.borrow_mut().total = Some((matches.min(COUNT_CAP), capped));
                refresh_status(&state, window, cx);
            })
            .ok();
        })
        .detach();
}

/// Sets the status: `…` while a find runs, else the counter.
fn refresh_status(state: &Shared, _: &mut Window, cx: &mut App) {
    let (view, status) = {
        let state = state.borrow();
        let status = if state.find.is_some() {
            "…".to_string()
        } else {
            let counts = state.model.counts();
            let index = counts.replaced + counts.skipped + 1;
            match state.total {
                Some((total, capped)) => counter(index, Some(total), capped),
                None => counter(index, None, false),
            }
        };
        (state.view.clone(), status)
    };
    view.update(cx, |view, cx| view.set_status(status, cx)).ok();
}

/// A key or `?` in the decision view.
fn press(state: &Shared, input: Input, window: &mut Window, cx: &mut App) {
    let (done, finding, view) = {
        let state = state.borrow();
        (state.done, state.find.is_some(), state.view.clone())
    };
    if done {
        return;
    }
    // Any key but `?` itself takes the list of keys away.
    if !matches!(input, Input::Help) {
        view.update(cx, |view, cx| {
            if !view.help_lines().is_empty() {
                view.set_help(Vec::new(), cx);
            }
        })
        .ok();
    }
    if finding {
        state.borrow_mut().queued.push_back(input);
        return;
    }
    apply(state, input, window, cx);
}

fn apply(state: &Shared, input: Input, window: &mut Window, cx: &mut App) {
    match input {
        Input::Help => {
            let view = state.borrow().view.clone();
            view.update(cx, |view, cx| view.set_help(help(), cx)).ok();
        }
        Input::Key(key) => {
            let effects = {
                let mut state = state.borrow_mut();
                state.replacement = None;
                // A skipped match is where the cursor ends if no more come.
                if key == Key::No && state.model.phase() == &Phase::Asking {
                    state.rest = state.current.as_ref().map(|(_, anchors)| anchors.end);
                }
                state.model.on_key(key)
            };
            run(state, effects, window, cx);
        }
    }
}

/// Does what the model asked for, in order, before anything else is sent to it.
fn run(state: &Shared, effects: Vec<Effect>, window: &mut Window, cx: &mut App) {
    for effect in effects {
        match effect {
            Effect::Replace { with, .. } => {
                if let Err(message) = replace(state, &with, window, cx) {
                    let counts = state.borrow().model.counts();
                    let end = End {
                        reason: Reason::Aborted,
                        counts,
                    };
                    finish_with(state, end, Some(message), true, window, cx);
                    return;
                }
            }
            Effect::Find { from } => {
                find(state, from, window, cx);
                return;
            }
            Effect::Finish(end) => {
                finish(state, end, true, window, cx);
                return;
            }
            // The keys that make these are not bound yet.
            Effect::UndoLast { .. }
            | Effect::Show(_)
            | Effect::StartBatch { .. }
            | Effect::StopBatch
            | Effect::Notice(_) => {}
        }
    }
}

/// Replaces the match on show with `with`, as its own undo entry.
fn replace(state: &Shared, with: &str, window: &mut Window, cx: &mut App) -> Result<(), String> {
    let (editor, query, scope, current, edits) = {
        let mut state = state.borrow_mut();
        (
            state.editor.upgrade(),
            state.query.clone(),
            state.scope.clone(),
            state.current.take(),
            state.edits.take(),
        )
    };
    let (Some(editor), Some((found, anchors)), Some(mut edits)) = (editor, current, edits) else {
        return Ok(());
    };
    let result = editor.update(cx, |editor, cx| {
        edits.replace(
            editor,
            &query,
            &scope,
            anchors.clone(),
            &found,
            with,
            window,
            cx,
        )
    });
    let mut state = state.borrow_mut();
    state.edits = Some(edits);
    match result {
        Ok(Replace::Replaced(replacement)) => {
            state
                .model
                .on_replaced(replacement.model_id(), replacement.range.clone());
            state.rest = Some(replacement.anchors.end);
            state.replacement = Some(replacement.anchors.clone());
        }
        Ok(Replace::Changed(_)) => state.rest = Some(anchors.end),
        Err(_) => return Err(READ_ONLY.to_string()),
    }
    Ok(())
}

/// Searches for the next match from `from` on the background executor; the keys pressed meanwhile
/// wait for it.
fn find(state: &Shared, from: usize, window: &mut Window, cx: &mut App) {
    let (editor, query, scope) = {
        let state = state.borrow();
        let Some(editor) = state.editor.upgrade() else {
            return;
        };
        (editor, state.query.clone(), state.scope.clone())
    };
    let cancel = Cancel::new();
    state.borrow_mut().find = Some(cancel.clone());
    refresh_status(state, window, cx);
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let task = cx.background_spawn(async move {
        engine::next(&snapshot, &query, MultiBufferOffset(from), &scope, &cancel)
    });
    let state = state.clone();
    window
        .spawn(cx, async move |cx| {
            let found = task.await;
            cx.update(|window, cx| landed(&state, found, window, cx))
                .ok();
        })
        .detach();
}

/// The result of a find: lands on the match, then runs the keys that waited.
fn landed(state: &Shared, found: Option<Found>, window: &mut Window, cx: &mut App) {
    {
        let mut state = state.borrow_mut();
        if state.done {
            return;
        }
        state.find = None;
    }
    let (effects, asking) = {
        let mut state = state.borrow_mut();
        let effects = state.model.on_match(found.clone());
        (effects, state.model.phase() == &Phase::Asking)
    };
    if !effects.is_empty() {
        run(state, effects, window, cx);
    } else if let (true, Some(found)) = (asking, found) {
        land(state, found, window, cx);
    }
    // Keys pressed while the find ran, one at a time: a key that starts another find leaves the
    // rest queued for it.
    loop {
        let input = {
            let mut state = state.borrow_mut();
            if state.done || state.find.is_some() {
                return;
            }
            state.queued.pop_front()
        };
        match input {
            Some(input) => apply(state, input, window, cx),
            None => return,
        }
    }
}

/// Shows the match: the question on the line, and the cursor at its end with no selection,
/// centered when the match was off screen.
fn land(state: &Shared, found: Found, window: &mut Window, cx: &mut App) {
    let Some(editor) = state.borrow().editor.upgrade() else {
        return;
    };
    // The folds are taken out of the state meanwhile so the editor events that come of the folding
    // can look at the rest.
    let mut folds = std::mem::take(&mut state.borrow_mut().folds);
    let anchors = editor.update(cx, |editor, cx| {
        folds.open(editor, &found.range, cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchors = match_anchors(&snapshot, &found.range);
        let start = snapshot.offset_to_point(MultiBufferOffset(found.range.start));
        let end = snapshot.offset_to_point(MultiBufferOffset(found.range.end));
        let display = editor.display_snapshot(cx);
        let visible = editor.multi_buffer_visible_range(&display, cx);
        drop(display);
        let autoscroll = if visible.start <= start && end <= visible.end {
            Autoscroll::fit()
        } else {
            Autoscroll::center()
        };
        let offset = MultiBufferOffset(found.range.end);
        let effects = SelectionEffects::scroll(autoscroll)
            .nav_history(false)
            .from_search(true);
        editor.change_selections(effects, window, cx, |s| s.select_ranges([offset..offset]));
        folds.refold(editor, Some(&found.range), window, cx);
        anchors
    });
    state.borrow_mut().folds = folds;
    let (view, prompt) = {
        let mut state = state.borrow_mut();
        let replacement = state.model.replacement_for(&found);
        let prompt = decision_prompt(&state.from, &replacement, state.regexp);
        state.current = Some((found, anchors));
        (state.view.clone(), prompt)
    };
    view.update(cx, |view, cx| view.set_prompt(prompt, cx)).ok();
    refresh_status(state, window, cx);
    refresh_highlights(state, cx);
}

/// Highlights the matches near the visible text, from the current match (or the last replacement)
/// on.
fn refresh_highlights(state: &Shared, cx: &mut App) {
    let (editor, query, scope, from, current, replacement) = {
        let state = state.borrow();
        if state.done {
            return;
        }
        let Some(editor) = state.editor.upgrade() else {
            return;
        };
        let current = state.current.as_ref().map(|(_, anchors)| anchors.clone());
        let from = current.as_ref().map(|anchors| anchors.start).or(state.rest);
        (
            editor,
            state.query.clone(),
            state.scope.clone(),
            from,
            current,
            state.replacement.clone(),
        )
    };
    editor.update(cx, |editor, cx| {
        let shown = Shown {
            query: &query,
            scope: &scope,
            from,
            current: current.as_ref(),
            replacement: replacement.as_ref(),
        };
        display::highlight(editor, shown, cx)
    });
}

fn finish(state: &Shared, end: End, hide: bool, window: &mut Window, cx: &mut App) {
    finish_with(state, end, None, hide, window, cx);
}

/// Ends the session once: the cursor goes where the point rules say, the edits become one undo
/// step, the view closes (`hide`) and the end message is shown.
fn finish_with(
    state: &Shared,
    end: End,
    message: Option<String>,
    hide: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let (editor, workspace, view, edits, current, rest, mut folds) = {
        let mut state = state.borrow_mut();
        if std::mem::replace(&mut state.done, true) {
            return;
        }
        if let Some(find) = state.find.take() {
            find.cancel();
        }
        state.count.cancel();
        (
            state.editor.upgrade(),
            state.workspace.clone(),
            state.view.upgrade(),
            state.edits.take(),
            state.current.take(),
            state.rest,
            std::mem::take(&mut state.folds),
        )
    };
    state.borrow_mut()._editor_events = None;
    if let Some(workspace) = workspace.upgrade() {
        forget(state, workspace.entity_id(), cx);
    }
    if let Some(editor) = editor {
        // On a match, the cursor is at its end already (`q`, RET, Escape, C-g); otherwise it goes
        // to the end of the last replacement or skipped match.
        let on_match = current.is_some() && matches!(end.reason, Reason::Exit | Reason::Aborted);
        editor.update(cx, |editor, cx| {
            if let (false, Some(rest)) = (on_match, rest) {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::fit()).nav_history(false),
                    window,
                    cx,
                    |s| s.select_anchor_ranges([rest..rest]),
                );
            }
            if let Some(edits) = edits {
                edits.finish(editor, cx);
            }
            display::clear(editor, cx);
            // The folds the session opened close again, but those around the cursor stay open.
            let cursor = editor
                .selections
                .newest::<MultiBufferOffset>(&editor.display_snapshot(cx));
            let cursor = cursor.head().0;
            folds.refold(
                editor,
                Some(&(cursor.saturating_sub(1)..cursor + 1)),
                window,
                cx,
            );
        });
    }
    let message = message.unwrap_or_else(|| end_message(&end));
    window.defer(cx, move |window, cx| {
        if hide && let Some(view) = view {
            workspace
                .update(cx, |workspace, cx| {
                    if workspace.active_modal::<QueryReplaceView>(cx) == Some(view) {
                        workspace.hide_modal(window, cx);
                    }
                })
                .ok();
        }
        bongos_minibuffer::message(window, cx, message);
    });
}
