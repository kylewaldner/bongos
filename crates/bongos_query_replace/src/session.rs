//! A replace running in an editor: the interactive one of `query-replace`, driven from the
//! decision view (see the end of this file), and the batch of `replace-string`, `replace-regexp`
//! and `!` in the decision view: every match of the scope is found on the background executor
//! and replaced on the main thread in chunks of [`crate::engine::SCAN_CHUNK`], one undo entry
//! each, yielding between chunks so the window keeps drawing. At the end the entries are merged into one undo
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
use std::sync::{Arc, OnceLock};
use std::task::Poll;
use std::time::{Duration, Instant};

use bongos_emacs::recenter::scroll_row_to;
use bongos_emacs_model::recenter::Cycle;
use bongos_isearch::display::Display;
use bongos_isearch::engine::Cancel;
use bongos_isearch_model::CaseMode;
use bongos_query_replace_model::session::{Effect, End, Found, Key, Phase, Reason, Session};
use bongos_query_replace_model::template::{self, Template};
use bongos_query_replace_model::text::{
    SHOWN_PROMPT, WALKING_PROMPT, counter, decision_prompt, end_message, help, notice,
};
use editor::{Addon, Editor, EditorEvent, HighlightKey, SelectionEffects, scroll::Autoscroll};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EntityId, Global,
    KeyContext, Subscription, Task, WeakEntity, Window,
};
use multi_buffer::{Anchor, MultiBufferOffset, ToOffset as _};
use project::search::SearchQuery;
use workspace::Workspace;

