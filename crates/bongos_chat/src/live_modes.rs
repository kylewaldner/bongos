//! What chats tell the Work view (design D23): the `chats` [`LiveModesSource`], which reports the
//! chats open on each task, and the start of the task's current todo when a chat starts on it.
//!
//! A chat is open from its launch until its process ends; an exited chat the panel still shows
//! does not count. The registry is told when a chat starts or ends, through [`watch_runtime`],
//! and when the chat index changes (a record found, moved or removed changes the task a chat
//! counts for), so the Work store recomputes its badges.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use bongos_work::{LiveModes, LiveModesRegistry, LiveModesSource, WorkStore, WriteError};
use bongos_work_model::commands::{Context, todo_start};
use bongos_work_model::derive::{self, Shape, current_todo};
use bongos_work_model::now;
use gpui::{App, Entity, Task};

use crate::{ChatRuntime, ChatRuntimeEvent, ChatState, ChatStore, ChatStoreEvent};

/// The name of the source in the registry.
pub const SOURCE_NAME: &str = "chats";

/// Whether a chat in `state` counts as open: its process runs.
pub(crate) fn is_open(state: ChatState) -> bool {
    !matches!(state, ChatState::Exited { .. } | ChatState::Closed)
}

/// Reports the open chats of a task: the live chats of `runtime` whose record is on that task.
struct ChatLiveModes {
    runtime: Entity<ChatRuntime>,
    store: Entity<ChatStore>,
}

impl LiveModesSource for ChatLiveModes {
    fn live_modes(&self, task_id: &str, cx: &App) -> LiveModes {
        let store = self.store.read(cx);
        let runtime = self.runtime.read(cx);
        let open_chats = runtime
            .live_ids()
            .filter(|id| {
                is_open(runtime.state(id))
                    && store
                        .find(id)
                        .is_some_and(|chat| chat.task_id.as_deref() == Some(task_id))
            })
            .count();
        LiveModes {
            open_chats,
            run: None,
        }
    }
}

/// Registers the `chats` source and tells the registry whenever a chat starts or ends.
pub(crate) fn init(runtime: &Entity<ChatRuntime>, store: &Entity<ChatStore>, cx: &mut App) {
    let source = Rc::new(ChatLiveModes {
        runtime: runtime.clone(),
        store: store.clone(),
    });
    LiveModesRegistry::add_source(SOURCE_NAME, source, cx);
    watch_runtime(runtime, cx);
    cx.subscribe(store, |_, event, cx| {
        let ChatStoreEvent::Changed = event;
        LiveModesRegistry::changed(cx);
    })
    .detach();
}

/// Calls [`LiveModesRegistry::changed`] when a chat of `runtime` becomes open or stops being open.
/// Other changes (a title, a bell) do not move the count, so they do not notify.
fn watch_runtime(runtime: &Entity<ChatRuntime>, cx: &mut App) {
    let open: Rc<RefCell<HashSet<String>>> = Rc::default();
    cx.subscribe(runtime, move |runtime, event, cx| {
        let ChatRuntimeEvent::Changed(id) = event;
        let now_open = is_open(runtime.read(cx).state(id));
        let changed = if now_open {
            open.borrow_mut().insert(id.clone())
        } else {
            open.borrow_mut().remove(id)
        };
        if changed {
            LiveModesRegistry::changed(cx);
        }
    })
    .detach();
}

/// A chat started on the leaf `task_id`: when the leaf's current todo (the first open one) has no
/// `started` time, writes it, through the store's writer as `chat` (03 D8: "a chat opened while it
/// is current"). Resolves to whether this call started it. A group, a task without an open todo,
/// and a todo that already started are left alone: the command's own refusal is not an error here.
pub fn start_current_todo(
    work: &Entity<WorkStore>,
    task_id: &str,
    cx: &mut App,
) -> Task<Result<bool, WriteError>> {
    let Some(tree) = work.read(cx).snapshot() else {
        return Task::ready(Ok(false));
    };
    let Some(task) = tree.task_by_id(task_id) else {
        return Task::ready(Ok(false));
    };
    let unstarted = derive::shape(task, &tree) == Shape::Leaf
        && current_todo(task).is_some_and(|todo| todo.started.is_none());
    if !unstarted {
        return Task::ready(Ok(false));
    }
    let id = task_id.to_string();
    let write = work.update(cx, |work, cx| {
        work.write(
            Context::new("chat", now()),
            move |tree, ctx| todo_start(tree, ctx, &id, None),
            cx,
        )
    });
    cx.spawn(async move |_| match write.await {
        Ok(_) => Ok(true),
        Err(WriteError::Refused(_)) => Ok(false),
        Err(error) => Err(error),
    })
}
