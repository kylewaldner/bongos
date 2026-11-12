//! The per-project Kotlin server lifecycle: which server each worktree runs, its status, and the
//! restart when that server changes. `KotlinHost` is the only way the manager touches language
//! servers, so the state machine is tested with a fake host.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::ExitStatus,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, Subscription, Task,
    WeakEntity,
};
use lsp::{IoKind, LanguageServerId, LanguageServerName};
use project::{LspStore, LspStoreEvent, lsp_store::LanguageServerLogType};
use rpc::proto;
use settings::{KotlinServerKind, SettingsStore, WorktreeId};

use crate::{
    KotlinDirs,
    install::prune,
    installed::server_name,
    intellij::{EULA_NOT_ACCEPTED_REASON, LicenseOutcome},
    license::{is_license_lost_exit, is_license_lost_signal, restarts_after},
    locks::LockRegistry,
    log::{KotlinLog, StderrMirror},
    manifest_cache::{BuildId, KotlinLspBuilds, ManifestCache},
    resolve::{Resolved, clear_project_override, project_key, resolve, set_project_override},
    updates::{KotlinUpdates, KotlinUpdatesEvent},
};

/// The name of the language server Kotlin buffers use, whichever server runs behind it.
pub const ADAPTER_NAME: &str = "kotlin";

/// The status text of a server whose build has expired (decision D18).
pub const EXPIRED_REASON: &str = "server expired, update bongos";

/// The status text of an install whose start was dropped before it ended.
pub const INTERRUPTED_REASON: &str = "install interrupted";

/// What an expired early-access build prints before it exits with `EXPIRED_EXIT_CODE`.
const EXPIRED_MESSAGE: &str = "This build of kotlin-server has expired.";
const EXPIRED_EXIT_CODE: i32 = 7;

/// Unexpected exits closer together than this are a crash loop: the first restarts the server, the
/// second ends it (a fallback for IntelliJ, `Failed` for kotlin-lsp).
pub const CRASH_WINDOW: Duration = Duration::from_secs(60);

/// How long the first start of a trial build must run, once added, without an unexpected exit to
/// pass (decision D8). The same length as `CRASH_WINDOW`.
pub const TRIAL_WINDOW: Duration = CRASH_WINDOW;

/// The status text of a kotlin-lsp that crashed twice within `CRASH_WINDOW`.
pub const CRASH_LOOP_REASON: &str = "crashed twice within a minute";

/// Why the IntelliJ server the user asked for is not the one running (the fallback table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    EulaNotAccepted,
    Unlicensed,
    LicenseCheckFailed,
    InstallFailed,
    StartFailed,
    CrashLoop,
    LicenseLost,
    Expired,
}

impl FallbackReason {
    /// The reason as the status item and the notice show it, after "fallback: ".
    pub fn text(self) -> &'static str {
        match self {
            Self::EulaNotAccepted => EULA_NOT_ACCEPTED_REASON,
            Self::Unlicensed => "IntelliJ unlicensed",
            Self::LicenseCheckFailed => "license check failed (offline?)",
            Self::InstallFailed => "IntelliJ install failed",
            Self::StartFailed => "IntelliJ failed to start",
            Self::CrashLoop => "IntelliJ crashed twice within a minute",
            Self::LicenseLost => "IntelliJ license lost",
            Self::Expired => "IntelliJ server expired",
        }
    }
}

/// What a manager tells the workspace that shows its project.
#[derive(Clone, Debug, PartialEq)]
pub enum KotlinEvent {
    /// A worktree fell back to kotlin-lsp: one notice line, once per fallback.
    FellBack {
        worktree_id: WorktreeId,
        message: String,
    },
    /// A trial build failed its first start and was rejected: one notice line.
    TrialFailed {
        worktree_id: WorktreeId,
        message: String,
    },
}

/// The one-line notice of a rejected trial build.
pub fn trial_failed_message(version: &str, fallback: &str) -> String {
    format!("Kotlin: kotlin-lsp {version} failed its first start; using {fallback} again")
}

/// A trial build whose command `start_command` returned (the launched mark of decision D8). Only
/// a start with this mark can fail the trial.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrialLaunch {
    pub build: BuildId,
    /// What the next start runs once this build is rejected, as the notice names it.
    pub fallback: String,
    /// The embedded pin's version, which the prune after a pass keeps.
    pub pin_version: String,
}

/// A launched trial of a worktree, until it passes, fails or its server is stopped.
struct LaunchedTrial {
    launch: TrialLaunch,
    /// The launched process, once it was added.
    server_id: Option<LanguageServerId>,
    /// Passes the trial when `TRIAL_WINDOW` has gone by since the server was added.
    _window: Option<Task<()>>,
}

/// The trial builds rejected in this run, in any project. A start skips them even when writing
/// `rejected` to `state.json` failed, so a failing build is never started again in this run.
#[derive(Default)]
pub struct RejectedTrials(Vec<BuildId>);

impl Global for RejectedTrials {}

impl RejectedTrials {
    pub fn builds(cx: &App) -> Vec<BuildId> {
        cx.try_global::<Self>()
            .map(|rejected| rejected.0.clone())
            .unwrap_or_default()
    }
}

fn same_build(a: &BuildId, b: &BuildId) -> bool {
    a.version == b.version && a.sha256.eq_ignore_ascii_case(&b.sha256)
}

/// A passed trial becomes the active build and the old active build the previous one. Runs under
/// the state lock. Returns false when the build was rejected meanwhile (by another process).
fn promote_trial(builds: &mut KotlinLspBuilds, build: &BuildId) -> bool {
    if builds
        .rejected
        .iter()
        .any(|rejected| same_build(rejected, build))
    {
        return false;
    }
    if builds
        .trial
        .as_ref()
        .is_some_and(|trial| same_build(trial, build))
    {
        builds.trial = None;
    }
    if !builds
        .active
        .as_ref()
        .is_some_and(|active| same_build(active, build))
    {
        if let Some(active) = builds.active.take() {
            builds.previous = Some(active);
        }
        builds.active = Some(build.clone());
    }
    true
}

/// A failed trial goes into `rejected` and stops being the trial. Should another process have
/// passed it meanwhile, the previous build becomes active again. Runs under the state lock.
fn reject_trial(builds: &mut KotlinLspBuilds, build: &BuildId) {
    if !builds
        .rejected
        .iter()
        .any(|rejected| same_build(rejected, build))
    {
        builds.rejected.push(build.clone());
    }
    if builds
        .trial
        .as_ref()
        .is_some_and(|trial| same_build(trial, build))
    {
        builds.trial = None;
    }
    if builds
        .active
        .as_ref()
        .is_some_and(|active| same_build(active, build))
    {
        builds.active = builds.previous.take();
    }
}

/// The one-line notice of a fallback.
pub fn fallback_message(reason: FallbackReason) -> String {
    format!(
        "Kotlin: using kotlin-lsp, the IntelliJ server is not available ({})",
        reason.text()
    )
}

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
    /// Stops the worktree's Kotlin server (only that one: other worktrees' servers run on),
    /// whether running, still starting or left by a failed start, waits until its process is gone,
    /// then starts it again for the open Kotlin buffers. `server_id` is the running server, when
    /// one was added. The new start asks the manager which server to run
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
    /// Servers whose build expired in this run. An expired server is never started or restarted
    /// again: a new pin comes with a new bongos.
    expired: Vec<KotlinServerKind>,
    /// The cancel flag of the install in progress.
    install_cancel: Option<Arc<AtomicBool>>,
    /// Open work-done progress, oldest first: token and the label to show.
    pending: Vec<(String, String)>,
    /// Keeps the stderr forwarder alive while the server runs.
    io_subscription: Option<lsp::Subscription>,
    /// Writes the running server's stderr to `kotlin.log` once.
    stderr_mirror: Option<StderrMirror>,
    /// Why the server asked for is not the one running, once a fallback happened.
    pub fallback: Option<Fallback>,
    /// The last line the server wrote to stderr that looks like a Gradle failure.
    pub gradle_error: Option<String>,
    /// When the running server exited unexpectedly, within the last `CRASH_WINDOW`. `kotlin:
    /// restart server` and a fallback forget them.
    crashes: Vec<Instant>,
    /// A fallback was caused by the stderr of a start that had not finished: that start's own
    /// failure, which `LspStore` reports right after the stderr, is not kotlin-lsp's.
    stale_start: bool,
    /// The trial build this worktree's current start launched, until it passes or fails.
    trial: Option<LaunchedTrial>,
}

/// A server that runs because the one the user asked for could not. Sticky for the session
/// (decision D14): it ends with `kotlin: restart server`, `kotlin: switch server`, a successful
/// license command, or a change of the resolved server; an app restart starts without one.
#[derive(Clone, Debug, PartialEq)]
pub struct Fallback {
    pub kind: FallbackReason,
    /// Why the requested server is not running, such as "IntelliJ unlicensed".
    pub reason: String,
}

impl Fallback {
    pub fn new(kind: FallbackReason) -> Self {
        Self {
            kind,
            reason: kind.text().to_string(),
        }
    }
}

/// Maps canonical worktree roots to their managers, so the adapter finds the manager of the project
/// that asked for a server. A root open in two projects belongs to the one registered last.
#[derive(Default)]
pub struct KotlinRegistry {
    managers: HashMap<String, WeakEntity<KotlinProjectManager>>,
    /// The manager of each project, by the project's entity id.
    projects: HashMap<EntityId, WeakEntity<KotlinProjectManager>>,
}

impl Global for KotlinRegistry {}

impl KotlinRegistry {
    pub fn manager(&self, worktree_root: &Path) -> Option<Entity<KotlinProjectManager>> {
        self.managers.get(&project_key(worktree_root))?.upgrade()
    }

    /// The manager of a project, once it has one.
    pub fn manager_of_project(&self, project: EntityId) -> Option<Entity<KotlinProjectManager>> {
        self.projects.get(&project)?.upgrade()
    }

