//! The isearch session: a string prompt of the minibuffer whose every change of the input moves
//! the origin editor's cursor to the nearest match. Focus stays in the prompt's input for the
//! whole session, so typing goes straight into it; every find and every count runs on the
//! background executor, and each new request cancels the find before it. The model
//! (`bongos_isearch_model::Isearch`) decides what to search and keeps the steps; a result is
//! applied only when it answers the model's newest request.
//!
//! A starter (`isearch::Forward` and the three others) saves the editor's selections, mark mode
//! and scroll for an abort, seeds the string from a selection, collapses to one cursor and opens
//! the session. Every exit closes the session, so `on_cancel` is the one place a session ends: it
//! reads the exit the command stored (none for a click outside or another modal, which exits at
//! the match), pushes the origin onto the mark ring and sets the final selection, or, for an
//! abort, puts everything back.
//!
//! While the session is open, a keystroke interceptor (`intercept`) watches its window. A key
//! that is not the session's ends it with an implicit exit: the session closes at the match and
//! the key runs again, once, in the origin editor.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_emacs_model::recenter::Cycle;
use bongos_isearch_model::{
    AbortOutcome, COUNT_CAP, Compiled, Direction, Isearch, Request, SearchSpec, compile, counter,
    from_input, prompt, to_input,
};
use bongos_minibuffer::{HistoryStore, Session, SessionHandle};
use editor::scroll::Autoscroll;
use editor::{Editor, EditorEvent, SelectionEffects};
use futures::future::{Either, select};
use gpui::{
    Action, App, AppContext as _, Entity, EntityId, Global, Keystroke, Subscription, Task,
    WeakEntity, Window,
};
use language::{Bias, Selection};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use project::search::SearchQuery;
use workspace::Workspace;

use crate::actions::{
    Abort, Backward, BackwardRegexp, DelChar, EditString, Exit, ExitOtherEnd, FirstMatch, Forward,
    ForwardRegexp, LastMatch, QuoteNewline, Recenter, RepeatBackward, RepeatForward, RingNext,
    RingPrevious, ToggleCaseFold, ToggleLaxWhitespace, ToggleRegexp, YankChar, YankKill, YankLine,
    YankSymbol, YankWord,
};
use crate::commands;
use crate::display::{self, Display};
use crate::engine::{Cancel, Count, Engine, FindOutcome, InvalidPattern, count, find_outcome};
use crate::intercept;
use crate::seams::push_origin;

/// The name the session is routed by (`minibuffer.enabled_for`).
pub const HOST: &str = "isearch";
/// The key context the session declares, which its key block names.
pub const KEY_CONTEXT: &str = "Isearch";
/// The history of text searches.
pub const TEXT_HISTORY: &str = "isearch";
/// The history of regexp searches.
pub const REGEXP_HISTORY: &str = "isearch-regexp";
/// The status while a find is still running when a frame is drawn.
pub const PENDING: &str = "\u{2026}";
/// How long a find runs before the status says it is still running (about a frame).
const PENDING_DELAY: Duration = Duration::from_millis(16);

/// The history ring of a search type.
pub fn history_name(regexp: bool) -> &'static str {
    if regexp { REGEXP_HISTORY } else { TEXT_HISTORY }
}

/// How the session is to end, stored by the command before it closes the session.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ExitKind {
    /// RET: the match selected, the cursor at its end (its start going backward).
    Select,
    /// C-RET: the cursor at the other end of the match, no selection.
    OtherEnd,
    /// C-g: everything as it was before the search.
    Abort,
    /// A key that is not the session's: the cursor at the match end (its start going backward),
    /// no selection, and then the keys run again in the origin editor.
    Implicit(Vec<Keystroke>),
}

/// What an abort puts back.
#[derive(Clone)]
struct Saved {
    selections: Arc<[Selection<Anchor>]>,
    mark_mode: bool,
    scroll: gpui::Point<f64>,
}

/// The find in flight, or the last one.
struct Find {
    generation: u64,
    cancel: Cancel,
    running: bool,
    _task: Task<()>,
}

/// The count of the matches of one compiled pattern, running or done.
struct Counting {
    key: Compiled,
    result: Option<Count>,
    cancel: Cancel,
    _task: Task<()>,
}

/// Timings kept for the tests: how long each `on_input` took on the main thread, and how many
/// running finds a newer request canceled.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct Stats {
    pub(crate) inputs: Vec<Duration>,
    pub(crate) canceled: usize,
}

