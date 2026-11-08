//! The app side of bongos chat sessions: the `chats` settings, [`ChatStore`] (the global index of
//! chat records, built from the work store's snapshot, and their serial writer), seeding of the
//! session types, the health of the agent CLIs, and the `chat` actions. Everything is off while `chats.enabled` is false. The
//! file formats and pure rules live in `bongos_chat_model`.

mod actions;
mod health;
mod settings;
mod store;

use std::path::{Path, PathBuf};

use ::settings::Settings as _;
pub use bongos_chat_model as model;
use bongos_work::WorkStore;
use gpui::{App, AppContext as _, Entity, Global};

pub use actions::{action_types, chat, init_palette};
pub use health::{
    ChatHealth, ChatHealthEvent, Health, HealthState, HealthStats, MIN_REFRESH_INTERVAL,
    PROBE_TIMEOUT, find_program, fix_logged_out, fix_login_unchecked, fix_not_found,
    fix_not_probed,
};
pub use settings::{AgentCommand, ChatsSettings};
pub use store::{ChatStats, ChatStore, ChatStoreEvent, ChatWriteError, IndexedChat};

/// Where chats keep their files outside task directories. The app passes the real directories;
/// tests pass temporary ones, so no test touches the real home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatDirs {
    /// The session types directory, unless `chats.types_dir` names another.
    pub types_root: PathBuf,
    /// Where each chat's generated launch files go, one directory per chat.
    pub runtime: PathBuf,
}

impl ChatDirs {
    /// `<home>/.bongos/chat-types` and `<data_dir>/chats`.
    pub fn new(home: &Path, data_dir: &Path) -> Self {
        Self {
            types_root: home.join(".bongos").join("chat-types"),
            runtime: data_dir.join("chats"),
        }
    }
}

struct GlobalChatStore(Entity<ChatStore>);

impl Global for GlobalChatStore {}

/// Registers the `chats` settings and creates the global [`ChatHealth`] and [`ChatStore`]. Call after
/// `bongos_work::init`. The store reads and writes nothing until `chats.enabled` is true.
pub fn init(dirs: ChatDirs, cx: &mut App) {
    ChatsSettings::register(cx);
    let health = cx.new(|cx| ChatHealth::new(Default::default(), cx));
    ChatHealth::set_global(health, cx);
    let Some(work) = WorkStore::global(cx) else {
        log::error!("chats: the work store is missing; chats stay off");
        return;
    };
    let store = cx.new(|cx| ChatStore::new(work, dirs, cx));
    cx.set_global(GlobalChatStore(store));
}

impl ChatStore {
    /// The global store, once [`init`] has run.
    pub fn global(cx: &App) -> Option<Entity<ChatStore>> {
        cx.try_global::<GlobalChatStore>()
            .map(|global| global.0.clone())
    }
}

#[cfg(test)]
mod health_tests;

#[cfg(test)]
mod store_tests;
