//! The per-project Kotlin server lifecycle: which server each worktree runs, its status, and the
//! restart when that server changes. `KotlinHost` is the only way the manager touches language
//! servers, so the state machine is tested with a fake host.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::ExitStatus,
    rc::Rc,
};

use gpui::{App, Context, Entity, EntityId, Global, Subscription, WeakEntity};
use lsp::{IoKind, LanguageServerId, LanguageServerName};
use project::{LspStore, LspStoreEvent};
use rpc::proto;
use settings::{KotlinServerKind, SettingsStore, WorktreeId};

use crate::{
    KotlinDirs,
    installed::server_name,
    log::KotlinLog,
    resolve::{Resolved, clear_project_override, project_key, resolve, set_project_override},
};

/// The name of the language server Kotlin buffers use, whichever server runs behind it.
pub const ADAPTER_NAME: &str = "kotlin";

#[derive(Clone, Debug, PartialEq)]
pub enum ServerStatus {
    Installing {
        done: u64,
        total: Option<u64>,
    },
    Starting,
    Indexing {
        label: String,
    },
    Ready,
    Failed {
        reason: String,
    },
    BothFailed {
        intellij: String,
        kotlin_lsp: String,
    },
}

impl ServerStatus {
    /// The text of a `state` line in `kotlin.log`.
    pub fn log_text(&self) -> String {
        match self {
            Self::Installing {
                done,
                total: Some(total),
            } => format!("installing {done}/{total}"),
            Self::Installing { done, total: None } => format!("installing {done}"),
            Self::Starting => "starting".into(),
            Self::Indexing { label } => format!("indexing {label}"),
            Self::Ready => "ready".into(),
            Self::Failed { reason } => format!("failed: {reason}"),
            Self::BothFailed {
                intellij,
                kotlin_lsp,
            } => format!("failed: intellij: {intellij}; kotlin-lsp: {kotlin_lsp}"),
        }
    }
}

/// How the manager asks for a Kotlin server restart.
pub trait KotlinHost: 'static {
    /// Stops the worktree's Kotlin server `server_id` (only that one: other worktrees' servers run
    /// on), waits until its process is gone, then starts it again for the open Kotlin buffers. The new start asks the manager which server to run
    /// (`KotlinProjectManager::begin_start`), so the manager changes the effective server before
    /// calling this.
    fn restart(&self, worktree_id: WorktreeId, server_id: Option<LanguageServerId>, cx: &mut App);
}

pub struct KotlinWorktreeState {
    pub root: PathBuf,
    pub requested: Resolved,
    pub effective: KotlinServerKind,
    /// `None` until a server has started for the worktree, and again after it is stopped.
    pub status: Option<ServerStatus>,
    pub server_id: Option<LanguageServerId>,
    /// Open work-done progress, oldest first: token and the label to show.
    pending: Vec<(String, String)>,
    /// Keeps the stderr forwarder alive while the server runs.
    io_subscription: Option<lsp::Subscription>,
}

/// Maps canonical worktree roots to their managers, so the adapter finds the manager of the project
/// that asked for a server. A root open in two projects belongs to the one registered last.
#[derive(Default)]
pub struct KotlinRegistry {
    managers: HashMap<String, WeakEntity<KotlinProjectManager>>,
}

impl Global for KotlinRegistry {}

impl KotlinRegistry {
    pub fn manager(&self, worktree_root: &Path) -> Option<Entity<KotlinProjectManager>> {
        self.managers.get(&project_key(worktree_root))?.upgrade()
    }

    fn register(&mut self, worktree_root: &Path, manager: WeakEntity<KotlinProjectManager>) {
        self.managers.insert(project_key(worktree_root), manager);
    }

    fn unregister(&mut self, worktree_root: &Path, manager: EntityId) {
        let key = project_key(worktree_root);
        if self
            .managers
            .get(&key)
            .is_some_and(|registered| registered.entity_id() == manager)
        {
            self.managers.remove(&key);
        }
    }
}

pub struct KotlinProjectManager {
    host: Rc<dyn KotlinHost>,
    dirs: KotlinDirs,
    log: KotlinLog,
    worktrees: BTreeMap<WorktreeId, KotlinWorktreeState>,
    _settings_subscription: Subscription,
}