pub(crate) struct SessionState {
    workspace: EntityId,
    pub(crate) editor: WeakEntity<Editor>,
    /// Where the cursor was when the search started: the mark an exit pushes.
    origin: Anchor,
    saved: Saved,
    /// The text the model's offsets are of, so a buffer change can move them.
    snapshot: MultiBufferSnapshot,
    /// The folds and collapsed buffers the session has opened.
    display: Display,
    pub(crate) model: Isearch,
    engine: Engine,
    pub(crate) handle: Option<SessionHandle>,
    /// The string the model was last given, as the input shows it (with newlines).
    pub(crate) string: String,
    /// The prompt last shown.
    pub(crate) prompt: String,
    /// The message of the pattern that does not compile, while the string is invalid.
    invalid: Option<String>,
    pending_exit: Option<ExitKind>,
    /// The exit waits for the running find, so it acts on the string as typed.
    exit_after_find: bool,
    /// Repeats pressed while a find was running, run in order as each find lands.
    pub(crate) queued_repeats: Vec<Direction>,
    /// Yanks pressed while a find was running, run in order as each find lands.
    pub(crate) queued_yanks: Vec<Command>,
    find: Option<Find>,
    counting: Option<Counting>,
    /// The history entry shown, while M-p and M-n walk the ring.
    pub(crate) ring_position: Option<usize>,
    /// The string typed before the ring was walked.
    ring_saved: String,
    /// The string as it was when M-e began, while the input is being edited. The interceptor
    /// lets every key through and the string is not searched until RET, C-s or C-r resume.
    pub(crate) editing: Option<String>,
    /// The input as edited, while editing.
    pub(crate) edit_text: String,
    /// The recenter position of the command before the one running, when that was C-l.
    pub(crate) cycle_before: Option<Cycle>,
    /// The recenter position the last command took, when it was C-l.
    pub(crate) last_cycle: Option<Cycle>,
    _release: Option<Subscription>,
    /// The keystroke interceptor, for as long as the session is open.
    _intercept: Option<Subscription>,
    /// The editor's scrolls and edits, for as long as the session is open.
    _editor_events: Option<Subscription>,
    #[cfg(test)]
    pub(crate) stats: Stats,
}

pub(crate) type Shared = Rc<RefCell<SessionState>>;

/// An in-session command, as a handler runs it.
pub(crate) type Command = fn(&Shared, &mut Window, &mut App);

impl SessionState {
    /// Whether a find is running, so the string on show is not the one searched yet.
    pub(crate) fn find_running(&self) -> bool {
        self.find.as_ref().is_some_and(|find| find.running)
    }
}

/// The open session of each workspace, by the workspace's entity id. A workspace shows one modal
/// at a time, so it has at most one session.
#[derive(Default)]
struct Sessions(HashMap<EntityId, Shared>);

impl Global for Sessions {}

/// The open session of a workspace.
pub(crate) fn session_of(workspace: EntityId, cx: &mut App) -> Option<Shared> {
    cx.default_global::<Sessions>().0.get(&workspace).cloned()
}

/// Forgets `state` as its workspace's session, unless another session took its place.
fn forget(state: &Shared, workspace: EntityId, cx: &mut App) {
    let sessions = &mut cx.default_global::<Sessions>().0;
    if sessions
        .get(&workspace)
        .is_some_and(|open| Rc::ptr_eq(open, state))
    {
        sessions.remove(&workspace);
    }
}

/// Registers the starters on every editor and the in-session commands on every workspace.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx| {
        let weak = cx.entity().downgrade();
        register_starter::<Forward>(editor, &weak, Direction::Forward, false);
        register_starter::<Backward>(editor, &weak, Direction::Backward, false);
        register_starter::<ForwardRegexp>(editor, &weak, Direction::Forward, true);
        register_starter::<BackwardRegexp>(editor, &weak, Direction::Backward, true);
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        // These three also act while the string is being edited: they resume or end the edit.
        register_in_session_always::<RepeatForward>(workspace, |state, window, cx| {
            repeat_command(state, Direction::Forward, window, cx)
        });
        register_in_session_always::<RepeatBackward>(workspace, |state, window, cx| {
            repeat_command(state, Direction::Backward, window, cx)
        });
        register_in_session_always::<Abort>(workspace, abort);
        register_in_session_always::<Exit>(workspace, |state, window, cx| {
            if state.borrow().editing.is_some() {
                commands::resume_editing(state, None, window, cx);
            } else {
                exit_command(state, ExitKind::Select, window, cx);
            }
        });
        register_in_session::<DelChar>(workspace, del_char);
        register_in_session::<ExitOtherEnd>(workspace, |state, window, cx| {
            exit_command(state, ExitKind::OtherEnd, window, cx)
        });
        register_in_session::<YankWord>(workspace, commands::yank_word);
        register_in_session::<YankChar>(workspace, commands::yank_char);
        register_in_session::<YankLine>(workspace, commands::yank_line);
        register_in_session::<YankSymbol>(workspace, commands::yank_symbol);
        register_in_session::<YankKill>(workspace, commands::yank_kill);
        register_in_session::<QuoteNewline>(workspace, commands::quote_newline);
        register_in_session::<EditString>(workspace, commands::edit_string);
        register_in_session::<ToggleCaseFold>(workspace, commands::toggle_case_fold);
        register_in_session::<ToggleRegexp>(workspace, commands::toggle_regexp);
        register_in_session::<ToggleLaxWhitespace>(workspace, commands::toggle_lax_whitespace);
        register_in_session::<FirstMatch>(workspace, commands::first_match);
        register_in_session::<LastMatch>(workspace, commands::last_match);
        register_in_session::<Recenter>(workspace, commands::recenter);
        register_in_session::<RingPrevious>(workspace, |state, window, cx| {
            walk_ring(state, true, window, cx)
        });
        register_in_session::<RingNext>(workspace, |state, window, cx| {
            walk_ring(state, false, window, cx)
        });
    })
    .detach();
}

