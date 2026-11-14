//! Status writes (D13, D14): the only statuses bongos ever writes to Linear. `started` when a
//! linked leaf's plan begins (and its nearest linked group follows with the first leaf that
//! actually moved), and `canceled` after Kyle drops a task and says yes. Approved creates live in
//! `subtickets`; nothing else reaches the client's `set_state`.
//!
//! The promise: [`write_state`] below holds the one `set_state` call of the app, and it takes a
//! [`StateWrite`], which only `bongos_linear_model::status::started_decision` (through
//! [`started_one`]) and `cancel_plan` (through [`apply_answer`]) hand out. There is no path from
//! a close, an advance, a result or a pull request change to it: this module only looks at the
//! plan todos' start and at `status`, and it asks before any cancel.
//!
//! A [`Watcher`] looks at the work tree on every store change and after every refresh:
//!
//! * a linked task whose `started_sent` is absent and whose plan has begun is read (its ticket and
//!   its team's states, through the gate), decided again on that fresh state and written; then
//!   `started_sent` is recorded, only after the write succeeded. A failed write records nothing
//!   and is retried by the next refresh (a store change does not retry it, so a failing network
//!   is not hammered);
//! * a stacked leaf records `started_sent` without a request;
//! * a linked task that turned `cancelled` in this session raises the cancel question in the
//!   active workspace (a read-key). With no window it waits in memory for the next activation. The
//!   answer is recorded as `linear_state.cancel`, so it is asked once; a task that is open again
//!   loses the record and nothing is written to Linear.
//!
//! Tasks that were already cancelled when the watcher first looked are not asked about: that
//! question was lost with an earlier session, which is the same as declining (risk 5).

use std::collections::{BTreeMap, BTreeSet};

use bongos_linear_model::api::{ApiError, IssueRef, LinearApi, StateKind, WorkflowState};
use bongos_linear_model::ident::parse_identifier;
use bongos_linear_model::status::{
    CancelAnswer, CancelQuestion, CancelTarget, NoQuestion, NoWrite, StartedDecision, StateWrite,
    cancel_plan, cancel_question, clear_cancel, may_cancel_group, record_cancel, record_started,
    started_decision,
};
use bongos_linear_model::sync::linked_identifier;
use bongos_minibuffer::ReadKey;
use bongos_work::{MINIBUFFER_NAME, WorkStore, WorkStoreEvent, observe_view_opened};
use bongos_work_model::commands::{Context as WorkContext, Outcome};
use bongos_work_model::rules::Place;
use bongos_work_model::{LinearCancel, Status, TaskId};
use futures::future::BoxFuture;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, Subscription};
use workspace::{MultiWorkspace, Workspace};

use crate::actions::show_toast;
use crate::refresh::{enabled, linked_ids, snapshot, toast_active};
use crate::sync::{self, Scope};
use crate::token;

// ---------------------------------------------------------------------------------------------
// Texts and keys (pure)

/// The question of a cancel.
pub(crate) fn question_text(question: &CancelQuestion) -> String {
    match question {
        CancelQuestion::Leaf(leaf) => format!("Cancel {} in Linear?", leaf.identifier),
        CancelQuestion::Group { group, leaves } => format!(
            "Cancel {} and its {} subticket{}?",
            group.identifier,
            leaves.len(),
            if leaves.len() == 1 { "" } else { "s" }
        ),
    }
}

