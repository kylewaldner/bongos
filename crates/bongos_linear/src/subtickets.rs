//! Subtickets out (D11): after an applied proposal adds tasks under a linked group, and on
//! `task: create missing subtickets`, offer to create the Linear tickets those tasks lack.
//!
//! Detection uses only the work tree: on every store change, [`Watcher`] looks for applied
//! `[[proposals]]` records under a linked group that the group's `linear_state.offered` does not
//! list yet (`bongos_linear_model::subtickets::unoffered`). Kyle's apply opens the offer at once:
//! a read-only preview buffer listing each title and description, the checklist modal, then the
//! confirm. An agent's or a workflow's apply opens nothing: the group gets the chip `N need
//! tickets` and a needs-you reason (see `needs_you`) until the command runs on it and is answered.
//! Either answer records the records as offered; "Not now" and the unchecked leaves are added to
//! `declined`, which only the command lists again.
//!
//! The action handlers (the command, the offer's preview, checklist and confirm) are in
//! `subtickets_actions`; this file holds what the background sync also reaches.
//!
//! The promise: nothing is sent to Linear before Kyle confirms. Detection, the preview, the
//! checklist and the question send nothing at all. The only function here that sends a request
//! is [`create`], and it takes a [`confirm::Confirmed`], which only the "yes" of the confirm
//! question constructs. Creation goes leaf by leaf, parents first: a UUID is written as the
//! leaf's `pending_create` before the request, so a retry after a crash reuses it and a
//! `Duplicate` answer links the ticket that exists instead of making a second one. A failure stops
//! at that leaf and the toast names what was created.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bongos_linear_model::subtickets::{
    Missing, Scope as Listed, Unoffered, begin_create, create_input, finish_create, missing,
    record_offered, unoffered,
};
use bongos_linear_model::sync::{linked_identifier, nearest_linked_ancestor};
use bongos_linear_model::{ApiError, IssueRef, IssueSummary};
use bongos_work::{WorkStore, WorkStoreEvent, WriteError};
use bongos_work_model::commands::{Change, Context as WorkContext, Outcome};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;
use bongos_work_model::{Author, TaskId};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, Subscription};
use settings::Settings as _;
use uuid::Uuid;
use workspace::{MultiWorkspace, Workspace};

use crate::subtickets_actions::open_offer;

use crate::LinearSettings;
use crate::actions::show_toast;
use crate::checklist::Row;
use crate::needs_you;
use crate::refresh;
use crate::sync::{self, Scope};

/// Who the Linear sync's own writes are by.
pub(crate) const SYNC: &str = "linear sync";

/// The context of a write the background sync makes: a workflow named [`SYNC`], with no run.
pub(crate) fn sync_context() -> WorkContext {
    WorkContext::new(SYNC, bongos_work_model::now()).with_author(Author::workflow(SYNC))
}

/// The toast of the command on a task with no linked group at or above it.
pub(crate) const NO_GROUP: &str =
    "Select a task in a linked group first (task: link to linear on the group)";

// ---------------------------------------------------------------------------------------------
// Texts (pure)

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The question of the checklist: "3 leaves have no subticket. Create them under SEQ-120?"
pub(crate) fn offer_question(items: &[Missing], identifier: &str) -> String {
    let count = items.len();
    let what = if items.iter().all(|item| !item.is_group) {
        plural(count, "leaf has", "leaves have")
    } else {
        plural(count, "task has", "tasks have")
    };
    let them = if count == 1 { "it" } else { "them" };
    format!("{what} no subticket. Create {them} under {identifier}?")
}

/// The title of the preview buffer.
pub(crate) fn preview_title(identifier: &str) -> String {
    format!("New subtickets under {identifier}")
}

/// The text of the preview: each ticket to create, its title and its description, nested under
/// the ticket it hangs under.
pub(crate) fn preview_text(items: &[Missing], identifier: &str) -> String {
    let mut text = format!(
        "{} under {identifier} (nothing is sent to Linear until you confirm)\n",
        plural(items.len(), "new subticket", "new subtickets")
    );
    let depths = depths(items);
    for (item, depth) in items.iter().zip(depths) {
        let indent = "  ".repeat(depth);
        text.push('\n');
        let group = if item.is_group { " (group)" } else { "" };
        text.push_str(&format!("{indent}- {}{group}\n", item.title));
        for line in item.description.lines() {
            text.push_str(&format!("{indent}    {line}\n"));
        }
    }
    text
}

