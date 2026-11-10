//! `task: link to linear`, `task: unlink from linear` and `task: open in linear`, on the task at
//! point.
//!
//! Only link sends requests: a read of the recent tickets (and of the ticket of the nearest
//! linked ancestor group, for a leaf) to fill the picker, then one read of the chosen ticket. Both
//! go through [`LinearSync::call`] under [`Scope::LinkToLinear`], after the token check every
//! sending command makes. Unlink and open work on what is stored in the task and send nothing.
//! None of them touches a task other than the one at point, and the token is never read here: no
//! message in this file can carry it.
//!
//! The link itself is one `WorkStore::write` of `bongos_linear_model::sync::link`, which sets
//! `linear`, `linear_state` and the goal together, seeds a group's `offered` list with the
//! proposals already applied under it, and re-checks on the tree the writer hands it that no other
//! task links the ticket.

use bongos_linear_model::ident::{parse_identifier, parse_issue_ref};
use bongos_linear_model::sync::{self as model, linked_identifier, nearest_linked_ancestor};
use bongos_linear_model::{ApiError, Issue, IssueRef, IssueSummary};
use bongos_work::{WorkStore, WriteError};
use bongos_work_model::commands::{Change, Context as WorkContext, Outcome};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;
use gpui::{App, Context, Task, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::LinearSettings;
use crate::actions::show_toast;
use crate::refresh;
use crate::sync::{self, Scope};
use crate::token;

/// How many recent tickets the picker lists after the subtickets.
pub(crate) const RECENT_LIMIT: usize = 20;

/// The first row of the picker: type a ticket that is not listed.
pub(crate) const TYPED_ROW: &str = "Type an identifier or a Linear URL...";

/// The toast for an open on a task that has no known ticket URL.
pub(crate) const LINK_FIRST: &str = "link the task first (task: link to linear)";

const NO_TASK: &str = "Select a task first; a todo has no Linear link";

/// One row of the picker, after [`TYPED_ROW`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Choice {
    Typed,
    Ticket(IssueSummary),
}

/// The rows of the picker: [`TYPED_ROW`], then `subtickets` that no task links yet, then the
/// `recent` tickets that were not listed already.
pub(crate) fn choices(
    tree: &WorkTree,
    task_id: &str,
    subtickets: &[IssueSummary],
    recent: &[IssueSummary],
) -> Vec<(Choice, String)> {
    let mut rows = vec![(Choice::Typed, TYPED_ROW.to_string())];
    let mut seen: Vec<&str> = Vec::new();
    for ticket in subtickets {
        if model::linked_task(tree, &ticket.id, &ticket.identifier, Some(task_id)).is_some() {
            continue;
        }
        seen.push(&ticket.id);
        rows.push((
            Choice::Ticket(ticket.clone()),
            format!(
                "{}  {}  (subticket of the group's ticket)",
                ticket.identifier, ticket.title
            ),
        ));
    }
    for ticket in recent {
        if seen.contains(&ticket.id.as_str()) {
            continue;
        }
        seen.push(&ticket.id);
        rows.push((
            Choice::Ticket(ticket.clone()),
            format!("{}  {}", ticket.identifier, ticket.title),
        ));
    }
    rows
}

/// The question that asks Kyle to confirm linking `goal` to `issue`.
pub(crate) fn confirm_prompt(goal: &str, issue: &Issue) -> String {
    format!(
        "Link {goal} to {id} '{title}'? The task takes Linear's title, and bongos will move {id} \
         to started when its plan begins and ask before cancelling it.",
        id = issue.identifier,
        title = issue.title.trim(),
    )
}

/// Whether `issue` can be linked to `task_id` on `tree`, as the write would decide it: `Err` is
/// the reason to show. Checked before the question, so a ticket linked elsewhere is refused
/// without asking.
pub(crate) fn check(tree: &WorkTree, task_id: &str, issue: &Issue) -> Result<(), String> {
    model::link(tree, &WorkContext::kyle(), task_id, issue).map(|_| ())
}

/// The ticket reference typed or pasted: an identifier, a UUID or a Linear URL.
pub(crate) fn parse_typed(text: &str) -> Option<IssueRef> {
    parse_issue_ref(text)
}

