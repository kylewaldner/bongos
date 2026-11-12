//! The plan todo and the plan chat: a new task starts with one `plan` todo while the builder is
//! on, `task: new without plan` makes one with the kind's defaults, and `task: plan` starts (or
//! shows) the task's `plan` chat and pastes its opening line once the chat runs.

use std::collections::HashSet;
use std::time::Duration;

use bongos_builder_model::next_number;
use bongos_chat::model::timestamp_utc;
use bongos_chat::{
    ChatRuntime, ChatRuntimeEvent, ChatState, ChatStore, ChatStoreEvent, SendOptions,
};
use bongos_work::model::commands::{self as model, Context as WorkContext, NewTask, Placement};
use bongos_work::model::layout::proposal_file;
use bongos_work::model::{Todo, TodoRole, TodoState};
use bongos_work::{DEFAULT_ACTIVITY, WorkStore};
use gpui::{App, Context, Global, Subscription, Task, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::PLAN_TYPE;
use crate::TaskBuilderSettings;
use crate::actions::show_toast;

/// The name of the todo a new task starts with.
pub const PLAN_TODO: &str = "plan";

/// How long after `task: plan` a new plan chat on the task still gets the opening line. The agent
/// picker the command opens may stay up for a while, but not forever.
const OPENING_LINE_WAIT: Duration = Duration::from_secs(300);

/// The one todo of a new task while the builder is on: `plan`, role `plans`, no workflow.
pub fn plan_todos() -> Vec<Todo> {
    vec![Todo {
        id: None,
        name: PLAN_TODO.to_string(),
        role: Some(TodoRole::Plans),
        workflow: None,
        state: TodoState::Open,
        started: None,
        extra: Default::default(),
    }]
}

/// Gives every new task a `plan` todo while `task_builder.enabled` is true, through the work
/// tree's hook; while it is false the hook says nothing, so the kind's defaults apply.
pub(crate) fn install_hooks(cx: &mut App) {
    bongos_work::set_new_task_todos(
        |cx| TaskBuilderSettings::get_global(cx).enabled.then(plan_todos),
        cx,
    );
}

/// `task: new without plan`: like `task: new`, a one-off asked for its goal, but with the todos of
/// its kind.
pub(crate) fn new_without_plan(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if bongos_work::ready(workspace, cx).is_none() {
        return;
    }
    bongos_work::ask(
        workspace,
        "Goal of the new task, in one sentence",
        "",
        false,
        Box::new(|goal, workspace, _, cx| {
            let Some(store) = WorkStore::global(cx) else {
                show_toast(workspace, bongos_work::WriteError::Off.to_string(), cx);
                return;
            };
            let placement = Placement::OneOff {
                activity: DEFAULT_ACTIVITY.to_string(),
            };
            let new = NewTask {
                goal,
                todos: None,
                ..NewTask::default()
            };
            let write = store.update(cx, |store, cx| {
                store.write(
                    WorkContext::kyle(),
                    move |tree, ctx| model::new_task(tree, ctx, placement.clone(), new.clone()),
                    cx,
                )
            });
            cx.spawn(async move |workspace, cx| {
                if let Err(error) = write.await {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_toast(workspace, error.to_string(), cx)
                        })
                        .ok();
                }
            })
            .detach();
        }),
        window,
        cx,
    );
}

/// `task: plan`: on the task at point, else on a task picked by name.
pub(crate) fn plan(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if bongos_work::ready(workspace, cx).is_none() {
        return;
    }
    match bongos_work::task_at_point(workspace, window, cx) {
        Some(id) => plan_task(id, workspace, window, cx),
        None => bongos_work::with_task(
            workspace,
            window,
            cx,
            "Plan which task?",
            "There is no task to plan",
            |task, _| task.file.is_some(),
            Box::new(plan_task),
        ),
    }
}

/// Shows the newest plan chat of the task that is not archived, running or resumed; with none,
/// starts one and pastes the opening line once it runs.
fn plan_task(
    task_id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(chat_id) = newest_plan_chat(&task_id, cx) {
        let open = bongos_chat::open_chat(&chat_id, workspace, window, cx);
        cx.spawn(async move |workspace, cx| {
            // An empty message says the chat offered something else; nothing is left to say.
            if let Err(message) = open.await
                && !message.is_empty()
            {
                workspace
                    .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                    .ok();
            }
        })
        .detach();
        return;
    }
    let known = chat_ids_of(&task_id, cx);
    await_opening_line(task_id.clone(), known, cx);
    bongos_chat::start_chat_on_task_of_type(task_id, PLAN_TYPE, workspace, window, cx);
}