    pub fn register_project(
        &mut self,
        project: EntityId,
        manager: WeakEntity<KotlinProjectManager>,
    ) {
        self.projects
            .retain(|_, manager| manager.upgrade().is_some());
        self.projects.insert(project, manager);
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
    /// `state.json`, where a trial's pass or rejection is written.
    cache: Arc<ManifestCache>,
    locks: Arc<LockRegistry>,
    _settings_subscription: Subscription,
    /// Hears when a newer kotlin-lsp build is downloaded.
    _updates_subscription: Option<Subscription>,
}

impl EventEmitter<KotlinEvent> for KotlinProjectManager {}

impl KotlinProjectManager {
    pub fn new(host: Rc<dyn KotlinHost>, dirs: KotlinDirs, cx: &mut Context<Self>) -> Self {
        let log = KotlinLog::new(&dirs.logs_dir);
        let _settings_subscription =
            cx.observe_global::<SettingsStore>(|this, cx| this.refresh_all(cx));
        let locks = LockRegistry::global(cx);
        let cache = Arc::new(ManifestCache::new(&dirs.install_root, locks.clone()));
        let _updates_subscription = KotlinUpdates::global(cx).map(|updates| {
            cx.subscribe(&updates, |this, _, event, cx| match event {
                KotlinUpdatesEvent::DownloadFinished { version } => {
                    this.newer_build_ready(version, cx)
                }
                KotlinUpdatesEvent::DownloadStarted { .. } => {}
            })
        });
        Self {
            host,
            dirs,
            log,
            worktrees: BTreeMap::new(),
            cache,
            locks,
            _settings_subscription,
            _updates_subscription,
        }
    }

    pub fn log(&self) -> &KotlinLog {
        &self.log
    }

    pub fn worktree(&self, worktree_id: WorktreeId) -> Option<&KotlinWorktreeState> {
        self.worktrees.get(&worktree_id)
    }

    /// Every worktree the manager tracks, in id order.
    pub fn worktrees(&self) -> impl Iterator<Item = (WorktreeId, &KotlinWorktreeState)> {
        self.worktrees.iter().map(|(id, state)| (*id, state))
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
                expired: Vec::new(),
                install_cancel: None,
                pending: Vec::new(),
                io_subscription: None,
                stderr_mirror: None,
                fallback: None,
                gradle_error: None,
                crashes: Vec::new(),
                stale_start: false,
                trial: None,
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
        state.stale_start = false;
        // A new start ends the launched mark of the last one; this start sets its own.
        state.trial = None;
        // A fallback only stands in for IntelliJ; once kotlin-lsp is asked for there is none.
        if requested.server != KotlinServerKind::Intellij {
            state.fallback = None;
        }
        state.effective = effective_server(requested.server, state.fallback.as_ref());
        let effective = state.effective;
        self.log.line(&format!(
            "start {} {}",
            state.root.display(),
            server_name(effective)
        ));
        self.set_status(worktree_id, Some(ServerStatus::Starting), cx);
        Some(effective)
    }

    /// The start of the worktree's server failed for `reason`. While a fallback runs kotlin-lsp,
    /// that leaves no server at all: `BothFailed`.
    pub fn start_failed(
        &mut self,
        worktree_id: WorktreeId,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        self.fail(worktree_id, reason, cx);
    }

    /// `Failed{reason}`, or `BothFailed` when kotlin-lsp fails while a fallback is active.
    fn fail(&mut self, worktree_id: WorktreeId, reason: String, cx: &mut Context<Self>) {
        let Some(state) = self.worktrees.get(&worktree_id) else {
            return;
        };
        let status = match &state.fallback {
            Some(fallback) if state.effective == KotlinServerKind::KotlinLsp => {
                ServerStatus::BothFailed {
                    intellij: fallback.reason.clone(),
                    kotlin_lsp: reason,
                }
            }
            _ => ServerStatus::Failed { reason },
        };
        self.set_status(worktree_id, Some(status), cx);
    }

    /// The IntelliJ start of the worktree could not go on for `reason`: fall back to kotlin-lsp,
    /// which the same start then runs. `detail` goes to `kotlin.log`. Returns false when there is
    /// nothing to fall back from (the worktree is unknown or does not run IntelliJ).
    pub fn intellij_unavailable(
        &mut self,
        worktree_id: WorktreeId,
        reason: FallbackReason,
        detail: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let runs_intellij = self
            .worktree(worktree_id)
            .is_some_and(|state| state.effective == KotlinServerKind::Intellij);
        if !runs_intellij {
            return false;
        }
        if reason == FallbackReason::Expired
            && let Some(state) = self.worktrees.get_mut(&worktree_id)
            && !state.expired.contains(&KotlinServerKind::Intellij)
        {
            state.expired.push(KotlinServerKind::Intellij);
        }
        self.fall_back(worktree_id, reason, detail, cx);
        self.set_status(worktree_id, Some(ServerStatus::Starting), cx);
        true
    }