/// The keys of the question with their labels, and the default (RET and C-g).
pub(crate) fn keys(question: &CancelQuestion) -> (Vec<(char, &'static str)>, char) {
    match question {
        CancelQuestion::Leaf(_) => (vec![('c', "Cancel ticket"), ('l', "Leave it")], 'c'),
        CancelQuestion::Group { .. } => (
            vec![
                ('l', "Leave all"),
                ('o', "Cancel leaves only"),
                ('a', "Cancel all"),
            ],
            'l',
        ),
    }
}

/// What a key of [`keys`] answers.
pub(crate) fn answer_of(key: char) -> CancelAnswer {
    match key {
        'c' => CancelAnswer::Cancel,
        'o' => CancelAnswer::LeavesOnly,
        'a' => CancelAnswer::All,
        _ => CancelAnswer::Leave,
    }
}

/// The toast after the writes of an answer, `None` when nothing was cancelled.
fn cancelled_message(identifiers: &[String]) -> Option<String> {
    match identifiers {
        [] => None,
        [one] => Some(format!("Cancelled {one} in Linear")),
        many => Some(format!("Cancelled {} tickets in Linear", many.len())),
    }
}

/// Fixed words for a failed request: the error's own text never reaches a log or a toast.
fn fixed(error: &ApiError) -> &'static str {
    match error {
        ApiError::Unauthorized => "the token was rejected",
        ApiError::RateLimited { .. } => "rate limited",
        ApiError::NotFound(_) => "not found in Linear",
        _ => "unreachable",
    }
}

// ---------------------------------------------------------------------------------------------
// The watcher

#[derive(Default)]
struct Watcher {
    subscription: Option<Subscription>,
    /// Whether the first look at the tree happened, which only notes what is cancelled already.
    baselined: bool,
    /// The linked tasks that were cancelled at the last look.
    cancelled: BTreeSet<TaskId>,
    /// The linked tasks that turned cancelled in this session and are still cancelled: the only
    /// ones a question is raised for.
    turned: BTreeSet<TaskId>,
    /// Tasks with a started write or a record in flight.
    busy: BTreeSet<TaskId>,
    /// Started attempts that failed since the last refresh.
    failed: BTreeSet<TaskId>,
    /// The questions waiting for the active workspace.
    questions: BTreeMap<TaskId, CancelQuestion>,
    /// The question whose prompt is open or whose answer is being applied.
    asking: Option<TaskId>,
    /// Questions whose prompt ended without an answer: asked again at the next activation only.
    set_aside: BTreeSet<TaskId>,
    /// Stacked leaves already toasted about in this session.
    noticed: BTreeSet<TaskId>,
    /// Toasts waiting for the active workspace.
    notices: Vec<String>,
}

struct GlobalWatcher(Entity<Watcher>);

impl Global for GlobalWatcher {}

struct ObserversInstalled;

impl Global for ObserversInstalled {}

/// Creates the watcher and hooks the activation of a workspace and the opening of the Work view,
/// which ask a waiting question. Call after [`sync::install`]; the store is attached now when it
/// exists, else by [`connect`].
pub(crate) fn install(cx: &mut App) {
    let entity = cx.new(|_| Watcher::default());
    cx.set_global(GlobalWatcher(entity));
    if !cx.has_global::<ObserversInstalled>() {
        cx.set_global(ObserversInstalled);
        observe_view_opened(cx, activated);
        cx.observe_new(|_: &mut Workspace, window, cx| {
            let Some(window) = window else {
                return;
            };
            cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
                    cx.defer(activated);
                }
            })
            .detach();
        })
        .detach();
    }
    connect(cx);
}

fn global(cx: &App) -> Option<Entity<Watcher>> {
    cx.try_global::<GlobalWatcher>()
        .map(|global| global.0.clone())
}

/// Attaches to the work store when it exists and is not attached yet.
pub(crate) fn connect(cx: &mut App) {
    if let Some(watcher) = global(cx) {
        watcher.update(cx, |watcher, cx| watcher.attach(cx));
    }
}

/// A refresh cycle ended: a failed started write is tried again.
pub(crate) fn refreshed(cx: &mut App) {
    if let Some(watcher) = global(cx) {
        watcher.update(cx, |watcher, cx| watcher.scan(true, cx));
    }
}

/// A workspace became active or the Work view opened: a waiting question is asked.
fn activated(cx: &mut App) {
    present(true, cx);
}