pub(crate) fn selected_task(
    workspace: &mut Workspace,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> Option<String> {
    let task = bongos_work::task_at_point(workspace, window, cx);
    if task.is_none() {
        show_toast(workspace, NO_TASK.to_string(), cx);
    }
    task
}

fn enabled(cx: &App) -> bool {
    LinearSettings::get_global(cx).enabled
}

/// `task: link to linear`. The token is read first; no token means a toast and no request.
pub(crate) fn link_to_linear(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let Some(task_id) = selected_task(workspace, window, cx) else {
        return;
    };
    if let Some(task) = tree.task_by_id(&task_id)
        && let Some(existing) = task.file.as_ref().and_then(linked_identifier)
    {
        let message = format!(
            "{} is already linked to {existing}; unlink it first",
            task.slug
        );
        show_toast(workspace, message, cx);
        return;
    }
    let handle = window.window_handle();
    token::with_token(cx, move |_, _, cx| {
        // The setting may have changed while the token was read.
        if !enabled(cx) {
            return;
        }
        let workspace = cx.entity().downgrade();
        // The token read has no window; the picker needs one.
        cx.defer(move |cx| {
            handle
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| start(workspace, task_id, window, cx))
                        .ok();
                })
                .ok();
        });
    });
}

type Recent = Result<Vec<IssueSummary>, ApiError>;
type Subtickets = Option<Result<Vec<Issue>, ApiError>>;

/// Starts the reads that fill the picker.
fn start(
    workspace: &mut Workspace,
    task_id: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    // The ticket of the nearest linked ancestor group, for a leaf: its subtickets come first.
    let parent = Place::locate(&tree, &task_id)
        .ok()
        .filter(|place| derive::shape(place.task, &tree) == Shape::Leaf)
        .and_then(|place| nearest_linked_ancestor(&place))
        .and_then(|ancestor| ancestor.file.as_ref()?.linear.as_deref())
        .and_then(parse_identifier)
        .map(IssueRef::from);
    let Some(linear) = sync::global(cx) else {
        show_toast(
            workspace,
            "The Linear sync is not available".to_string(),
            cx,
        );
        return;
    };
    let started = linear.update(cx, |linear, cx| {
        let recent = linear.call(Scope::LinkToLinear, cx, |client| {
            client.recent(RECENT_LIMIT)
        })?;
        let parent = match parent {
            Some(reference) => Some(linear.call(Scope::LinkToLinear, cx, |client| {
                client.issues(vec![reference])
            })?),
            None => None,
        };
        Ok::<_, sync::Refusal>((recent, parent))
    });
    let (recent, parent) = match started {
        Ok(started) => started,
        Err(refusal) => {
            show_toast(workspace, refusal.to_string(), cx);
            return;
        }
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let recent: Recent = recent.await;
        let parent: Subtickets = match parent {
            Some(read) => Some(read.await),
            None => None,
        };
        workspace
            .update_in(cx, |workspace, window, cx| {
                open_picker(workspace, task_id, recent, parent, window, cx)
            })
            .ok();
    })
    .detach();
}

fn open_picker(
    workspace: &mut Workspace,
    task_id: String,
    recent: Recent,
    parent: Subtickets,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    // A failed read still leaves the typed row, so a ticket can be linked by its identifier.
    let mut recent_tickets = Vec::new();
    match recent {
        Ok(tickets) => recent_tickets = tickets,
        Err(error) => show_toast(
            workspace,
            format!("Could not list recent Linear tickets: {error}"),
            cx,
        ),
    }
    let mut subtickets = Vec::new();
    match parent {
        Some(Ok(issues)) => {
            subtickets = issues
                .into_iter()
                .flat_map(|issue| issue.children)
                .map(|child| child.summary())
                .collect();
        }
        Some(Err(error)) => show_toast(
            workspace,
            format!("Could not read the group's ticket: {error}"),
            cx,
        ),
        None => {}
    }
    let rows = choices(&tree, &task_id, &subtickets, &recent_tickets);
    let (picks, labels): (Vec<Choice>, Vec<String>) = rows.into_iter().unzip();
    let selected = usize::from(picks.len() > 1);
    bongos_work::pick(
        workspace,
        "Link to Linear ticket",
        labels,
        selected,
        Box::new(move |index, workspace, window, cx| match &picks[index] {
            Choice::Ticket(ticket) => match parse_identifier(&ticket.identifier) {
                Some(identifier) => chosen(workspace, task_id, identifier.into(), window, cx),
                None => show_toast(
                    workspace,
                    format!("{} is not a Linear identifier", ticket.identifier),
                    cx,
                ),
            },
            Choice::Typed => ask_ticket(workspace, task_id, window, cx),
        }),
        window,
        cx,
    );
}

fn ask_ticket(
    workspace: &mut Workspace,
    task_id: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    bongos_work::ask(
        workspace,
        "Linear ticket (identifier or URL)",
        "",
        false,
        Box::new(
            move |text, workspace, window, cx| match parse_typed(&text) {
                Some(reference) => chosen(workspace, task_id, reference, window, cx),
                None => show_toast(workspace, "Not a Linear identifier or URL".to_string(), cx),
            },
        ),
        window,
        cx,
    );
}

