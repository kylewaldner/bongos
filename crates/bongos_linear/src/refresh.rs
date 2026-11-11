//! Refresh on demand: reading the linked tasks' tickets again and writing what changed.
//!
//! Never in the background: there is no timer and no polling. A refresh starts when the Work view
//! opens (`observe_view_opened`), when the focused task changes, and on `task: refresh from
//! linear`. The first two read only the linked tasks of the view's project (or the focused task
//! and its linked ancestors) and skip a task that was read in the last [`WINDOW_MS`]; the command
//! reads every linked task under the task at point (all of them with none) and ignores the window.
//! With no linked task, or while `linear.enabled` is false, nothing is sent and the token is not
//! even read.
//!
//! One refresh cycle runs at a time. A trigger that arrives meanwhile is queued, and everything
//! queued becomes one follow-up cycle. A cycle reads its tasks in batches sized by the estimated
//! complexity of a ticket read (the HTTP client then shrinks the budget when the server reports
//! a higher figure), one request at a time, every one through [`LinearSync::call`]. Failures
//! mark the batch's tasks (`unreachable`, `rate limited`, `not found`) and never block work;
//! automatic cycles stop after a rate limit or a rejected token, and only an explicit command
//! tries again. At the end the cycle writes every changed title and description in ONE
//! `WorkStore::write`, which re-checks on the tree it is handed that each task still has the link
//! the read was for, and writes nothing when nothing changed.
//!
//! The entity also keeps [`LinearSync::set_linked`] in step with the work store.

use std::collections::BTreeSet;

use bongos_linear_model::graphql::{BATCH_BUDGET, batch_size};
use bongos_linear_model::ident::parse_identifier;
use bongos_linear_model::sync::{RefreshDiff, linked_identifier, refresh_change, refresh_diff};
use bongos_linear_model::{ApiError, Issue, IssueRef};
use bongos_work::{Focus, WorkStore, WorkStoreEvent, WriteError, observe_view_opened};
use bongos_work_model::commands::{Change, Context as WorkContext};
use bongos_work_model::rules::Place;
use bongos_work_model::store::{Task, WorkTree};
use futures::future::BoxFuture;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, Subscription, WeakEntity, Window,
};
use settings::Settings as _;
use workspace::{MultiWorkspace, Workspace};

use crate::LinearSettings;
use crate::actions::show_toast;
use crate::client::now_ms;
use crate::sync::{self, ReadState, Refusal, Scope};
use crate::token;

/// A task read this recently is not read again by an automatic refresh.
pub(crate) const WINDOW_MS: u64 = 60_000;

/// Toast of `task: refresh from linear` with nothing linked under the task.
pub(crate) const NO_LINKED: &str = "No linked tasks to refresh";

/// The refusal reason of a write with nothing left to write; never shown.
const NOTHING: &str = "nothing to write";

// ---------------------------------------------------------------------------------------------
// Scopes: which tasks a trigger reads (pure)

/// The id of a synced task (`linear` and `linear_state` both set).
fn synced_id(task: &Task) -> Option<&str> {
    let file = task.file.as_ref()?;
    linked_identifier(file)?;
    Some(file.id.as_str())
}

fn linked_under(task: &Task) -> Vec<String> {
    task.walk()
        .into_iter()
        .filter_map(synced_id)
        .map(str::to_string)
        .collect()
}

/// Every linked task in `tree`.
pub(crate) fn linked_ids(tree: &WorkTree) -> Vec<String> {
    tree.tasks()
        .into_iter()
        .filter_map(synced_id)
        .map(str::to_string)
        .collect()
}

/// What opening the Work view reads: the linked tasks of the focused task's project (or one-off),
/// else every linked task.
pub(crate) fn view_scope(tree: &WorkTree, focused: Option<&str>) -> Vec<String> {
    let place = focused.and_then(|id| Place::locate(tree, id).ok());
    match place {
        Some(place) => match place.project {
            Some(project) => linked_under(&project.task),
            None => linked_under(place.ancestors.last().copied().unwrap_or(place.task)),
        },
        None => linked_ids(tree),
    }
}

/// What focusing a task reads: the task when it is linked, and its linked ancestors.
pub(crate) fn focus_scope(tree: &WorkTree, id: &str) -> Vec<String> {
    let Ok(place) = Place::locate(tree, id) else {
        return Vec::new();
    };
    std::iter::once(place.task)
        .chain(place.ancestors.iter().copied())
        .filter_map(synced_id)
        .map(str::to_string)
        .collect()
}