fn with<R>(cx: &mut App, f: impl FnOnce(&mut Watcher) -> R) -> Option<R> {
    let watcher = global(cx)?;
    Some(watcher.update(cx, |watcher, _| f(watcher)))
}

/// What a look at the tree found to do.
#[derive(Default)]
struct Plan {
    /// Linked leaves that are stacked: `started_sent` is recorded with no request.
    stacked: Vec<TaskId>,
    /// Tasks whose cancel record goes because they are open again.
    clear: Vec<TaskId>,
    /// Tasks whose plan has begun and that were not recorded yet: read, then written.
    started: Vec<TaskId>,
    present: bool,
}

impl Watcher {
    fn attach(&mut self, cx: &mut Context<Self>) {
        if self.subscription.is_some() {
            return;
        }
        let Some(store) = WorkStore::global(cx) else {
            return;
        };
        self.subscription = Some(cx.subscribe(&store, |this, _, event, cx| {
            if matches!(event, WorkStoreEvent::Changed) {
                this.scan(false, cx);
            }
        }));
        self.scan(false, cx);
    }

    /// Looks at the tree. Sends nothing itself: what it finds runs after the current effect
    /// cycle, so no store is written from inside the store's own event.
    fn scan(&mut self, retry: bool, cx: &mut Context<Self>) {
        if retry {
            self.failed.clear();
        }
        let Some(tree) = (if enabled(cx) { snapshot(cx) } else { None }) else {
            return;
        };
        let mut plan = Plan::default();
        let ids = linked_ids(&tree);
        let mut cancelled_now = BTreeSet::new();
        for id in &ids {
            let Some(task) = tree.task_by_id(id) else {
                continue;
            };
            let Some(file) = &task.file else {
                continue;
            };
            let is_cancelled = file.status == Status::Cancelled;
            if is_cancelled {
                cancelled_now.insert(id.clone());
            }
            let recorded = file
                .linear_state
                .as_ref()
                .is_some_and(|state| state.cancel.is_some());
            if recorded && !is_cancelled && !self.busy.contains(id) {
                self.busy.insert(id.clone());
                plan.clear.push(id.clone());
            }
            if self.busy.contains(id) || self.failed.contains(id) {
                continue;
            }
            // The decision with a placeholder ticket: `Nothing` ends it, a stacked leaf needs no
            // request, and the rest is decided again on the ticket's real state.
            let placeholder = WorkflowState {
                id: String::new(),
                name: String::new(),
                kind: StateKind::Backlog,
                position: 0.0,
            };
            match started_decision(&tree, id, &placeholder, &[]) {
                StartedDecision::Nothing => {}
                StartedDecision::Record(NoWrite::Stacked) => {
                    self.busy.insert(id.clone());
                    plan.stacked.push(id.clone());
                }
                StartedDecision::Write(_) | StartedDecision::Record(_) => {
                    self.busy.insert(id.clone());
                    plan.started.push(id.clone());
                }
            }
        }
        self.look_at_cancels(&tree, &ids, cancelled_now);
        plan.present = !self.questions.is_empty() || !self.notices.is_empty();
        cx.defer(move |cx| run(plan, cx));
    }

