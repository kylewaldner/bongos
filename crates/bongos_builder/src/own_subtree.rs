//! The `own-subtree` approval rule for agent proposals. An agent's change to the project is
//! approved on its own when every task and file it touches is the task its chat is indexed under
//! or below it; one outside it, or from a chat the index does not know, waits for a person.
//!
//! The chat's task comes from the chat index, never from the change: the hook reads a snapshot of
//! the index that [`refresh`] keeps (the landing engine judges on a background thread, where the
//! chat store cannot be read).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use bongos_chat::ChatStore;
use bongos_project_repo::{ApprovalHooks, OwnSubtreeHook};
use gpui::{App, Global};

use crate::later;

/// The task each indexed chat works on, by chat id.
#[derive(Clone, Default)]
pub(crate) struct ChatTasks(Arc<RwLock<HashMap<String, String>>>);

impl Global for ChatTasks {}

/// Registers the hook and keeps its view of the chat index current.
pub(crate) fn install(cx: &mut App) {
    let tasks = ChatTasks::default();
    let lookup = tasks.clone();
    ApprovalHooks::register(
        Arc::new(OwnSubtreeHook::new(Arc::new(move |chat: &str| {
            lookup.0.read().ok()?.get(chat).cloned()
        }))),
        cx,
    );
    cx.set_global(tasks);
    refresh(cx);
    if let Some(store) = ChatStore::global(cx) {
        cx.observe(&store, |_, cx| refresh(cx)).detach();
    }
}

/// Reads the chat index again.
pub(crate) fn refresh(cx: &mut App) {
    let Some(tasks) = cx.try_global::<ChatTasks>().cloned() else {
        return;
    };
    let index: HashMap<String, String> = later::chats(cx)
        .into_iter()
        .map(|(id, owner)| (id, owner.task_id))
        .collect();
    if let Ok(mut tasks) = tasks.0.write() {
        *tasks = index;
    }
}
