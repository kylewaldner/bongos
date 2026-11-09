//! The Chats panel's saved state (design D20): the `bongos_chat` namespace of the key value store
//! holds `view` (`list`, or the id of the chat shown), `all_tasks`, `filter`, `archived_expanded`
//! and `last_agent`, so a new panel (a new window, a restart) comes back as the last one was.
//!
//! Every value is plain text. Reads are synchronous; writes run in the background. A missing or
//! unreadable value is the default.

use db::kvp::KeyValueStore;
use gpui::App;
use util::ResultExt as _;

/// The namespace of the key value store.
pub(crate) const KVP_NAMESPACE: &str = "bongos_chat";
/// `list`, or the id of the chat the panel shows.
pub(crate) const VIEW_KEY: &str = "view";
/// `true` while the list shows the chats of every task.
pub(crate) const ALL_TASKS_KEY: &str = "all_tasks";
/// What the list is filtered by.
pub(crate) const FILTER_KEY: &str = "filter";
/// `true` while the Archived section of the list is open.
pub(crate) const ARCHIVED_EXPANDED_KEY: &str = "archived_expanded";
/// The name of the agent `chat: new` started last.
pub(crate) const LAST_AGENT_KEY: &str = "last_agent";

/// The value of the `view` key while the list is shown.
pub(crate) const LIST_VIEW: &str = "list";

/// The saved value of `key`.
pub(crate) fn read(key: &str, cx: &App) -> Option<String> {
    KeyValueStore::global(cx)
        .scoped(KVP_NAMESPACE)
        .read(key)
        .log_err()
        .flatten()
}

/// Saves `value` under `key`; `None` forgets it.
pub(crate) fn write(key: &'static str, value: Option<String>, cx: &App) {
    let db = KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        let store = db.scoped(KVP_NAMESPACE);
        match value {
            Some(value) => store.write(key.to_string(), value).await,
            None => store.delete(key.to_string()).await,
        }
    });
}

/// Saves a flag: `true` is kept, `false` forgets the key (false is the default of each flag).
pub(crate) fn write_flag(key: &'static str, on: bool, cx: &App) {
    write(key, on.then(|| "true".to_string()), cx);
}

/// A saved flag.
pub(crate) fn read_flag(key: &str, cx: &App) -> bool {
    read(key, cx).as_deref() == Some("true")
}