/// A starter runs on a full editor that is in a workspace; anywhere else (a query editor, the
/// session's own input) it passes the key on.
fn register_starter<A: Action>(
    editor: &mut Editor,
    weak: &WeakEntity<Editor>,
    direction: Direction,
    regexp: bool,
) {
    let weak = weak.clone();
    editor
        .register_action(move |_: &A, window, cx| {
            let Some(editor) = weak.upgrade() else {
                return;
            };
            start(&editor, direction, regexp, window, cx);
        })
        .detach();
}

/// An in-session command acts on its workspace's session and passes the key on when there is
/// none, or while the string is being edited, so the key edits the input as in any prompt.
fn register_in_session<A: Action>(
    workspace: &mut Workspace,
    handler: impl Fn(&Shared, &mut Window, &mut App) + 'static,
) {
    register_in_session_with::<A>(workspace, false, handler);
}

/// Like [`register_in_session`], for the commands that also act while the string is edited.
fn register_in_session_always<A: Action>(
    workspace: &mut Workspace,
    handler: impl Fn(&Shared, &mut Window, &mut App) + 'static,
) {
    register_in_session_with::<A>(workspace, true, handler);
}

fn register_in_session_with<A: Action>(
    workspace: &mut Workspace,
    while_editing: bool,
    handler: impl Fn(&Shared, &mut Window, &mut App) + 'static,
) {
    workspace.register_action(move |_, _: &A, window, cx| {
        let Some(state) = session_of(cx.entity_id(), cx) else {
            cx.propagate();
            return;
        };
        {
            let mut state = state.borrow_mut();
            if state.editing.is_some() && !while_editing {
                drop(state);
                cx.propagate();
                return;
            }
            // Only a command right after C-l continues its cycle (Recenter sets it again).
            state.cycle_before = state.last_cycle.take();
        }
        handler(&state, window, cx);
    });
}

/// Opens a session on `editor`.
pub(crate) fn start(
    editor: &Entity<Editor>,
    direction: Direction,
    regexp: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = {
        let editor = editor.read(cx);
        if !editor.mode().is_full() {
            None
        } else {
            editor.workspace()
        }
    };
    let Some(workspace) = workspace else {
        cx.propagate();
        return;
    };

    let (saved, seed, origin, from) = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let saved = Saved {
            selections: editor.selections.all_anchors(&display),
            mark_mode: editor.selection_mark_mode(),
            scroll: editor.scroll_position(cx),
        };
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let newest = editor.selections.newest_anchor().clone();
        let start = newest.start.to_offset(&snapshot);
        let end = newest.end.to_offset(&snapshot);
        let head = newest.head();
        // A selection seeds the string, and the search starts past it, so the first match is
        // the next occurrence rather than the selection itself.
        let seed = (start != end).then(|| snapshot.text_for_range(start..end).collect::<String>());
        let from = match (&seed, direction) {
            (Some(_), Direction::Forward) => end,
            (Some(_), Direction::Backward) => start,
            (None, _) => head.to_offset(&snapshot),
        };
        // Mark mode stays off for the whole session (the mark push needs it off), and the
        // search runs from one cursor, the primary one.
        editor.set_selection_mark_mode(false, cx);
        editor.change_selections(
            SelectionEffects::no_scroll().nav_history(false),
            window,
            cx,
            |s| s.select_anchor_ranges([head..head]),
        );
        (saved, seed, head, from.0)
    });

    let model = Isearch::new(from, direction, SearchSpec::new(regexp));
    let first_prompt = prompt(model.current(), model.spec());
    let id = workspace.entity_id();
    let state = Rc::new(RefCell::new(SessionState {
        workspace: id,
        editor: editor.downgrade(),
        origin,
        saved,
        snapshot: editor.read(cx).buffer().read(cx).snapshot(cx),
        display: Display::default(),
        model,
        engine: Engine::new(),
        handle: None,
        string: String::new(),
        prompt: first_prompt.clone(),
        invalid: None,
        pending_exit: None,
        exit_after_find: false,
        queued_repeats: Vec::new(),
        queued_yanks: Vec::new(),
        find: None,
        counting: None,
        ring_position: None,
        ring_saved: String::new(),
        editing: None,
        edit_text: String::new(),
        cycle_before: None,
        last_cycle: None,
        _release: None,
        _intercept: None,
        _editor_events: None,
        #[cfg(test)]
        stats: Stats::default(),
    }));

    let weak = Rc::downgrade(&state);
    let session = Session::string(HOST, first_prompt)
        .history(history_name(regexp))
        .initial_input(to_input(seed.as_deref().unwrap_or("")))
        .key_context(KEY_CONTEXT)
        .on_input({
            let weak = weak.clone();
            move |text, handle, window, cx| {
                if let Some(state) = weak.upgrade() {
                    input_changed(&state, from_input(text), handle, window, cx);
                }
            }
        })
        // RET and C-j of the minibuffer itself, when a key block leaves them to it.
        .on_confirm({
            let weak = weak.clone();
            move |_, window, cx| {
                if let Some(state) = weak.upgrade() {
                    finish(&state, Some(ExitKind::Select), window, cx);
                }
            }
        })
        .on_cancel({
            let weak = weak.clone();
            move |window, cx| {
                if let Some(state) = weak.upgrade() {
                    finish(&state, None, window, cx);
                }
            }
        });

    // Registered before it opens: a session this one replaces ends later, deferred.
    cx.default_global::<Sessions>().0.insert(id, state.clone());
    let release = cx.observe_release(&workspace, {
        let weak = weak.clone();
        move |_, cx| {
            if let Some(state) = weak.upgrade() {
                forget(&state, id, cx);
            }
        }
    });
    state.borrow_mut()._release = Some(release);
    let handle = workspace.update(cx, |workspace, cx| {
        bongos_minibuffer::open(workspace, session, window, cx)
    });
    let interceptor = intercept::install(
        weak.clone(),
        window.window_handle(),
        workspace.downgrade(),
        cx,
    );
    let editor_events = window.subscribe(editor, cx, {
        move |_, event: &EditorEvent, window, cx| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            match event {
                EditorEvent::ScrollPositionChanged { .. } => scrolled(&state, cx),
                EditorEvent::BufferEdited => buffer_edited(&state, window, cx),
                _ => {}
            }
        }
    });
    {
        let mut state = state.borrow_mut();
        state.handle = Some(handle.clone());
        state._intercept = Some(interceptor);
        state._editor_events = Some(editor_events);
    }

    if let Some(seed) = seed {
        let request = {
            let mut state = state.borrow_mut();
            state.string = seed.clone();
            state.model.set_string(&seed)
        };
        run(&state, request, &handle, window, cx);
    }
}