/// What the command reads for the task at point: the linked tasks at and under it.
pub(crate) fn subtree_scope(tree: &WorkTree, id: &str) -> Vec<String> {
    tree.task_by_id(id).map(linked_under).unwrap_or_default()
}

/// Whether Linear answered for a task less than [`WINDOW_MS`] before `now_ms`.
pub(crate) fn is_fresh(answered_at: Option<u64>, now_ms: u64) -> bool {
    answered_at.is_some_and(|at| now_ms.saturating_sub(at) < WINDOW_MS)
}

/// `items` split into batches of the size [`batch_size`] gives for `budget_points`.
pub(crate) fn batches<T: Clone>(items: &[T], budget_points: u32) -> Vec<Vec<T>> {
    items
        .chunks(batch_size(budget_points))
        .map(<[T]>::to_vec)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The report of an explicit refresh

/// What a cycle did, for the toast of the command.
#[derive(Debug, Default)]
pub(crate) struct Report {
    read: usize,
    not_found: usize,
    updated: usize,
    error: Option<ApiError>,
    refusal: Option<Refusal>,
    save_failed: bool,
}

/// The toast for a rate limit: when it ends, when Linear said.
pub(crate) fn rate_limit_message(reset_at: Option<u64>, now_ms: u64) -> String {
    match reset_at {
        Some(reset) if reset > now_ms => format!(
            "Linear rate limit reached; it resets in {} min",
            (reset - now_ms).div_ceil(60_000)
        ),
        _ => "Linear rate limit reached; try again later".to_string(),
    }
}

impl Report {
    /// The text of the toast. Fixed words and counts only: nothing from the request, the
    /// response or the token.
    pub(crate) fn message(&self, now_ms: u64) -> String {
        if let Some(refusal) = self.refusal {
            return refusal.to_string();
        }
        match &self.error {
            Some(ApiError::Unauthorized) => return Refusal::Rejected.to_string(),
            Some(ApiError::RateLimited { reset_at }) => {
                return rate_limit_message(*reset_at, now_ms);
            }
            Some(_) => return "Could not reach Linear; the tasks keep their last titles".into(),
            None => {}
        }
        let mut text = format!(
            "Refreshed {} linked task{} from Linear",
            self.read,
            if self.read == 1 { "" } else { "s" }
        );
        if self.updated > 0 {
            text.push_str(&format!("; {} changed", self.updated));
        }
        if self.not_found > 0 {
            text.push_str(&format!("; {} not found in Linear", self.not_found));
        }
        if self.save_failed {
            text.push_str("; the changes could not be saved");
        }
        text
    }
}

// ---------------------------------------------------------------------------------------------
// The entity

/// What was asked while a cycle ran.
#[derive(Clone, Debug, Default)]
struct Queued {
    /// Tasks of an opening or focus trigger: skipped when read within the window.
    auto: BTreeSet<String>,
    /// Tasks of the explicit command: read regardless.
    forced: BTreeSet<String>,
}

impl Queued {
    fn merge(&mut self, other: Queued) {
        self.auto.extend(other.auto);
        self.forced.extend(other.forced);
    }
}

struct Plan {
    token_present: bool,
    explicit: bool,
    batches: Vec<Vec<(String, IssueRef)>>,
}

/// The result of one batch.
#[derive(Default)]
struct Step {
    diff: RefreshDiff,
    error: Option<ApiError>,
    /// Whether the cycle ends here: a rate limit or a rejected token fails every batch.
    stop: bool,
    toast: Option<String>,
}

/// Who triggers refreshes and keeps the sync's linked set current. One per app.
#[derive(Default)]
pub(crate) struct Refresher {
    in_flight: bool,
    pending: Option<Queued>,
    /// Added to the clock, so a test can let a minute pass.
    skew_ms: u64,
    budget_override: Option<u32>,
    store_subscription: Option<Subscription>,
    focus_subscription: Option<Subscription>,
    last_focus: Option<String>,
}

struct GlobalRefresher(Entity<Refresher>);

impl Global for GlobalRefresher {}

struct ObserverInstalled;

impl Global for ObserverInstalled {}

/// Creates the refresher and hooks the view-opened signal. Call after [`sync::install`]. The
/// store and focus are attached now when they exist, else by [`connect`].
pub(crate) fn install(cx: &mut App) {
    let entity = cx.new(|_| Refresher::default());
    cx.set_global(GlobalRefresher(entity));
    if !cx.has_global::<ObserverInstalled>() {
        cx.set_global(ObserverInstalled);
        observe_view_opened(cx, |cx| {
            if let Some(refresher) = global(cx) {
                refresher.update(cx, |refresher, cx| refresher.view_opened(cx));
            }
        });
    }
    connect(cx);
}

pub(crate) fn global(cx: &App) -> Option<Entity<Refresher>> {
    cx.try_global::<GlobalRefresher>()
        .map(|global| global.0.clone())
}

/// Attaches to the work store and the focus, whichever exist and are not attached yet. The app
/// creates both before the sync; a test that creates them later calls this.
pub(crate) fn connect(cx: &mut App) {
    if let Some(refresher) = global(cx) {
        refresher.update(cx, |refresher, cx| refresher.attach(cx));
    }
}

/// Recomputes which tasks are linked from the store's tree, after a link or unlink.
pub(crate) fn sync_linked(cx: &mut App) {
    if let Some(refresher) = global(cx) {
        refresher.update(cx, |refresher, cx| refresher.sync_linked(cx));
    }
}

pub(crate) fn enabled(cx: &App) -> bool {
    LinearSettings::get_global(cx).enabled
}

/// The tree of an enabled, loaded store.
pub(crate) fn snapshot(cx: &App) -> Option<std::sync::Arc<WorkTree>> {
    let store = WorkStore::global(cx)?;
    let store = store.read(cx);
    store.is_enabled().then(|| store.snapshot()).flatten()
}

impl Refresher {
    fn attach(&mut self, cx: &mut Context<Self>) {
        if self.store_subscription.is_none()
            && let Some(store) = WorkStore::global(cx)
        {
            self.store_subscription = Some(cx.subscribe(&store, |this, _, event, cx| {
                if matches!(event, WorkStoreEvent::Changed) {
                    this.sync_linked(cx);
                }
            }));
            self.sync_linked(cx);
        }
        if self.focus_subscription.is_none()
            && let Some(focus) = Focus::global(cx)
        {
            self.last_focus = focus.read(cx).task_id().map(str::to_string);
            self.focus_subscription = Some(cx.observe(&focus, |this, focus, cx| {
                let now = focus.read(cx).task_id().map(str::to_string);
                if now != this.last_focus {
                    this.last_focus = now.clone();
                    if let Some(id) = now {
                        this.focused(&id, cx);
                    }
                }
            }));
        }
    }

    fn sync_linked(&mut self, cx: &mut Context<Self>) {
        let Some(linear) = sync::global(cx) else {
            return;
        };
        let ids = snapshot(cx)
            .map(|tree| linked_ids(&tree))
            .unwrap_or_default();
        linear.update(cx, |linear, _| linear.set_linked(ids));
    }

    fn now(&self) -> u64 {
        now_ms() + self.skew_ms
    }

    fn budget(&self) -> u32 {
        self.budget_override.unwrap_or(BATCH_BUDGET)
    }

    /// The Work view opened (or `task: focus` began).
    pub(crate) fn view_opened(&mut self, cx: &mut Context<Self>) {
        let Some(tree) = snapshot(cx) else {
            return;
        };
        let focused = bongos_work::focused(&tree, cx);
        self.request(view_scope(&tree, focused.as_deref()), false, cx);
    }

    /// A task was focused.
    fn focused(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(tree) = snapshot(cx) else {
            return;
        };
        self.request(focus_scope(&tree, id), false, cx);
    }

    /// Asks for `ids` to be read: now, or in the one follow-up when a cycle is running. `forced`
    /// ignores the 60 second window. Nothing happens for no ids or while the sync is off.
    pub(crate) fn request(&mut self, ids: Vec<String>, forced: bool, cx: &mut Context<Self>) {
        if ids.is_empty() || !enabled(cx) {
            return;
        }
        let mut queued = Queued::default();
        let set = if forced {
            &mut queued.forced
        } else {
            &mut queued.auto
        };
        set.extend(ids);
        if self.in_flight {
            self.pending
                .get_or_insert_with(Queued::default)
                .merge(queued);
        } else {
            self.start(queued, cx);
        }
    }

    fn start(&mut self, queued: Queued, cx: &mut Context<Self>) {
        self.in_flight = true;
        cx.spawn(async move |this, cx| {
            let outcome = cycle(&this, queued, cx).await;
            // A started write that failed is tried again after each refresh.
            cx.update(crate::status::refreshed);
            this.update(cx, |this, cx| {
                this.in_flight = false;
                if let Some(next) = this.pending.take() {
                    this.start(next, cx);
                }
            })
            .ok();
            if let Some(message) = outcome {
                cx.update(|cx| toast_active(message, cx));
            }
        })
        .detach();
    }

    /// Decides what a cycle reads: the forced tasks and the automatic ones not read within the
    /// window, still linked in the tree, as batches.
    fn plan(&mut self, queued: Queued, cx: &mut Context<Self>) -> Option<Plan> {
        let tree = snapshot(cx)?;
        let linear = sync::global(cx)?;
        self.sync_linked(cx);
        let now = self.now();
        let explicit = !queued.forced.is_empty();
        let mut ids = queued.forced;
        if explicit || linear.read(cx).auto_allowed(now) {
            ids.extend(
                queued
                    .auto
                    .into_iter()
                    .filter(|id| !is_fresh(linear.read(cx).answered_at(id), now)),
            );
        }
        let mut entries = Vec::new();
        for id in ids {
            let reference = tree
                .task_by_id(&id)
                .and_then(|task| task.file.as_ref())
                .and_then(linked_identifier)
                .and_then(parse_identifier)
                .map(IssueRef::from);
            match reference {
                Some(reference) => entries.push((id, reference)),
                None if tree.task_by_id(&id).is_some() => {
                    linear.update(cx, |linear, _| {
                        linear.set_read(&id, ReadState::NotFound, now)
                    });
                }
                None => {}
            }
        }
        if entries.is_empty() {
            return None;
        }
        Some(Plan {
            token_present: linear.read(cx).token_present(),
            explicit,
            batches: batches(&entries, self.budget()),
        })
    }
}

/// One cycle. `Some(text)` for a cycle that has a toast to show: an explicit one, or the first
/// rejection of the token.
async fn cycle(this: &WeakEntity<Refresher>, queued: Queued, cx: &mut AsyncApp) -> Option<String> {
    let plan = this.update(cx, |this, cx| this.plan(queued, cx)).ok()??;
    if !plan.token_present && !token::load(cx).await {
        // No token: the explicit command already said so, and an automatic refresh stays quiet.
        return None;
    }
    let explicit = plan.explicit;
    let mut report = Report::default();
    let mut diff = RefreshDiff::default();
    let mut toast = None;
    let batch_count = plan.batches.len();
    for index in 0..batch_count {
        let ids: Vec<String> = plan.batches[index]
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        let refs: Vec<IssueRef> = plan.batches[index]
            .iter()
            .map(|(_, reference)| reference.clone())
            .collect();
        let unsent: Vec<String> = plan.batches[index + 1..]
            .iter()
            .flatten()
            .map(|(id, _)| id.clone())
            .collect();
        let sent = cx.update(|cx| send(&ids, refs, cx));
        let result = match sent {
            Ok(request) => request.await,
            Err(refusal) => {
                report.refusal = Some(refusal);
                break;
            }
        };
        let step = this
            .update(cx, |this, cx| this.absorb(&ids, &unsent, result, cx))
            .ok()?;
        report.read += ids.len() - step.diff.not_found.len();
        report.not_found += step.diff.not_found.len();
        diff.changes.extend(step.diff.changes);
        diff.not_found.extend(step.diff.not_found);
        toast = toast.or(step.toast);
        if let Some(error) = step.error {
            report.error = Some(error);
        }
        if step.stop {
            break;
        }
    }
    if !diff.changes.is_empty() {
        let changed = diff.changes.len();
        let written = cx.update(|cx| apply_changes(diff, cx));
        if let Some(written) = written {
            match written.await {
                Ok(_) => report.updated = changed,
                Err(WriteError::Refused(_)) => {}
                Err(error) => {
                    log::warn!("could not save the Linear refresh: {error}");
                    report.save_failed = true;
                }
            }
        }
    }
    let now = this.update(cx, |this, _| this.now()).ok()?;
    if explicit {
        Some(report.message(now))
    } else {
        toast
    }
}

/// Starts the read of one batch through the gate.
fn send(
    ids: &[String],
    refs: Vec<IssueRef>,
    cx: &mut App,
) -> Result<BoxFuture<'static, Result<Vec<Issue>, ApiError>>, Refusal> {
    let linear = sync::global(cx).ok_or(Refusal::NoClient)?;
    linear.update(cx, |linear, cx| {
        linear.call(Scope::LinkedTasks(ids), cx, move |client| {
            client.issues(refs)
        })
    })
}

