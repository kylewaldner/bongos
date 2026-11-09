//! The app side of bongos chat sessions: the `chats` settings, [`ChatStore`] (the global index of
//! chat records, built from the work store's snapshot, and their serial writer), seeding of the
//! session types, the health of the agent CLIs, [`ChatRuntime`] (the running chats and their live
//! state), the send path, the `chat` actions and their commands (new, open, next, previous, close,
//! with the scope rules of [`scope`] and the never-prompted cleanup of [`cleanup`]), the commands
//! archive, restore, rename and delete, the offer to archive a finished task's chats
//! ([`lifecycle`]), fork and fork to task ([`fork`]), the artifacts view with its stale marker
//! ([`artifacts_view`]) and the session type files command ([`types_cmd`]). Everything is off
//! while `chats.enabled` is false. The file formats and pure rules live in `bongos_chat_model`.

mod actions;
mod artifacts_view;
mod cleanup;
mod commands;
mod facts;
mod fork;
mod health;
mod launch;
mod layout;
mod lifecycle;
mod list_view;
mod live_modes;
mod panel;
mod runtime;
mod scope;
mod send;
mod session_view;
mod settings;
mod store;
mod types_cmd;
mod view_state;

use std::path::{Path, PathBuf};

use ::settings::Settings as _;
pub use bongos_chat_model as model;
use bongos_work::WorkStore;
use gpui::{App, AppContext as _, Entity, Global};

pub use actions::{action_types, chat, init_palette};
pub use artifacts_view::{ROW_NOTE, STALE_LINE, artifacts_text, is_stale, stale_chat};
pub use cleanup::{Cleanup, KeepReason, cleanup_verdict};
pub use commands::{agent_choices, list_types};
pub use facts::task_facts;
pub use health::{
    ChatHealth, ChatHealthEvent, Health, HealthState, HealthStats, MIN_REFRESH_INTERVAL,
    PROBE_TIMEOUT, find_program, fix_logged_out, fix_login_unchecked, fix_not_found,
    fix_not_probed,
};
pub use launch::{
    CLAUDE_SETTINGS_FILE, CODEX_INSTRUCTIONS_FILE, CONTEXT_FILE, LaunchError, LaunchRequest,
    PLUGIN_DIR, SYSTEM_FILE,
};
pub use layout::PROMPT_PANE_OFF;
pub use live_modes::{SOURCE_NAME, start_current_todo};
pub use panel::{ChatPanel, init_panel};
pub use runtime::{
    ACTIVE_DEBOUNCE, CAPTURE_INTERVAL, CAPTURE_WINDOW, ChatRuntime, ChatRuntimeEvent, ChatState,
    LiveChat, MID_TURN, RuntimeStats,
};
pub use scope::{
    ChatScope, CwdSource, IMPLEMENT_TYPE, START_FIRST, project_repositories, resolve_scope,
};
pub use send::{
    BRACKETED_WAIT, ChatToast, SendError, SendOptions, Sent, chat_for_prompts_file, send,
};
pub use session_view::{
    PromptPaneProvider, SessionView, SessionViewEvent, set_prompt_pane_provider,
};
pub use settings::{AgentCommand, ChatsSettings};
pub use store::{ChatStats, ChatStore, ChatStoreEvent, ChatWriteError, IndexedChat};
pub use types_cmd::type_files;

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

/// Registers the actions of the `chat` namespace that have handlers on `workspace`.
pub fn register(workspace: &mut workspace::Workspace) {
    send::register(workspace);
    panel::register(workspace);
    commands::register(workspace);
    artifacts_view::register(workspace);
    types_cmd::register(workspace);
}

struct GlobalChatStore(Entity<ChatStore>);

impl Global for GlobalChatStore {}

/// Registers the `chats` settings and creates the global [`ChatHealth`], [`ChatStore`] and
/// [`ChatRuntime`]. Call after `bongos_work::init`. The store reads and writes nothing, and the
/// runtime starts nothing, until `chats.enabled` is true.
pub fn init(dirs: ChatDirs, cx: &mut App) {
    ChatsSettings::register(cx);
    let health = cx.new(|cx| ChatHealth::new(Default::default(), cx));
    ChatHealth::set_global(health, cx);
    let Some(work) = WorkStore::global(cx) else {
        log::error!("chats: the work store is missing; chats stay off");
        return;
    };
    let store = cx.new(|cx| ChatStore::new(work, dirs, cx));
    cx.set_global(GlobalChatStore(store.clone()));
    let runtime = cx.new(|cx| ChatRuntime::new(store.clone(), util::paths::home_dir().clone(), cx));
    live_modes::init(&runtime, &store, cx);
    lifecycle::init(&store, cx);
    ChatRuntime::set_global(runtime, cx);
}

impl ChatStore {
    /// The global store, once [`init`] has run.
    pub fn global(cx: &App) -> Option<Entity<ChatStore>> {
        cx.try_global::<GlobalChatStore>()
            .map(|global| global.0.clone())
    }
}

#[cfg(test)]
mod artifacts_tests;

#[cfg(test)]
mod commands_tests;

#[cfg(test)]
mod facts_tests;

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod list_tests;

#[cfg(test)]
mod panel_tests;

#[cfg(test)]
mod fork_tests;

#[cfg(test)]
mod health_tests;

#[cfg(test)]
mod runtime_tests;

#[cfg(test)]
mod send_tests;

#[cfg(test)]
mod store_tests;