/// Reads the chosen ticket, then asks for the confirmation.
fn chosen(
    workspace: &mut Workspace,
    task_id: String,
    reference: IssueRef,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(linear) = sync::global(cx) else {
        show_toast(
            workspace,
            "The Linear sync is not available".to_string(),
            cx,
        );
        return;
    };
    let wanted = reference.clone();
    let started = linear.update(cx, |linear, cx| {
        linear.call(Scope::LinkToLinear, cx, |client| {
            client.issues(vec![wanted])
        })
    });
    let read = match started {
        Ok(read) => read,
        Err(refusal) => {
            show_toast(workspace, refusal.to_string(), cx);
            return;
        }
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let result = read.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Ok(issues) => match issues.into_iter().next() {
                    Some(issue) => confirm(workspace, task_id, issue, window, cx),
                    None => show_toast(
                        workspace,
                        format!("{reference} was not found in Linear"),
                        cx,
                    ),
                },
                Err(error) => show_toast(workspace, error.to_string(), cx),
            })
            .ok();
    })
    .detach();
}

/// Refuses a ticket another task links, else asks Link / Cancel.
fn confirm(
    workspace: &mut Workspace,
    task_id: String,
    issue: Issue,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    if let Err(reason) = check(&tree, &task_id, &issue) {
        show_toast(workspace, reason, cx);
        return;
    }
    let Some(task) = tree.task_by_id(&task_id) else {
        return;
    };
    let prompt = confirm_prompt(task.goal(), &issue);
    let weak = cx.entity().downgrade();
    bongos_minibuffer::yes_or_no(
        workspace,
        bongos_work::MINIBUFFER_NAME,
        prompt,
        false,
        move |yes, _, cx| {
            if !yes {
                return;
            }
            weak.update(cx, |workspace, cx| {
                finish_link(workspace, task_id, issue, cx)
            })
            .ok();
        },
        window,
        cx,
    );
}

/// Writes the link in one command and toasts the outcome. The command re-checks the ticket on
/// the tree the writer hands it, so a link made since the question still refuses.
pub(crate) fn finish_link(
    workspace: &mut Workspace,
    task_id: String,
    issue: Issue,
    cx: &mut Context<Workspace>,
) {
    let identifier = issue.identifier.clone();
    let Some(write) = queue(workspace, cx, move |tree, ctx| {
        model::link(tree, ctx, &task_id, &issue)
    }) else {
        return;
    };
    cx.spawn(async move |workspace, cx| {
        let message = match write.await {
            Ok(_) => format!("Linked to {identifier}"),
            Err(error) => error.to_string(),
        };
        workspace
            .update(cx, |workspace, cx| {
                // The store has the new link: the sync's linked set follows it.
                refresh::sync_linked(cx);
                show_toast(workspace, message, cx)
            })
            .ok();
    })
    .detach();
}

/// Queues `command` on the store's writer; `None`, after a toast, when there is no store.
pub(crate) fn queue(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
    command: impl Fn(&WorkTree, &WorkContext) -> Outcome + Send + 'static,
) -> Option<Task<Result<Change, WriteError>>> {
    let Some(store) = WorkStore::global(cx) else {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return None;
    };
    Some(store.update(cx, |store, cx| {
        store.write(WorkContext::kyle(), command, cx)
    }))
}

/// `task: unlink from linear`: removes `linear` and `linear_state` and keeps the goal. No request.
pub(crate) fn unlink_from_linear(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let Some(task_id) = selected_task(workspace, window, cx) else {
        return;
    };
    let identifier = tree
        .task_by_id(&task_id)
        .and_then(|task| task.file.as_ref())
        .and_then(|file| linked_identifier(file))
        .map(str::to_string);
    let Some(write) = queue(workspace, cx, move |tree, ctx| {
        model::unlink(tree, ctx, &task_id)
    }) else {
        return;
    };
    cx.spawn(async move |workspace, cx| {
        let message = match (write.await, identifier) {
            (Ok(_), Some(identifier)) => format!("Unlinked from {identifier}"),
            (Ok(_), None) => "Unlinked".to_string(),
            (Err(error), _) => error.to_string(),
        };
        workspace
            .update(cx, |workspace, cx| {
                refresh::sync_linked(cx);
                show_toast(workspace, message, cx)
            })
            .ok();
    })
    .detach();
}

/// `task: open in linear`: opens the stored ticket URL. No request.
pub(crate) fn open_in_linear(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let Some(task_id) = selected_task(workspace, window, cx) else {
        return;
    };
    let url = tree
        .task_by_id(&task_id)
        .and_then(|task| task.file.as_ref())
        .and_then(|file| file.linear_state.as_ref())
        .map(|state| state.url.as_str())
        .filter(|url| !url.is_empty());
    match url {
        Some(url) => cx.open_url(url),
        None => show_toast(workspace, LINK_FIRST.to_string(), cx),
    }
}