pub(crate) fn handle_of(state: &Shared) -> Option<SessionHandle> {
    state.borrow().handle.clone()
}

/// The input changed (typed, yanked, edited): search for the new string. Only the model and a
/// spawned find happen here, so a character never waits for a search.
fn input_changed(
    state: &Shared,
    string: String,
    handle: &SessionHandle,
    window: &mut Window,
    cx: &mut App,
) {
    #[cfg(test)]
    let started = std::time::Instant::now();
    let request = {
        let mut state = state.borrow_mut();
        state.last_cycle = None;
        if state.editing.is_some() {
            // Edited freely: nothing is searched until the edit ends.
            state.edit_text = string;
            return;
        }
        if state.string == string {
            // The session set this input itself and has searched for it already.
            return;
        }
        state.string = string.clone();
        state.ring_position = None;
        state.model.set_string(&string)
    };
    run(state, request, handle, window, cx);
    #[cfg(test)]
    state.borrow_mut().stats.inputs.push(started.elapsed());
}

/// Cancels the running find. Its result, should it still arrive, is not applied.
pub(crate) fn stop_find(state: &mut SessionState) {
    if let Some(find) = &mut state.find
        && find.running
    {
        find.cancel.cancel();
        find.running = false;
        #[cfg(test)]
        {
            state.stats.canceled += 1;
        }
    }
}

/// Carries out a request of the model: a find on the background executor, or, when the model
/// already has the answer, showing it.
pub(crate) fn run(
    state: &Shared,
    request: Request,
    handle: &SessionHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(editor) = state.borrow().editor.upgrade() else {
        return;
    };
    stop_find(&mut state.borrow_mut());
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let Request::Find {
        generation,
        spec,
        from,
        direction,
        wrap,
    } = request
    else {
        let spec = state.borrow().model.spec().clone();
        match query_for(state, &spec) {
            Some(query) => start_count(state, &spec, query, snapshot, handle, window, cx),
            None => drop_count(state),
        }
        show(state, &editor, handle, window, cx);
        return;
    };
    let Some(query) = query_for(state, &spec) else {
        refresh_status(state, handle, cx);
        return;
    };
    start_count(
        state,
        &spec,
        query.clone(),
        snapshot.clone(),
        handle,
        window,
        cx,
    );

    let cancel = Cancel::new();
    let search = cx.background_spawn({
        let cancel = cancel.clone();
        async move {
            find_outcome(
                &snapshot,
                &query,
                MultiBufferOffset(from),
                direction,
                wrap,
                &cancel,
            )
        }
    });
    let weak = Rc::downgrade(state);
    let handle = handle.clone();
    let task = window.spawn(cx, async move |cx| {
        let timer = cx.background_executor().timer(PENDING_DELAY);
        let outcome = match select(search, timer).await {
            Either::Left((outcome, _)) => outcome,
            Either::Right(((), search)) => {
                let weak = weak.clone();
                let handle = handle.clone();
                cx.update(move |_, cx| {
                    if let Some(state) = weak.upgrade() {
                        show_pending(&state, generation, &handle, cx);
                    }
                })
                .ok();
                search.await
            }
        };
        cx.update(move |window, cx| {
            if let Some(state) = weak.upgrade() {
                found(&state, generation, outcome, &handle, window, cx);
            }
        })
        .ok();
    });
    state.borrow_mut().find = Some(Find {
        generation,
        cancel,
        running: true,
        _task: task,
    });
}