use crate::actions::{
    Abort, All, EditReplacement, Exit, Help, No, Previous, Recenter, ThisAndExit, ThisAndWait,
    Undo, Yes,
};
use crate::display::{self, Shown};
use crate::edits::{Edits, NO_TRANSACTION, READ_ONLY, ReadOnly, Replace, match_anchors};
use crate::engine::{self, Hit, Replacer, ScanEnd};
use crate::prompts::{self, Mode};
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
        register_key::<All>(workspace, Input::Key(Key::All));
        register_key::<ThisAndExit>(workspace, Input::Key(Key::ThisAndExit));
        register_key::<ThisAndWait>(workspace, Input::Key(Key::ThisAndWait));
        register_key::<Previous>(workspace, Input::Key(Key::Previous));
        register_key::<Undo>(workspace, Input::Key(Key::Undo));
        register_key::<EditReplacement>(workspace, Input::Edit);
        register_key::<Recenter>(workspace, Input::Recenter);
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
    let (mut edits, scan) = editor.update(cx, |editor, cx| {
        let edits = Edits::start(editor, cx);
        let scan = start_scan(
            editor,
            &cancel,
            ScanRequest {
                query,
                scope,
                from,
                preserve_case,
                template,
                counter: 0,
            },
            cx,
        );
        (edits, scan)
    });

    let editor = editor.downgrade();
    window
        .spawn(cx, async move |cx| {
            let outcome = apply_scan(
                &editor,
                &mut edits,
                scan,
                &cancel,
                |replaced, _, _| {
                    model.on_batch_progress(replaced);
                },
                cx,
            )
            .await;
            let Some(outcome) = outcome else {
                return;
            };
            let finished = editor.update_in(cx, |editor, window, cx| {
                let started = Instant::now();
                // The cursor goes to the end of the last replacement, which also ends the region.
                if let Some(end) = outcome.end {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::fit()),
                        window,
                        cx,
                        |s| s.select_anchor_ranges([end..end]),
                    );
                }
                edits.finish(editor, cx);
                cx.notify();
                record_slice(started.elapsed());
            });
            if finished.is_err() {
                return;
            }
            let message = match outcome.read_only {
                Some(error) => error.to_string(),
                None => model
                    .on_batch_end(outcome.interrupted)
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

/// What a batch scan looks for and how it makes the replacements: `counter` is the number of
/// replacements made before the batch (the `\#` of its first match).
struct ScanRequest {
    query: Arc<SearchQuery>,
    scope: Range<Anchor>,
    from: usize,
    preserve_case: bool,
    template: Template,
    counter: usize,
}

/// The scan of a running batch, on the background executor.
struct Scan {
    /// The matches with their replacements, [`engine::SCAN_CHUNK`] at a time.
    chunks: mpsc::UnboundedReceiver<Vec<Hit>>,
    task: Task<ScanEnd>,
    /// The number of matches, once the scan has found them all.
    total: Arc<OnceLock<usize>>,
}

/// Marks `editor` as running a batch (its key context gets [`BATCH_KEY_CONTEXT`], where `C-g`
/// sets `cancel`) and starts the scan, on the text as it is now. The anchors of the matches are
/// taken on that text, so a chunk applied later still finds them after edits made since.
fn start_scan(
    editor: &mut Editor,
    cancel: &Cancel,
    request: ScanRequest,
    cx: &mut Context<Editor>,
) -> Scan {
    let ScanRequest {
        query,
        scope,
        from,
        preserve_case,
        template,
        counter,
    } = request;
    editor.register_addon(Batch {
        cancel: cancel.clone(),
    });
    cx.notify();
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let (sender, chunks) = mpsc::unbounded();
    let total = Arc::new(OnceLock::new());
    let task = cx.background_spawn({
        let cancel = cancel.clone();
        let total = total.clone();
        async move {
            // The model session that computes the replacements, a twin of the one that counts.
            let session = Session::new(preserve_case, template);
            let replacer = Replacer {
                session: &session,
                counter,
            };
            let end = engine::scan(
                &snapshot,
                &query,
                MultiBufferOffset(from),
                &scope,
                &cancel,
                Some(replacer),
                |chunk| {
                    sender.unbounded_send(chunk).ok();
                },
            );
            if let ScanEnd::Done { matches } = end {
                total.set(matches).ok();
            }
            end
        }
    });
    Scan {
        chunks,
        task,
        total,
    }
}

/// How a batch ended.
struct BatchEnd {
    /// The end of the last replacement made.
    end: Option<Anchor>,
    /// `C-g` stopped it before every match was replaced.
    interrupted: bool,
    /// The editor became read-only, which stopped it.
    read_only: Option<ReadOnly>,
}

/// Applies the chunks of `scan` in order, each as one undo entry, yielding to the other tasks of
/// the main thread between them; `progress` gets the number of replacements made after each.
/// `cancel` stops it before the next chunk, so only whole chunks of whole replacements are ever
/// made. At the end the editor no longer runs a batch. `None` when the editor or its window is
/// gone.
async fn apply_scan(
    editor: &WeakEntity<Editor>,
    edits: &mut Edits,
    scan: Scan,
    cancel: &Cancel,
    mut progress: impl FnMut(usize, &mut Window, &mut App),
    cx: &mut AsyncWindowContext,
) -> Option<BatchEnd> {
    let Scan {
        mut chunks, task, ..
    } = scan;
    let mut replaced = 0;
    let mut end = None;
    let mut stopped = false;
    let mut read_only = None;
    while let Some(chunk) = chunks.next().await {
        // Checked on the main thread right before the edit, so a C-g handled before this point
        // always stops the batch here.
        if cancel.is_canceled() {
            stopped = true;
            break;
        }
        let started = Instant::now();
        let applied = editor.update_in(cx, |editor, window, cx| {
            let applied = edits.apply_chunk(editor, chunk, window, cx);
            if let Ok(applied) = &applied {
                replaced += applied.replaced;
                end = applied.end.or(end);
                progress(replaced, window, cx);
            }
            applied
        });
        match applied {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                read_only = Some(error);
                break;
            }
            // The editor or its window is gone.
            Err(_) => {
                cancel.cancel();
                return None;
            }
        }
        record_slice(started.elapsed());
        yield_now().await;
    }
    let interrupted = if stopped || read_only.is_some() {
        cancel.cancel();
        stopped
    } else {
        matches!(task.await, ScanEnd::Canceled { .. })
    };
    editor
        .update(cx, |editor, cx| {
            editor.unregister_addon::<Batch>();
            cx.notify();
        })
        .ok()?;
    Some(BatchEnd {
        end,
        interrupted,
        read_only,
    })
}

#[cfg(test)]
thread_local! {
    /// The time each step of a batch took on the main thread, for the perf tests.
    pub(crate) static SLICES: RefCell<Vec<Duration>> = const { RefCell::new(Vec::new()) };
}