    /// Updates which tasks turned cancelled and the questions waiting.
    fn look_at_cancels(
        &mut self,
        tree: &bongos_work_model::store::WorkTree,
        ids: &[TaskId],
        cancelled_now: BTreeSet<TaskId>,
    ) {
        if self.baselined {
            self.turned
                .extend(cancelled_now.difference(&self.cancelled).cloned());
        }
        self.baselined = true;
        self.turned.retain(|id| cancelled_now.contains(id));
        self.cancelled = cancelled_now;
        let mut fresh: BTreeMap<TaskId, CancelQuestion> = BTreeMap::new();
        for id in ids.iter().filter(|id| self.turned.contains(*id)) {
            match cancel_question(tree, id) {
                Ok(question) => {
                    fresh.insert(id.clone(), question);
                }
                Err(NoQuestion::Stacked(_)) if self.noticed.contains(id) => {}
                Err(error @ NoQuestion::Stacked(_)) => {
                    self.noticed.insert(id.clone());
                    self.notices.push(error.to_string());
                }
                Err(_) => {}
            }
        }
        // A group's question covers the leaves and subgroups under it.
        let groups: BTreeSet<TaskId> = fresh
            .iter()
            .filter(|(_, question)| matches!(question, CancelQuestion::Group { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        fresh.retain(|id, _| {
            Place::locate(tree, id).ok().is_none_or(|place| {
                !place.ancestors.iter().any(|ancestor| {
                    ancestor
                        .id()
                        .is_some_and(|ancestor| groups.contains(ancestor))
                })
            })
        });
        // The question being asked stays as it is.
        if let Some(asking) = &self.asking {
            fresh.remove(asking);
            if let Some(open) = self.questions.get(asking) {
                fresh.insert(asking.clone(), open.clone());
            }
        }
        self.questions = fresh;
        self.set_aside.retain(|id| self.questions.contains_key(id));
    }
}

/// Runs what a look found.
fn run(plan: Plan, cx: &mut App) {
    for id in plan.clear {
        cx.spawn(async move |cx| {
            let target = id.clone();
            let written = write(cx, move |tree, ctx| {
                clear_cancel(tree, ctx, &target).ok_or_else(|| "nothing to clear".to_string())
            })
            .await;
            finish_local(&id, written, cx);
        })
        .detach();
    }
    for id in plan.stacked {
        cx.spawn(async move |cx| {
            let target = id.clone();
            let written = write(cx, move |tree, ctx| {
                record_started(tree, ctx, &target, Some(&NoWrite::Stacked))
            })
            .await;
            finish_local(&id, written, cx);
        })
        .detach();
    }
    if !plan.started.is_empty() {
        let ids = plan.started;
        cx.spawn(async move |cx| {
            let mut ids = ids.into_iter();
            while let Some(id) = ids.next() {
                let result = started_one(&id, cx).await;
                let failed = result.is_err();
                if let Err(reason) = result {
                    log::warn!("linear started was not written: {reason}");
                }
                cx.update(|cx| {
                    with(cx, |watcher| {
                        watcher.busy.remove(&id);
                        if failed {
                            watcher.failed.insert(id.clone());
                        }
                    })
                });
                if failed {
                    // A failing network is not asked again for each task.
                    cx.update(|cx| {
                        with(cx, |watcher| {
                            for id in ids.by_ref() {
                                watcher.busy.remove(&id);
                                watcher.failed.insert(id);
                            }
                        })
                    });
                    break;
                }
            }
        })
        .detach();
    }
    if plan.present {
        present(false, cx);
    }
}

fn finish_local(id: &str, written: Result<(), String>, cx: &mut AsyncApp) {
    if let Err(reason) = written {
        log::warn!("linear status record was not written: {reason}");
    }
    cx.update(|cx| {
        with(cx, |watcher| watcher.busy.remove(id));
    });
}

// ---------------------------------------------------------------------------------------------
// Local writes and the gate

/// One `WorkStore::write`; `Refused` (the record was made meanwhile) is not an error.
async fn write(
    cx: &mut AsyncApp,
    command: impl Fn(&bongos_work_model::store::WorkTree, &WorkContext) -> Outcome + Send + 'static,
) -> Result<(), String> {
    let task = cx.update(|cx| {
        WorkStore::global(cx).map(|store| {
            store.update(cx, |store, cx| {
                store.write(crate::subtickets::sync_context(), command, cx)
            })
        })
    });
    match task {
        Some(task) => match task.await {
            Ok(_) | Err(bongos_work::WriteError::Refused(_)) => Ok(()),
            Err(error) => Err(error.to_string()),
        },
        None => Err(bongos_work::WriteError::Off.to_string()),
    }
}

/// Reads the token when the sync does not know about one yet.
async fn ensure_token(cx: &mut AsyncApp) -> Result<(), String> {
    let present =
        cx.update(|cx| sync::global(cx).is_some_and(|sync| sync.read(cx).token_present()));
    if present || token::load(cx).await {
        Ok(())
    } else {
        Err("no Linear token".to_string())
    }
}

/// Starts a request about the linked task through the gate: refused while `linear.enabled` is
/// false, without a token, or for a task that is not linked.
fn call<T>(
    task_id: &str,
    cx: &mut AsyncApp,
    request: impl FnOnce(&dyn LinearApi) -> BoxFuture<'static, Result<T, ApiError>>,
) -> Result<BoxFuture<'static, Result<T, ApiError>>, String> {
    cx.update(|cx| {
        let linear =
            sync::global(cx).ok_or_else(|| "the Linear sync is not available".to_string())?;
        linear
            .update(cx, |linear, cx| {
                linear.call(Scope::LinkedTask(task_id), cx, request)
            })
            .map_err(|refusal| refusal.to_string())
    })
}

/// Fixed words for `error`, noting a rejected token or a rate limit for the gate.
fn failed_request(error: &ApiError, cx: &mut AsyncApp) -> String {
    cx.update(|cx| {
        if let Some(linear) = sync::global(cx) {
            linear.update(cx, |linear, _| match error {
                ApiError::Unauthorized => {
                    linear.mark_rejected();
                }
                ApiError::RateLimited { reset_at } => linear.mark_limited(*reset_at),
                _ => {}
            });
        }
    });
    fixed(error).to_string()
}

/// A ticket's state and its team's states, read fresh.
struct Ticket {
    issue_id: String,
    team_id: String,
    state: WorkflowState,
    team_states: Vec<WorkflowState>,
}

async fn read_ticket(task_id: &str, cx: &mut AsyncApp) -> Result<Ticket, String> {
    let (identifier, issue_id) = cx
        .update(|cx| {
            let tree = snapshot(cx)?;
            let file = tree.task_by_id(task_id)?.file.as_ref()?;
            let identifier = linked_identifier(file)?.to_string();
            Some((identifier, file.linear_state.as_ref()?.issue_id.clone()))
        })
        .ok_or_else(|| "the task is not linked".to_string())?;
    let reference = parse_identifier(&identifier)
        .map(IssueRef::from)
        .ok_or_else(|| format!("{identifier} is not an identifier"))?;
    let read = call(task_id, cx, |client| client.issues(vec![reference]))?;
    let issues = read.await.map_err(|error| failed_request(&error, cx))?;
    let issue = issues
        .into_iter()
        .find(|issue| issue.id == issue_id || issue.identifier.eq_ignore_ascii_case(&identifier))
        .ok_or_else(|| format!("{identifier} not found in Linear"))?;
    let team_id = issue.team_id.clone();
    let states = call(task_id, cx, |client| client.team_states(&team_id))?;
    let team_states = states.await.map_err(|error| failed_request(&error, cx))?;
    Ok(Ticket {
        issue_id: issue.id,
        team_id: issue.team_id,
        state: issue.state,
        team_states,
    })
}

/// The one request that changes a ticket's status. Its only callers are [`started_one`] and
/// [`apply_answer`], and a [`StateWrite`] comes only from the decisions of
/// `bongos_linear_model::status`.
async fn write_state(write: &StateWrite, cx: &mut AsyncApp) -> Result<(), String> {
    let request = call(&write.task_id, cx, |client| {
        client.set_state(&write.issue_id, &write.state_id)
    })?;
    request.await.map_err(|error| failed_request(&error, cx))
}

// ---------------------------------------------------------------------------------------------
// D13: started

/// Decides on the ticket's real state, writes, and only then records `started_sent`.
async fn started_one(id: &str, cx: &mut AsyncApp) -> Result<(), String> {
    ensure_token(cx).await?;
    let ticket = read_ticket(id, cx).await?;
    let decision = cx.update(|cx| {
        snapshot(cx).map(|tree| started_decision(&tree, id, &ticket.state, &ticket.team_states))
    });
    let reason = match decision {
        None | Some(StartedDecision::Nothing) => return Ok(()),
        Some(StartedDecision::Write(state_write)) => {
            write_state(&state_write, cx).await?;
            None
        }
        Some(StartedDecision::Record(reason)) => Some(reason),
    };
    let target = id.to_string();
    write(cx, move |tree, ctx| {
        record_started(tree, ctx, &target, reason.as_ref())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// D14: cancel

/// Asks the waiting question in the active workspace and shows the waiting toasts. With no
/// window, or while another prompt is open, they wait. `again` also asks the questions whose
/// prompt ended without an answer.
fn present(again: bool, cx: &mut App) {
    if !enabled(cx) {
        return;
    }
    let window = cx
        .active_window()
        .into_iter()
        .chain(cx.windows())
        .find_map(|window| window.downcast::<MultiWorkspace>());
    let Some(window) = window else {
        return;
    };
    let (notices, question) = with(cx, |watcher| {
        if again {
            watcher.set_aside.clear();
        }
        let question = match watcher.asking {
            Some(_) => None,
            None => watcher
                .questions
                .iter()
                .find(|(id, _)| !watcher.set_aside.contains(*id))
                .map(|(id, question)| (id.clone(), question.clone())),
        };
        (std::mem::take(&mut watcher.notices), question)
    })
    .unwrap_or_default();
    if notices.is_empty() && question.is_none() {
        return;
    }
    let mut notices = Some(notices);
    let mut question = question;
    window
        .update(cx, |multi, window, cx| {
            multi.workspace().update(cx, |workspace, cx| {
                for message in notices.take().unwrap_or_default() {
                    show_toast(workspace, message, cx);
                }
                let Some((id, question)) = question.take() else {
                    return;
                };
                if workspace.has_active_modal(window, cx) {
                    return;
                }
                let (choices, default) = keys(&question);
                let (answered, cancelled) = (id.clone(), id.clone());
                let asked = question.clone();
                let mut prompt = ReadKey::new(MINIBUFFER_NAME, question_text(&question));
                for (key, label) in choices {
                    prompt = prompt.key(key, label);
                }
                let prompt = prompt
                    .default(default)
                    .on_answer(move |key, _, cx| answered_with(answered, asked, answer_of(key), cx))
                    .on_cancel(move |_, cx| {
                        with(cx, |watcher| {
                            if watcher.asking.as_deref() == Some(cancelled.as_str()) {
                                watcher.asking = None;
                            }
                            watcher.set_aside.insert(cancelled.clone());
                        });
                    });
                with(cx, |watcher| watcher.asking = Some(id));
                bongos_minibuffer::read_key(workspace, prompt, window, cx);
            });
        })
        .ok();
}

/// Kyle answered: apply the answer, then ask the next question.
fn answered_with(id: TaskId, question: CancelQuestion, answer: CancelAnswer, cx: &mut App) {
    cx.spawn(async move |cx| {
        let message = apply_answer(&question, answer, cx).await;
        cx.update(|cx| {
            with(cx, |watcher| {
                if message.is_err() {
                    watcher.set_aside.insert(id.clone());
                } else {
                    watcher.questions.remove(&id);
                }
                watcher.asking = None;
            });
            let text = match message {
                Ok(text) => text,
                Err(reason) => Some(format!("Could not cancel in Linear: {reason}")),
            };
            if let Some(text) = text {
                toast_active(text, cx);
            }
            present(false, cx);
        });
    })
    .detach();
}

/// Applies Kyle's answer: reads the states of the tickets it may move, writes each move once,
/// and records the answers. `Ok` carries the toast, `Err` the reason nothing more was done (the
/// question stays, and is asked again at the next activation).
async fn apply_answer(
    question: &CancelQuestion,
    answer: CancelAnswer,
    cx: &mut AsyncApp,
) -> Result<Option<String>, String> {
    let (targets, wanted) = targets_of(question, answer);
    let mut tickets: BTreeMap<String, Ticket> = BTreeMap::new();
    if !wanted.is_empty() {
        ensure_token(cx).await?;
        for target in &wanted {
            let ticket = read_ticket(&target.task_id, cx).await?;
            tickets.insert(ticket.issue_id.clone(), ticket);
        }
    }
    let tree = cx
        .update(|cx| snapshot(cx))
        .ok_or_else(|| "the work tree is not loaded".to_string())?;
    let plan = cancel_plan(
        &tree,
        question,
        answer,
        &|issue_id| tickets.get(issue_id).map(|ticket| ticket.state.clone()),
        &|team_id| {
            tickets
                .values()
                .find(|ticket| ticket.team_id == team_id)
                .map(|ticket| ticket.team_states.clone())
                .unwrap_or_default()
        },
    );
    for id in &plan.declined {
        record(id, LinearCancel::Declined, cx).await?;
    }
    let mut done = Vec::new();
    for state_write in &plan.writes {
        write_state(state_write, cx).await?;
        record(&state_write.task_id, LinearCancel::Cancelled, cx).await?;
        done.push(identifier_of(&targets, &state_write.task_id));
    }
    // A ticket the plan did not move (already completed or canceled in Linear, or missing) is
    // answered as declined, so it is not asked about again. Not one that is open again, nor a
    // group that still has an open leaf: that one is asked again once its leaves are done.
    let tree = cx
        .update(|cx| snapshot(cx))
        .ok_or_else(|| "the work tree is not loaded".to_string())?;
    for target in &targets {
        let handled =
            plan.declined.contains(&target.task_id) || plan.cancelled.contains(&target.task_id);
        let still_cancelled = tree.task_by_id(&target.task_id).is_some_and(|task| {
            task.file
                .as_ref()
                .is_some_and(|file| file.status == Status::Cancelled)
        });
        if !handled && still_cancelled && may_cancel_group(&tree, &target.task_id).is_ok() {
            record(&target.task_id, LinearCancel::Declined, cx).await?;
        }
    }
    Ok(cancelled_message(&done))
}

fn identifier_of(targets: &[CancelTarget], task_id: &str) -> String {
    targets
        .iter()
        .find(|target| target.task_id == task_id)
        .map(|target| target.identifier.clone())
        .unwrap_or_else(|| task_id.to_string())
}

/// Every ticket of the question, and those of them the answer may move.
fn targets_of(
    question: &CancelQuestion,
    answer: CancelAnswer,
) -> (Vec<CancelTarget>, Vec<CancelTarget>) {
    match question {
        CancelQuestion::Leaf(leaf) => {
            let wanted = if answer == CancelAnswer::Cancel {
                vec![leaf.clone()]
            } else {
                Vec::new()
            };
            (vec![leaf.clone()], wanted)
        }
        CancelQuestion::Group { group, leaves } => {
            let mut all = leaves.clone();
            all.push(group.clone());
            let mut wanted = Vec::new();
            if matches!(answer, CancelAnswer::LeavesOnly | CancelAnswer::All) {
                wanted.extend(leaves.iter().cloned());
            }
            if answer == CancelAnswer::All {
                wanted.push(group.clone());
            }
            (all, wanted)
        }
    }
}

async fn record(id: &str, answer: LinearCancel, cx: &mut AsyncApp) -> Result<(), String> {
    let target = id.to_string();
    write(cx, move |tree, ctx| {
        record_cancel(tree, ctx, &target, answer)
    })
    .await
}