/// The query for `spec`, or `None` when there is nothing to search: the string is empty, or it is
/// an invalid regexp, whose message the status then shows.
fn query_for(state: &Shared, spec: &SearchSpec) -> Option<Arc<SearchQuery>> {
    let mut state = state.borrow_mut();
    state.invalid = None;
    if spec.string.is_empty() {
        return None;
    }
    match state.engine.query(spec) {
        Ok(query) => Some(query),
        Err(InvalidPattern(message)) => {
            state.invalid = Some(message.lines().next().unwrap_or_default().to_string());
            None
        }
    }
}

/// Counts the matches of `spec` in the background, unless the count of the same pattern is
/// already running or done: a repeat keeps the count.
fn start_count(
    state: &Shared,
    spec: &SearchSpec,
    query: Arc<SearchQuery>,
    snapshot: MultiBufferSnapshot,
    handle: &SessionHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let key = compile(spec);
    if state
        .borrow()
        .counting
        .as_ref()
        .is_some_and(|counting| counting.key == key)
    {
        return;
    }
    drop_count(state);
    let cancel = Cancel::new();
    let counted = cx.background_spawn({
        let cancel = cancel.clone();
        async move { count(&snapshot, &query, COUNT_CAP, &cancel) }
    });
    let weak = Rc::downgrade(state);
    let handle = handle.clone();
    let task = window.spawn(cx, {
        let key = key.clone();
        async move |cx| {
            let Some(result) = counted.await else {
                return;
            };
            cx.update(move |_, cx| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                {
                    let mut state = state.borrow_mut();
                    match &mut state.counting {
                        Some(counting) if counting.key == key => counting.result = Some(result),
                        _ => return,
                    }
                }
                refresh_status(&state, &handle, cx);
            })
            .ok();
        }
    });
    state.borrow_mut().counting = Some(Counting {
        key,
        result: None,
        cancel,
        _task: task,
    });
}

fn drop_count(state: &Shared) {
    if let Some(counting) = state.borrow_mut().counting.take() {
        counting.cancel.cancel();
    }
}

/// A find landed. A canceled or stale one is dropped; otherwise the model takes it and the view
/// follows. A repeat or an exit that waited for it runs now.
fn found(
    state: &Shared,
    generation: u64,
    outcome: FindOutcome,
    handle: &SessionHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let applied = {
        let mut state = state.borrow_mut();
        match &mut state.find {
            Some(find) if find.generation == generation && find.running => find.running = false,
            _ => return,
        }
        match outcome {
            FindOutcome::Canceled => false,
            FindOutcome::Found(range) => state
                .model
                .apply(generation, Some(range.start.0..range.end.0)),
            FindOutcome::NotFound => state.model.apply(generation, None),
        }
    };
    if !applied {
        return;
    }
    let editor = state.borrow().editor.upgrade();
    if let Some(editor) = editor {
        show(state, &editor, handle, window, cx);
    }
    let repeat = {
        let mut state = state.borrow_mut();
        (!state.queued_repeats.is_empty()).then(|| state.queued_repeats.remove(0))
    };
    if let Some(direction) = repeat {
        repeat_command(state, direction, window, cx);
    } else {
        let yank = {
            let mut state = state.borrow_mut();
            (!state.queued_yanks.is_empty()).then(|| state.queued_yanks.remove(0))
        };
        if let Some(yank) = yank {
            yank(state, window, cx);
        }
    }
    // A waiting exit closes once no find is left to wait for, so it can never be stranded.
    let close = {
        let state = state.borrow();
        state.exit_after_find && !state.find.as_ref().is_some_and(|find| find.running)
    };
    if close {
        handle.close(window, cx);
    }
}

/// Moves the cursor to the current step's match (the origin without one) and shows the prompt
/// and the counter.
fn show(
    state: &Shared,
    editor: &Entity<Editor>,
    handle: &SessionHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let (target, text, found) = {
        let state = state.borrow();
        let step = state.model.current();
        let target = match (&step.found, step.direction) {
            (Some(found), Direction::Forward) => found.end,
            (Some(found), Direction::Backward) => found.start,
            (None, _) => state.model.origin(),
        };
        (target, prompt(step, state.model.spec()), step.found.clone())
    };
    // The folds that hide the match open first, so the cursor lands on visible text; the ones
    // the search has left behind close once it is there.
    if let Some(found) = &found {
        with_display(state, |display| {
            editor.update(cx, |editor, cx| display.open(editor, found, cx))
        });
    }
    land(editor, target, window, cx);
    with_display(state, |display| {
        editor.update(cx, |editor, cx| {
            display.refold(editor, found.as_ref(), window, cx)
        })
    });
    refresh_highlights(state, editor, cx);
    let changed = {
        let mut state = state.borrow_mut();
        (state.prompt != text).then(|| {
            state.prompt = text.clone();
            text
        })
    };
    if let Some(text) = changed {
        handle.set_prompt(text, window, cx);
    }
    refresh_status(state, handle, cx);
}