impl KotlinProjectManager {
    pub fn new(host: Rc<dyn KotlinHost>, dirs: KotlinDirs, cx: &mut Context<Self>) -> Self {
        let log = KotlinLog::new(&dirs.logs_dir);
        let _settings_subscription =
            cx.observe_global::<SettingsStore>(|this, cx| this.refresh_all(cx));
        Self {
            host,
            dirs,
            log,
            worktrees: BTreeMap::new(),
            _settings_subscription,
        }
    }

    pub fn log(&self) -> &KotlinLog {
        &self.log
    }

    pub fn worktree(&self, worktree_id: WorktreeId) -> Option<&KotlinWorktreeState> {
        self.worktrees.get(&worktree_id)
    }

    pub fn status(&self, worktree_id: WorktreeId) -> Option<&ServerStatus> {
        self.worktree(worktree_id)?.status.as_ref()
    }

    pub fn add_worktree(&mut self, worktree_id: WorktreeId, root: PathBuf, cx: &mut Context<Self>) {
        let requested = resolve(&self.dirs.state_dir, &root, worktree_id, cx);
        let manager = cx.weak_entity();
        cx.default_global::<KotlinRegistry>()
            .register(&root, manager);
        self.worktrees.insert(
            worktree_id,
            KotlinWorktreeState {
                root,
                requested,
                effective: requested.server,
                status: None,
                server_id: None,
                pending: Vec::new(),
                io_subscription: None,
            },
        );
        cx.notify();
    }

    pub fn remove_worktree(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        if let Some(state) = self.worktrees.remove(&worktree_id) {
            let manager = cx.entity_id();
            cx.default_global::<KotlinRegistry>()
                .unregister(&state.root, manager);
            cx.notify();
        }
    }

    /// Called when a start of the Kotlin server begins and there is no explicit binary path: the
    /// server to run, resolved now, so a setting changed since the last start applies.
    pub fn begin_start(
        &mut self,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) -> Option<KotlinServerKind> {
        let state = self.worktrees.get_mut(&worktree_id)?;
        let requested = resolve(&self.dirs.state_dir, &state.root, worktree_id, cx);
        state.requested = requested;
        state.effective = requested.server;
        self.log.line(&format!(
            "start {} {}",
            state.root.display(),
            server_name(requested.server)
        ));
        self.set_status(worktree_id, Some(ServerStatus::Starting), cx);
        Some(requested.server)
    }

    pub fn start_failed(
        &mut self,
        worktree_id: WorktreeId,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        self.set_status(worktree_id, Some(ServerStatus::Failed { reason }), cx);
    }

    /// "Use setting" is `None`; a server writes the per-project override. Restarts only when the
    /// server that resolves for the worktree changed.
    pub fn switch_server(
        &mut self,
        worktree_id: WorktreeId,
        choice: Option<KotlinServerKind>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.worktrees.get(&worktree_id) else {
            anyhow::bail!("no Kotlin worktree {worktree_id:?}");
        };
        match choice {
            Some(server) => set_project_override(&self.dirs.state_dir, &state.root, server)?,
            None => clear_project_override(&self.dirs.state_dir, &state.root)?,
        }
        self.refresh_worktree(worktree_id, cx);
        Ok(())
    }

    /// `kotlin: restart server`: the same server again.
    pub fn restart_server(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        if self.worktrees.contains_key(&worktree_id) {
            self.restart(worktree_id, cx);
        }
    }