fn record_slice(_elapsed: Duration) {
    #[cfg(test)]
    SLICES.with(|slices| slices.borrow_mut().push(_elapsed));
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
    /// The replacement, as typed.
    pub to: String,
    pub regexp: bool,
    /// Whether the scope is a region the user made, which is kept visible.
    pub region: bool,
}

/// One thing the user asked for in the decision view.
#[derive(Clone, Copy, Debug)]
enum Input {
    Key(Key),
    Help,
    /// `e`: edit the replacement string.
    Edit,
    /// `C-l`: recenter.
    Recenter,
}

/// The open interactive replace of a workspace.
struct Interactive {
    editor: WeakEntity<Editor>,
    workspace: WeakEntity<Workspace>,
    view: WeakEntity<QueryReplaceView>,
    model: Session,
    query: Arc<SearchQuery>,
    scope: Range<Anchor>,
    /// What the model makes the replacements with, for the scan of a `!` batch.
    preserve_case: bool,
    template: Template,
    from: String,
    /// The replacement string, as typed, which `e` starts from.
    to: String,
    regexp: bool,
    edits: Option<Edits>,
    /// The replacement of the last decision, highlighted until the next one.
    replacement: Option<Range<Anchor>>,
    /// The inserted text of every replacement the model's stack holds, oldest first, for `^`.
    history: Vec<Range<Anchor>>,
    /// The position of the last `C-l`, while it is the last thing pressed.
    recenter: Option<Cycle>,
    /// Whether the replacement string is being edited: the view is closed meanwhile, which is
    /// not an exit.
    editing: bool,
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
    /// The `!` batch running, which holds the edits meanwhile.
    batch: Option<Running>,
    done: bool,
}

/// A `!` batch of the interactive replace, while it runs.
struct Running {
    cancel: Cancel,
    /// The matches the scan found, once it found them all.
    scanned: Arc<OnceLock<usize>>,
    /// The matches decided before the batch.
    before: usize,
}

/// The prompt while a `!` batch runs.
pub const BATCH_PROMPT: &str = "Replacing all; C-g to stop";

/// The status while a `!` batch runs: `Replacing… 12000/50000`, the matches decided so far over
/// all of them, with no total while it is unknown.
pub fn batch_status(decided: usize, total: Option<usize>) -> String {
    match total {
        Some(total) => format!("Replacing… {decided}/{total}"),
        None => format!("Replacing… {decided}"),
    }
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
        to,
        regexp,
        region,
    } = request;
    let mut model = Session::new(preserve_case, template.clone());
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
        preserve_case,
        template,
        from,
        to,
        regexp,
        edits: Some(edits),
        replacement: None,
        history: Vec::new(),
        recenter: None,
        editing: false,
        folds: Display::default(),
        _editor_events: None,
        current: None,
        rest: None,
        queued: VecDeque::new(),
        find: None,
        total: None,
        count: Cancel::new(),
        batch: None,
        done: false,
    }));
    if !show_view(&state, window, cx) {
        // The modal could not open (the one before it refused to close): nothing was asked.
        let edits = state.borrow_mut().edits.take();
        if let Some(edits) = edits {
            editor.update(cx, |editor, cx| edits.finish(editor, cx));
        }
        return;
    }
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

/// What closing the view does: the session ends as `Exit`, unless the replacement string is being
/// edited.
fn dismiss_handler(state: &Shared) -> Box<dyn FnOnce(&mut Window, &mut App)> {
    let state = state.clone();
    Box::new(move |window, cx| {
        let counts = {
            let mut state = state.borrow_mut();
            if state.editing {
                return;
            }
            // The batch stops before its next chunk and ends the session when it does.
            if let Some(batch) = &state.batch {
                batch.cancel.cancel();
                return;
            }
            // Out of `^` shown on a replacement, the cursor stays on it.
            if matches!(state.model.phase(), Phase::Walking { .. }) {
                state.rest = None;
            }
            state.model.counts()
        };
        let end = End {
            reason: Reason::Exit,
            counts,
        };
        finish(&state, end, false, window, cx);
    })
}