/// The ids of every indexed chat on the task `task_id`.
fn chat_ids_of(task_id: &str, cx: &App) -> HashSet<String> {
    let Some(store) = ChatStore::global(cx) else {
        return HashSet::new();
    };
    store
        .read(cx)
        .index()
        .values()
        .filter(|chat| chat.task_id.as_deref() == Some(task_id))
        .map(|chat| chat.entry.id())
        .collect()
}

/// The newest chat of type `plan` on the task, not archived.
fn newest_plan_chat(task_id: &str, cx: &App) -> Option<String> {
    let store = ChatStore::global(cx)?;
    store
        .read(cx)
        .index()
        .values()
        .filter(|chat| chat.task_id.as_deref() == Some(task_id))
        .filter_map(|chat| chat.entry.record())
        .filter(|record| record.chat_type == PLAN_TYPE && !record.archived)
        .max_by_key(|record| timestamp_utc(&record.created))
        .map(|record| record.id.clone())
}

/// A plan chat on `task_id` that is not in `known` and has a running process.
fn new_running_plan_chat(task_id: &str, known: &HashSet<String>, cx: &App) -> Option<String> {
    let runtime = ChatRuntime::global(cx)?;
    let runtime = runtime.read(cx);
    let store = ChatStore::global(cx)?;
    let store = store.read(cx);
    store
        .index()
        .values()
        .filter(|chat| chat.task_id.as_deref() == Some(task_id))
        .filter_map(|chat| chat.entry.record())
        .filter(|record| record.chat_type == PLAN_TYPE && !known.contains(&record.id))
        .find(|record| {
            runtime.live(&record.id).is_some_and(|live| {
                !matches!(live.state(), ChatState::Exited { .. } | ChatState::Closed)
            })
        })
        .map(|record| record.id.clone())
}

/// What waits for the plan chat `task: plan` started.
struct PendingOpening {
    _subscriptions: Vec<Subscription>,
    _expire: Task<()>,
}

/// At most one chat waits for its opening line; a newer `task: plan` replaces it.
#[derive(Default)]
struct GlobalPendingOpening(Option<PendingOpening>);

impl Global for GlobalPendingOpening {}

/// Waits for the plan chat to run on `task_id` (one that is not in `known`), then pastes its
/// opening line, once. Gives up after [`OPENING_LINE_WAIT`].
fn await_opening_line(task_id: String, known: HashSet<String>, cx: &mut Context<Workspace>) {
    let (Some(runtime), Some(store)) = (ChatRuntime::global(cx), ChatStore::global(cx)) else {
        return;
    };
    let check = move |cx: &mut Context<Workspace>| {
        if let Some(chat_id) = new_running_plan_chat(&task_id, &known, cx) {
            // Dropping the pending state drops the subscriptions: the line is pasted once.
            cx.default_global::<GlobalPendingOpening>().0 = None;
            send_opening_line(chat_id, &task_id, cx);
        }
    };
    let check = std::rc::Rc::new(check);
    let on_runtime = check.clone();
    let on_store = check;
    let subscriptions = vec![
        cx.subscribe(&runtime, move |_, _, _: &ChatRuntimeEvent, cx| {
            on_runtime(cx)
        }),
        cx.subscribe(&store, move |_, _, _: &ChatStoreEvent, cx| on_store(cx)),
    ];
    let expire = cx.spawn(async move |_, cx| {
        cx.background_executor().timer(OPENING_LINE_WAIT).await;
        cx.update(|cx| cx.default_global::<GlobalPendingOpening>().0 = None);
    });
    cx.default_global::<GlobalPendingOpening>().0 = Some(PendingOpening {
        _subscriptions: subscriptions,
        _expire: expire,
    });
}

/// The opening line of a plan chat: which proposal to write and where.
fn opening_line(task_id: &str, cx: &App) -> Option<String> {
    let tree = WorkStore::global(cx)?.read(cx).snapshot()?;
    let task = tree.task_by_id(task_id)?;
    let number = next_number(task);
    Some(format!(
        "Plan this task and write proposal {number} at {}.",
        proposal_file(&task.dir, number).display()
    ))
}

/// Pastes the opening line into the chat, submitted when its auto-submit says so.
fn send_opening_line(chat_id: String, task_id: &str, cx: &mut Context<Workspace>) {
    let Some(text) = opening_line(task_id, cx) else {
        return;
    };
    let submit = bongos_prompts::effective_auto_submit(Some(&chat_id), cx);
    let send = bongos_chat::send(&chat_id, text, SendOptions { submit }, cx);
    cx.spawn(async move |workspace, cx| {
        if let Err(error) = send.await {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, error.to_string(), cx)
                })
                .ok();
        }
    })
    .detach();
}
