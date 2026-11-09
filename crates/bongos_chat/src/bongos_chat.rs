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
pub use commands::{
    agent_choices, list_types, offer_resume_and_send, open_chat, start_chat_on_task,
    start_chat_on_task_of_type,
};
pub use facts::task_facts;
pub use health::{
    ChatHealth, ChatHealthEvent, Health, HealthState, HealthStats, MIN_REFRESH_INTERVAL,
    PROBE_TIMEOUT, find_program, fix_logged_out, fix_login_unchecked, fix_not_found,
    fix_not_probed,
};
pub use launch::{
    CLAUDE_SETTINGS_FILE, CODEX_INSTRUCTIONS_FILE, CONTEXT_FILE, ContextExtension, LaunchError,
    LaunchRequest, PLUGIN_DIR, SYSTEM_FILE, set_context_extension,
};
pub use layout::PROMPT_PANE_OFF;
pub use live_modes::{SOURCE_NAME, start_current_todo};
pub use panel::{ChatPanel, focus_prompts, init_panel, open_prompts_in_center};
pub use runtime::{
    ACTIVE_DEBOUNCE, CAPTURE_INTERVAL, CAPTURE_WINDOW, ChatRuntime, ChatRuntimeEvent, ChatState,
    LiveChat, MID_TURN, RuntimeStats,
};
pub use scope::{
    ChatScope, CwdSource, IMPLEMENT_TYPE, START_FIRST, project_repositories, resolve_scope,
};
pub use send::{
    BRACKETED_WAIT, ChatRef, ChatToast, SendError, SendOptions, Sent, chat_for_prompts_file,
    chats_for_prompts_file, send, set_auto_submit, set_prompts_file,
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
    init_in(
        util::paths::home_dir().clone(),
        std::collections::BTreeMap::new(),
        dirs,
        cx,
    )
}

/// [`init`] with the home directory the agents keep their own files in and the environment added
/// to the CLI probes and launches, for a test that runs fake CLIs without touching the real home.
pub fn init_in(
    home: PathBuf,
    env: std::collections::BTreeMap<String, String>,
    dirs: ChatDirs,
    cx: &mut App,
) {
    ChatsSettings::register(cx);
    let health = cx.new(|cx| ChatHealth::new(env, cx));
    ChatHealth::set_global(health, cx);
    let Some(work) = WorkStore::global(cx) else {
        log::error!("chats: the work store is missing; chats stay off");
        return;
    };
    let store = cx.new(|cx| ChatStore::new(work, dirs, cx));
    cx.set_global(GlobalChatStore(store.clone()));
    let runtime = cx.new(|cx| ChatRuntime::new(store.clone(), home, cx));
    live_modes::init(&runtime, &store, cx);
    lifecycle::init(&store, cx);
    end_chats_on_quit(&runtime, cx);
    ChatRuntime::set_global(runtime, cx);
}

/// On quit every running chat is ended and its `last_active` written (design D18).
fn end_chats_on_quit(runtime: &Entity<ChatRuntime>, cx: &mut App) {
    // Weak, so the hook does not keep the runtime alive.
    let runtime = runtime.downgrade();
    cx.on_app_quit(move |cx| {
        let end = runtime
            .update(cx, |runtime, cx| runtime.end_for_quit(cx))
            .ok();
        async move {
            if let Some(end) = end {
                end.await;
            }
        }
    })
    .detach();
}

/// The names of the chats that are running, for the quit prompt. Empty while `chats.enabled` is
/// false or before [`init`].
pub fn live_chats(cx: &App) -> Vec<String> {
    ChatRuntime::global(cx).map_or_else(Vec::new, |runtime| runtime.read(cx).running_names(cx))
}

/// The text of the quit prompt for the chats named `names` (design D18): `None` when none run.
pub fn quit_prompt_text(names: &[String]) -> Option<String> {
    let list = names.join(", ");
    match names.len() {
        0 => None,
        1 => Some(format!(
            "1 chat is running ({list}). Quitting ends it; it resumes from the Chats panel."
        )),
        count => Some(format!(
            "{count} chats are running ({list}). Quitting ends them; they resume from the Chats panel."
        )),
    }
}

/// What the app's quit asks before its own confirmation: the prompt text while chats run, `None`
/// otherwise. Asked whatever `confirm_quit` says, because quitting ends the chats.
pub fn quit_prompt(cx: &App) -> Option<String> {
    quit_prompt_text(&live_chats(cx))
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
mod quit_tests;

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