/// Runs `f` on the session's folds, which are taken out of the state meanwhile so the editor
/// events that come of it can look at the rest.
fn with_display<R>(state: &Shared, f: impl FnOnce(&mut Display) -> R) -> R {
    let mut display = std::mem::take(&mut state.borrow_mut().display);
    let result = f(&mut display);
    state.borrow_mut().display = display;
    result
}

/// Highlights the matches near the visible text, the current one apart, as of the model's
/// string; no highlights while there is nothing to search for.
fn refresh_highlights(state: &Shared, editor: &Entity<Editor>, cx: &mut App) {
    let (query, current) = {
        let mut state = state.borrow_mut();
        let spec = state.model.spec().clone();
        let query = if spec.string.is_empty() {
            None
        } else {
            state.engine.query(&spec).ok()
        };
        let step = state.model.current();
        let current = if step.failing {
            None
        } else {
            step.found.clone()
        };
        (query, current)
    };
    editor.update(cx, |editor, cx| {
        display::highlight(editor, query.as_deref(), current.as_ref(), cx)
    });
}

/// The editor scrolled: the matches near the new visible text are highlighted.
fn scrolled(state: &Shared, cx: &mut App) {
    let Some(editor) = state.borrow().editor.upgrade() else {
        return;
    };
    refresh_highlights(state, &editor, cx);
}

/// The text changed under the search (a reload, a formatter, a collaborator): the model's offsets
/// follow it through anchors taken in the text they were of, the highlights and the count start
/// over, and the current match is looked for again from its moved start.
fn buffer_edited(state: &Shared, window: &mut Window, cx: &mut App) {
    let (Some(editor), Some(handle)) = (state.borrow().editor.upgrade(), handle_of(state)) else {
        return;
    };
    let new = editor.read(cx).buffer().read(cx).snapshot(cx);
    {
        let mut state = state.borrow_mut();
        let old = std::mem::replace(&mut state.snapshot, new.clone());
        let len = old.len().0;
        state.model.rebase(|offset| {
            old.anchor_at(MultiBufferOffset(offset.min(len)), Bias::Right)
                .to_offset(&new)
                .0
        });
        stop_find(&mut state);
    }
    drop_count(state);
    refresh_highlights(state, &editor, cx);
    let request = {
        let mut state = state.borrow_mut();
        let spec = state.model.spec().clone();
        state.model.set_spec(spec)
    };
    run(state, request, &handle, window, cx);
    // An exit that waited for the find stopped above is not left stranded.
    let close = {
        let state = state.borrow();
        state.exit_after_find && !state.find.as_ref().is_some_and(|find| find.running)
    };
    if close {
        handle.close(window, cx);
    }
}

/// Puts the one cursor at `offset`, scrolled into view, without a navigation history entry and
/// without lighting up the other occurrences of a selection.
fn land(editor: &Entity<Editor>, offset: usize, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        let len = editor.buffer().read(cx).snapshot(cx).len();
        let offset = MultiBufferOffset(offset).min(len);
        let effects = SelectionEffects::scroll(Autoscroll::fit())
            .nav_history(false)
            .from_search(true);
        editor.change_selections(effects, window, cx, |s| s.select_ranges([offset..offset]));
    });
}

/// The status after the input: the invalid regexp's message, nothing while failing, else the
/// counter once the count of the current pattern is known. Left alone while a find runs.
fn refresh_status(state: &Shared, handle: &SessionHandle, cx: &mut App) {
    let status = {
        let state = state.borrow();
        if let Some(message) = &state.invalid {
            format!("[Invalid regexp: {message}]")
        } else if state.find.as_ref().is_some_and(|find| find.running) {
            return;
        } else {
            status_text(&state)
        }
    };
    handle.set_status(status, cx);
}

fn status_text(state: &SessionState) -> String {
    let step = state.model.current();
    if step.failing || step.string.is_empty() {
        return String::new();
    }
    let (Some(found), Some(counting)) = (&step.found, &state.counting) else {
        return String::new();
    };
    let Some(count) = &counting.result else {
        return String::new();
    };
    if counting.key != compile(state.model.spec()) {
        return String::new();
    }
    let index = count
        .index_of(MultiBufferOffset(found.start))
        .map(|index| index + 1);
    let total = if count.capped {
        COUNT_CAP
    } else {
        count.starts.len()
    };
    counter(index, total)
}