/// The question of the confirm, after the checklist.
pub(crate) fn confirm_question(count: usize, identifier: &str) -> String {
    format!(
        "Create {} under {identifier}? (no: Not now) ",
        plural(count, "subticket", "subtickets")
    )
}

/// How deep each item is nested under the other items: a group's tasks are one deeper.
fn depths(items: &[Missing]) -> Vec<usize> {
    let mut known: BTreeMap<&str, usize> = BTreeMap::new();
    items
        .iter()
        .map(|item| {
            let depth = known
                .get(item.parent_task_id.as_str())
                .map_or(0, |parent| parent + 1);
            known.insert(&item.task_id, depth);
            depth
        })
        .collect()
}

/// The rows of the checklist.
pub(crate) fn rows(items: &[Missing]) -> Vec<Row> {
    items
        .iter()
        .zip(depths(items))
        .map(|(item, depth)| Row {
            label: if item.is_group {
                format!("{} (group)", item.title)
            } else {
                item.title.clone()
            },
            depth,
        })
        .collect()
}

/// What the checklist's flags select: the checked items whose parent item (if any) is selected,
/// without groups left with nothing under them; and the leaves left out, which are declined.
pub(crate) fn select(items: &[Missing], checked: &[bool]) -> (Vec<Missing>, Vec<TaskId>) {
    let mut kept: Vec<bool> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let parent_kept = items
            .iter()
            .position(|other| other.task_id == item.parent_task_id)
            .is_none_or(|parent| kept[parent]);
        kept.push(checked.get(index).copied().unwrap_or(false) && parent_kept);
    }
    // A group with nothing kept under it gets no ticket.
    for index in (0..items.len()).rev() {
        if items[index].is_group && kept[index] {
            let id = &items[index].task_id;
            let any_child = items
                .iter()
                .zip(&kept)
                .any(|(other, kept)| *kept && &other.parent_task_id == id);
            kept[index] = any_child;
        }
    }
    let chosen = items
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(item, _)| item.clone())
        .collect();
    let declined = items
        .iter()
        .zip(&kept)
        .filter(|(item, kept)| !**kept && !item.is_group)
        .map(|(item, _)| item.task_id.clone())
        .collect();
    (chosen, declined)
}

/// The linked group the command works on for the task at point: the task when it is a linked
/// group, else its nearest linked ancestor group.
pub(crate) fn group_for(tree: &WorkTree, task_id: &str) -> Option<TaskId> {
    let place = Place::locate(tree, task_id).ok()?;
    let own = place
        .task
        .file
        .as_ref()
        .filter(|file| linked_identifier(file).is_some())
        .filter(|_| derive::shape(place.task, tree) == Shape::Group)
        .map(|file| file.id.clone());
    own.or_else(|| nearest_linked_ancestor(&place)?.id().map(str::to_string))
}

pub(crate) fn identifier_of(tree: &WorkTree, group_id: &str) -> Option<String> {
    let file = tree.task_by_id(group_id)?.file.as_ref()?;
    linked_identifier(file).map(str::to_string)
}

pub(crate) fn enabled(cx: &App) -> bool {
    LinearSettings::get_global(cx).enabled
}

/// The tree of an enabled, loaded store.
fn snapshot(cx: &App) -> Option<Arc<WorkTree>> {
    let store = WorkStore::global(cx)?;
    let store = store.read(cx);
    store.is_enabled().then(|| store.snapshot()).flatten()
}

// ---------------------------------------------------------------------------------------------
// The watcher

type OfferKey = (TaskId, u32, String);

fn key(offer: &Unoffered) -> OfferKey {
    (offer.root_id.clone(), offer.number, offer.hash.clone())
}

/// One offer: the tasks of a linked group that need a ticket, and the applied records it answers.
pub(crate) struct Offer {
    pub(crate) group_id: TaskId,
    pub(crate) identifier: String,
    pub(crate) items: Vec<Missing>,
    pub(crate) offers: Vec<Unoffered>,
}