    /// Moves the worktree from IntelliJ to kotlin-lsp for `reason`: the fallback state, one
    /// `kotlin.log` line and one notice. Does not restart; the caller does when a server runs.
    /// Returns false when a fallback is already active or IntelliJ is not the effective server.
    fn fall_back(
        &mut self,
        worktree_id: WorktreeId,
        reason: FallbackReason,
        detail: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return false;
        };
        if state.fallback.is_some() || state.effective != KotlinServerKind::Intellij {
            return false;
        }
        state.fallback = Some(Fallback::new(reason));
        state.effective = KotlinServerKind::KotlinLsp;
        state.crashes.clear();
        state.pending.clear();
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({detail})")
        };
        self.log.line(&format!(
            "fallback {} intellij -> kotlin-lsp: {}{detail}",
            state.root.display(),
            reason.text()
        ));
        cx.emit(KotlinEvent::FellBack {
            worktree_id,
            message: fallback_message(reason),
        });
        cx.notify();
        true
    }

    /// A fallback while a server runs: kotlin-lsp replaces it now.
    fn fall_back_and_restart(
        &mut self,
        worktree_id: WorktreeId,
        reason: FallbackReason,
        detail: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.fall_back(worktree_id, reason, detail, cx) {
            return false;
        }
        self.restart(worktree_id, cx);
        true
    }

    /// Whether `server` expired for the worktree in this run.
    pub fn is_expired(&self, worktree_id: WorktreeId, server: KotlinServerKind) -> bool {
        self.worktree(worktree_id)
            .is_some_and(|state| state.expired.contains(&server))
    }

    /// A newer kotlin-lsp build `version` is installed and recorded as the trial. Each worktree
    /// stopped by an expired kotlin-lsp build forgets the expired mark and restarts; that start
    /// runs the new build as a trial. A worktree running a build that has not expired is left
    /// alone: it picks the build up at its next start.
    pub fn newer_build_ready(&mut self, version: &str, cx: &mut Context<Self>) {
        let stopped: Vec<WorktreeId> = self
            .worktrees
            .iter()
            .filter(|(_, state)| {
                state.effective == KotlinServerKind::KotlinLsp
                    && state.expired.contains(&KotlinServerKind::KotlinLsp)
                    && matches!(
                        &state.status,
                        Some(ServerStatus::Failed { reason }) if reason == EXPIRED_REASON
                    )
            })
            .map(|(id, _)| *id)
            .collect();
        for worktree_id in stopped {
            if let Some(state) = self.worktrees.get_mut(&worktree_id) {
                state
                    .expired
                    .retain(|server| *server != KotlinServerKind::KotlinLsp);
                self.log.line(&format!(
                    "restart {}: kotlin-lsp {version} replaces the expired build",
                    state.root.display()
                ));
            }
            self.restart(worktree_id, cx);
        }
    }

    /// Marks `server` expired for the worktree: status `Failed`, and no restart from here on.
    pub fn mark_expired(
        &mut self,
        worktree_id: WorktreeId,
        server: KotlinServerKind,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        if !state.expired.contains(&server) {
            state.expired.push(server);
        }
        self.fail(worktree_id, EXPIRED_REASON.into(), cx);
    }

    /// An install begins: status `Installing` with nothing downloaded. Returns the flag that
    /// `cancel_install` sets and the installer polls.
    pub fn begin_install(
        &mut self,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(state) = self.worktrees.get_mut(&worktree_id) {
            state.install_cancel = Some(cancel.clone());
            self.log.line(&format!(
                "install {} {}",
                state.root.display(),
                server_name(state.effective)
            ));
            self.set_status(
                worktree_id,
                Some(ServerStatus::Installing {
                    done: 0,
                    total: None,
                }),
                cx,
            );
        }
        cancel
    }

    /// Bytes downloaded so far, of `total` when the server said.
    pub fn install_progress(
        &mut self,
        worktree_id: WorktreeId,
        done: u64,
        total: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if self
            .worktree(worktree_id)
            .is_some_and(|state| state.install_cancel.is_some())
        {
            self.set_status(
                worktree_id,
                Some(ServerStatus::Installing { done, total }),
                cx,
            );
        }
    }

    /// The install ended. Success goes on to start the server; an error is the status, and the
    /// next start tries again from zero.
    pub fn end_install(
        &mut self,
        worktree_id: WorktreeId,
        outcome: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.worktrees.get_mut(&worktree_id) {
            state.install_cancel = None;
        }
        match outcome {
            Ok(()) => self.set_status(worktree_id, Some(ServerStatus::Starting), cx),
            Err(reason) => self.fail(worktree_id, reason, cx),
        }
    }

    /// The start that began the install with `cancel` was dropped before the install ended (Zed
    /// abandoned the start): the status must not stay `Installing`. Does nothing when another
    /// install has begun since.
    pub fn abandon_install(
        &mut self,
        worktree_id: WorktreeId,
        cancel: &Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .worktree(worktree_id)
            .and_then(|state| state.install_cancel.as_ref())
            .is_some_and(|current| Arc::ptr_eq(current, cancel));
        if !current {
            return;
        }
        cancel.store(true, Ordering::Relaxed);
        self.log.line("install interrupted");
        self.end_install(worktree_id, Err(INTERRUPTED_REASON.into()), cx);
    }

    /// `kotlin: cancel install`. Returns whether an install was running. The installer stops at its
    /// next chunk and removes its partial files; the status becomes `Failed("install cancelled")`.
    pub fn cancel_install(&mut self, worktree_id: WorktreeId) -> bool {
        let Some(cancel) = self
            .worktree(worktree_id)
            .and_then(|state| state.install_cancel.as_ref())
        else {
            return false;
        };
        cancel.store(true, Ordering::Relaxed);
        self.log.line("install cancel requested");
        true
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
        // A switch ends a fallback (D14).
        self.refresh_worktree(worktree_id, true, cx);
        Ok(())
    }

    /// `kotlin: restart server`: the same server again, with the fallback and the crash history
    /// forgotten. An expired server stays failed.
    pub fn restart_server(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        // The server asked for, not the fallback: a manual restart tries it again (D14).
        let target = state.requested.server;
        if state.expired.contains(&target) {
            self.log.line("restart skipped: server expired");
            return;
        }
        if state.fallback.take().is_some() {
            self.log
                .line(&format!("fallback ended {}", state.root.display()));
        }
        state.crashes.clear();
        // A trial this worktree launched ends with the restart: its exit is the user's doing.
        state.trial = None;
        state.effective = target;
        self.log.line("restart requested");
        self.restart(worktree_id, cx);
    }

    /// `kotlin: reimport project`. kotlin-lsp has no reimport command, and both servers import
    /// Gradle projects when they start, so a reimport is a restart.
    pub fn reimport_project(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        self.log.line("reimport requested");
        self.restart_server(worktree_id, cx);
    }

    /// A license command ended and `license status` answered `outcome`. A licensed answer restarts
    /// the worktree with the IntelliJ server (D14: a successful activation ends a fallback, and
    /// the crash history starts over); anything else changes nothing. Returns whether it restarted.
    pub fn license_checked(
        &mut self,
        worktree_id: WorktreeId,
        outcome: LicenseOutcome,
        cx: &mut Context<Self>,
    ) -> bool {
        if !restarts_after(outcome) {
            self.log
                .line(&format!("license check after command: {outcome:?}"));
            return false;
        }
        let Some(requested) = self
            .worktree(worktree_id)
            .map(|state| state.requested.server)
        else {
            return false;
        };
        self.log
            .line("license licensed: restarting with the IntelliJ server");
        if requested == KotlinServerKind::Intellij {
            self.restart_server(worktree_id, cx);
            return true;
        }
        if let Err(error) = self.switch_server(worktree_id, Some(KotlinServerKind::Intellij), cx) {
            self.log
                .line(&format!("cannot switch to IntelliJ: {error:#}"));
            return false;
        }
        true
    }

    /// How many times the worktree's server exited unexpectedly since the last restart request.
    pub fn crash_count(&self, worktree_id: WorktreeId) -> usize {
        self.worktree(worktree_id)
            .map_or(0, |state| state.crashes.len())
    }

    fn refresh_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<WorktreeId> = self.worktrees.keys().copied().collect();
        for worktree_id in ids {
            self.refresh_worktree(worktree_id, false, cx);
        }
    }

    /// Re-resolves the worktree's server. A change of the resolved server, or `end_fallback`
    /// (`kotlin: switch server`), ends a fallback and the crash history. A restart happens only
    /// when the server to run changed and one is running or starting; a settings change that
    /// resolves to the same server does nothing, and a fallback stays (D14).
    fn refresh_worktree(
        &mut self,
        worktree_id: WorktreeId,
        end_fallback: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let resolved = resolve(&self.dirs.state_dir, &state.root, worktree_id, cx);
        let requested_changed = resolved.server != state.requested.server;
        state.requested = resolved;
        // A new choice of server starts its crash history over, with or without a fallback.
        if requested_changed || end_fallback {
            state.crashes.clear();
        }
        if (requested_changed || end_fallback) && state.fallback.take().is_some() {
            self.log
                .line(&format!("fallback ended {}", state.root.display()));
            // With no fallback left, a server that is not running is no longer "both failed".
            if let Some(ServerStatus::BothFailed { kotlin_lsp, .. }) = &state.status {
                state.status = Some(ServerStatus::Failed {
                    reason: kotlin_lsp.clone(),
                });
            }
            cx.notify();
        }
        let target = effective_server(resolved.server, state.fallback.as_ref());
        let previous = state.effective;
        if target == previous {
            return;
        }
        state.effective = target;
        self.log.line(&format!(
            "switch {} {} -> {}",
            state.root.display(),
            server_name(previous),
            server_name(target)
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

    /// `start_command` returned the command of the trial build `launch` for the worktree (after
    /// the JDK, expiry and install checks passed). From here on, until the trial passes, a start
    /// failure or an exit of this process fails the trial.
    pub fn trial_launched(
        &mut self,
        worktree_id: WorktreeId,
        launch: TrialLaunch,
        _cx: &mut Context<Self>,
    ) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        self.log
            .line(&format!("trial: {} started", launch.build.version));
        state.trial = Some(LaunchedTrial {
            launch,
            server_id: None,
            _window: None,
        });
    }

    /// The trial build the worktree's current start launched, while it is neither passed nor
    /// failed.
    pub fn launched_trial(&self, worktree_id: WorktreeId) -> Option<&BuildId> {
        self.worktree(worktree_id)?
            .trial
            .as_ref()
            .map(|trial| &trial.launch.build)
    }

    /// `TRIAL_WINDOW` went by since the trial's server was added: if it still runs, it passed.
    fn trial_window_ended(
        &mut self,
        worktree_id: WorktreeId,
        server_id: LanguageServerId,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let current = state.trial.as_ref().is_some_and(|trial| {
            trial.server_id == Some(server_id) && state.server_id == Some(server_id)
        });
        if !current {
            return;
        }
        let Some(trial) = state.trial.take() else {
            return;
        };
        self.trial_passed(trial.launch, cx);
    }

    /// The trial passed: under the state lock it becomes the active build and the old active build
    /// the previous one; then the builds nothing refers to any more are pruned.
    fn trial_passed(&mut self, launch: TrialLaunch, cx: &mut Context<Self>) {
        let version = launch.build.version.clone();
        self.log.line(&format!("trial: {version} passed"));
        let (cache, locks, log) = (self.cache.clone(), self.locks.clone(), self.log.clone());
        let server_dir = self
            .dirs
            .install_root
            .join(server_name(KotlinServerKind::KotlinLsp));
        cx.background_spawn(async move {
            match cache
                .with_state(|state| promote_trial(&mut state.kotlin_lsp, &launch.build))
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    log.line(&format!(
                        "trial: {version} was rejected meanwhile; not made active"
                    ));
                    return;
                }
                Err(error) => {
                    // The trial stays pending and runs again at the next start.
                    log.line(&format!("trial: {version} pass not recorded: {error:#}"));
                    return;
                }
            }
            match prune(
                &cache,
                &locks,
                &server_dir,
                Some(&launch.pin_version),
                SystemTime::now(),
            )
            .await
            {
                Ok(pruned) => {
                    for removed in &pruned.removed {
                        log.line(&format!("prune: removed kotlin-lsp {removed}"));
                    }
                }
                Err(error) => ::log::warn!("prune: skipped: {error:#}"),
            }
        })
        .detach();
    }

    /// The launched trial failed for `reason`: it is rejected (in this run at once, in
    /// `state.json` under the state lock), one notice shows, and the worktree restarts with the
    /// resolver's next choice. The crash-restart policy does not apply.
    fn fail_trial(
        &mut self,
        worktree_id: WorktreeId,
        launch: TrialLaunch,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        let version = launch.build.version.clone();
        self.log.line(&format!(
            "trial: {version} failed: {reason}; using {}",
            launch.fallback
        ));
        let rejected = &mut cx.default_global::<RejectedTrials>().0;
        if !rejected
            .iter()
            .any(|rejected| same_build(rejected, &launch.build))
        {
            rejected.push(launch.build.clone());
        }
        let (cache, log, build) = (self.cache.clone(), self.log.clone(), launch.build.clone());
        cx.background_spawn(async move {
            if let Err(error) = cache
                .with_state(|state| reject_trial(&mut state.kotlin_lsp, &build))
                .await
            {
                log.line(&format!(
                    "trial: {} rejection not recorded: {error:#}",
                    build.version
                ));
            }
        })
        .detach();
        if let Some(state) = self.worktrees.get_mut(&worktree_id) {
            state.crashes.clear();
            state.pending.clear();
        }
        cx.emit(KotlinEvent::TrialFailed {
            worktree_id,
            message: trial_failed_message(&version, &launch.fallback),
        });
        self.restart(worktree_id, cx);
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
        state.stale_start = false;
        let starts_window = state
            .trial
            .as_ref()
            .is_some_and(|trial| trial.server_id.is_none());
        self.set_status(worktree_id, Some(ServerStatus::Ready), cx);
        if starts_window {
            let window = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(TRIAL_WINDOW).await;
                this.update(cx, |this, cx| {
                    this.trial_window_ended(worktree_id, server_id, cx)
                })
                .ok();
            });
            if let Some(trial) = self
                .worktrees
                .get_mut(&worktree_id)
                .and_then(|state| state.trial.as_mut())
            {
                trial.server_id = Some(server_id);
                trial._window = Some(window);
            }
        }
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
        state.stderr_mirror = None;
        // A stop or restart is never a trial failure: the trial stays pending for the next start.
        if state
            .trial
            .as_ref()
            .is_some_and(|trial| trial.server_id == Some(server_id))
            && let Some(trial) = state.trial.take()
        {
            self.log.line(&format!(
                "trial: {} stopped before its window ended; still pending",
                trial.launch.build.version
            ));
        }
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
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
        self.server_terminated_at(server_id, exit_status, Instant::now(), cx);
    }

    /// The crash policy, with the time of the exit given. An expired build (exit 7) and a lost
    /// license (a pinned exit code) are never restarted: IntelliJ falls back, kotlin-lsp fails.
    /// Any other exit restarts the same server once; a second exit within `CRASH_WINDOW` of the
    /// first is a crash loop.
    pub fn server_terminated_at(
        &mut self,
        server_id: LanguageServerId,
        exit_status: Option<ExitStatus>,
        now: Instant,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree_id) = self.worktree_of_server(server_id) else {
            return;
        };
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let effective = state.effective;
        // The process is gone: nothing more comes from it.
        state.server_id = None;
        state.pending.clear();
        state.io_subscription = None;
        state.stderr_mirror = None;
        let detail =
            exit_status.map_or_else(|| "unknown status".into(), |status| status.to_string());
        let code = exit_status.and_then(|status| status.code());
        self.log.line(&format!("exit {detail}"));

        // Any exit of a launched trial within its window, code 7 included, rejects the build; the
        // crash-restart policy and the expired mark are for builds that already passed.
        let trial = state
            .trial
            .take_if(|trial| trial.server_id == Some(server_id));
        if let Some(trial) = trial {
            self.fail_trial(worktree_id, trial.launch, &format!("exited ({detail})"), cx);
            return;
        }

        if code == Some(EXPIRED_EXIT_CODE) {
            if let Some(state) = self.worktrees.get_mut(&worktree_id)
                && !state.expired.contains(&effective)
            {
                state.expired.push(effective);
            }
            if effective == KotlinServerKind::Intellij {
                self.fall_back_and_restart(worktree_id, FallbackReason::Expired, &detail, cx);
            } else {
                self.mark_expired(worktree_id, effective, cx);
            }
            return;
        }
        if effective == KotlinServerKind::Intellij && is_license_lost_exit(code) {
            self.fall_back_and_restart(worktree_id, FallbackReason::LicenseLost, &detail, cx);
            return;
        }

        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        state
            .crashes
            .retain(|crash| now.saturating_duration_since(*crash) < CRASH_WINDOW);
        state.crashes.push(now);
        if state.crashes.len() < 2 {
            self.log.line("crash: restarting once");
            self.restart(worktree_id, cx);
            return;
        }
        self.log.line(&format!("crash loop: {CRASH_LOOP_REASON}"));
        if effective == KotlinServerKind::Intellij {
            self.fall_back_and_restart(worktree_id, FallbackReason::CrashLoop, &detail, cx);
        } else {
            self.fail(worktree_id, CRASH_LOOP_REASON.into(), cx);
        }
    }

    /// A message or stderr text of the worktree's running server. A pinned license signal from
    /// the IntelliJ server falls back once (D10); the fallback runs kotlin-lsp, whose messages are
    /// never matched, so nothing retries IntelliJ on its own.
    pub fn server_text(&mut self, worktree_id: WorktreeId, text: &str, cx: &mut Context<Self>) {
        let runs_intellij = self.worktree(worktree_id).is_some_and(|state| {
            state.effective == KotlinServerKind::Intellij && state.fallback.is_none()
        });
        if let Some(state) = self.worktrees.get_mut(&worktree_id)
            && let Some(line) = text.lines().rev().find(|line| is_gradle_error(line))
        {
            state.gradle_error = Some(line.trim().to_string());
        }
        if runs_intellij && is_license_lost_signal(text) {
            let line = text
                .lines()
                .find(|line| is_license_lost_signal(line))
                .unwrap_or(text)
                .trim()
                .to_string();
            self.fall_back_and_restart(worktree_id, FallbackReason::LicenseLost, &line, cx);
        }
    }

    /// The start of the worktree's server failed after the adapter handed over a binary: the
    /// process did not spawn, or it did not answer `initialize` in time. A start the adapter itself
    /// refused already has its status and is left alone.
    pub fn server_start_failed(
        &mut self,
        worktree_id: WorktreeId,
        error: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let error = error.lines().next().unwrap_or(error).trim().to_string();
        // The IntelliJ start whose stderr already caused the fallback: kotlin-lsp is on its way.
        if std::mem::take(&mut state.stale_start) {
            self.log
                .line(&format!("start failed after the fallback: {error}"));
            return;
        }
        // The launched trial's process did not spawn or did not answer `initialize`.
        let trial = state.trial.take_if(|trial| trial.server_id.is_none());
        if let Some(trial) = trial {
            self.fail_trial(
                worktree_id,
                trial.launch,
                &format!("failed to start: {error}"),
                cx,
            );
            return;
        }
        if !matches!(state.status, None | Some(ServerStatus::Starting)) {
            return;
        }
        let effective = state.effective;
        self.log.line(&format!("start failed: {error}"));
        if effective == KotlinServerKind::Intellij {
            self.fall_back_and_restart(worktree_id, FallbackReason::StartFailed, &error, cx);
        } else {
            self.fail(worktree_id, format!("failed to start: {error}"), cx);
        }
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
                self.startup_stderr(*worktree_id, text, cx);
            }
            LspStoreEvent::LanguageServerStartFailed {
                name,
                worktree_id,
                error,
            } if is_kotlin(name) => self.server_start_failed(*worktree_id, error, cx),
            LspStoreEvent::LanguageServerLog(server_id, LanguageServerLogType::Log(_), text) => {
                if let Some(worktree_id) = self.worktree_of_server(*server_id) {
                    self.server_text(worktree_id, text, cx);
                }
            }
            // `window/showMessage` carries the server's name only; it applies to every worktree
            // whose running Kotlin server is IntelliJ.
            LspStoreEvent::LanguageServerPrompt(request) if request.lsp_name == ADAPTER_NAME => {
                let running: Vec<WorktreeId> = self
                    .worktrees
                    .iter()
                    .filter(|(_, state)| state.server_id.is_some())
                    .map(|(worktree_id, _)| *worktree_id)
                    .collect();
                for worktree_id in running {
                    self.server_text(worktree_id, &request.message, cx);
                }
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

    /// A line the server wrote to stderr while starting. An expired build says so here before it
    /// exits, and that is what ends its start for good.
    pub fn startup_stderr(&mut self, worktree_id: WorktreeId, text: &str, cx: &mut Context<Self>) {
        let had_fallback = self
            .worktree(worktree_id)
            .is_some_and(|state| state.fallback.is_some());
        self.read_startup_stderr(worktree_id, text, cx);
        if let Some(state) = self.worktrees.get_mut(&worktree_id)
            && !had_fallback
            && state.fallback.is_some()
            && state.server_id.is_none()
        {
            state.stale_start = true;
        }
    }

    fn read_startup_stderr(&mut self, worktree_id: WorktreeId, text: &str, cx: &mut Context<Self>) {
        let mirror = self
            .worktrees
            .get(&worktree_id)
            .and_then(|state| state.stderr_mirror.clone());
        match mirror {
            Some(mirror) => mirror.startup(text),
            None => self.log.stderr(text),
        }
        if let Some(state) = self.worktrees.get_mut(&worktree_id)
            && let Some(line) = text.lines().rev().find(|line| is_gradle_error(line))
        {
            state.gradle_error = Some(line.trim().to_string());
        }
        if text.contains(EXPIRED_MESSAGE)
            && let Some(effective) = self
                .worktrees
                .get(&worktree_id)
                .map(|state| state.effective)
        {
            if effective == KotlinServerKind::Intellij {
                if let Some(state) = self.worktrees.get_mut(&worktree_id)
                    && !state.expired.contains(&effective)
                {
                    state.expired.push(effective);
                }
                self.fall_back_and_restart(
                    worktree_id,
                    FallbackReason::Expired,
                    EXPIRED_MESSAGE,
                    cx,
                );
            } else if let Some(state) = self.worktrees.get_mut(&worktree_id)
                && state.trial.is_some()
            {
                // An expired trial is the trial's failure, not the end of kotlin-lsp for the run.
                // Its own start failure or exit 7 follows and is not counted again.
                if let Some(trial) = state.trial.take_if(|trial| trial.server_id.is_none()) {
                    state.stale_start = true;
                    self.fail_trial(worktree_id, trial.launch, "expired", cx);
                }
            } else {
                self.mark_expired(worktree_id, effective, cx);
            }
            return;
        }
        self.server_text(worktree_id, text, cx);
    }

    /// Starts mirroring the server's stderr for the worktree. The startup text, if the server wrote
    /// any, is delivered by the same update that added the server, so the held live lines are
    /// released by a task, which runs only after that update.
    fn begin_stderr_mirror(
        &mut self,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) -> Option<StderrMirror> {
        let state = self.worktrees.get_mut(&worktree_id)?;
        let mirror = StderrMirror::new(self.log.clone());
        state.stderr_mirror = Some(mirror.clone());
        let release = mirror.clone();
        cx.spawn(async move |_, _| release.release()).detach();
        Some(mirror)
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
        let Some(mirror) = self.begin_stderr_mirror(worktree_id, cx) else {
            return;
        };
        // Stderr arrives off the main thread: a line with a license signal is sent over.
        let (signal_sender, mut signals) = futures::channel::mpsc::unbounded::<String>();
        let subscription = server.on_io(move |kind, text| {
            if matches!(kind, IoKind::StdErr) {
                mirror.live(text);
                if is_license_lost_signal(text) {
                    signal_sender.unbounded_send(text.to_string()).ok();
                }
            }
        });
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(text) = signals.next().await {
                let delivered = this.update(cx, |this, cx| {
                    if this.worktree_of_server(server_id) == Some(worktree_id) {
                        this.server_text(worktree_id, &text, cx);
                    }
                });
                if delivered.is_err() {
                    break;
                }
            }
        })
        .detach();
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