/// The find has run for about a frame: the status says it is still running.
fn show_pending(state: &Shared, generation: u64, handle: &SessionHandle, cx: &mut App) {
    let running = state
        .borrow()
        .find
        .as_ref()
        .is_some_and(|find| find.running && find.generation == generation);
    if running {
        handle.set_status(PENDING, cx);
    }
}

/// C-s and C-r. While a find runs they wait for it, so they step on from the string as typed.
fn repeat_command(state: &Shared, direction: Direction, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    if state.borrow().editing.is_some() {
        commands::resume_editing(state, Some(direction), window, cx);
        return;
    }
    let regexp = {
        let mut state = state.borrow_mut();
        if state.find.as_ref().is_some_and(|find| find.running) {
            state.queued_repeats.push(direction);
            return;
        }
        state.model.spec().regexp
    };
    let last = HistoryStore::global(cx)
        .ring(history_name(regexp))
        .get(0)
        .map(str::to_string);
    let (request, recalled) = {
        let mut state = state.borrow_mut();
        let request = state.model.repeat(direction, last.as_deref());
        let string = state.model.spec().string.clone();
        let recalled = (string != state.string).then(|| {
            state.string = string.clone();
            string
        });
        (request, recalled)
    };
    if let Some(string) = recalled {
        handle.set_input(&to_input(&string), window, cx);
    }
    run(state, request, &handle, window, cx);
}

/// Backspace: the string loses its last character and the search returns to the match found
/// for the shorter string.
fn del_char(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    let (request, string) = {
        let mut state = state.borrow_mut();
        let mut string = state.string.clone();
        if string.pop().is_none() {
            return;
        }
        state.string = string.clone();
        state.ring_position = None;
        (state.model.set_string(&string), string)
    };
    handle.set_input(&to_input(&string), window, cx);
    run(state, request, &handle, window, cx);
}

/// C-g and Escape: a failing suffix goes first; otherwise the session ends where it started.
fn abort(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    if state.borrow().editing.is_some() {
        commands::cancel_editing(state, window, cx);
        return;
    }
    let outcome = {
        let mut state = state.borrow_mut();
        stop_find(&mut state);
        state.queued_repeats.clear();
        state.queued_yanks.clear();
        // An exit that waited for the find is called off with it: should C-g only trim, the
        // session goes on as if the exit had not been asked for.
        state.exit_after_find = false;
        state.pending_exit = None;
        state.model.abort()
    };
    match outcome {
        AbortOutcome::TrimmedFailingSuffix(string) => {
            state.borrow_mut().string = string.clone();
            handle.set_input(&to_input(&string), window, cx);
            run(state, Request::None, &handle, window, cx);
        }
        AbortOutcome::Quit => {
            state.borrow_mut().pending_exit = Some(ExitKind::Abort);
            handle.close(window, cx);
        }
    }
}

/// RET and C-RET. While a find runs the exit waits for it.
fn exit_command(state: &Shared, kind: ExitKind, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    {
        let mut state = state.borrow_mut();
        state.pending_exit = Some(kind);
        if state.find.as_ref().is_some_and(|find| find.running) {
            state.exit_after_find = true;
            return;
        }
    }
    handle.close(window, cx);
}

/// Whether the search string is being edited (M-e), when every key goes to the input.
pub(crate) fn is_editing(state: &Shared) -> bool {
    state.borrow().editing.is_some()
}

/// Whether the session is ending on a key that is not its own, so every later key is held to
/// run after it.
pub(crate) fn holds_keys(state: &Shared) -> bool {
    matches!(state.borrow().pending_exit, Some(ExitKind::Implicit(_)))
}

/// A key that is not the session's (see `intercept`): the session ends at the match and `keys`
/// run again in the origin editor. While a find runs the exit waits for it, so the keys act on the
/// match of the string as typed; keys typed meanwhile join them.
pub(crate) fn exit_implicitly(
    state: &Shared,
    keys: Vec<Keystroke>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    {
        let mut state = state.borrow_mut();
        match &mut state.pending_exit {
            Some(ExitKind::Implicit(held)) => {
                held.extend(keys);
                return;
            }
            // RET (or C-g) is already waiting for the find: that exit stands.
            Some(_) => return,
            None => {}
        }
        state.pending_exit = Some(ExitKind::Implicit(keys));
        if state.find.as_ref().is_some_and(|find| find.running) {
            state.exit_after_find = true;
            return;
        }
    }
    handle.close(window, cx);
}