impl Refresher {
    /// Takes in the result of one batch: per-task read states, suggestions and the diff.
    fn absorb(
        &mut self,
        ids: &[String],
        unsent: &[String],
        result: Result<Vec<Issue>, ApiError>,
        cx: &mut Context<Self>,
    ) -> Step {
        let now = self.now();
        let (Some(linear), Some(tree)) = (sync::global(cx), snapshot(cx)) else {
            return Step {
                stop: true,
                ..Step::default()
            };
        };
        let mut step = Step::default();
        match result {
            Ok(issues) => {
                step.diff = refresh_diff(&tree, ids, &issues);
                let diff = &step.diff;
                linear.update(cx, |linear, _| {
                    for id in ids {
                        let state = if diff.not_found.contains(id) {
                            ReadState::NotFound
                        } else {
                            ReadState::Fresh { at: now }
                        };
                        linear.set_read(id, state, now);
                    }
                    linear.replace_suggestions(ids, diff.suggestions.clone());
                    linear.clear_limit();
                });
            }
            Err(error) => {
                let state = ReadState::from_error(&error);
                let stop = matches!(error, ApiError::Unauthorized | ApiError::RateLimited { .. });
                step.stop = stop;
                // Fixed words only: the error's own text never reaches a log or a toast.
                let kind = match &error {
                    ApiError::Unauthorized => "the token was rejected",
                    ApiError::RateLimited { .. } => "rate limited",
                    _ => "unreachable",
                };
                log::warn!("linear refresh failed: {kind}");
                linear.update(cx, |linear, _| {
                    let failed = ids
                        .iter()
                        .chain(stop.then_some(unsent).into_iter().flatten());
                    for id in failed {
                        linear.set_read(id, state, now);
                    }
                    match &error {
                        ApiError::Unauthorized => {
                            if linear.mark_rejected() {
                                step.toast = Some(Refusal::Rejected.to_string());
                            }
                        }
                        ApiError::RateLimited { reset_at } => linear.mark_limited(*reset_at),
                        _ => {}
                    }
                });
                step.error = Some(error);
            }
        }
        linear.update(cx, |linear, _| linear.note_low_requests());
        crate::chips::changed(cx);
        step
    }
}