/// The server a worktree runs: the requested one, or kotlin-lsp while a fallback stands in for
/// IntelliJ.
fn effective_server(requested: KotlinServerKind, fallback: Option<&Fallback>) -> KotlinServerKind {
    if fallback.is_some() {
        KotlinServerKind::KotlinLsp
    } else {
        requested
    }
}

/// A stderr line that reports a Gradle failure.
fn is_gradle_error(line: &str) -> bool {
    let line = line.to_lowercase();
    line.contains("gradle") && (line.contains("error") || line.contains("fail"))
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
    use crate::{KotlinSettings, license::LICENSE_LOST_SIGNALS, resolve::project_override};

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
            // A server whose start failed, or that crashed, is not live any more.
            inner.live.set(inner.live.get().saturating_sub(1));
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
            config_dir: temp.path().join("config"),
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

        fn expired(&self, cx: &mut TestAppContext) -> bool {
            self.manager.read_with(cx, |manager, _| {
                manager.is_expired(worktree(), KotlinServerKind::KotlinLsp)
            })
        }

        fn expired_intellij(&self, cx: &mut TestAppContext) -> bool {
            self.manager.read_with(cx, |manager, _| {
                manager.is_expired(worktree(), KotlinServerKind::Intellij)
            })
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

        // An unexpected exit restarts the server once.
        harness.manager.update(cx, |manager, cx| {
            manager.server_added(server_id, worktree(), cx);
            manager.server_terminated(server_id, None, cx);
        });
        assert_eq!(harness.status(cx), Some(ServerStatus::Starting));
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp", "stop", "start kotlin-lsp"]
        );
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));
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

    fn exited_with(code: i32) -> Option<ExitStatus> {
        use std::os::unix::process::ExitStatusExt as _;
        Some(ExitStatus::from_raw(code << 8))
    }

    /// Exit code 7 is an expired build: failed, never restarted, never crash-looping.
    #[gpui::test]
    fn test_an_expired_exit_fails_without_a_restart(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);

        // Another exit code is an ordinary crash, which restarts.
        harness.manager.update(cx, |manager, cx| {
            manager.server_terminated(LanguageServerId(1), exited_with(1), cx)
        });
        cx.run_until_parked();
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));
        assert!(!harness.expired(cx));

        let server_id = harness.server_id(cx);
        harness.manager.update(cx, |manager, cx| {
            manager.server_terminated(server_id, exited_with(7), cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: EXPIRED_REASON.into()
            })
        );
        assert!(harness.expired(cx));

        // The explicit restart is refused too: only a new bongos build has a new pin.
        harness
            .manager
            .update(cx, |manager, cx| manager.restart_server(worktree(), cx));
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp", "stop", "start kotlin-lsp"]
        );
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: EXPIRED_REASON.into()
            })
        );
        assert!(harness.log().contains("restart skipped: server expired"));
    }

    fn stderr_lines(harness: &Harness) -> Vec<String> {
        harness
            .log()
            .lines()
            .filter_map(|line| {
                line.split_once(" stderr ")
                    .map(|(_, text)| text.to_string())
            })
            .collect()
    }

    #[gpui::test]
    fn test_stderr_read_during_startup_is_logged_once(cx: &mut TestAppContext) {
        let harness = setup(cx);
        let mirror = harness
            .manager
            .update(cx, |manager, cx| {
                manager.begin_stderr_mirror(worktree(), cx)
            })
            .unwrap();
        // The line read between the subscription and the capture being taken is in both.
        mirror.live("before\n");
        mirror.live("both\n");
        harness.manager.update(cx, |manager, cx| {
            manager.startup_stderr(worktree(), "before\nboth\n", cx)
        });
        mirror.live("after\n");
        cx.run_until_parked();
        assert_eq!(stderr_lines(&harness), ["before", "both", "after"]);
    }

    #[gpui::test]
    fn test_stderr_with_no_startup_text_is_logged_once(cx: &mut TestAppContext) {
        let harness = setup(cx);
        let mirror = harness
            .manager
            .update(cx, |manager, cx| {
                manager.begin_stderr_mirror(worktree(), cx)
            })
            .unwrap();
        mirror.live("one\n");
        // The release task runs after the update that added the server.
        cx.run_until_parked();
        mirror.live("two\n");
        assert_eq!(stderr_lines(&harness), ["one", "two"]);
    }

    #[gpui::test]
    fn test_stderr_of_a_start_that_failed_is_logged_once(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.manager.update(cx, |manager, cx| {
            manager.startup_stderr(worktree(), "boom\nboom\n", cx)
        });
        // Repeated lines stay: only the overlap of two sources is dropped.
        assert_eq!(stderr_lines(&harness), ["boom", "boom"]);
    }

    #[gpui::test]
    fn test_a_gradle_failure_on_stderr_is_kept_for_the_menu(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.manager.update(cx, |manager, cx| {
            manager.startup_stderr(
                worktree(),
                "loading\nGradle import failed: no wrapper\n",
                cx,
            )
        });
        let error = harness.manager.read_with(cx, |manager, _| {
            manager.worktree(worktree()).unwrap().gradle_error.clone()
        });
        assert_eq!(error.as_deref(), Some("Gradle import failed: no wrapper"));
    }

    #[gpui::test]
    fn test_a_gradle_failure_on_live_stderr_is_kept_for_the_menu(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.manager.update(cx, |manager, cx| {
            manager.server_text(worktree(), "Gradle import failed: offline\n", cx)
        });
        let error = harness.manager.read_with(cx, |manager, _| {
            manager.worktree(worktree()).unwrap().gradle_error.clone()
        });
        assert_eq!(error.as_deref(), Some("Gradle import failed: offline"));
    }

    #[gpui::test]
    fn test_the_expiry_message_on_startup_stderr_fails_the_server(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness
            .manager
            .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
        harness.manager.update(cx, |manager, cx| {
            manager.startup_stderr(worktree(), "loading\n", cx)
        });
        assert_eq!(harness.status(cx), Some(ServerStatus::Starting));
        harness.manager.update(cx, |manager, cx| {
            manager.startup_stderr(worktree(), "This build of kotlin-server has expired.\n", cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: EXPIRED_REASON.into()
            })
        );
        assert!(harness.expired(cx));
        // The other server is not affected.
        let intellij = harness.manager.read_with(cx, |manager, _| {
            manager.is_expired(worktree(), KotlinServerKind::Intellij)
        });
        assert!(!intellij);
    }

    #[gpui::test]
    fn test_install_status_and_cancel(cx: &mut TestAppContext) {
        let harness = setup(cx);
        assert!(
            !harness
                .manager
                .update(cx, |manager, _| manager.cancel_install(worktree())),
            "nothing to cancel"
        );
        let cancel = harness
            .manager
            .update(cx, |manager, cx| manager.begin_install(worktree(), cx));
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Installing {
                done: 0,
                total: None
            })
        );
        harness.manager.update(cx, |manager, cx| {
            manager.install_progress(worktree(), 52, Some(137), cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Installing {
                done: 52,
                total: Some(137)
            })
        );
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(
            harness
                .manager
                .update(cx, |manager, _| manager.cancel_install(worktree()))
        );
        assert!(cancel.load(Ordering::Relaxed));
        harness.manager.update(cx, |manager, cx| {
            manager.end_install(worktree(), Err("install cancelled".into()), cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: "install cancelled".into()
            })
        );
        // Progress that arrives after the end does not bring the status back.
        harness.manager.update(cx, |manager, cx| {
            manager.install_progress(worktree(), 60, Some(137), cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: "install cancelled".into()
            })
        );
    }

    /// Opens the `kotlin: switch server` choices and confirms the row labeled `label`, as the
    /// picker does.
    fn pick(harness: &Harness, label: &str, cx: &mut TestAppContext) {
        let manager = harness.manager.clone();
        cx.update(|cx| {
            let mut delegate = crate::commands::SwitchServerDelegate::new(
                WeakEntity::new_invalid(),
                None,
                manager,
                worktree(),
                cx,
            );
            delegate.selected_index = delegate
                .choices
                .iter()
                .position(|(choice_label, _)| choice_label == label)
                .unwrap_or_else(|| panic!("no choice {label:?}"));
            delegate.choose(cx);
        });
    }

    fn override_of(harness: &Harness) -> Option<KotlinServerKind> {
        project_override(&harness.dirs.state_dir, &harness.root).unwrap()
    }

    #[gpui::test]
    fn test_the_picker_writes_the_override_and_restarts(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);

        pick(&harness, "IntelliJ server", cx);
        assert_eq!(override_of(&harness), Some(KotlinServerKind::Intellij));
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp", "stop", "start intellij"]
        );

        pick(&harness, "kotlin-lsp", cx);
        assert_eq!(override_of(&harness), Some(KotlinServerKind::KotlinLsp));
        cx.run_until_parked();
        assert_eq!(harness.events().last().unwrap(), "start kotlin-lsp");
        assert_eq!(harness.host.0.max_live.get(), 1);
    }

    #[gpui::test]
    fn test_use_setting_clears_the_override_and_restarts(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);
        pick(&harness, "IntelliJ server", cx);
        cx.run_until_parked();
        assert_eq!(override_of(&harness), Some(KotlinServerKind::Intellij));

        pick(&harness, "Use setting (kotlin-lsp)", cx);
        assert_eq!(override_of(&harness), None);
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
    }

    #[gpui::test]
    fn test_restart_clears_the_fallback_and_the_crash_history(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);
        harness.manager.update(cx, |manager, cx| {
            manager.server_terminated(LanguageServerId(1), exited_with(1), cx)
        });
        cx.run_until_parked();
        harness.manager.update(cx, |manager, _| {
            manager.worktrees.get_mut(&worktree()).unwrap().fallback =
                Some(Fallback::new(FallbackReason::Unlicensed));
            assert_eq!(manager.crash_count(worktree()), 1);
        });

        harness
            .manager
            .update(cx, |manager, cx| manager.restart_server(worktree(), cx));
        cx.run_until_parked();
        harness.manager.read_with(cx, |manager, _| {
            assert_eq!(manager.crash_count(worktree()), 0);
            assert_eq!(manager.worktree(worktree()).unwrap().fallback, None);
        });
        assert_eq!(
            harness.events(),
            [
                "start kotlin-lsp",
                "stop",
                "start kotlin-lsp",
                "stop",
                "start kotlin-lsp"
            ]
        );
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));
    }

    #[gpui::test]
    fn test_reimport_restarts_the_server(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);

        harness
            .manager
            .update(cx, |manager, cx| manager.reimport_project(worktree(), cx));
        assert_eq!(harness.status(cx), Some(ServerStatus::Starting));
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp", "stop", "start kotlin-lsp"]
        );
        assert!(harness.log().contains("reimport requested"));
    }

    // -----------------------------------------------------------------------------------------
    // The fallback and crash policy (the fallback table, D10, D14)
    // -----------------------------------------------------------------------------------------

    impl Harness {
        fn server_id(&self, cx: &mut TestAppContext) -> LanguageServerId {
            self.manager.read_with(cx, |manager, _| {
                manager
                    .worktree(worktree())
                    .and_then(|state| state.server_id)
                    .expect("a running server")
            })
        }

        fn fallback(&self, cx: &mut TestAppContext) -> Option<FallbackReason> {
            self.manager.read_with(cx, |manager, _| {
                manager
                    .worktree(worktree())
                    .and_then(|state| state.fallback.as_ref())
                    .map(|fallback| fallback.kind)
            })
        }

        fn effective(&self, cx: &mut TestAppContext) -> KotlinServerKind {
            self.manager.read_with(cx, |manager, _| {
                manager.worktree(worktree()).unwrap().effective
            })
        }

        /// Counts the notices the manager asks the workspace to show.
        fn count_notices(&self, cx: &mut TestAppContext) -> Rc<RefCell<Vec<String>>> {
            let notices = Rc::new(RefCell::new(Vec::new()));
            cx.update(|cx| {
                let notices = notices.clone();
                cx.subscribe(&self.manager, move |_, event: &KotlinEvent, _| {
                    if let KotlinEvent::FellBack { message, .. } = event {
                        notices.borrow_mut().push(message.clone());
                    }
                })
                .detach();
            });
            notices
        }

        fn crash(&self, cx: &mut TestAppContext, code: i32, at: Instant) {
            let server_id = self.server_id(cx);
            self.manager.update(cx, |manager, cx| {
                manager.server_terminated_at(server_id, exited_with(code), at, cx)
            });
            cx.run_until_parked();
        }

        fn fallback_lines(&self) -> usize {
            self.log()
                .lines()
                .filter(|line| line.contains(" fallback ") && line.contains(" -> kotlin-lsp: "))
                .count()
        }
    }

    const INTELLIJ: &str = r#"{ "kotlin": { "server": "intellij" } }"#;

    /// A worktree whose setting asks for IntelliJ, running it.
    fn intellij_running(cx: &mut TestAppContext) -> Harness {
        let harness = setup(cx);
        set_user(cx, INTELLIJ);
        harness.first_start(cx);
        assert_eq!(harness.events(), ["start intellij"]);
        harness
    }

    /// The rows that end an IntelliJ start before a process runs: the adapter reports them, the
    /// fallback is recorded, and the same start goes on with kotlin-lsp.
    #[gpui::test]
    fn test_pre_start_rows_fall_back_to_kotlin_lsp(cx: &mut TestAppContext) {
        for (reason, text) in [
            (
                FallbackReason::EulaNotAccepted,
                "IntelliJ EULA not accepted",
            ),
            (FallbackReason::Unlicensed, "IntelliJ unlicensed"),
            (
                FallbackReason::LicenseCheckFailed,
                "license check failed (offline?)",
            ),
            (FallbackReason::InstallFailed, "IntelliJ install failed"),
            (FallbackReason::Expired, "IntelliJ server expired"),
        ] {
            let harness = setup(cx);
            set_user(cx, INTELLIJ);
            let notices = harness.count_notices(cx);
            let server = harness
                .manager
                .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
            assert_eq!(server, Some(KotlinServerKind::Intellij));
            let fell_back = harness.manager.update(cx, |manager, cx| {
                manager.intellij_unavailable(worktree(), reason, "detail", cx)
            });
            assert!(fell_back, "{reason:?}");
            assert_eq!(harness.fallback(cx), Some(reason));
            assert_eq!(harness.effective(cx), KotlinServerKind::KotlinLsp);
            assert_eq!(harness.status(cx), Some(ServerStatus::Starting));
            assert_eq!(*notices.borrow(), [fallback_message(reason)]);
            assert!(notices.borrow()[0].contains(text), "{:?}", notices.borrow());
            assert_eq!(harness.fallback_lines(), 1, "{}", harness.log());
            assert!(harness.log().contains(text), "{}", harness.log());
            // Sticky: the next start runs kotlin-lsp without trying IntelliJ.
            let server = harness
                .manager
                .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
            assert_eq!(server, Some(KotlinServerKind::KotlinLsp), "{reason:?}");
            assert_eq!(
                harness.expired_intellij(cx),
                reason == FallbackReason::Expired
            );
            set_user(cx, "{}");
        }
    }

    #[gpui::test]
    fn test_kotlin_lsp_has_nothing_to_fall_back_from(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness
            .manager
            .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
        let fell_back = harness.manager.update(cx, |manager, cx| {
            manager.intellij_unavailable(worktree(), FallbackReason::InstallFailed, "", cx)
        });
        assert!(!fell_back);
        assert_eq!(harness.fallback(cx), None);
    }

    /// Spawn error or no `initialize` reply in 120 s: IntelliJ falls back and kotlin-lsp starts;
    /// kotlin-lsp is `Failed`.
    #[gpui::test]
    fn test_a_failed_start_falls_back_or_fails(cx: &mut TestAppContext) {
        let harness = setup(cx);
        set_user(cx, INTELLIJ);
        harness
            .manager
            .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
        harness.manager.update(cx, |manager, cx| {
            manager.server_start_failed(worktree(), "oneshot canceled\nmore", cx)
        });
        assert_eq!(harness.fallback(cx), Some(FallbackReason::StartFailed));
        cx.run_until_parked();
        assert_eq!(harness.events(), ["stop", "start kotlin-lsp"]);
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));

        let harness = setup(cx);
        set_user(cx, "{}");
        harness
            .manager
            .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
        harness.manager.update(cx, |manager, cx| {
            manager.server_start_failed(worktree(), "No such file or directory (os error 2)", cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: "failed to start: No such file or directory (os error 2)".into()
            })
        );
        assert_eq!(harness.fallback(cx), None);
        cx.run_until_parked();
        assert!(harness.events().is_empty(), "{:?}", harness.events());

        // A start the adapter refused already has its status: the event changes nothing.
        harness.manager.update(cx, |manager, cx| {
            manager.start_failed(worktree(), "no JDK".into(), cx)
        });
        harness.manager.update(cx, |manager, cx| {
            manager.server_start_failed(worktree(), "no JDK", cx)
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: "no JDK".into()
            })
        );
    }

    #[gpui::test]
    fn test_intellij_crashes_restart_once_then_fall_back(cx: &mut TestAppContext) {
        let harness = intellij_running(cx);
        let notices = harness.count_notices(cx);
        let start = Instant::now();

        harness.crash(cx, 1, start);
        assert_eq!(
            harness.events(),
            ["start intellij", "stop", "start intellij"]
        );
        assert_eq!(harness.fallback(cx), None);
        assert!(harness.log().contains("crash: restarting once"));

        harness.crash(cx, 1, start + Duration::from_secs(30));
        assert_eq!(harness.fallback(cx), Some(FallbackReason::CrashLoop));
        assert_eq!(
            harness.events(),
            [
                "start intellij",
                "stop",
                "start intellij",
                "stop",
                "start kotlin-lsp"
            ]
        );
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));
        assert_eq!(notices.borrow().len(), 1);
        assert_eq!(harness.host.0.max_live.get(), 1);
    }

    #[gpui::test]
    fn test_kotlin_lsp_crashes_restart_once_then_fail(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);
        let start = Instant::now();
        harness.crash(cx, 1, start);
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp", "stop", "start kotlin-lsp"]
        );
        harness.crash(cx, 1, start + Duration::from_secs(59));
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::Failed {
                reason: CRASH_LOOP_REASON.into()
            })
        );
        // Failed stays failed: no restart, no fallback.
        cx.run_until_parked();
        assert_eq!(harness.events().len(), 3);
        assert_eq!(harness.fallback(cx), None);
        // A manual restart always works.
        harness
            .manager
            .update(cx, |manager, cx| manager.restart_server(worktree(), cx));
        cx.run_until_parked();
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));
        assert_eq!(harness.events().len(), 5);
    }

    #[gpui::test]
    fn test_a_second_crash_after_61_seconds_restarts_again(cx: &mut TestAppContext) {
        for intellij in [false, true] {
            let harness = setup(cx);
            set_user(cx, if intellij { INTELLIJ } else { "{}" });
            harness.first_start(cx);
            let server = if intellij {
                "start intellij"
            } else {
                "start kotlin-lsp"
            };
            let start = Instant::now();
            harness.crash(cx, 1, start);
            harness.crash(cx, 1, start + Duration::from_secs(61));
            harness.crash(cx, 1, start + Duration::from_secs(122));
            assert_eq!(
                harness.events(),
                [server, "stop", server, "stop", server, "stop", server],
                "intellij: {intellij}"
            );
            assert_eq!(harness.status(cx), Some(ServerStatus::Ready));
            assert_eq!(harness.fallback(cx), None);
            assert_eq!(
                harness
                    .manager
                    .read_with(cx, |manager, _| manager.crash_count(worktree())),
                1,
                "older crashes are forgotten"
            );
        }
    }

    #[gpui::test]
    fn test_a_license_signal_falls_back_once_and_never_retries(cx: &mut TestAppContext) {
        let harness = intellij_running(cx);
        let notices = harness.count_notices(cx);
        // Ordinary messages are not signals.
        harness.manager.update(cx, |manager, cx| {
            manager.server_text(worktree(), "Indexing 3/5 modules", cx)
        });
        assert_eq!(harness.fallback(cx), None);

        harness.manager.update(cx, |manager, cx| {
            manager.server_text(worktree(), "No valid license found", cx)
        });
        assert_eq!(harness.fallback(cx), Some(FallbackReason::LicenseLost));
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            ["start intellij", "stop", "start kotlin-lsp"]
        );

        // The same text again, from kotlin-lsp, a crash of kotlin-lsp, and a long wait: IntelliJ is
        // never started on its own and there is no second notice.
        harness.manager.update(cx, |manager, cx| {
            manager.server_text(worktree(), "No valid license found", cx)
        });
        harness.crash(cx, 1, Instant::now());
        cx.executor().advance_clock(Duration::from_secs(3600));
        cx.run_until_parked();
        assert_eq!(
            harness.events(),
            [
                "start intellij",
                "stop",
                "start kotlin-lsp",
                "stop",
                "start kotlin-lsp"
            ]
        );
        assert_eq!(harness.fallback(cx), Some(FallbackReason::LicenseLost));
        assert_eq!(notices.borrow().len(), 1);
        assert_eq!(harness.fallback_lines(), 1);
    }

    #[gpui::test]
    fn test_every_license_signal_source_falls_back(cx: &mut TestAppContext) {
        let lost = |harness: &Harness, cx: &mut TestAppContext| {
            assert_eq!(harness.fallback(cx), Some(FallbackReason::LicenseLost));
            cx.run_until_parked();
            assert_eq!(harness.events().last().unwrap(), "start kotlin-lsp");
        };
        // A pinned exit code.
        let harness = intellij_running(cx);
        harness.crash(cx, 11, Instant::now());
        lost(&harness, cx);
        // Stderr written while starting.
        let harness = intellij_running(cx);
        harness.manager.update(cx, |manager, cx| {
            manager.startup_stderr(
                worktree(),
                "booting\nLicense: not licensed, feature requests are parked (NoLicenseDiscovered)\n",
                cx,
            )
        });
        lost(&harness, cx);
        assert!(harness.log().contains("NoLicenseDiscovered"));
        // `window/logMessage` and `window/showMessage` arrive as text of the running server.
        for pinned in LICENSE_LOST_SIGNALS {
            let harness = intellij_running(cx);
            harness.manager.update(cx, |manager, cx| {
                manager.server_text(worktree(), &format!("server: {pinned}."), cx)
            });
            lost(&harness, cx);
        }
    }

    #[gpui::test]
    fn test_an_expired_build_falls_back_or_fails_without_a_restart(cx: &mut TestAppContext) {
        let harness = intellij_running(cx);
        harness.crash(cx, 7, Instant::now());
        assert_eq!(harness.fallback(cx), Some(FallbackReason::Expired));
        assert_eq!(
            harness.events(),
            ["start intellij", "stop", "start kotlin-lsp"]
        );
        assert!(harness.expired_intellij(cx));

        // kotlin-lsp expired while it stands in: no server at all, and no restart.
        harness.crash(cx, 7, Instant::now());
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::BothFailed {
                intellij: "IntelliJ server expired".into(),
                kotlin_lsp: EXPIRED_REASON.into(),
            })
        );
        assert_eq!(harness.events().len(), 3);
    }

    #[gpui::test]
    fn test_kotlin_lsp_failing_during_a_fallback_is_both_failed(cx: &mut TestAppContext) {
        // Its start fails.
        let harness = intellij_running(cx);
        harness.manager.update(cx, |manager, cx| {
            manager.server_text(worktree(), "No valid license found", cx)
        });
        cx.run_until_parked();
        harness.manager.update(cx, |manager, cx| {
            manager.end_install(
                worktree(),
                Err("install failed: network is down".into()),
                cx,
            )
        });
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::BothFailed {
                intellij: "IntelliJ license lost".into(),
                kotlin_lsp: "install failed: network is down".into(),
            })
        );

        // It crash-loops.
        let harness = intellij_running(cx);
        harness.crash(cx, 1, Instant::now());
        harness.crash(cx, 1, Instant::now());
        assert_eq!(harness.fallback(cx), Some(FallbackReason::CrashLoop));
        harness.crash(cx, 1, Instant::now());
        harness.crash(cx, 1, Instant::now());
        assert_eq!(
            harness.status(cx),
            Some(ServerStatus::BothFailed {
                intellij: "IntelliJ crashed twice within a minute".into(),
                kotlin_lsp: CRASH_LOOP_REASON.into(),
            })
        );
    }

    #[gpui::test]
    fn test_one_notice_per_fallback(cx: &mut TestAppContext) {
        let harness = intellij_running(cx);
        let notices = harness.count_notices(cx);
        for _ in 0..3 {
            harness.manager.update(cx, |manager, cx| {
                manager.server_text(worktree(), "No valid license found", cx);
                manager.intellij_unavailable(worktree(), FallbackReason::Unlicensed, "", cx);
            });
            cx.run_until_parked();
        }
        assert_eq!(notices.borrow().len(), 1);

        // A manual restart tries IntelliJ again; failing again is a new fallback with its notice.
        harness
            .manager
            .update(cx, |manager, cx| manager.restart_server(worktree(), cx));
        cx.run_until_parked();
        assert_eq!(harness.events().last().unwrap(), "start intellij");
        harness.crash(cx, 1, Instant::now());
        harness.crash(cx, 1, Instant::now());
        assert_eq!(
            *notices.borrow(),
            [
                fallback_message(FallbackReason::LicenseLost),
                fallback_message(FallbackReason::CrashLoop)
            ]
        );
        assert_eq!(harness.fallback_lines(), 2);
    }

    /// D14: the fallback stays until restart, switch, a successful license command, or a change
    /// of the resolved server; nothing else brings IntelliJ back.
    #[gpui::test]
    fn test_the_fallback_is_sticky(cx: &mut TestAppContext) {
        let lose = |harness: &Harness, cx: &mut TestAppContext| {
            harness.manager.update(cx, |manager, cx| {
                manager.server_text(worktree(), "No valid license found", cx)
            });
            cx.run_until_parked();
            assert_eq!(harness.effective(cx), KotlinServerKind::KotlinLsp);
        };
        let harness = intellij_running(cx);
        lose(&harness, cx);
        let before = harness.events().len();

        // A settings change that resolves to the same server keeps it.
        set_user(
            cx,
            r#"{ "buffer_font_size": 15, "kotlin": { "server": "intellij" } }"#,
        );
        cx.run_until_parked();
        assert_eq!(harness.events().len(), before);
        assert_eq!(harness.fallback(cx), Some(FallbackReason::LicenseLost));

        // `kotlin: switch server` to IntelliJ, already the requested server, ends it.
        harness
            .manager
            .update(cx, |manager, cx| {
                manager.switch_server(worktree(), Some(KotlinServerKind::Intellij), cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(harness.fallback(cx), None);
        assert_eq!(harness.events().last().unwrap(), "start intellij");

        // A successful license command ends it.
        lose(&harness, cx);
        harness.manager.update(cx, |manager, cx| {
            manager.license_checked(worktree(), LicenseOutcome::Unlicensed, cx)
        });
        cx.run_until_parked();
        assert_eq!(harness.fallback(cx), Some(FallbackReason::LicenseLost));
        harness.manager.update(cx, |manager, cx| {
            manager.license_checked(worktree(), LicenseOutcome::Licensed, cx)
        });
        cx.run_until_parked();
        assert_eq!(harness.fallback(cx), None);
        assert_eq!(harness.events().last().unwrap(), "start intellij");

        // A change of the resolved server ends it: kotlin-lsp, already running, is not restarted.
        harness
            .manager
            .update(cx, |manager, cx| {
                manager.switch_server(worktree(), None, cx)
            })
            .unwrap();
        cx.run_until_parked();
        lose(&harness, cx);
        let before = harness.events().len();
        set_user(cx, "{}");
        cx.run_until_parked();
        assert_eq!(harness.fallback(cx), None);
        assert_eq!(harness.events().len(), before);
        assert_eq!(harness.effective(cx), KotlinServerKind::KotlinLsp);

        // An app restart (a new manager) asks for IntelliJ again.
        set_user(cx, INTELLIJ);
        lose(&harness, cx);
        let fresh = cx.new(|cx| {
            KotlinProjectManager::new(Rc::new(harness.host.clone()), harness.dirs.clone(), cx)
        });
        let server = fresh.update(cx, |manager, cx| {
            manager.add_worktree(worktree(), harness.root.clone(), cx);
            manager.begin_start(worktree(), cx)
        });
        assert_eq!(server, Some(KotlinServerKind::Intellij));
    }

    /// `LspStore` reports a failed start's stderr, then the failure. When the stderr already made
    /// IntelliJ fall back, the failure is IntelliJ's, not kotlin-lsp's: no `BothFailed`.
    #[gpui::test]
    fn test_a_failed_start_after_a_stderr_fallback_is_not_both_failed(cx: &mut TestAppContext) {
        for (stderr, reason) in [
            (EXPIRED_MESSAGE, FallbackReason::Expired),
            ("No valid license found", FallbackReason::LicenseLost),
        ] {
            let harness = setup(cx);
            set_user(cx, INTELLIJ);
            let notices = harness.count_notices(cx);
            harness
                .manager
                .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
            harness.manager.update(cx, |manager, cx| {
                manager.startup_stderr(worktree(), &format!("{stderr}\n"), cx);
                manager.server_start_failed(worktree(), "server shut down", cx);
            });
            assert_eq!(harness.fallback(cx), Some(reason));
            assert_eq!(harness.status(cx), Some(ServerStatus::Starting));
            assert_eq!(notices.borrow().len(), 1);
            assert_eq!(harness.fallback_lines(), 1, "{}", harness.log());
            assert!(
                !harness.log().contains("start failed: "),
                "{}",
                harness.log()
            );
            cx.run_until_parked();
            assert_eq!(harness.events(), ["stop", "start kotlin-lsp"]);
            assert_eq!(harness.status(cx), Some(ServerStatus::Ready));

            // kotlin-lsp's own failed start afterwards is a real one.
            harness
                .manager
                .update(cx, |manager, cx| manager.begin_start(worktree(), cx));
            harness.manager.update(cx, |manager, cx| {
                manager.server_start_failed(worktree(), "spawn error", cx)
            });
            assert_eq!(
                harness.status(cx),
                Some(ServerStatus::BothFailed {
                    intellij: reason.text().into(),
                    kotlin_lsp: "failed to start: spawn error".into(),
                })
            );
            set_user(cx, "{}");
        }
    }

    /// A switch starts the crash history over even when it ends no fallback.
    #[gpui::test]
    fn test_a_switch_forgets_the_crash_history(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);
        let start = Instant::now();
        harness.crash(cx, 1, start);
        harness
            .manager
            .update(cx, |manager, cx| {
                manager.switch_server(worktree(), Some(KotlinServerKind::Intellij), cx)
            })
            .unwrap();
        cx.run_until_parked();
        harness.crash(cx, 1, start + Duration::from_secs(20));
        assert_eq!(harness.fallback(cx), None);
        assert_eq!(
            harness.events(),
            [
                "start kotlin-lsp",
                "stop",
                "start kotlin-lsp",
                "stop",
                "start intellij",
                "stop",
                "start intellij"
            ]
        );
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));
    }

    const TRIAL: &str = "263.2.0";
    const ACTIVE: &str = "263.1.0";
    const PREVIOUS: &str = "263.0.0";
    const PIN: &str = "262.0.0";

    fn build_id(version: &str) -> BuildId {
        BuildId {
            version: version.into(),
            sha256: format!("{:0>64}", version.replace('.', "")),
        }
    }

    fn trial_launch() -> TrialLaunch {
        TrialLaunch {
            build: build_id(TRIAL),
            fallback: format!("kotlin-lsp {ACTIVE}"),
            pin_version: PIN.into(),
        }
    }

    impl Harness {
        /// `state.json`, read and written the way another process would.
        fn cache(&self) -> ManifestCache {
            ManifestCache::new(
                &self.dirs.install_root,
                crate::locks::tests::thread_registry(),
            )
        }

        fn builds(&self) -> KotlinLspBuilds {
            self.cache().read_state().kotlin_lsp
        }

        /// Active, previous and a pending trial, each installed, plus an older build and the pin.
        fn seed_trial(&self) {
            futures::executor::block_on(self.cache().with_state(|state| {
                state.kotlin_lsp.active = Some(build_id(ACTIVE));
                state.kotlin_lsp.previous = Some(build_id(PREVIOUS));
                state.kotlin_lsp.trial = Some(build_id(TRIAL));
            }))
            .unwrap();
            let long_ago = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
            for version in ["261.0.0", PIN, PREVIOUS, ACTIVE, TRIAL] {
                let dir = self.server_dir().join(version);
                fs::create_dir_all(&dir).unwrap();
                let marker = fs::File::create(dir.join(crate::installed::MARKER)).unwrap();
                marker.set_modified(long_ago).unwrap();
            }
        }

        fn server_dir(&self) -> PathBuf {
            self.dirs.install_root.join("kotlin-lsp")
        }

        fn versions(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.server_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect();
            names.sort();
            names
        }

        /// A start that `start_command` answered with the trial's command; the server is added
        /// when `added`.
        fn trial_start(&self, added: bool, cx: &mut TestAppContext) -> LanguageServerId {
            let server = self
                .manager
                .update(cx, |manager, cx| manager.begin_start(worktree(), cx))
                .unwrap();
            self.manager.update(cx, |manager, cx| {
                manager.trial_launched(worktree(), trial_launch(), cx)
            });
            let server_id = self.host.0.record_start(server);
            if added {
                self.manager.update(cx, |manager, cx| {
                    manager.server_added(server_id, worktree(), cx)
                });
            }
            server_id
        }

        fn crash_count(&self, cx: &mut TestAppContext) -> usize {
            self.manager
                .read_with(cx, |manager, _| manager.crash_count(worktree()))
        }

        fn launched_trial(&self, cx: &mut TestAppContext) -> Option<BuildId> {
            self.manager
                .read_with(cx, |manager, _| manager.launched_trial(worktree()).cloned())
        }

        /// Every trial notice the manager emits from now on.
        fn trial_notices(&self, cx: &mut TestAppContext) -> Rc<RefCell<Vec<String>>> {
            let notices = Rc::new(RefCell::new(Vec::new()));
            cx.update(|cx| {
                let notices = notices.clone();
                cx.subscribe(&self.manager, move |_, event: &KotlinEvent, _| {
                    if let KotlinEvent::TrialFailed { message, .. } = event {
                        notices.borrow_mut().push(message.clone());
                    }
                })
                .detach();
            });
            notices
        }
    }

    #[gpui::test]
    fn test_a_trial_that_runs_through_its_window_passes_and_prunes(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.seed_trial();
        let notices = harness.trial_notices(cx);
        harness.trial_start(true, cx);

        cx.executor()
            .advance_clock(TRIAL_WINDOW - Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(harness.launched_trial(cx), Some(build_id(TRIAL)));
        assert_eq!(harness.builds().active, Some(build_id(ACTIVE)));

        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(harness.launched_trial(cx), None);
        assert_eq!(
            harness.builds(),
            KotlinLspBuilds {
                active: Some(build_id(TRIAL)),
                previous: Some(build_id(ACTIVE)),
                trial: None,
                rejected: Vec::new(),
            }
        );
        // The old previous build and the stray one are gone; the pin stays.
        assert_eq!(harness.versions(), [PIN, ACTIVE, TRIAL]);
        assert!(notices.borrow().is_empty());
        assert_eq!(harness.events(), ["start kotlin-lsp"]);
        let log = harness.log();
        assert!(log.contains("trial: 263.2.0 started"), "{log}");
        assert!(log.contains("trial: 263.2.0 passed"), "{log}");
        assert!(log.contains("prune: removed kotlin-lsp 263.0.0"), "{log}");
    }

    /// What every rejection leaves: the build in `rejected` (on disk and for this run), the active
    /// build unchanged, one notice, one restart, no crash counted and nothing marked expired.
    fn assert_rejected(harness: &Harness, notices: &RefCell<Vec<String>>, cx: &mut TestAppContext) {
        cx.run_until_parked();
        assert_eq!(harness.launched_trial(cx), None);
        assert_eq!(
            harness.builds(),
            KotlinLspBuilds {
                active: Some(build_id(ACTIVE)),
                previous: Some(build_id(PREVIOUS)),
                trial: None,
                rejected: vec![build_id(TRIAL)],
            }
        );
        assert_eq!(
            cx.update(|cx| RejectedTrials::builds(cx)),
            [build_id(TRIAL)]
        );
        assert_eq!(
            *notices.borrow(),
            [trial_failed_message(TRIAL, &format!("kotlin-lsp {ACTIVE}"))]
        );
        assert_eq!(
            *notices.borrow(),
            ["Kotlin: kotlin-lsp 263.2.0 failed its first start; using kotlin-lsp 263.1.0 again"]
        );
        assert_eq!(
            harness.events(),
            ["start kotlin-lsp", "stop", "start kotlin-lsp"]
        );
        assert_eq!(harness.status(cx), Some(ServerStatus::Ready));
        assert_eq!(harness.crash_count(cx), 0);
        assert!(!harness.expired(cx));

        // The restarted server is no trial: the window passes nothing, and no second notice shows.
        cx.executor().advance_clock(TRIAL_WINDOW * 2);
        cx.run_until_parked();
        assert_eq!(harness.builds().active, Some(build_id(ACTIVE)));
        assert_eq!(notices.borrow().len(), 1);
        assert!(harness.log().contains("trial: 263.2.0 failed: "));
    }

    #[gpui::test]
    fn test_an_exit_two_seconds_after_start_rejects_the_trial(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.seed_trial();
        let notices = harness.trial_notices(cx);
        let server_id = harness.trial_start(true, cx);
        cx.executor().advance_clock(Duration::from_secs(2));
        harness.manager.update(cx, |manager, cx| {
            manager.server_terminated(server_id, exited_with(1), cx)
        });
        assert_rejected(&harness, &notices, cx);
    }

    #[gpui::test]
    fn test_a_failed_start_of_the_launched_trial_rejects_it(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.seed_trial();
        let notices = harness.trial_notices(cx);
        harness.trial_start(false, cx);
        harness.manager.update(cx, |manager, cx| {
            manager.server_start_failed(worktree(), "initialize timed out", cx)
        });
        // The same failure reported again is not a second rejection.
        harness.manager.update(cx, |manager, cx| {
            manager.server_start_failed(worktree(), "initialize timed out", cx)
        });
        assert_rejected(&harness, &notices, cx);
    }

    #[gpui::test]
    fn test_an_expired_exit_within_the_window_rejects_the_trial(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.seed_trial();
        let notices = harness.trial_notices(cx);
        let server_id = harness.trial_start(true, cx);
        cx.executor().advance_clock(Duration::from_secs(30));
        harness.manager.update(cx, |manager, cx| {
            manager.server_terminated(server_id, exited_with(EXPIRED_EXIT_CODE), cx)
        });
        assert_rejected(&harness, &notices, cx);
    }

    #[gpui::test]
    fn test_an_expired_message_while_the_trial_starts_rejects_it_once(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.seed_trial();
        let notices = harness.trial_notices(cx);
        harness.trial_start(false, cx);
        harness.manager.update(cx, |manager, cx| {
            manager.startup_stderr(worktree(), "This build of kotlin-server has expired.\n", cx);
            // The start failure that follows the message belongs to the rejected trial.
            manager.server_start_failed(worktree(), "server exited", cx);
        });
        assert_rejected(&harness, &notices, cx);
    }

    #[gpui::test]
    fn test_a_restart_within_the_window_does_not_reject_the_trial(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.seed_trial();
        let notices = harness.trial_notices(cx);
        let server_id = harness.trial_start(true, cx);
        cx.executor().advance_clock(Duration::from_secs(10));
        // `kotlin: restart server`: the store stops the server and reports it removed.
        harness
            .manager
            .update(cx, |manager, cx| manager.server_removed(server_id, cx));
        cx.executor().advance_clock(TRIAL_WINDOW);
        cx.run_until_parked();
        assert_eq!(harness.launched_trial(cx), None);
        // Neither rejected nor passed: the trial stays pending for the next start.
        assert_eq!(harness.builds().trial, Some(build_id(TRIAL)));
        assert_eq!(harness.builds().active, Some(build_id(ACTIVE)));
        assert!(harness.builds().rejected.is_empty());
        assert!(cx.update(|cx| RejectedTrials::builds(cx)).is_empty());
        assert!(notices.borrow().is_empty());
        assert!(harness.log().contains("still pending"));
    }

    /// The crash-restart policy is bypassed only while a launched trial is in its window: a start
    /// without a launched trial crashes and restarts once, as before.
    #[gpui::test]
    fn test_only_a_launched_trial_skips_the_crash_policy(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.seed_trial();
        let notices = harness.trial_notices(cx);
        harness.first_start(cx);
        harness.manager.update(cx, |manager, cx| {
            manager.server_terminated(LanguageServerId(1), exited_with(1), cx)
        });
        cx.run_until_parked();
        assert_eq!(harness.crash_count(cx), 1);
        assert!(notices.borrow().is_empty());
        assert!(harness.builds().rejected.is_empty());
        assert_eq!(harness.builds().trial, Some(build_id(TRIAL)));
    }

    /// A newer build downloaded: the worktree stopped by an expired build forgets the mark and
    /// restarts; the worktree running a build that has not expired is left alone.
    #[gpui::test]
    async fn test_a_newer_build_restarts_only_the_expired_worktree(cx: &mut TestAppContext) {
        let harness = setup(cx);
        let second = WorktreeId::from_usize(2);
        let second_root = harness.root.parent().unwrap().join("project2");
        fs::create_dir_all(&second_root).unwrap();
        harness.manager.update(cx, |manager, cx| {
            manager.add_worktree(second, second_root, cx)
        });
        harness.first_start(cx);
        let server = harness
            .manager
            .update(cx, |manager, cx| manager.begin_start(second, cx))
            .unwrap();
        let server_id = harness.host.0.record_start(server);
        harness.manager.update(cx, |manager, cx| {
            manager.server_added(server_id, second, cx);
            manager.mark_expired(second, KotlinServerKind::KotlinLsp, cx);
        });
        let expired = Some(ServerStatus::Failed {
            reason: EXPIRED_REASON.into(),
        });
        let status_of = |cx: &mut TestAppContext, id| {
            harness
                .manager
                .read_with(cx, |manager, _| manager.status(id).cloned())
        };
        assert_eq!(status_of(cx, second), expired);
        assert_eq!(harness.events(), ["start kotlin-lsp", "start kotlin-lsp"]);

        harness
            .manager
            .update(cx, |manager, cx| manager.newer_build_ready("9.1.0", cx));
        cx.run_until_parked();

        assert_eq!(
            harness.events(),
            [
                "start kotlin-lsp",
                "start kotlin-lsp",
                "stop",
                "start kotlin-lsp"
            ],
            "only the expired worktree restarted"
        );
        assert_eq!(status_of(cx, worktree()), Some(ServerStatus::Ready));
        assert_eq!(status_of(cx, second), Some(ServerStatus::Ready));
        harness.manager.read_with(cx, |manager, _| {
            assert!(!manager.is_expired(second, KotlinServerKind::KotlinLsp));
        });
    }

    /// A user restart during a trial start ends the launched mark, so the trial is not blamed
    /// for what the restart does to its server.
    #[gpui::test]
    async fn test_a_restart_forgets_a_launched_trial(cx: &mut TestAppContext) {
        let harness = setup(cx);
        harness.first_start(cx);
        let build = BuildId {
            version: "9.1.0".into(),
            sha256: "b".repeat(64),
        };
        harness.manager.update(cx, |manager, cx| {
            manager.trial_launched(
                worktree(),
                TrialLaunch {
                    build: build.clone(),
                    fallback: "the previous build".into(),
                    pin_version: "9.0.0".into(),
                },
                cx,
            )
        });
        assert_eq!(
            harness
                .manager
                .read_with(cx, |manager, _| manager.launched_trial(worktree()).cloned()),
            Some(build)
        );
        harness
            .manager
            .update(cx, |manager, cx| manager.restart_server(worktree(), cx));
        assert_eq!(
            harness
                .manager
                .read_with(cx, |manager, _| manager.launched_trial(worktree()).cloned()),
            None
        );
        cx.run_until_parked();
    }
}