/// Watches the store for applied records that need an offer, and keeps the groups that need
/// tickets for the chip and the needs-you reason. One per app.
#[derive(Default)]
pub(crate) struct Watcher {
    subscription: Option<Subscription>,
    /// Records whose offer was opened or answered in this session: never opened twice.
    handled: BTreeSet<OfferKey>,
    /// Groups with an offer open or a create running.
    busy: BTreeSet<TaskId>,
    /// Per linked group with an agent's or a workflow's apply, how many tasks need a ticket.
    need: BTreeMap<TaskId, usize>,
}

struct GlobalWatcher(Entity<Watcher>);

impl Global for GlobalWatcher {}

/// Creates the watcher and registers the needs-you source. Call after [`sync::install`]; the
/// store is attached now when it exists, else by [`connect`].
pub(crate) fn install(cx: &mut App) {
    let entity = cx.new(|_| Watcher::default());
    cx.set_global(GlobalWatcher(entity));
    needs_you::install(cx);
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

/// The linked groups that need tickets after an agent's or a workflow's apply, with how many.
pub(crate) fn need_tickets(cx: &App) -> Vec<(TaskId, usize)> {
    global(cx)
        .map(|watcher| {
            watcher
                .read(cx)
                .need
                .iter()
                .map(|(id, count)| (id.clone(), *count))
                .collect()
        })
        .unwrap_or_default()
}

/// The chip of `task_id` when it is a group that needs tickets.
pub(crate) fn need_chip(task_id: &str, cx: &App) -> Option<String> {
    let watcher = global(cx)?;
    let count = *watcher.read(cx).need.get(task_id)?;
    Some(needs_you::need_tickets(count))
}

pub(crate) fn release(group_id: &str, cx: &mut App) {
    if let Some(watcher) = global(cx) {
        watcher.update(cx, |watcher, _| {
            watcher.busy.remove(group_id);
        });
    }
}

/// Marks `group_id` as having an offer open; false when one already is.
pub(crate) fn claim(group_id: &str, cx: &mut App) -> bool {
    global(cx).is_some_and(|watcher| {
        watcher.update(cx, |watcher, _| watcher.busy.insert(group_id.to_string()))
    })
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
                this.scan(cx);
            }
        }));
        self.scan(cx);
    }

    /// Looks for applied records not offered yet. Sends nothing.
    fn scan(&mut self, cx: &mut Context<Self>) {
        let tree = if enabled(cx) { snapshot(cx) } else { None };
        let mut need = BTreeMap::new();
        let mut opens = Vec::new();
        let mut quiet = Vec::new();
        if let Some(tree) = tree {
            let mut groups: BTreeMap<TaskId, Vec<Unoffered>> = BTreeMap::new();
            for offer in unoffered(&tree) {
                groups
                    .entry(offer.group_id.clone())
                    .or_default()
                    .push(offer);
            }
            for (group_id, offers) in groups {
                if self.busy.contains(&group_id) {
                    continue;
                }
                let (Ok(items), Some(identifier)) = (
                    missing(&tree, &group_id, Listed::AfterApply),
                    identifier_of(&tree, &group_id),
                ) else {
                    continue;
                };
                let fresh: Vec<OfferKey> = offers
                    .iter()
                    .map(key)
                    .filter(|key| !self.handled.contains(key))
                    .collect();
                if items.is_empty() {
                    // Nothing to offer: the records are only noted as offered.
                    if !fresh.is_empty() {
                        self.handled.extend(fresh);
                        quiet.push((group_id, offers));
                    }
                    continue;
                }
                let person_fresh = offers
                    .iter()
                    .any(|offer| offer.by_person && fresh.contains(&key(offer)));
                if person_fresh {
                    self.handled.extend(fresh);
                    self.busy.insert(group_id.clone());
                    opens.push(Offer {
                        group_id,
                        identifier,
                        items,
                        offers,
                    });
                } else if offers.iter().any(|offer| !offer.by_person) {
                    need.insert(group_id, items.len());
                }
            }
        }
        let need_changed = need != self.need;
        self.need = need;
        cx.defer(move |cx| {
            if need_changed {
                crate::chips::changed(cx);
                needs_you::changed(cx);
            }
            for (group_id, offers) in quiet {
                note_offered(group_id, offers, cx);
            }
            for offer in opens {
                open_in_active_window(offer, cx);
            }
        });
    }
}

/// Records `offers` as offered with nothing declined: there was nothing to create.
fn note_offered(group_id: TaskId, offers: Vec<Unoffered>, cx: &mut App) {
    let Some(store) = WorkStore::global(cx) else {
        return;
    };
    let write = store.update(cx, |store, cx| {
        store.write(
            sync_context(),
            move |tree, ctx| record_offered(tree, ctx, &group_id, &offers, &[]),
            cx,
        )
    });
    log_failure(write, cx);
}