/// The one write of a cycle: every changed title and description, if any is left to write.
/// `None` when nothing is, so that a refresh that finds nothing new writes nothing.
pub(crate) fn apply_changes(
    diff: RefreshDiff,
    cx: &mut App,
) -> Option<gpui::Task<Result<Change, WriteError>>> {
    if diff.changes.is_empty() {
        return None;
    }
    let store = WorkStore::global(cx)?;
    let tree = store.read(cx).snapshot()?;
    let ctx = WorkContext::new("linear", bongos_work_model::now());
    refresh_change(&tree, &ctx, &diff)?;
    Some(store.update(cx, |store, cx| {
        store.write(
            ctx,
            move |tree, ctx| refresh_change(tree, ctx, &diff).ok_or_else(|| NOTHING.to_string()),
            cx,
        )
    }))
}

/// Shows `message` in the active window's workspace.
pub(crate) fn toast_active(message: String, cx: &mut App) {
    let window = cx
        .active_window()
        .into_iter()
        .chain(cx.windows())
        .find_map(|window| window.downcast::<MultiWorkspace>());
    if let Some(window) = window {
        window
            .update(cx, |multi, _, cx| {
                multi
                    .workspace()
                    .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            })
            .ok();
    }
}

/// `task: refresh from linear`: reads every linked task at and under the task at point, or all
/// of them with none, whatever the 60 second window says.
pub(crate) fn refresh_from_linear(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let ids = match bongos_work::task_at_point(workspace, window, cx) {
        Some(id) => subtree_scope(&tree, &id),
        None => linked_ids(&tree),
    };
    if ids.is_empty() {
        show_toast(workspace, NO_LINKED.to_string(), cx);
        return;
    }
    token::with_token(cx, move |_, _, cx| {
        // The setting may have changed while the token was read.
        if !enabled(cx) {
            return;
        }
        if let Some(refresher) = global(cx) {
            refresher.update(cx, |refresher, cx| refresher.request(ids, true, cx));
        }
    });
}

#[cfg(test)]
impl Refresher {
    pub(crate) fn advance_clock(&mut self, ms: u64) {
        self.skew_ms += ms;
    }

    pub(crate) fn set_budget(&mut self, points: u32) {
        self.budget_override = Some(points);
    }

    /// Whether a cycle is running.
    pub(crate) fn busy(&self) -> bool {
        self.in_flight
    }
}