/// Opens the decision view in the session's workspace. False when the modal could not open.
fn show_view(state: &Shared, window: &mut Window, cx: &mut App) -> bool {
    let Some(workspace) = state.borrow().workspace.upgrade() else {
        return false;
    };
    let mut on_dismiss = Some(dismiss_handler(state));
    workspace.update(cx, |workspace, cx| {
        workspace.hide_modal(window, cx);
        workspace.toggle_modal(window, cx, |_, cx| {
            QueryReplaceView::new("", on_dismiss.take().expect("built once"), cx)
        });
    });
    let Some(view) = workspace.read(cx).active_modal::<QueryReplaceView>(cx) else {
        return false;
    };
    state.borrow_mut().view = view.downgrade();
    true
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
        let status = if let Some(batch) = &state.batch {
            let counts = state.model.counts();
            let total = match (batch.scanned.get(), state.total) {
                (Some(scanned), _) => Some(batch.before + scanned),
                (None, Some((total, false))) => Some(total),
                (None, _) => None,
            };
            batch_status(counts.replaced + counts.skipped, total)
        } else if state.find.is_some() {
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
        (
            state.done || state.editing,
            state.find.is_some(),
            state.view.clone(),
        )
    };
    if done || ignored_in_batch(state, input) {
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

/// Whether `input` does nothing because a `!` batch is running: only `C-g` acts then.
fn ignored_in_batch(state: &Shared, input: Input) -> bool {
    state.borrow().batch.is_some() && !matches!(input, Input::Key(Key::Abort))
}

fn apply(state: &Shared, input: Input, window: &mut Window, cx: &mut App) {
    if ignored_in_batch(state, input) {
        return;
    }
    if !matches!(input, Input::Recenter) {
        state.borrow_mut().recenter = None;
    }
    match input {
        Input::Help => {
            let view = state.borrow().view.clone();
            view.update(cx, |view, cx| view.set_help(help(), cx)).ok();
        }
        Input::Edit => edit(state, window, cx),
        Input::Recenter => recenter(state, window, cx),
        Input::Key(key) => {
            let (effects, returned) = {
                let mut state = state.borrow_mut();
                let previous = state.replacement.take();
                let walking = matches!(state.model.phase(), Phase::Walking { .. });
                // A skipped match is where the cursor ends if no more come.
                if key == Key::No
                    && (state.model.phase() == &Phase::Asking || walking && state.current.is_some())
                {
                    state.rest = state.current.as_ref().map(|(_, anchors)| anchors.end);
                }
                // Out of `^` shown on a replacement, the cursor stays on it.
                if walking && state.current.is_none() && matches!(key, Key::Exit | Key::Abort) {
                    state.rest = None;
                }
                let effects = state.model.on_key(key);
                // A notice leaves everything as it was.
                if matches!(effects.as_slice(), [Effect::Notice(_)]) {
                    state.replacement = previous;
                }
                let returned = walking
                    && !matches!(state.model.phase(), Phase::Walking { .. } | Phase::Done(_));
                (effects, returned)
            };
            if returned {
                // A decision key after `^` acts on the match the session was on, so the cursor goes
                // back to it first.
                relocate(state, window, cx);
            }
            run(state, effects, window, cx);
        }
    }
}

/// Does what the model asked for, in order, before anything else is sent to it.
fn run(state: &Shared, effects: Vec<Effect>, window: &mut Window, cx: &mut App) {
    // Where the search goes after an undo: the start of the restored match.
    let mut restart = None;
    for effect in effects {
        match effect {
            Effect::Replace { with, .. } => {
                if let Err(message) = replace(state, &with, window, cx) {
                    abort(state, message, window, cx);
                    return;
                }
            }
            Effect::UndoLast { id } => match undo(state, id, cx) {
                Ok(start) => restart = start,
                Err(message) => {
                    abort(state, message, window, cx);
                    return;
                }
            },
            Effect::Find { from } => {
                find(state, restart.take().unwrap_or(from), window, cx);
                return;
            }
            Effect::Finish(end) => {
                finish(state, end, true, window, cx);
                return;
            }
            Effect::Show(_) => show(state, window, cx),
            Effect::Notice(notice_of) => {
                let view = state.borrow().view.clone();
                view.update(cx, |view, cx| view.set_status(notice(notice_of), cx))
                    .ok();
            }
            Effect::StartBatch { from } => {
                start_batch(state, from.start, window, cx);
                return;
            }
            Effect::StopBatch => {
                if let Some(batch) = &state.borrow().batch {
                    batch.cancel.cancel();
                }
            }
        }
    }
}

/// `!`: replaces every match from `from` on without asking, chunk by chunk, while the view shows
/// the progress. Only `C-g` acts meanwhile. When it ends the session ends.
fn start_batch(state: &Shared, from: usize, window: &mut Window, cx: &mut App) {
    let (editor, edits, request, before) = {
        let mut state = state.borrow_mut();
        let Some(editor) = state.editor.upgrade() else {
            return;
        };
        let Some(edits) = state.edits.take() else {
            return;
        };
        // The match asked about is the first one of the batch.
        state.current = None;
        state.replacement = None;
        let counts = state.model.counts();
        let request = ScanRequest {
            query: state.query.clone(),
            scope: state.scope.clone(),
            from,
            preserve_case: state.preserve_case,
            template: state.template.clone(),
            counter: counts.replaced,
        };
        (editor, edits, request, counts.replaced + counts.skipped)
    };
    let cancel = Cancel::new();
    let scan = editor.update(cx, |editor, cx| {
        editor.clear_background_highlights(HighlightKey::QueryReplace, cx);
        start_scan(editor, &cancel, request, cx)
    });
    state.borrow_mut().batch = Some(Running {
        cancel: cancel.clone(),
        scanned: scan.total.clone(),
        before,
    });
    show_prompt(state, cx);
    refresh_status(state, window, cx);

    let state = state.clone();
    let editor = editor.downgrade();
    let mut edits = edits;
    window
        .spawn(cx, async move |cx| {
            let outcome = apply_scan(
                &editor,
                &mut edits,
                scan,
                &cancel,
                |replaced, window, cx| {
                    state.borrow_mut().model.on_batch_progress(replaced);
                    refresh_status(&state, window, cx);
                },
                cx,
            )
            .await;
            let ended = cx.update(|window, cx| {
                let started = Instant::now();
                end_batch(&state, edits, outcome, window, cx);
                record_slice(started.elapsed());
            });
            // The window is gone: nothing can show the end, but the session is over.
            if ended.is_err() {
                state.borrow_mut().done = true;
            }
        })
        .detach();
}

/// The `!` batch ended (`outcome` is `None` when its editor is gone): the edits come back to the
/// session, which ends with the cursor at the end of the last replacement and its edits as one undo
/// step.
fn end_batch(
    state: &Shared,
    edits: Edits,
    outcome: Option<BatchEnd>,
    window: &mut Window,
    cx: &mut App,
) {
    let effects = {
        let mut state = state.borrow_mut();
        state.batch = None;
        if state.done {
            // Ended some other way meanwhile: the edits still become one undo step.
            let editor = state.editor.upgrade();
            drop(state);
            if let Some(editor) = editor {
                editor.update(cx, |editor, cx| edits.finish(editor, cx));
            }
            return;
        }
        state.edits = Some(edits);
        if let Some(end) = outcome.as_ref().and_then(|outcome| outcome.end) {
            state.rest = Some(end);
        }
        match &outcome {
            Some(BatchEnd {
                read_only: Some(_), ..
            }) => None,
            outcome => Some(
                state
                    .model
                    .on_batch_end(outcome.as_ref().is_none_or(|outcome| outcome.interrupted)),
            ),
        }
    };
    match effects {
        Some(effects) => run(state, effects, window, cx),
        None => abort(state, READ_ONLY.to_string(), window, cx),
    }
}

/// Ends the session after an edit failed, with `message` as the end message.
fn abort(state: &Shared, message: String, window: &mut Window, cx: &mut App) {
    let counts = state.borrow().model.counts();
    let end = End {
        reason: Reason::Aborted,
        counts,
    };
    finish_with(state, end, Some(message), true, window, cx);
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
            state.history.push(replacement.anchors.clone());
            state.replacement = Some(replacement.anchors);
        }
        // Nothing was replaced, but the model counted a replacement: it gets an entry that no
        // transaction stands behind, so `u` has nothing to undo for it.
        Ok(Replace::Changed(_)) => {
            state.model.on_replaced(NO_TRANSACTION, found.range.clone());
            state.history.push(anchors.clone());
            state.rest = Some(anchors.end);
        }
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
    run_queued(state, window, cx);
}

/// Runs the keys pressed while a find ran, one at a time: a key that starts another find leaves
/// the rest queued for it, and `e` leaves them for when the decision view comes back.
fn run_queued(state: &Shared, window: &mut Window, cx: &mut App) {
    loop {
        let input = {
            let mut state = state.borrow_mut();
            if state.done || state.editing || state.find.is_some() {
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

/// Moves the cursor to the end of `range` with no selection (centered when it was off screen),
/// opening the folds that hide it and closing the ones the session opened before. Returns the
/// anchors of the range.
fn place(
    state: &Shared,
    range: &Range<usize>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Range<Anchor>> {
    let editor = state.borrow().editor.upgrade()?;
    // The folds are taken out of the state meanwhile so the editor events that come of the folding
    // can look at the rest.
    let mut folds = std::mem::take(&mut state.borrow_mut().folds);
    let anchors = editor.update(cx, |editor, cx| {
        folds.open(editor, range, cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchors = match_anchors(&snapshot, range);
        let start = snapshot.offset_to_point(MultiBufferOffset(range.start));
        let end = snapshot.offset_to_point(MultiBufferOffset(range.end));
        let display = editor.display_snapshot(cx);
        let visible = editor.multi_buffer_visible_range(&display, cx);
        drop(display);
        let autoscroll = if visible.start <= start && end <= visible.end {
            Autoscroll::fit()
        } else {
            Autoscroll::center()
        };
        let offset = MultiBufferOffset(range.end);
        let effects = SelectionEffects::scroll(autoscroll)
            .nav_history(false)
            .from_search(true);
        editor.change_selections(effects, window, cx, |s| s.select_ranges([offset..offset]));
        folds.refold(editor, Some(range), window, cx);
        anchors
    });
    state.borrow_mut().folds = folds;
    Some(anchors)
}

/// Shows the match: the question on the line, and the cursor at its end with no selection,
/// centered when the match was off screen.
fn land(state: &Shared, found: Found, window: &mut Window, cx: &mut App) {
    let Some(anchors) = place(state, &found.range, window, cx) else {
        return;
    };
    state.borrow_mut().current = Some((found, anchors));
    show_prompt(state, cx);
    refresh_status(state, window, cx);
    refresh_highlights(state, cx);
}

/// Puts the question that goes with the phase of the model on the line.
fn show_prompt(state: &Shared, cx: &mut App) {
    let (view, prompt) = {
        let state = state.borrow();
        let prompt = match state.model.phase() {
            Phase::Asking => state.current.as_ref().map(|(found, _)| {
                let replacement = state.model.replacement_for(found);
                decision_prompt(&state.from, &replacement, state.regexp)
            }),
            Phase::Shown => Some(SHOWN_PROMPT.to_string()),
            Phase::Walking { .. } => Some(WALKING_PROMPT.to_string()),
            Phase::Batch => Some(BATCH_PROMPT.to_string()),
            _ => None,
        };
        (state.view.clone(), prompt)
    };
    if let Some(prompt) = prompt {
        view.update(cx, |view, cx| view.set_prompt(prompt, cx)).ok();
    }
}

/// The offsets `range` stands for now.
fn offsets_of(state: &Shared, range: &Range<Anchor>, cx: &mut App) -> Option<Range<usize>> {
    let editor = state.borrow().editor.upgrade()?;
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    Some(range.start.to_offset(&snapshot).0..range.end.to_offset(&snapshot).0)
}

/// Shows the replacement the model points at after `,` or `^`: the cursor at its end, the
/// highlight of a replacement just made, and the question that goes with it.
fn show(state: &Shared, window: &mut Window, cx: &mut App) {
    let shown = {
        let state = state.borrow();
        match state.model.phase() {
            Phase::Walking { depth } => state
                .history
                .len()
                .checked_sub(*depth)
                .and_then(|index| state.history.get(index))
                .cloned(),
            _ => state.replacement.clone(),
        }
    };
    if let Some(shown) = shown {
        if let Some(range) = offsets_of(state, &shown, cx) {
            place(state, &range, window, cx);
        }
        state.borrow_mut().replacement = Some(shown);
    }
    show_prompt(state, cx);
    refresh_status(state, window, cx);
    refresh_highlights(state, cx);
}

/// Takes the cursor back to the match the session was on before `^`.
fn relocate(state: &Shared, window: &mut Window, cx: &mut App) {
    let anchors = state
        .borrow()
        .current
        .as_ref()
        .map(|(_, anchors)| anchors.clone());
    if let Some(range) = anchors.and_then(|anchors| offsets_of(state, &anchors, cx)) {
        place(state, &range, window, cx);
    }
}

/// Undoes the replacement `id` and forgets it (`u`). Returns where the restored match starts.
fn undo(state: &Shared, id: u64, cx: &mut App) -> Result<Option<usize>, String> {
    let (editor, edits, anchors) = {
        let mut state = state.borrow_mut();
        state.current = None;
        state.replacement = None;
        let anchors = state.history.pop();
        state.rest = anchors.as_ref().map(|anchors| anchors.start);
        (state.editor.upgrade(), state.edits.take(), anchors)
    };
    let (Some(editor), Some(mut edits)) = (editor, edits) else {
        return Ok(None);
    };
    let result = editor.update(cx, |editor, cx| {
        edits.undo(id, editor, cx)?;
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        Ok(anchors.map(|anchors| anchors.start.to_offset(&snapshot).0))
    });
    state.borrow_mut().edits = Some(edits);
    result.map_err(|error: crate::edits::ReadOnly| error.to_string())
}

/// Scrolls the match on show to the middle, the top and the bottom of the window in turn.
fn recenter(state: &Shared, window: &mut Window, cx: &mut App) {
    let (editor, previous) = {
        let state = state.borrow();
        (state.editor.upgrade(), state.recenter)
    };
    let Some(editor) = editor else {
        return;
    };
    let cycle = Cycle::after(previous);
    let scrolled = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let row = editor.selections.newest_display(&display).head().row();
        drop(display);
        scroll_row_to(editor, row, cycle, window, cx)
    });
    state.borrow_mut().recenter = scrolled.then_some(cycle);
}

/// Asks for a new replacement string (`e`). The view closes while the prompt is open, and
/// opens again on the same match when it is answered or cancelled.
fn edit(state: &Shared, window: &mut Window, cx: &mut App) {
    let (workspace, from, to, regexp) = {
        let mut state = state.borrow_mut();
        if !matches!(
            state.model.phase(),
            Phase::Asking | Phase::Shown | Phase::Walking { .. }
        ) {
            return;
        }
        state.editing = true;
        (
            state.workspace.upgrade(),
            state.from.clone(),
            state.to.clone(),
            state.regexp,
        )
    };
    let Some(workspace) = workspace else {
        state.borrow_mut().editing = false;
        return;
    };
    let on_done = {
        let state = state.clone();
        move |to: String, window: &mut Window, cx: &mut App| {
            if let Ok(template) = template::parse(&to, regexp) {
                let mut state = state.borrow_mut();
                state.model.set_template(template.clone());
                state.template = template;
                state.to = to;
            }
            resume(&state, window, cx);
        }
    };
    let on_cancel = {
        let state = state.clone();
        move |window: &mut Window, cx: &mut App| resume(&state, window, cx)
    };
    // Deferred: the key that asked for this runs inside an update of the workspace.
    window.defer(cx, move |window, cx| {
        workspace.update(cx, |workspace, cx| {
            let mode = Mode {
                regexp,
                interactive: true,
            };
            prompts::edit_replacement(
                workspace,
                mode,
                CaseMode::Smart,
                &from,
                &to,
                on_done,
                on_cancel,
                window,
                cx,
            );
        });
    });
}

/// The replacement string was answered or the prompt cancelled: the decision view comes back on
/// the same match.
fn resume(state: &Shared, window: &mut Window, cx: &mut App) {
    {
        let mut state = state.borrow_mut();
        if state.done || !std::mem::replace(&mut state.editing, false) {
            return;
        }
    }
    if !show_view(state, window, cx) {
        let counts = state.borrow().model.counts();
        let end = End {
            reason: Reason::Exit,
            counts,
        };
        finish(state, end, false, window, cx);
        return;
    }
    show_prompt(state, cx);
    refresh_status(state, window, cx);
    refresh_highlights(state, cx);
    run_queued(state, window, cx);
}

/// Highlights the matches near the visible text, from the current match (or the last replacement)
/// on.
fn refresh_highlights(state: &Shared, cx: &mut App) {
    let (editor, query, scope, from, current, replacement) = {
        let state = state.borrow();
        // A batch shows no matches: they change with every chunk.
        if state.done || state.batch.is_some() {
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