    fn refresh_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<WorktreeId> = self.worktrees.keys().copied().collect();
        for worktree_id in ids {
            self.refresh_worktree(worktree_id, cx);
        }
    }

    /// Re-resolves the worktree's server. A restart happens only when the server changed and one
    /// is running or starting; a settings change that resolves to the same server does nothing.
    fn refresh_worktree(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let resolved = resolve(&self.dirs.state_dir, &state.root, worktree_id, cx);
        let changed = resolved.server != state.effective;
        let previous = state.effective;
        state.requested = resolved;
        if !changed {
            return;
        }
        state.effective = resolved.server;
        self.log.line(&format!(
            "switch {} {} -> {}",
            state.root.display(),
            server_name(previous),
            server_name(resolved.server)
        ));
        if state.status.is_some() {
            self.restart(worktree_id, cx);
        }
        cx.notify();
    }

    fn restart(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        self.set_status(worktree_id, Some(ServerStatus::Starting), cx);
        let server_id = self
            .worktrees
            .get(&worktree_id)
            .and_then(|state| state.server_id);
        let host = self.host.clone();
        host.restart(worktree_id, server_id, cx);
    }

    fn set_status(
        &mut self,
        worktree_id: WorktreeId,
        status: Option<ServerStatus>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        if state.status == status {
            return;
        }
        let text = status
            .as_ref()
            .map_or_else(|| "stopped".to_string(), ServerStatus::log_text);
        self.log.line(&format!(
            "state {} {} {text}",
            state.root.display(),
            server_name(state.effective)
        ));
        state.status = status;
        cx.notify();
    }

    fn worktree_of_server(&self, server_id: LanguageServerId) -> Option<WorktreeId> {
        self.worktrees
            .iter()
            .find(|(_, state)| state.server_id == Some(server_id))
            .map(|(worktree_id, _)| *worktree_id)
    }

    /// The Kotlin server of the worktree is running. No work in progress means ready.
    pub fn server_added(
        &mut self,
        server_id: LanguageServerId,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        state.server_id = Some(server_id);
        state.pending.clear();
        self.set_status(worktree_id, Some(ServerStatus::Ready), cx);
    }

    pub fn server_removed(&mut self, server_id: LanguageServerId, cx: &mut Context<Self>) {
        let Some(worktree_id) = self.worktree_of_server(server_id) else {
            return;
        };
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        state.server_id = None;
        state.pending.clear();
        state.io_subscription = None;
        // A restart already says `starting`, and a failure stays failed.
        if matches!(
            state.status,
            Some(ServerStatus::Ready | ServerStatus::Indexing { .. })
        ) {
            self.set_status(worktree_id, None, cx);
        }
    }

    /// The server's process ended without a shutdown being asked for.
    pub fn server_terminated(
        &mut self,
        server_id: LanguageServerId,
        exit_status: Option<ExitStatus>,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree_id) = self.worktree_of_server(server_id) else {
            return;
        };
        let detail =
            exit_status.map_or_else(|| "unknown status".into(), |status| status.to_string());
        self.log.line(&format!("exit {detail}"));
        self.set_status(
            worktree_id,
            Some(ServerStatus::Failed {
                reason: format!("server exited unexpectedly ({detail})"),
            }),
            cx,
        );
    }

    pub fn work_started(
        &mut self,
        server_id: LanguageServerId,
        token: String,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree_id) = self.worktree_of_server(server_id) else {
            return;
        };
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        state.pending.retain(|(existing, _)| *existing != token);
        state.pending.push((token, label));
        self.show_progress(worktree_id, cx);
    }

    pub fn work_progress(
        &mut self,
        server_id: LanguageServerId,
        token: String,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree_id) = self.worktree_of_server(server_id) else {
            return;
        };
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        match state
            .pending
            .iter_mut()
            .find(|(existing, _)| *existing == token)
        {
            Some((_, label)) => {
                if let Some(message) = message.filter(|message| !message.is_empty()) {
                    *label = message;
                }
            }
            None => state
                .pending
                .push((token, message.unwrap_or_else(|| "working".into()))),
        }
        self.show_progress(worktree_id, cx);
    }

    pub fn work_ended(
        &mut self,
        server_id: LanguageServerId,
        token: String,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree_id) = self.worktree_of_server(server_id) else {
            return;
        };
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        state.pending.retain(|(existing, _)| *existing != token);
        self.show_progress(worktree_id, cx);
    }

    /// Indexing shows the newest open work; with none open the server is ready. A failed or
    /// starting server keeps that state.
    fn show_progress(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(state) = self.worktrees.get(&worktree_id) else {
            return;
        };
        if !matches!(
            state.status,
            Some(ServerStatus::Ready | ServerStatus::Indexing { .. })
        ) {
            return;
        }
        let status = match state.pending.last() {
            Some((_, label)) => ServerStatus::Indexing {
                label: label.clone(),
            },
            None => ServerStatus::Ready,
        };
        self.set_status(worktree_id, Some(status), cx);
    }

    /// Everything the manager reads from an `LspStore`: server start and stop, progress, and
    /// stderr. Other servers' events are ignored.
    pub fn handle_lsp_event(
        &mut self,
        lsp_store: &Entity<LspStore>,
        event: &LspStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            LspStoreEvent::LanguageServerAdded(server_id, name, Some(worktree_id))
                if is_kotlin(name) =>
            {
                if !self.worktrees.contains_key(worktree_id) {
                    return;
                }
                self.server_added(*server_id, *worktree_id, cx);
                self.attach(lsp_store, *server_id, *worktree_id, cx);
            }
            LspStoreEvent::LanguageServerRemoved(server_id) => self.server_removed(*server_id, cx),
            LspStoreEvent::LanguageServerStartupStderr {
                name,
                worktree_id,
                text,
            } if is_kotlin(name) && self.worktrees.contains_key(worktree_id) => {
                self.log.stderr(text);
            }
            LspStoreEvent::LanguageServerUpdate {
                language_server_id,
                message,
                ..
            } => match message {
                proto::update_language_server::Variant::WorkStart(start) => {
                    let label = start
                        .message
                        .clone()
                        .filter(|message| !message.is_empty())
                        .or_else(|| start.title.clone())
                        .unwrap_or_else(|| "working".into());
                    self.work_started(*language_server_id, token_key(&start.token), label, cx);
                }
                proto::update_language_server::Variant::WorkProgress(progress) => {
                    self.work_progress(
                        *language_server_id,
                        token_key(&progress.token),
                        progress.message.clone(),
                        cx,
                    );
                }
                proto::update_language_server::Variant::WorkEnd(end) => {
                    self.work_ended(*language_server_id, token_key(&end.token), cx);
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Forwards the running server's stderr to `kotlin.log` and watches for it exiting.
    fn attach(
        &mut self,
        lsp_store: &Entity<LspStore>,
        server_id: LanguageServerId,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = lsp_store.read(cx).language_server_for_id(server_id) else {
            return;
        };
        let log = self.log.clone();
        let subscription = server.on_io(move |kind, text| {
            if matches!(kind, IoKind::StdErr) {
                log.stderr(text);
            }
        });
        if let Some(state) = self.worktrees.get_mut(&worktree_id) {
            state.io_subscription = Some(subscription);
        }
        let (sender, receiver) = futures::channel::oneshot::channel();
        server.on_termination(move |exit_status| {
            sender.send(exit_status).ok();
        });
        cx.spawn(async move |this, cx| {
            if let Ok(exit_status) = receiver.await {
                this.update(cx, |this, cx| {
                    this.server_terminated(server_id, exit_status, cx)
                })
                .ok();
            }
        })
        .detach();
    }
}

fn is_kotlin(name: &LanguageServerName) -> bool {
    name.0.as_ref() == ADAPTER_NAME
}

fn token_key(token: &Option<proto::ProgressToken>) -> String {
    format!(
        "{:?}",
        token.as_ref().and_then(|token| token.value.as_ref())
    )
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        fs,
    };

    use gpui::{AppContext as _, BorrowAppContext as _, TestAppContext};
    use settings::{ActiveSettingsProfileName, Settings as _, SettingsStore};

    use super::*;
    use crate::KotlinSettings;

    struct FakeInner {
        events: RefCell<Vec<String>>,
        live: Cell<usize>,
        max_live: Cell<usize>,
        next_id: Cell<usize>,
        manager: RefCell<Option<WeakEntity<KotlinProjectManager>>>,
    }

    /// Stops the old server at once and starts the new one later, as Zed does after the stop task.
    #[derive(Clone)]
    struct FakeHost(Rc<FakeInner>);

    impl FakeInner {
        fn record_start(&self, server: KotlinServerKind) -> LanguageServerId {
            self.events
                .borrow_mut()
                .push(format!("start {}", server_name(server)));
            self.live.set(self.live.get() + 1);
            self.max_live.set(self.max_live.get().max(self.live.get()));
            self.next_id.set(self.next_id.get() + 1);
            LanguageServerId(self.next_id.get())
        }
    }

    impl KotlinHost for FakeHost {
        fn restart(&self, worktree_id: WorktreeId, _: Option<LanguageServerId>, cx: &mut App) {
            let inner = self.0.clone();
            inner.events.borrow_mut().push("stop".into());
            inner.live.set(inner.live.get() - 1);
            cx.spawn(async move |cx| {
                let manager = inner.manager.borrow().clone().unwrap();
                let server = manager
                    .update(cx, |manager, cx| manager.begin_start(worktree_id, cx))
                    .unwrap()
                    .unwrap();
                let server_id = inner.record_start(server);
                manager
                    .update(cx, |manager, cx| {
                        manager.server_added(server_id, worktree_id, cx)
                    })
                    .unwrap();
            })
            .detach();
        }
    }

    struct Harness {
        manager: Entity<KotlinProjectManager>,
        host: FakeHost,
        dirs: KotlinDirs,
        root: PathBuf,
        _temp: tempfile::TempDir,
    }

    fn worktree() -> WorktreeId {
        WorktreeId::from_usize(1)
    }

    fn setup(cx: &mut TestAppContext) -> Harness {
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            SettingsStore::observe_active_settings_profile_name(cx).detach();
            KotlinSettings::register(cx);
        });
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        let dirs = KotlinDirs {
            state_dir: temp.path().join("state"),
            logs_dir: temp.path().join("logs"),
            install_root: temp.path().join("languages"),
        };
        let host = FakeHost(Rc::new(FakeInner {
            events: RefCell::default(),
            live: Cell::new(0),
            max_live: Cell::new(0),
            next_id: Cell::new(0),
            manager: RefCell::default(),
        }));
        let manager =
            cx.new(|cx| KotlinProjectManager::new(Rc::new(host.clone()), dirs.clone(), cx));
        *host.0.manager.borrow_mut() = Some(manager.downgrade());
        manager.update(cx, |manager, cx| {
            manager.add_worktree(worktree(), root.clone(), cx)
        });
        Harness {
            manager,
            host,
            dirs,
            root,
            _temp: temp,
        }
    }

    impl Harness {
        /// The first start, which Zed does when a Kotlin buffer opens.
        fn first_start(&self, cx: &mut TestAppContext) {
            let server = self
                .manager
                .update(cx, |manager, cx| manager.begin_start(worktree(), cx))
                .unwrap();
            let server_id = self.host.0.record_start(server);
            self.manager.update(cx, |manager, cx| {
                manager.server_added(server_id, worktree(), cx)
            });
        }

        fn events(&self) -> Vec<String> {
            self.host.0.events.borrow().clone()
        }

        fn status(&self, cx: &mut TestAppContext) -> Option<ServerStatus> {
            self.manager
                .read_with(cx, |manager, _| manager.status(worktree()).cloned())
        }

        fn log(&self) -> String {
            fs::read_to_string(KotlinLog::new(&self.dirs.logs_dir).path()).unwrap_or_default()
        }
    }

    fn set_user(cx: &mut TestAppContext, json: &str) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.set_user_settings(json, cx).unwrap();
            });
        });
    }

    fn set_profile(cx: &mut TestAppContext, name: &str) {
        cx.update(|cx| cx.set_global(ActiveSettingsProfileName(name.to_string())));
    }

    #[gpui::test]
    fn test_switch_stops_then_starts_exactly_one_server(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);
        assert_eq!(harness.events(), ["start kotlin-lsp"]);
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));

        harness
            .manager
            .update(cx, |manager, cx| {
                manager.switch_server(worktree(), Some(KotlinServerKind::Intellij), cx)
            })
            .unwrap();
        // The status says starting while the old server stops.
        assert_eq!(harness.status(cx), Some(ServerStatus::Starting));
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp", "stop", "start intellij"]
        );
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));

        // "Use setting" returns to the setting's server, again stop first.
        harness
            .manager
            .update(cx, |manager, cx| {
                manager.switch_server(worktree(), None, cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            [
                "start kotlin-lsp",
                "stop",
                "start intellij",
                "stop",
                "start kotlin-lsp"
            ]
        );
        assert_eq!(harness.host.0.max_live.get(), 1, "two servers ran at once");

        // Choosing the server that already runs restarts nothing.
        harness
            .manager
            .update(cx, |manager, cx| {
                manager.switch_server(worktree(), Some(KotlinServerKind::KotlinLsp), cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(harness.events().len(), 5);

        let log = harness.log();
        assert!(log.contains("switch "), "{log}");
        assert!(log.contains("kotlin-lsp -> intellij"), "{log}");
        assert!(log.contains(" ready"), "{log}");
    }

    #[gpui::test]
    fn test_profile_change_restarts_only_when_the_resolution_changed(cx: &mut TestAppContext) {
        let harness = setup(cx);
        set_user(
            cx,
            r#"{
                "profiles": {
                    "same": { "settings": {} },
                    "work": { "settings": { "kotlin": { "server": "intellij" } } }
                }
            }"#,
        );
        harness.first_start(cx);

        set_profile(cx, "same");
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp"],
            "same server: no restart"
        );

        set_profile(cx, "work");
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp", "stop", "start intellij"]
        );

        // A user setting that does not change the effective server restarts nothing.
        set_user(
            cx,
            r#"{
                "buffer_font_size": 15,
                "profiles": { "work": { "settings": { "kotlin": { "server": "intellij" } } } }
            }"#,
        );
        cx.run_until_parked();
        assert_eq!(harness.events().len(), 3);

        set_profile(cx, "none");
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            [
                "start kotlin-lsp",
                "stop",
                "start intellij",
                "stop",
                "start kotlin-lsp"
            ]
        );
        assert_eq!(harness.host.0.max_live.get(), 1);
    }

    #[gpui::test]
    fn test_changed_server_before_the_first_start_restarts_nothing(cx: &mut TestAppContext) {
        let harness = setup(cx);
        set_user(cx, r#"{ "kotlin": { "server": "intellij" } }"#);
        cx.run_until_parked();
        assert_eq!(harness.events(), Vec::<String>::new());
        // The first start reads the setting as it is now.
        let server = harness
            .manager
            .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
        assert_eq!(server, Some(KotlinServerKind::Intellij));
        assert_eq!(harness.status(cx), Some(ServerStatus::Starting));
    }

    #[gpui::test]
    fn test_progress_drives_indexing_and_ready(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);
        let server_id = LanguageServerId(1);
        let update = |cx: &mut TestAppContext, f: &dyn Fn(&mut KotlinProjectManager, &mut Context<KotlinProjectManager>)| {
            harness.manager.update(cx, |manager, cx| f(manager, cx));
        };

        update(cx, &|manager, cx| {
            manager.work_started(server_id, "a".into(), "Importing".into(), cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Indexing {
                label: "Importing".into()
            })
        );
        update(cx, &|manager, cx| {
            manager.work_started(server_id, "b".into(), "Indexing".into(), cx)
        });
        update(cx, &|manager, cx| {
            manager.work_progress(
                server_id,
                "b".into(),
                Some("indexing 3/5 modules".into()),
                cx,
            )
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Indexing {
                label: "indexing 3/5 modules".into()
            })
        );
        // The first work ends while the second is open: still indexing.
        update(cx, &|manager, cx| {
            manager.work_ended(server_id, "a".into(), cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Indexing {
                label: "indexing 3/5 modules".into()
            })
        );
        update(cx, &|manager, cx| {
            manager.work_ended(server_id, "b".into(), cx)
        });
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));

        // Progress of another server changes nothing.
        update(cx, &|manager, cx| {
            manager.work_started(LanguageServerId(99), "z".into(), "Other".into(), cx)
        });
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));

        let log = harness.log();
        assert!(log.contains("indexing indexing 3/5 modules"), "{log}");
        assert!(log.trim_end().ends_with("ready"), "{log}");
    }

    #[gpui::test]
    fn test_failed_and_stopped_states(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);
        let server_id = LanguageServerId(1);

        // Progress never hides a failure.
        harness.manager.update(cx, |manager, cx| {
            manager.start_failed(worktree(), "kotlin-lsp is not installed".into(), cx);
            manager.work_started(server_id, "a".into(), "Importing".into(), cx);
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: "kotlin-lsp is not installed".into()
            })
        );

        harness.manager.update(cx, |manager, cx| {
            manager.server_added(server_id, worktree(), cx);
            manager.server_removed(server_id, cx);
        });
        assert_eq!(
            harness.status(cx),
            None,
            "a removed ready server is stopped"
        );
        assert!(harness.log().contains(" stopped"));

        harness.manager.update(cx, |manager, cx| {
            manager.server_added(server_id, worktree(), cx);
            manager.server_terminated(server_id, None, cx);
        });
        assert!(matches!(
            harness.status(cx),
            Some(ServerStatus::Failed { reason }) if reason.contains("exited unexpectedly")
        ));
    }

    #[gpui::test]
    fn test_registry_finds_the_manager_by_canonical_root(cx: &mut TestAppContext) {
        let harness = setup(cx);
        let found = cx.update(|cx| {
            // A path with `..` still finds the manager of the canonical root.
            let spelled = harness.root.join("..").join("project");
            cx.global::<KotlinRegistry>().manager(&spelled)
        });
        assert_eq!(
            found.map(|manager| manager.entity_id()),
            Some(harness.manager.entity_id())
        );
        harness
            .manager
            .update(cx, |manager, cx| manager.remove_worktree(worktree(), cx));
        let found = cx.update(|cx| cx.global::<KotlinRegistry>().manager(&harness.root));
        assert!(found.is_none());
    }
}