/// M-p (`older`) and M-n: the next entry of the history of the current type replaces the string,
/// searched from the origin. M-n past the newest entry brings back what was typed.
fn walk_ring(state: &Shared, older: bool, window: &mut Window, cx: &mut App) {
    let Some(handle) = handle_of(state) else {
        return;
    };
    let regexp = state.borrow().model.spec().regexp;
    let ring = HistoryStore::global(cx).ring(history_name(regexp));
    let string = {
        let mut state = state.borrow_mut();
        let next = match (older, state.ring_position) {
            (true, None) => Some(0),
            (true, Some(position)) => Some(position + 1),
            (false, None) => return,
            (false, Some(0)) => None,
            (false, Some(position)) => Some(position - 1),
        };
        match next {
            Some(position) => {
                let Some(entry) = ring.get(position) else {
                    return;
                };
                if state.ring_position.is_none() {
                    state.ring_saved = state.string.clone();
                }
                state.ring_position = Some(position);
                entry.to_string()
            }
            None => {
                state.ring_position = None;
                std::mem::take(&mut state.ring_saved)
            }
        }
    };
    let request = {
        let mut state = state.borrow_mut();
        // Back to the first step, so the entry is searched from the origin.
        state.model.set_string("");
        state.string = string.clone();
        state.model.set_string(&string)
    };
    handle.set_input(&to_input(&string), window, cx);
    run(state, request, &handle, window, cx);
}

/// The session has closed and focus is back in the origin editor: carry out the exit. An abort
/// restores the selections, mark mode and scroll and records nothing. Every other exit pushes the
/// origin onto the mark ring first, then sets the final selection, and records the string. A
/// session that ends without a command (a click outside, another modal) leaves the cursor where
/// it is, at the match end unless the click put it elsewhere. An implicit exit then runs its keys
/// again, after this, where focus now is: the origin editor.
fn finish(state: &Shared, fallback: Option<ExitKind>, window: &mut Window, cx: &mut App) {
    let (workspace, editor, exit, origin, saved, string, regexp, found, direction, mut folds) = {
        let mut state = state.borrow_mut();
        stop_find(&mut state);
        if let Some(counting) = state.counting.take() {
            counting.cancel.cancel();
        }
        state._release = None;
        state._intercept = None;
        state._editor_events = None;
        state.handle = None;
        let exit = state.pending_exit.take().or(fallback);
        let step = state.model.current();
        (
            state.workspace,
            state.editor.clone(),
            exit,
            state.origin,
            state.saved.clone(),
            state.string.clone(),
            state.model.spec().regexp,
            step.found.clone(),
            step.direction,
            std::mem::take(&mut state.display),
        )
    };
    forget(state, workspace, cx);
    if let Some(ExitKind::Implicit(keys)) = &exit {
        // Deferred, so the exit below is done first; scheduled before it, so the keys run even
        // when the origin editor is gone.
        let keys = keys.clone();
        window.defer(cx, move |window, cx| {
            for key in keys {
                window.dispatch_keystroke(key, cx);
            }
        });
    }
    let Some(editor) = editor.upgrade() else {
        return;
    };
    editor.update(cx, |editor, cx| display::clear(editor, cx));
    if exit == Some(ExitKind::Abort) {
        restore(&editor, &saved, &mut folds, window, cx);
        return;
    }

    push_origin(&editor, origin, window, cx);
    if let (Some(found), Some(exit)) = (found, exit) {
        editor.update(cx, |editor, cx| {
            let len = editor.buffer().read(cx).snapshot(cx).len();
            let start = MultiBufferOffset(found.start).min(len);
            let end = MultiBufferOffset(found.end).min(len);
            let (point, other) = match direction {
                Direction::Forward => (end, start),
                Direction::Backward => (start, end),
            };
            let range = match exit {
                ExitKind::Select => other..point,
                ExitKind::Implicit(_) => point..point,
                ExitKind::OtherEnd | ExitKind::Abort => other..other,
            };
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::fit()).nav_history(false),
                window,
                cx,
                |s| s.select_ranges([range]),
            );
        });
    }
    if !string.is_empty() {
        HistoryStore::global(cx).push(history_name(regexp), &string, cx);
    }
}

/// Puts back every selection, mark mode, the folds and the scroll position as they were before
/// the search.
fn restore(
    editor: &Entity<Editor>,
    saved: &Saved,
    folds: &mut Display,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |editor, cx| {
        editor.change_selections(
            SelectionEffects::no_scroll().nav_history(false),
            window,
            cx,
            |s| s.select_anchors(saved.selections.to_vec()),
        );
        // The folds the search opened close before the scroll goes back, which counts rows with
        // them closed; the cursor is back outside them by now.
        folds.restore(editor, window, cx);
        editor.set_selection_mark_mode(saved.mark_mode, cx);
        editor.set_scroll_position(saved.scroll, window, cx);
        cx.notify();
    });
}

/// Delivers a find result to the session as a landed find does, for the tests.
#[cfg(test)]
pub(crate) fn deliver(
    state: &Shared,
    generation: u64,
    outcome: FindOutcome,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(handle) = handle_of(state) {
        found(state, generation, outcome, &handle, window, cx);
    }
}

/// The generation of the newest find, for the tests.
#[cfg(test)]
pub(crate) fn last_generation(state: &Shared) -> Option<u64> {
    state.borrow().find.as_ref().map(|find| find.generation)
}

/// The session of `workspace`, for the tests.
#[cfg(test)]
pub(crate) fn test_session(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::VisualTestContext,
) -> Option<Shared> {
    let id = workspace.entity_id();
    cx.update(|_, cx| session_of(id, cx))
}