fn log_failure(write: gpui::Task<Result<Change, WriteError>>, cx: &mut App) {
    cx.background_spawn(async move {
        if let Err(error) = write.await {
            log::warn!("could not record the subtickets offer: {error}");
        }
    })
    .detach();
}

/// Opens the offer in the active window's workspace; with no window, it is left for the next
/// change.
fn open_in_active_window(offer: Offer, cx: &mut App) {
    let window = cx
        .active_window()
        .into_iter()
        .chain(cx.windows())
        .find_map(|window| window.downcast::<MultiWorkspace>());
    let Some(window) = window else {
        forget(offer, cx);
        return;
    };
    let mut offer = Some(offer);
    let opened = window.update(cx, |multi, window, cx| {
        let workspace = multi.workspace().clone();
        if let Some(offer) = offer.take() {
            workspace.update(cx, |workspace, cx| open_offer(workspace, offer, window, cx));
        }
    });
    if opened.is_err()
        && let Some(offer) = offer
    {
        forget(offer, cx);
    }
}

/// An offer that could not open: it may open on a later change.
fn forget(offer: Offer, cx: &mut App) {
    if let Some(watcher) = global(cx) {
        watcher.update(cx, |watcher, _| {
            watcher.busy.remove(&offer.group_id);
            for item in &offer.offers {
                watcher.handled.remove(&key(item));
            }
        });
    }
}

/// Writes the answer on the group: its records as offered, `declined` added to its list.
pub(crate) fn record(
    workspace: &mut Workspace,
    offer: &Offer,
    declined: Vec<TaskId>,
    ctx: WorkContext,
    cx: &mut Context<Workspace>,
) {
    if offer.offers.is_empty() && declined.is_empty() {
        return;
    }
    let (group_id, offers) = (offer.group_id.clone(), offer.offers.clone());
    let Some(store) = WorkStore::global(cx) else {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return;
    };
    let write = store.update(cx, |store, cx| {
        store.write(
            ctx,
            move |tree, ctx| record_offered(tree, ctx, &group_id, &offers, &declined),
            cx,
        )
    });
    log_failure(write, cx);
}

/// The confirmed selection, which only the "yes" of the confirm question makes.
pub(crate) mod confirm {
    use bongos_linear_model::subtickets::Missing;
    use bongos_work_model::TaskId;

    /// Kyle confirmed creating tickets for `items` under the linked group `group_id`. The only
    /// way to make one is [`answer`] with a yes, so a create request needs Kyle's confirm.
    pub(crate) struct Confirmed {
        pub(crate) group_id: TaskId,
        pub(crate) identifier: String,
        pub(crate) items: Vec<Missing>,
        _confirmed: (),
    }

    /// The answer of the confirm question: `Some` only for a yes.
    pub(crate) fn answer(
        yes: bool,
        offer: &super::Offer,
        items: Vec<Missing>,
    ) -> Option<Confirmed> {
        yes.then(|| Confirmed {
            group_id: offer.group_id.clone(),
            identifier: offer.identifier.clone(),
            items,
            _confirmed: (),
        })
    }
}

pub(crate) use confirm::Confirmed;

// ---------------------------------------------------------------------------------------------
// Creation: the only code here that sends a create request.

/// Why the creation of one ticket stopped.
fn stopped(created: &[String], title: &str, reason: &str) -> String {
    let done = if created.is_empty() {
        "Created no subticket".to_string()
    } else {
        format!("Created {}", created.join(", "))
    };
    format!("{done}; stopped at {title}: {reason}")
}

async fn write(
    cx: &mut AsyncApp,
    ctx: WorkContext,
    command: impl Fn(&WorkTree, &WorkContext) -> Outcome + Send + 'static,
) -> Result<Change, String> {
    let task = cx.update(|cx| {
        WorkStore::global(cx)
            .map(|store| store.update(cx, |store, cx| store.write(ctx, command, cx)))
    });
    match task {
        Some(task) => task.await.map_err(|error| error.to_string()),
        None => Err(WriteError::Off.to_string()),
    }
}

type Pending<T> = futures::future::BoxFuture<'static, Result<T, ApiError>>;

/// Starts a request about the linked group through the gate.
fn send<T>(
    group_id: &str,
    cx: &mut AsyncApp,
    request: impl FnOnce(&dyn bongos_linear_model::LinearApi) -> Pending<T>,
) -> Result<Pending<T>, String> {
    cx.update(|cx| {
        let linear =
            sync::global(cx).ok_or_else(|| "The Linear sync is not available".to_string())?;
        linear
            .update(cx, |linear, cx| {
                linear.call(Scope::LinkedTask(group_id), cx, request)
            })
            .map_err(|refusal| refusal.to_string())
    })
}

/// The ticket Linear has under `id`, after a `Duplicate` answer: the create went through before.
async fn read_by_id(group_id: &str, id: Uuid, cx: &mut AsyncApp) -> Result<IssueSummary, String> {
    let read = send(group_id, cx, |client| client.issues(vec![IssueRef::Id(id)]))?;
    let wanted = id.hyphenated().to_string();
    let issues = read.await.map_err(|error| error.to_string())?;
    issues
        .into_iter()
        .find(|issue| issue.id.eq_ignore_ascii_case(&wanted))
        .map(|issue| issue.summary())
        .ok_or_else(|| format!("{wanted} not found in Linear"))
}

/// Creates the confirmed tickets, parents first, and returns the toast. Each leaf: its
/// `pending_create` (a new UUID, or the one a crash left), the create with that UUID, then the
/// link. Stops at the first failure.
/// Each write is made as `ctx`, a person's context without its attestation: the writes are
/// state-only, so they need none.
pub(crate) async fn create(confirmed: Confirmed, ctx: WorkContext, cx: &mut AsyncApp) -> String {
    let Confirmed {
        group_id,
        identifier,
        items,
        ..
    } = confirmed;
    cx.update(refresh::sync_linked);
    let mut created = Vec::new();
    for item in items {
        let Some(tree) = cx.update(|cx| snapshot(cx)) else {
            return stopped(&created, &item.title, &WriteError::Off.to_string());
        };
        let Some(file) = tree
            .task_by_id(&item.task_id)
            .and_then(|task| task.file.as_ref())
        else {
            return stopped(&created, &item.title, "the task is gone");
        };
        if linked_identifier(file).is_some() {
            // Linked meanwhile: nothing to create.
            continue;
        }
        let pending = file
            .linear_state
            .as_ref()
            .and_then(|state| state.pending_create.as_deref())
            .map(Uuid::parse_str);
        let id = match pending {
            Some(Ok(id)) => id,
            Some(Err(_)) => {
                return stopped(&created, &item.title, "its pending create is not a UUID");
            }
            None => {
                let id = Uuid::new_v4();
                let task_id = item.task_id.clone();
                if let Err(reason) = write(cx, ctx.clone(), move |tree, ctx| {
                    begin_create(tree, ctx, &task_id, id, true)
                })
                .await
                {
                    return stopped(&created, &item.title, &reason);
                }
                id
            }
        };
        let Some(tree) = cx.update(|cx| snapshot(cx)) else {
            return stopped(&created, &item.title, &WriteError::Off.to_string());
        };
        let input = match create_input(&tree, &item, id) {
            Ok(input) => input,
            Err(reason) => return stopped(&created, &item.title, &reason),
        };
        let description = input.description.clone();
        let request = match send(&group_id, cx, move |client| client.create(input)) {
            Ok(request) => request,
            Err(reason) => return stopped(&created, &item.title, &reason),
        };
        let issue = match request.await {
            Ok(issue) => issue,
            Err(ApiError::Duplicate(_)) => match read_by_id(&group_id, id, cx).await {
                Ok(issue) => issue,
                Err(reason) => return stopped(&created, &item.title, &reason),
            },
            Err(error) => return stopped(&created, &item.title, &error.to_string()),
        };
        let task_id = item.task_id.clone();
        let linked = issue.clone();
        if let Err(reason) = write(cx, ctx.clone(), move |tree, ctx| {
            finish_create(tree, ctx, &task_id, &linked, &description)
        })
        .await
        {
            return stopped(&created, &item.title, &reason);
        }
        created.push(issue.identifier);
    }
    if created.is_empty() {
        format!("Every task under {identifier} already has a ticket")
    } else {
        format!("Created {} under {identifier}", created.join(", "))
    }
}
