//! The background check for a newer signed Kotlin server manifest.
//!
//! `KotlinUpdates` is one GPUI global entity. While `kotlin.manifest_url` is empty it makes no
//! request and shows nothing. Otherwise it checks `STARTUP_DELAY` after startup, then wakes every
//! `WAKE_INTERVAL` (a wake-up after a suspend still comes), and fetches only when the last check of
//! any bongos process on this data dir is older than `MAX_AGE`. A change of the setting checks at
//! once, skipping the age test. A check never shows a popup, a notification or a status change:
//! it writes one `kotlin.log` line (`log.rs`).
//!
//! A check runs the three phases `manifest_cache.rs` describes: claim a lease under the state lock,
//! fetch the manifest and its `.sig` with no lock held (both together capped at `FETCH_TIMEOUT`),
//! then store a verified higher serial, record the outcome and clear the lease. Every exit after a
//! claim goes through phase 3, so the lease is cleared on errors and timeouts too.
//!
//! The setting is read only from the global settings (`KotlinSettings::get_global`), never with a
//! worktree location: a project's `.bongos/settings.json` cannot point bongos at another server.
//!
//! Once a kotlin-lsp start was requested in this run (`KotlinUpdates::start_requested`), each check
//! is followed by a background download of the newest build in the cached manifest that is newer
//! than the pin, the active build and the trial, not rejected and not installed. One download runs
//! at a time, through `spawn_ensure_installed_with` in `InstallMode::Background`: it holds only that
//! version's lock, tried once (a busy lock skips the download until the next check), so a start of
//! another build never waits on it. The install leaves a pending-trial marker; once the version lock
//! is released the build is recorded as the trial under the state lock and the marker removed.
//! Without a verified cached manifest (an empty URL, or no trusted key) nothing downloads.
//!
//! At startup, before this process starts a server, `prune` deletes the kotlin-lsp builds nothing
//! refers to.

use std::{
    cmp::Ordering as VersionOrder,
    fs,
    path::{Path, PathBuf},
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use futures::{
    AsyncReadExt as _, StreamExt as _,
    future::{Either, select},
};
use gpui::{App, AppContext as _, BackgroundExecutor, Context, Entity, EventEmitter, Global, Task};
use http_client::{AsyncBody, HttpClient, Url, http::header::CONTENT_LENGTH};
use settings::{KotlinServerKind, Settings as _, SettingsStore};

use crate::{
    KotlinSettings,
    install::{
        InstallCancelled, InstallMode, VersionBusy, pending_marker_path, prune,
        spawn_ensure_installed_with,
    },
    installed::{check_installed, server_name},
    locks::LockRegistry,
    log::KotlinLog,
    manifest::{Manifest, ManifestEntry, current_platform},
    manifest_cache::{
        BuildId, Claim, FetchOutcome, FetchedManifest, KotlinLspBuilds, ManifestCache,
        read_for_start,
    },
    remote_manifest::{
        MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES, TrustedKey, compare_versions, sig_url,
        trusted_keys,
    },
};

/// How long after startup the first check runs.
pub const STARTUP_DELAY: Duration = Duration::from_secs(5);
/// How old the last check may be before a wake-up fetches again.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the background timer wakes to test the age of the last check.
pub const WAKE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// The cap on both requests of one check together.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The check's timings. Tests shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateTimings {
    pub startup_delay: Duration,
    pub max_age: Duration,
    pub wake_interval: Duration,
    pub fetch_timeout: Duration,
}

impl Default for UpdateTimings {
    fn default() -> Self {
        Self {
            startup_delay: STARTUP_DELAY,
            max_age: MAX_AGE,
            wake_interval: WAKE_INTERVAL,
            fetch_timeout: FETCH_TIMEOUT,
        }
    }
}

/// The wall clock `last_check` and the lease are measured in. Tests derive it from fake time.
pub type WallClock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// What `KotlinUpdates` needs. `KotlinUpdates::init` builds the app's.
pub struct UpdateSetup {
    pub install_root: PathBuf,
    pub logs_dir: PathBuf,
    pub http: Arc<dyn HttpClient>,
    pub locks: Arc<LockRegistry>,
    pub keys: Vec<TrustedKey>,
    pub clock: WallClock,
    pub timings: UpdateTimings,
    /// This machine's platform key, `None` where no Kotlin server runs.
    pub platform: Option<String>,
    /// The embedded kotlin-lsp pin for `platform`.
    pub pin: Option<ManifestEntry>,
}

/// What one check did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// `kotlin.manifest_url` is empty or refused; nothing was requested.
    Off,
    /// The last check is recent; nothing was requested.
    Fresh,
    /// Another check, in this process or another, holds the fetch lease.
    Busy,
    /// A fetch ran; the outcome is in `kotlin.log` and `state.json`.
    Finished(FetchOutcome),
    /// The state file could not be read or written, so nothing was fetched or stored.
    Skipped(String),
}

impl Refresh {
    /// One line for whoever asked for the check.
    pub fn describe(&self) -> String {
        match self {
            Self::Off => "kotlin.manifest_url is empty".into(),
            Self::Fresh => "the last check is recent".into(),
            Self::Busy => "a check is already running".into(),
            Self::Finished(outcome) => outcome.describe(),
            Self::Skipped(reason) => format!("the check was skipped: {reason}"),
        }
    }
}

/// How far the background download of a newer build is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    pub version: String,
    pub done: u64,
    pub total: Option<u64>,
}

/// What `KotlinUpdates` tells windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KotlinUpdatesEvent {
    /// A background download of kotlin-lsp `version` began.
    DownloadStarted { version: String },
}

/// The background download in flight: its progress and the flag that stops it.
struct ActiveDownload {
    progress: DownloadProgress,
    cancel: Arc<AtomicBool>,
}

struct GlobalKotlinUpdates(Entity<KotlinUpdates>);

impl Global for GlobalKotlinUpdates {}

/// The background manifest check of this process.
pub struct KotlinUpdates {
    checker: Arc<Checker>,
    /// The setting as last read, to see a change.
    raw_url: String,
    /// The URL to fetch, `None` while the setting is empty or refused.
    url: Option<Url>,
    /// Whether the "off" line was written since the setting last changed.
    off_logged: bool,
    /// A check of this entity is in flight.
    running: bool,
    /// A setting change found a check in flight: check again, forced, when a check can run.
    force_pending: bool,
    /// A kotlin-lsp start was requested in this run; only then does a build download.
    start_requested: Arc<AtomicBool>,
    /// A background download (or its planning) is in flight.
    downloading: bool,
    /// The download itself, once planned; cleared when it ends.
    download: Option<ActiveDownload>,
    _settings: gpui::Subscription,
    _wake: Task<()>,
}

/// The parts of a check that run on the background executor.
struct Checker {
    cache: ManifestCache,
    http: Arc<dyn HttpClient>,
    log: KotlinLog,
    keys: Vec<TrustedKey>,
    clock: WallClock,
    executor: BackgroundExecutor,
    max_age: Duration,
    fetch_timeout: Duration,
    install_root: PathBuf,
    locks: Arc<LockRegistry>,
    platform: Option<String>,
    pin: Option<ManifestEntry>,
}

/// What a background download pass does.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DownloadPlan {
    Nothing,
    /// Download and install this build.
    Download(ManifestEntry),
    /// This build is installed with a pending-trial marker (its recording failed before): record it.
    Record(ManifestEntry),
}

impl EventEmitter<KotlinUpdatesEvent> for KotlinUpdates {}

impl KotlinUpdates {
    /// Creates the app's check: the app's HTTP client, the release keys (plus the debug-only key
    /// override), the process-wide lock registry the installer shares, and the embedded pin.
    pub fn init(install_root: &Path, logs_dir: &Path, manifest: &Manifest, cx: &mut App) {
        let platform = current_platform();
        let setup = UpdateSetup {
            install_root: install_root.to_path_buf(),
            logs_dir: logs_dir.to_path_buf(),
            http: cx.http_client(),
            locks: LockRegistry::global(cx),
            keys: trusted_keys(true),
            clock: Arc::new(Utc::now),
            timings: UpdateTimings::default(),
            platform: platform.map(str::to_string),
            pin: platform
                .and_then(|platform| manifest.entry(KotlinServerKind::KotlinLsp, platform).ok()),
        };
        let updates = cx.new(|cx| Self::new(setup, cx));
        cx.set_global(GlobalKotlinUpdates(updates));
    }

    /// The app's check, once `init` ran.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalKotlinUpdates>()
            .map(|global| global.0.clone())
    }

    pub fn new(setup: UpdateSetup, cx: &mut Context<Self>) -> Self {
        let settings = KotlinSettings::get_global(cx);
        let raw_url = settings.manifest_url.clone();
        let url = settings.manifest_url_for_fetch();
        let checker = Arc::new(Checker {
            cache: ManifestCache::new(&setup.install_root, setup.locks.clone()),
            http: setup.http,
            log: KotlinLog::new(&setup.logs_dir),
            keys: setup.keys,
            clock: setup.clock,
            executor: cx.background_executor().clone(),
            max_age: setup.timings.max_age,
            fetch_timeout: setup.timings.fetch_timeout,
            install_root: setup.install_root,
            locks: setup.locks,
            platform: setup.platform,
            pin: setup.pin,
        });
        let start_requested = Arc::new(AtomicBool::new(false));
        // The startup pass: prune unused kotlin-lsp builds, unless a start came first.
        cx.background_spawn({
            let checker = checker.clone();
            let start_requested = start_requested.clone();
            async move {
                if !start_requested.load(Ordering::SeqCst) {
                    checker.prune().await;
                }
            }
        })
        .detach();
        let timings = setup.timings;
        let wake = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(timings.startup_delay).await;
            loop {
                let woke = this.update(cx, |this, cx| this.start_check(false, false, cx));
                let Ok(check) = woke else {
                    break;
                };
                drop(check);
                cx.background_executor().timer(timings.wake_interval).await;
            }
        });
        Self {
            checker,
            raw_url,
            url,
            off_logged: false,
            running: false,
            force_pending: false,
            start_requested,
            downloading: false,
            download: None,
            _settings: cx.observe_global::<SettingsStore>(|this, cx| this.settings_changed(cx)),
            _wake: wake,
        }
    }

    /// Checks now, skipping the age test. A live lease (a check in this or another process)
    /// gives `Refresh::Busy` ("a check is already running"). Shows nothing; the caller reports.
    pub fn refresh(&mut self, cx: &mut Context<Self>) -> Task<Refresh> {
        self.start_check(true, false, cx)
    }

    /// Notes that a kotlin-lsp start was requested in this run, which allows background downloads,
    /// and looks for one now. Does nothing before `init`.
    pub fn start_requested(cx: &mut App) {
        if let Some(updates) = Self::global(cx) {
            updates.update(cx, |updates, cx| updates.note_start(cx));
        }
    }

    /// The progress of the background download, `None` while none runs.
    pub fn download_progress(&self) -> Option<&DownloadProgress> {
        self.download.as_ref().map(|download| &download.progress)
    }

    /// Cancels the background download. It stops at its next chunk, removes its partial files and
    /// releases the version lock; nothing is recorded, so the next check offers the build again.
    /// A running server is untouched. Returns whether a download was running.
    pub fn cancel_download(&mut self, _cx: &mut Context<Self>) -> bool {
        let Some(download) = &self.download else {
            return false;
        };
        download.cancel.store(true, Ordering::SeqCst);
        self.checker.note(&format!(
            "download: kotlin-lsp {} cancel requested",
            download.progress.version
        ));
        true
    }

    /// `start_requested` for this entity.
    pub fn note_start(&mut self, cx: &mut Context<Self>) {
        self.start_requested.store(true, Ordering::SeqCst);
        self.maybe_download(cx);
    }

    /// Starts a background download pass if a start was requested, a manifest URL is set and no
    /// pass is in flight.
    fn maybe_download(&mut self, cx: &mut Context<Self>) {
        if !self.start_requested.load(Ordering::SeqCst) || self.downloading {
            return;
        }
        let Some(url) = self.url.clone() else {
            return;
        };
        self.downloading = true;
        let checker = self.checker.clone();
        cx.spawn(async move |this, cx| {
            let plan = cx
                .background_spawn({
                    let checker = checker.clone();
                    async move { checker.plan_download(&url) }
                })
                .await;
            match plan {
                DownloadPlan::Nothing => {}
                DownloadPlan::Record(entry) => {
                    cx.background_spawn(async move { checker.record_trial(&entry).await })
                        .await
                }
                DownloadPlan::Download(entry) => {
                    let version = entry.version.clone();
                    checker.note(&format!("download: kotlin-lsp {version} started"));
                    let cancel = Arc::new(AtomicBool::new(false));
                    let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded();
                    let forwarder = this.update(cx, |this, cx| {
                        this.download = Some(ActiveDownload {
                            progress: DownloadProgress {
                                version: version.clone(),
                                done: 0,
                                total: None,
                            },
                            cancel: cancel.clone(),
                        });
                        cx.emit(KotlinUpdatesEvent::DownloadStarted {
                            version: version.clone(),
                        });
                        cx.notify();
                        cx.spawn(async move |this, cx| {
                            while let Some((done, total)) = progress_rx.next().await {
                                let updated = this.update(cx, |this, cx| {
                                    if let Some(download) = &mut this.download {
                                        download.progress.done = done;
                                        download.progress.total = total;
                                        cx.notify();
                                    }
                                });
                                if updated.is_err() {
                                    break;
                                }
                            }
                        })
                    })
                    .ok();
                    let install = cx.update(|cx| {
                        spawn_ensure_installed_with(
                            InstallMode::Background,
                            checker.locks.clone(),
                            KotlinServerKind::KotlinLsp,
                            entry.clone(),
                            checker.server_dir(),
                            checker.http.clone(),
                            Arc::new(move |done, total| {
                                progress_tx.unbounded_send((done, total)).ok();
                            }),
                            cancel,
                            cx,
                        )
                    });
                    // The version lock is released when the install task returns.
                    let result = install.await;
                    // The progress sender went with the install: show what it reported last.
                    if let Some(forwarder) = forwarder {
                        forwarder.await;
                    }
                    cx.background_spawn(async move {
                        match result {
                            Ok(_) => checker.record_trial(&entry).await,
                            Err(error) if error.downcast_ref::<InstallCancelled>().is_some() => {
                                checker.note(&format!("download: kotlin-lsp {version} cancelled"))
                            }
                            Err(error) if error.downcast_ref::<VersionBusy>().is_some() => checker
                                .note(&format!(
                                    "download: kotlin-lsp {version} skipped: another install holds it"
                                )),
                            Err(error) => checker.note(&format!(
                                "download: kotlin-lsp {version} failed: {error:#}"
                            )),
                        }
                    })
                    .await
                }
            }
            this.update(cx, |this, cx| {
                this.downloading = false;
                this.download = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let settings = KotlinSettings::get_global(cx);
        if settings.manifest_url == self.raw_url {
            return;
        }
        self.raw_url = settings.manifest_url.clone();
        self.url = settings.manifest_url_for_fetch();
        self.off_logged = false;
        self.start_check(true, true, cx).detach();
    }

    /// Starts one check. The returned task only reports; dropping it does not stop the check, so
    /// its lease is always cleared.
    fn start_check(
        &mut self,
        force: bool,
        url_changed: bool,
        cx: &mut Context<Self>,
    ) -> Task<Refresh> {
        let Some(url) = self.url.clone() else {
            if force || !self.off_logged {
                if self.raw_url.trim().is_empty() {
                    self.checker.log.manifest_off();
                } else {
                    self.checker.log.manifest_refused();
                }
                self.off_logged = true;
            }
            return Task::ready(Refresh::Off);
        };
        if self.running {
            if url_changed {
                self.force_pending = true;
            }
            return Task::ready(Refresh::Busy);
        }
        // Take the pending flag first: `force || take(..)` would leave it set when `force` is true.
        let pending = std::mem::take(&mut self.force_pending);
        let force = force || pending;
        self.running = true;
        let checker = self.checker.clone();
        let (sender, receiver) = futures::channel::oneshot::channel();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { checker.check(url, force).await })
                .await;
            this.update(cx, |this, cx| {
                this.running = false;
                this.maybe_download(cx);
                if url_changed && result == Refresh::Busy {
                    // Another process holds the lease: the next wake-up checks the new URL.
                    this.force_pending = true;
                } else if this.force_pending {
                    // The setting changed while this check ran: check the new URL now.
                    this.start_check(true, true, cx).detach();
                }
            })
            .ok();
            sender.send(result).ok();
        })
        .detach();
        cx.background_spawn(async move {
            receiver
                .await
                .unwrap_or_else(|_| Refresh::Skipped("the check stopped".into()))
        })
    }
}

impl Checker {
    fn server_dir(&self) -> PathBuf {
        self.install_root
            .join(server_name(KotlinServerKind::KotlinLsp))
    }

    /// One line in `kotlin.log` and the app log.
    fn note(&self, text: &str) {
        ::log::info!("{text}");
        self.log.line(text);
    }

    /// The startup prune of unused kotlin-lsp builds.
    async fn prune(&self) {
        let pin = self.pin.as_ref().map(|pin| pin.version.as_str());
        match prune(
            &self.cache,
            &self.locks,
            &self.server_dir(),
            pin,
            SystemTime::now(),
        )
        .await
        {
            Ok(pruned) => {
                for version in &pruned.removed {
                    self.note(&format!("prune: removed kotlin-lsp {version}"));
                }
                for version in &pruned.busy {
                    ::log::info!("prune: kept kotlin-lsp {version}: its lock is busy");
                }
            }
            Err(error) => ::log::warn!("prune: skipped: {error:#}"),
        }
    }

    /// The build a background download pass handles, from the cached manifest and `state.json`
    /// read as files.
    fn plan_download(&self, url: &Url) -> DownloadPlan {
        let (Some(platform), Some(pin)) = (&self.platform, &self.pin) else {
            return DownloadPlan::Nothing;
        };
        let (cached, state) = read_for_start(&self.install_root, url, &self.keys);
        let Some(cached) = cached else {
            return DownloadPlan::Nothing;
        };
        let builds = &state.kotlin_lsp;
        let mut best = pin.version.as_str();
        for build in [&builds.active, &builds.trial].into_iter().flatten() {
            if compare_versions(&build.version, best) == Some(VersionOrder::Greater) {
                best = &build.version;
            }
        }
        let today = (self.clock)().date_naive();
        let rejected = |entry: &ManifestEntry| {
            builds.rejected.iter().any(|id| {
                id.version == entry.version && id.sha256.eq_ignore_ascii_case(&entry.sha256)
            })
        };
        let newest = cached
            .manifest
            .kotlin_lsp
            .iter()
            .filter_map(|build| build.entry(platform))
            .filter(|entry| {
                compare_versions(&entry.version, best) == Some(VersionOrder::Greater)
                    && !entry.is_expired(today)
                    && !rejected(entry)
            })
            .max_by(|a, b| compare_versions(&a.version, &b.version).unwrap_or(VersionOrder::Equal));
        let Some(entry) = newest else {
            return DownloadPlan::Nothing;
        };
        if check_installed(&self.install_root, KotlinServerKind::KotlinLsp, entry).is_none() {
            DownloadPlan::Download(entry.clone())
        } else if pending_marker_path(&self.server_dir(), &entry.version).exists() {
            DownloadPlan::Record(entry.clone())
        } else {
            DownloadPlan::Nothing
        }
    }

    /// Records an installed build with a pending-trial marker as the trial, then removes the
    /// marker. Called with no version lock held; takes the state lock only for the write. Under
    /// that lock the build is recorded only when it is not rejected and is newer than the active
    /// build and the trial, so a process planning from an older manifest never replaces a newer
    /// trial. A build not recorded loses its marker too, so the startup prune can delete it.
    async fn record_trial(&self, entry: &ManifestEntry) {
        let marker = pending_marker_path(&self.server_dir(), &entry.version);
        if !marker.exists() {
            // Another install of this build finished first; its process records it.
            return;
        }
        let trial = BuildId {
            version: entry.version.clone(),
            sha256: entry.sha256.clone(),
        };
        let version = &entry.version;
        match self
            .cache
            .with_state(|state| record_if_newer(&mut state.kotlin_lsp, trial))
            .await
        {
            Ok(outcome) => {
                if let Err(error) = fs::remove_file(&marker)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    ::log::warn!("removing {}: {error}", marker.display());
                }
                self.note(&match outcome {
                    TrialRecord::Recorded => {
                        format!("download: kotlin-lsp {version} installed as the trial")
                    }
                    TrialRecord::AlreadyRecorded => {
                        format!("download: kotlin-lsp {version} is already recorded")
                    }
                    TrialRecord::Rejected => {
                        format!("download: kotlin-lsp {version} not recorded: it was rejected")
                    }
                    TrialRecord::NotNewer(other) => format!(
                        "download: kotlin-lsp {version} not recorded: {other} is as new or newer"
                    ),
                });
            }
            Err(error) => self.note(&format!(
                "download: kotlin-lsp {version} is installed but not recorded yet: {error:#}"
            )),
        }
    }

    async fn check(&self, url: Url, force: bool) -> Refresh {
        let host = url.host_str().unwrap_or_default().to_string();
        // Phase 1: claim the fetch under the state lock.
        let lease = match self
            .cache
            .claim_fetch((self.clock)(), self.max_age, force)
            .await
        {
            Ok(Claim::Claimed(lease)) => lease,
            Ok(Claim::Fresh) => return Refresh::Fresh,
            Ok(Claim::Busy) => return Refresh::Busy,
            Err(error) => {
                let reason = format!("{error:#}");
                self.log.manifest_skipped(&host, &reason);
                return Refresh::Skipped(reason);
            }
        };
        // Phase 2: fetch with no lock held.
        let fetched = self.fetch(&url).await;
        // Phase 3: store, record and clear the lease.
        let finished = (self.clock)();
        let outcome = match self
            .cache
            .finish_fetch(lease, finished, fetched, &url, &self.keys)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                let outcome = FetchOutcome::Failed {
                    reason: format!("storing the manifest: {error:#}"),
                };
                // Record the failure and give the lease back; if even that fails, the lease runs
                // out on its own within `FETCH_LEASE`.
                let line = outcome.describe();
                let released = self
                    .cache
                    .with_state(|state| {
                        if state.fetch_lease == Some(lease.until) {
                            state.fetch_lease = None;
                        }
                        state.last_check = Some(finished);
                        state.last_outcome = Some(line);
                    })
                    .await;
                if let Err(error) = released {
                    ::log::warn!("manifest: the fetch lease stays until it runs out: {error:#}");
                }
                outcome
            }
        };
        self.log.manifest_outcome(&host, &outcome);
        Refresh::Finished(outcome)
    }

    /// The manifest and its signature, both requests together capped at `fetch_timeout`.
    async fn fetch(&self, url: &Url) -> Result<FetchedManifest> {
        let fetch = pin!(fetch_pair(self.http.as_ref(), url));
        let timer = self.executor.timer(self.fetch_timeout);
        match select(fetch, timer).await {
            Either::Left((fetched, _)) => fetched,
            Either::Right(((), _)) => Err(anyhow!(
                "timed out after {} s",
                self.fetch_timeout.as_secs()
            )),
        }
    }
}

/// What `record_if_newer` did with a downloaded build.
#[derive(Debug, PartialEq, Eq)]
enum TrialRecord {
    Recorded,
    /// The build already is the trial or the active build.
    AlreadyRecorded,
    Rejected,
    /// The active build or the trial, this version, is as new or newer.
    NotNewer(String),
}

/// Makes `trial` the trial of `builds` when it is not rejected and is newer than the active build
/// and the current trial. Runs under the state lock.
fn record_if_newer(builds: &mut KotlinLspBuilds, trial: BuildId) -> TrialRecord {
    let same =
        |id: &BuildId| id.version == trial.version && id.sha256.eq_ignore_ascii_case(&trial.sha256);
    if builds.rejected.iter().any(&same) {
        return TrialRecord::Rejected;
    }
    if [&builds.active, &builds.trial]
        .into_iter()
        .flatten()
        .any(&same)
    {
        return TrialRecord::AlreadyRecorded;
    }
    for build in [&builds.active, &builds.trial].into_iter().flatten() {
        if compare_versions(&trial.version, &build.version) != Some(VersionOrder::Greater) {
            return TrialRecord::NotNewer(build.version.clone());
        }
    }
    builds.trial = Some(trial);
    TrialRecord::Recorded
}

/// GET the manifest, then GET its `.sig`, following redirects. Nothing is added to the requests:
/// no query, no cookie, no conditional or identifying header beyond what the client sends anyway.
async fn fetch_pair(http: &dyn HttpClient, url: &Url) -> Result<FetchedManifest> {
    let servers = get_capped(http, url, MAX_MANIFEST_BYTES, "manifest").await?;
    let sig = get_capped(http, &sig_url(url), MAX_SIGNATURE_BYTES, "signature").await?;
    Ok(FetchedManifest { servers, sig })
}

/// The body at `url`, refused when the server says or sends more than `cap` bytes.
async fn get_capped(http: &dyn HttpClient, url: &Url, cap: usize, what: &str) -> Result<Vec<u8>> {
    let response = http
        .get(url.as_str(), AsyncBody::default(), true)
        .await
        .with_context(|| format!("fetching the {what}"))?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "fetching the {what}: the server answered {status}"
    );
    let length = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(length) = length {
        ensure!(
            length <= cap as u64,
            "the {what} is {length} bytes, more than {cap}"
        );
    }
    let mut bytes = Vec::new();
    response
        .into_body()
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("reading the {what}"))?;
    ensure!(bytes.len() <= cap, "the {what} is more than {cap} bytes");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        install::{
            ensure_installed,
            tests::{entry_for, files, sha256, tar_gz},
        },
        locks::tests::thread_registry,
        manifest::ArchiveKind,
        manifest_cache::tests::{key_pair, now, release_keys, servers, signed, signed_with},
    };
    use collections::HashMap;
    use futures::{FutureExt as _, executor::block_on};
    use gpui::{BorrowAppContext as _, TestAppContext};
    use http_client::{
        FakeHttpClient, Response,
        http::{HeaderMap, Method},
    };
    use settings::{LocalSettingsKind, LocalSettingsPath, WorktreeId};
    use std::{fs, io, pin::Pin, sync::Mutex, task::Poll};
    use util::rel_path::RelPath;

    const HOUR: Duration = Duration::from_secs(60 * 60);
    const PLATFORM: &str = "linux-x64";
    const PIN_VERSION: &str = "262.1.0";
    const A: &str = "https://a.example.com/kotlin/servers.json";
    const B: &str = "https://b.example.com/kotlin/servers.json";

    #[derive(Clone)]
    enum Route {
        Body(Vec<u8>),
        Offline,
        /// Headers arrive, the body never ends.
        Stall,
        /// The first bytes arrive, then the body never ends.
        StallAfter(Vec<u8>, usize),
    }

    #[derive(Clone, Debug)]
    struct Seen {
        method: Method,
        url: String,
        query: Option<String>,
        headers: HeaderMap,
    }

    #[derive(Default)]
    struct Server {
        routes: Mutex<HashMap<String, Route>>,
        seen: Mutex<Vec<Seen>>,
    }

    impl Server {
        fn serve(&self, url: &str, route: Route) {
            self.routes.lock().unwrap().insert(url.to_string(), route);
        }

        fn serve_pair(&self, url: &str, fetched: FetchedManifest) {
            self.serve(url, Route::Body(fetched.servers));
            self.serve(&format!("{url}.sig"), Route::Body(fetched.sig));
        }

        fn urls(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|seen| seen.url.clone())
                .collect()
        }

        fn client(self: &Arc<Self>) -> Arc<dyn HttpClient> {
            let server = self.clone();
            FakeHttpClient::create(move |request| {
                let url = request.uri().to_string();
                server.seen.lock().unwrap().push(Seen {
                    method: request.method().clone(),
                    url: url.clone(),
                    query: request.uri().query().map(str::to_string),
                    headers: request.headers().clone(),
                });
                let route = server.routes.lock().unwrap().get(&url).cloned();
                async move {
                    match route {
                        Some(Route::Body(bytes)) => Ok(Response::builder()
                            .status(200)
                            .body(AsyncBody::from(bytes))
                            .unwrap()),
                        Some(Route::Stall) => Ok(Response::builder()
                            .status(200)
                            .body(AsyncBody::from_reader(Endless))
                            .unwrap()),
                        Some(Route::StallAfter(bytes, sent)) => Ok(Response::builder()
                            .status(200)
                            .body(AsyncBody::from_reader(futures::AsyncReadExt::chain(
                                futures::io::Cursor::new(bytes[..sent].to_vec()),
                                Endless,
                            )))
                            .unwrap()),
                        Some(Route::Offline) => Err(anyhow!("network is unreachable")),
                        None => Ok(Response::builder()
                            .status(404)
                            .body(AsyncBody::default())
                            .unwrap()),
                    }
                }
            })
        }
    }

    /// A body that never sends a byte and never ends.
    struct Endless;

    impl futures::AsyncRead for Endless {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
    }

    struct Harness {
        root: tempfile::TempDir,
        server: Arc<Server>,
        start: std::time::Instant,
        /// The archive every test build is served as.
        archive: Vec<u8>,
    }

    impl Harness {
        fn new(cx: &mut TestAppContext) -> Self {
            cx.update(|cx| {
                let store = SettingsStore::new(cx, &settings::default_settings());
                cx.set_global(store);
                KotlinSettings::register(cx);
            });
            Self {
                root: tempfile::tempdir().unwrap(),
                server: Arc::default(),
                start: cx.executor().now(),
                archive: tar_gz(&files(256 * 1024, true)),
            }
        }

        /// The embedded kotlin-lsp pin of the test process.
        fn pin(&self) -> ManifestEntry {
            ManifestEntry {
                version: PIN_VERSION.into(),
                url: "https://download.jetbrains.com/pin/kotlin-server.tar.gz".into(),
                ..entry_for(&self.archive, ArchiveKind::TarGz)
            }
        }

        /// The manifest entry of `version`, as the manifest at `url` gives it.
        fn build(&self, url: &str, version: &str) -> ManifestEntry {
            let cached = self
                .cache()
                .load(&Url::parse(url).unwrap(), &release_keys());
            cached
                .unwrap()
                .manifest
                .kotlin_lsp
                .iter()
                .find(|build| build.version == version)
                .and_then(|build| build.entry(PLATFORM))
                .unwrap()
                .clone()
        }

        /// Serves a signed manifest at `url` listing `versions`, each an archive of `self.archive`,
        /// and serves each archive.
        fn serve_builds(&self, url: &str, serial: u64, versions: &[&str]) {
            let digest = sha256(&self.archive);
            let builds: Vec<serde_json::Value> = versions
                .iter()
                .map(|version| {
                    serde_json::json!({
                        "version": version, "built": "2026-11-10", "expires": "2026-12-09",
                        "requires": 1,
                        "platform": { PLATFORM: {
                            "url": archive_url(version), "sha256": digest, "archive": "tar.gz",
                            "strip_components": 1, "launcher": "bin/launcher"
                        } }
                    })
                })
                .collect();
            let servers = serde_json::to_vec(&serde_json::json!({
                "schema": 1, "kind": "bongos-kotlin-servers", "serial": serial,
                "published": "2026-11-11", "kotlin-lsp": builds
            }))
            .unwrap();
            self.server
                .serve_pair(url, signed_with(servers, "release-1", &key_pair(7)));
            for version in versions {
                self.server
                    .serve(&archive_url(version), Route::Body(self.archive.clone()));
            }
            self.server
                .serve(&self.pin().url, Route::Body(self.archive.clone()));
        }

        fn server_dir(&self) -> PathBuf {
            self.install_root().join("kotlin-lsp")
        }

        fn archive_requests(&self) -> usize {
            self.server
                .urls()
                .iter()
                .filter(|url| url.ends_with(".tar.gz"))
                .count()
        }

        fn download_lines(&self) -> Vec<String> {
            let text =
                fs::read_to_string(self.root.path().join("logs/kotlin.log")).unwrap_or_default();
            text.lines()
                .filter_map(|line| line.split_once(' ').map(|(_, rest)| rest.to_string()))
                .filter(|line| line.starts_with("download:"))
                .collect()
        }

        fn install_root(&self) -> PathBuf {
            self.root.path().join("kotlin")
        }

        fn clock(&self, cx: &TestAppContext) -> WallClock {
            let executor = cx.executor();
            let start = self.start;
            Arc::new(move || now() + chrono::Duration::from_std(executor.now() - start).unwrap())
        }

        /// One `KotlinUpdates` with its own lock registry, like one bongos process.
        fn updates(&self, cx: &mut TestAppContext) -> Entity<KotlinUpdates> {
            self.updates_with(cx, Arc::new(LockRegistry::new(Arc::new(cx.executor()))))
        }

        fn updates_with(
            &self,
            cx: &mut TestAppContext,
            locks: Arc<LockRegistry>,
        ) -> Entity<KotlinUpdates> {
            let setup = UpdateSetup {
                install_root: self.install_root(),
                logs_dir: self.root.path().join("logs"),
                http: self.server.client(),
                locks,
                keys: release_keys(),
                clock: self.clock(cx),
                timings: UpdateTimings::default(),
                platform: Some(PLATFORM.into()),
                pin: Some(self.pin()),
            };
            cx.new(|cx| KotlinUpdates::new(setup, cx))
        }

        /// The cache seen from outside, as another process would.
        fn cache(&self) -> ManifestCache {
            ManifestCache::new(&self.install_root(), thread_registry())
        }

        fn set_last_check(&self, ago: Duration) {
            let at = now() - chrono::Duration::from_std(ago).unwrap();
            block_on(self.cache().with_state(|state| state.last_check = Some(at))).unwrap();
        }

        fn cached_serial(&self, url: &str) -> Option<u64> {
            self.cache()
                .load(&Url::parse(url).unwrap(), &release_keys())
                .map(|cached| cached.manifest.serial)
        }

        fn manifest_lines(&self) -> Vec<String> {
            let text =
                fs::read_to_string(self.root.path().join("logs/kotlin.log")).unwrap_or_default();
            text.lines()
                .filter_map(|line| line.split_once(' ').map(|(_, rest)| rest.to_string()))
                .filter(|line| line.starts_with("manifest:"))
                .collect()
        }

        fn fetches(&self) -> usize {
            self.server
                .urls()
                .iter()
                .filter(|url| !url.ends_with(".sig"))
                .count()
        }
    }

    fn set_user(cx: &mut TestAppContext, json: &str) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.set_user_settings(json, cx).unwrap();
            });
        });
    }

    fn set_user_url(cx: &mut TestAppContext, url: &str) {
        set_user(
            cx,
            &format!(r#"{{ "kotlin": {{ "manifest_url": "{url}" }} }}"#),
        );
    }

    fn advance(cx: &mut TestAppContext, by: Duration) {
        cx.executor().advance_clock(by);
        cx.run_until_parked();
    }

    /// Advances fake time an hour at a time, so every hourly wake-up runs.
    fn advance_hours(cx: &mut TestAppContext, hours: u32) {
        for _ in 0..hours {
            advance(cx, HOUR);
        }
    }

    fn archive_url(version: &str) -> String {
        format!(
            "https://download.jetbrains.com/language-server/kotlin-server/{version}/kotlin-server-{version}.tar.gz"
        )
    }

    /// Runs the app until `done`, sleeping between tries: an install also waits on file system
    /// threads. Fake time does not move, so nothing done here waited on a lock's timer.
    fn wait_until(cx: &mut TestAppContext, what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..1000 {
            cx.run_until_parked();
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn start(cx: &mut TestAppContext, updates: &Entity<KotlinUpdates>) {
        updates.update(cx, |updates, cx| updates.note_start(cx));
    }

    fn refresh(cx: &mut TestAppContext, updates: &Entity<KotlinUpdates>) -> Refresh {
        let task = updates.update(cx, |updates, cx| updates.refresh(cx));
        cx.run_until_parked();
        task.now_or_never().expect("the refresh finished")
    }

    #[gpui::test]
    fn test_an_empty_url_makes_no_request_in_three_days(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.set_last_check(Duration::from_secs(30 * 24 * 60 * 60));
        let _updates = harness.updates(cx);
        advance_hours(cx, 72);
        assert!(harness.server.urls().is_empty());
        // One "off" line at startup, not one per wake-up.
        assert_eq!(
            harness.manifest_lines(),
            ["manifest: off (kotlin.manifest_url is empty)"]
        );
    }

    #[gpui::test]
    fn test_a_stale_check_fetches_once_at_startup(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        set_user_url(cx, A);
        harness.set_last_check(25 * HOUR);
        let _updates = harness.updates(cx);

        advance(cx, Duration::from_secs(4));
        assert!(harness.server.urls().is_empty());
        advance(cx, Duration::from_secs(1));
        assert_eq!(harness.server.urls(), [A.to_string(), format!("{A}.sig")]);
        assert_eq!(harness.cached_serial(A), Some(3));
        assert_eq!(
            harness.manifest_lines(),
            ["manifest: fetched serial 3 (host a.example.com)"]
        );
        let state = harness.cache().read_state();
        assert_eq!(state.fetch_lease, None);
        assert_eq!(state.last_check, Some(now() + chrono::Duration::seconds(5)));
    }

    #[gpui::test]
    fn test_a_fresh_check_does_not_fetch_at_startup(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        set_user_url(cx, A);
        harness.set_last_check(HOUR);
        let _updates = harness.updates(cx);
        advance(cx, Duration::from_secs(5));
        assert!(harness.server.urls().is_empty());
        assert!(harness.manifest_lines().is_empty());
        // It goes stale 23 hours later and the next wake-up fetches.
        advance_hours(cx, 22);
        assert_eq!(harness.fetches(), 0);
        advance_hours(cx, 1);
        assert_eq!(harness.fetches(), 1);
    }

    #[gpui::test]
    fn test_one_fetch_per_day(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        set_user_url(cx, A);
        let _updates = harness.updates(cx);
        advance_hours(cx, 72);
        // At 5 s, then 24 h and 48 h later.
        assert_eq!(harness.fetches(), 3);
    }

    #[gpui::test]
    fn test_a_url_change_fetches_the_new_url_at_once(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        harness.server.serve_pair(B, signed(4, "263.4.0"));
        set_user_url(cx, A);
        let _updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        assert_eq!(harness.fetches(), 1);

        set_user_url(cx, B);
        cx.run_until_parked();
        assert_eq!(
            &harness.server.urls()[2..],
            &[B.to_string(), format!("{B}.sig")]
        );
        assert_eq!(harness.cached_serial(B), Some(4));

        // Another settings change leaves the URL alone and fetches nothing.
        set_user(
            cx,
            &format!(r#"{{ "kotlin": {{ "manifest_url": "{B}", "server": "intellij" }} }}"#),
        );
        cx.run_until_parked();
        assert_eq!(harness.fetches(), 2);

        // Emptying it logs "off" once and fetches nothing.
        set_user(cx, "{}");
        advance_hours(cx, 48);
        assert_eq!(harness.fetches(), 2);
        let lines = harness.manifest_lines();
        assert_eq!(
            lines.last().unwrap(),
            "manifest: off (kotlin.manifest_url is empty)"
        );
        assert_eq!(lines.iter().filter(|line| line.contains("off")).count(), 1);
    }

    #[gpui::test]
    fn test_a_url_change_during_a_check_checks_the_new_url_after_it(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve(A, Route::Stall);
        harness.server.serve_pair(B, signed(4, "263.4.0"));
        set_user_url(cx, A);
        let _updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        set_user_url(cx, B);
        cx.run_until_parked();
        assert_eq!(harness.fetches(), 1);
        advance(cx, FETCH_TIMEOUT);
        assert_eq!(harness.cached_serial(B), Some(4));
        // Exactly one check of the new URL, then nothing until a day has passed.
        advance_hours(cx, 23);
        assert_eq!(harness.fetches(), 2);
    }

    #[gpui::test]
    fn test_a_body_that_never_ends_times_out_without_holding_the_lock(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve(A, Route::Stall);
        set_user_url(cx, A);
        let locks = Arc::new(LockRegistry::new(Arc::new(cx.executor())));
        let updates = harness.updates_with(cx, locks.clone());
        advance(cx, STARTUP_DELAY);
        assert_eq!(harness.server.urls(), [A]);
        let lease = harness.cache().read_state().fetch_lease;
        assert!(lease.is_some());

        // The foreground answers at once: a forced check reports the one in flight.
        assert_eq!(refresh(cx, &updates), Refresh::Busy);
        assert_eq!(Refresh::Busy.describe(), "a check is already running");

        // The state lock is free while the fetch is stalled: a write from another task of this
        // process, through the same lock registry, finishes without any time passing.
        let cache = ManifestCache::new(&harness.install_root(), locks);
        let write = cx.executor().spawn(async move {
            cache
                .with_state(|state| state.last_outcome = Some("other".into()))
                .await
        });
        cx.run_until_parked();
        write.now_or_never().expect("the write finished").unwrap();

        advance(cx, FETCH_TIMEOUT - Duration::from_secs(1));
        assert!(harness.manifest_lines().is_empty());
        advance(cx, Duration::from_secs(1));
        assert_eq!(
            harness.manifest_lines(),
            ["manifest: rejected: timed out after 30 s (host a.example.com)"]
        );
        let state = harness.cache().read_state();
        assert_eq!(state.fetch_lease, None);
        assert_eq!(
            state.last_outcome.as_deref(),
            Some("manifest: rejected: timed out after 30 s")
        );
        assert_eq!(harness.cached_serial(A), None);
    }

    #[gpui::test]
    fn test_a_bad_signature_leaves_the_cache_and_logs_why(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        set_user_url(cx, A);
        let updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        let stored = fs::read(harness.cache().cache_path()).unwrap();

        harness.server.serve_pair(
            A,
            signed_with(servers(9, "263.9.0"), "release-1", &key_pair(8)),
        );
        let result = refresh(cx, &updates);
        assert!(
            matches!(result, Refresh::Finished(FetchOutcome::Rejected { .. })),
            "{result:?}"
        );
        assert_eq!(fs::read(harness.cache().cache_path()).unwrap(), stored);
        let line = harness.manifest_lines().pop().unwrap();
        assert!(line.starts_with("manifest: rejected: "), "{line}");
        assert!(line.contains("signature"), "{line}");
        assert!(line.ends_with("(host a.example.com)"), "{line}");

        // A lower serial, signed well, is refused too.
        harness.server.serve_pair(A, signed(2, "263.2.0"));
        assert_eq!(
            refresh(cx, &updates),
            Refresh::Finished(FetchOutcome::NotNewer {
                serial: 2,
                cached: 3
            })
        );
        assert_eq!(fs::read(harness.cache().cache_path()).unwrap(), stored);
        assert_eq!(
            harness.manifest_lines().pop().unwrap(),
            "manifest: rejected: serial 2 is not newer than 3 (host a.example.com)"
        );
    }

    #[gpui::test]
    fn test_offline_logs_one_line(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve(A, Route::Offline);
        set_user_url(cx, A);
        let _updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        let lines = harness.manifest_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            lines[0],
            "manifest: rejected: fetching the manifest: network is unreachable (host a.example.com)"
        );
        // The failure counts as a check: no retry until a day has passed, and the lease is gone.
        let state = harness.cache().read_state();
        assert_eq!(state.fetch_lease, None);
        assert!(state.last_check.is_some());
        advance_hours(cx, 23);
        assert_eq!(harness.manifest_lines().len(), 1);
        // No notification: a check touches nothing but `state.json`, `cache.json` and the logs.
        let mut entries: Vec<_> = fs::read_dir(harness.install_root().join("manifest"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        entries.sort();
        assert_eq!(entries, [".lock", "state.json"]);
    }

    #[gpui::test]
    fn test_requests_carry_nothing_extra(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        set_user_url(cx, A);
        // Something was cached and checked before: still no conditional request.
        let _updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        advance_hours(cx, 25);
        let seen = harness.server.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4);
        for request in seen {
            assert_eq!(request.method, Method::GET);
            assert_eq!(request.query, None);
            assert!(request.headers.is_empty(), "{:?}", request.headers);
        }
    }

    #[gpui::test]
    fn test_redirects_are_followed(cx: &mut TestAppContext) {
        let policies = Arc::new(Mutex::new(Vec::new()));
        let client = FakeHttpClient::create({
            let policies = policies.clone();
            move |request| {
                policies.lock().unwrap().push(
                    request
                        .extensions()
                        .get::<http_client::RedirectPolicy>()
                        .cloned(),
                );
                async move { Err(anyhow!("offline")) }
            }
        });
        let url = Url::parse(A).unwrap();
        cx.executor()
            .spawn(async move { fetch_pair(client.as_ref(), &url).await.ok() })
            .detach();
        cx.run_until_parked();
        assert_eq!(
            *policies.lock().unwrap(),
            [Some(http_client::RedirectPolicy::FollowAll)]
        );
    }

    #[gpui::test]
    fn test_two_processes_on_one_data_dir_fetch_once(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve(A, Route::Stall);
        set_user_url(cx, A);
        let _first = harness.updates(cx);
        let _second = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        assert_eq!(harness.server.urls(), [A]);
        advance(cx, FETCH_TIMEOUT);
        assert_eq!(harness.fetches(), 1);

        // Served normally, still one fetch between them.
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        advance_hours(cx, 25);
        assert_eq!(harness.fetches(), 2);
        assert_eq!(harness.cached_serial(A), Some(3));
    }

    #[gpui::test]
    fn test_a_forced_refresh_skips_the_age_test_but_not_a_live_lease(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        set_user_url(cx, A);
        harness.set_last_check(HOUR);
        let updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        assert_eq!(harness.fetches(), 0);

        assert_eq!(
            refresh(cx, &updates),
            Refresh::Finished(FetchOutcome::Stored { serial: 3 })
        );
        assert_eq!(harness.fetches(), 1);

        // Another process holds a live lease.
        let until = now() + chrono::Duration::seconds(60);
        block_on(
            harness
                .cache()
                .with_state(|state| state.fetch_lease = Some(until)),
        )
        .unwrap();
        assert_eq!(refresh(cx, &updates), Refresh::Busy);
        assert_eq!(harness.fetches(), 1);
        assert_eq!(harness.cache().read_state().fetch_lease, Some(until));

        // With no URL there is nothing to refresh.
        set_user(cx, "{}");
        cx.run_until_parked();
        assert_eq!(refresh(cx, &updates), Refresh::Off);
    }

    #[gpui::test]
    fn test_a_project_manifest_url_is_ignored(cx: &mut TestAppContext) {
        let harness = Harness::new(cx);
        const EVIL: &str = "https://evil.example.org/servers.json";
        harness.server.serve_pair(EVIL, signed(9, "263.9.0"));
        harness.server.serve_pair(A, signed(3, "263.3.0"));
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        WorktreeId::from_usize(1),
                        LocalSettingsPath::InWorktree(RelPath::empty_arc()),
                        LocalSettingsKind::Settings,
                        Some(&format!(
                            r#"{{ "kotlin": {{ "manifest_url": "{EVIL}" }} }}"#
                        )),
                        cx,
                    )
                    .unwrap();
            });
        });
        let _updates = harness.updates(cx);
        advance_hours(cx, 48);
        assert!(harness.server.urls().is_empty());

        // Positive control: the user's value is used, and the project's still is not.
        set_user_url(cx, A);
        cx.run_until_parked();
        assert_eq!(harness.server.urls(), [A.to_string(), format!("{A}.sig")]);
        advance_hours(cx, 48);
        assert!(
            harness
                .server
                .urls()
                .iter()
                .all(|url| !url.contains("evil"))
        );
    }

    #[gpui::test]
    fn test_a_newer_build_downloads_only_after_a_start_request(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        harness.serve_builds(A, 3, &["263.3.0"]);
        set_user_url(cx, A);
        let updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        assert_eq!(harness.cached_serial(A), Some(3));
        advance_hours(cx, 48);
        assert_eq!(harness.archive_requests(), 0);
        assert!(!harness.server_dir().join("263.3.0").exists());

        start(cx, &updates);
        wait_until(cx, "the trial", || {
            harness.cache().read_state().kotlin_lsp.trial.is_some()
        });
        let entry = harness.build(A, "263.3.0");
        assert_eq!(
            harness.cache().read_state().kotlin_lsp.trial,
            Some(BuildId {
                version: "263.3.0".into(),
                sha256: entry.sha256.clone(),
            })
        );
        assert_eq!(harness.archive_requests(), 1);
        assert!(
            check_installed(&harness.install_root(), KotlinServerKind::KotlinLsp, &entry).is_some()
        );
        assert!(!pending_marker_path(&harness.server_dir(), "263.3.0").exists());
        assert_eq!(
            harness.download_lines(),
            [
                "download: kotlin-lsp 263.3.0 started",
                "download: kotlin-lsp 263.3.0 installed as the trial",
            ]
        );

        // Nothing is newer than the trial now: later checks and starts download nothing.
        start(cx, &updates);
        refresh(cx, &updates);
        cx.run_until_parked();
        assert_eq!(harness.archive_requests(), 1);
    }

    #[gpui::test]
    fn test_a_checksum_mismatch_deletes_the_file_and_records_nothing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        harness.serve_builds(A, 3, &["263.3.0"]);
        let mut tampered = harness.archive.clone();
        tampered[1000] ^= 0xff;
        harness
            .server
            .serve(&archive_url("263.3.0"), Route::Body(tampered));
        set_user_url(cx, A);
        let updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        start(cx, &updates);
        wait_until(cx, "the failure", || {
            harness
                .download_lines()
                .iter()
                .any(|line| line.contains("failed"))
        });
        cx.run_until_parked();
        assert_eq!(harness.archive_requests(), 1);
        let lines = harness.download_lines();
        assert!(lines.last().unwrap().contains("SHA-256"), "{lines:?}");
        assert_eq!(harness.cache().read_state().kotlin_lsp.trial, None);
        let mut left: Vec<String> = fs::read_dir(harness.server_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [".263.3.0.lock"]);
    }

    #[gpui::test]
    fn test_a_start_never_waits_on_a_download(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        harness.serve_builds(A, 3, &["263.2.0", "263.3.0"]);
        // Build B stalls mid-body, holding B's version lock.
        harness.server.serve(
            &archive_url("263.3.0"),
            Route::StallAfter(harness.archive.clone(), 64 * 1024),
        );
        set_user_url(cx, A);
        let locks = Arc::new(LockRegistry::new(Arc::new(cx.executor())));
        let updates = harness.updates_with(cx, locks.clone());
        advance(cx, STARTUP_DELAY);
        let active = harness.build(A, "263.2.0");
        crate::installed::tests::install(&harness.install_root(), &active);
        block_on(harness.cache().with_state(|state| {
            state.kotlin_lsp.active = Some(BuildId {
                version: active.version.clone(),
                sha256: active.sha256.clone(),
            })
        }))
        .unwrap();

        start(cx, &updates);
        let partial = harness.server_dir().join(".263.3.0.download");
        wait_until(cx, "the stalled download", || {
            fs::metadata(&partial).is_ok_and(|metadata| metadata.len() > 0)
        });
        assert!(
            locks
                .try_version_lock(&harness.server_dir(), "263.3.0")
                .unwrap()
                .is_none()
        );

        // Starts in this process, through the same lock registry, with no fake time passing.
        let mut run_start = |entry: ManifestEntry| {
            let (locks, dir, http) = (locks.clone(), harness.server_dir(), harness.server.client());
            let mut task = cx.executor().spawn(async move {
                ensure_installed(
                    &locks,
                    KotlinServerKind::KotlinLsp,
                    &entry,
                    &dir,
                    http.as_ref(),
                    &|_, _| {},
                    &AtomicBool::new(false),
                )
                .await
            });
            let mut result = None;
            wait_until(cx, "the start", || {
                result = (&mut task).now_or_never();
                result.is_some()
            });
            result.unwrap().unwrap()
        };
        // The pin, not installed yet, then installed; then the active build.
        let pin = run_start(harness.pin());
        assert_eq!(pin.version, PIN_VERSION);
        run_start(harness.pin());
        assert_eq!(run_start(active).version, "263.2.0");
        assert_eq!(harness.archive_requests(), 2);

        // The state lock is free too.
        let cache = ManifestCache::new(&harness.install_root(), locks.clone());
        let write = cx.executor().spawn(async move {
            cache
                .with_state(|state| state.last_outcome = Some("other".into()))
                .await
        });
        cx.run_until_parked();
        write.now_or_never().expect("the write finished").unwrap();

        // The download is still in flight and recorded nothing.
        assert!(partial.exists());
        assert_eq!(harness.cache().read_state().kotlin_lsp.trial, None);
        assert!(updates.read_with(cx, |updates, _| updates.downloading));
    }

    #[gpui::test]
    fn test_a_download_skips_a_version_another_process_holds(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        harness.serve_builds(A, 3, &["263.3.0"]);
        set_user_url(cx, A);
        let updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        let other_process = thread_registry();
        let held = other_process
            .try_version_lock(&harness.server_dir(), "263.3.0")
            .unwrap()
            .unwrap();

        // No fake time passes: the download gives up at once instead of waiting.
        start(cx, &updates);
        cx.run_until_parked();
        assert_eq!(harness.archive_requests(), 0);
        assert_eq!(
            harness.download_lines(),
            [
                "download: kotlin-lsp 263.3.0 started",
                "download: kotlin-lsp 263.3.0 skipped: another install holds it",
            ]
        );
        assert!(!updates.read_with(cx, |updates, _| updates.downloading));

        // The next check tries again.
        drop(held);
        refresh(cx, &updates);
        wait_until(cx, "the trial", || {
            harness.cache().read_state().kotlin_lsp.trial.is_some()
        });
        assert_eq!(harness.archive_requests(), 1);
    }

    /// Installs nothing, but leaves the pending-trial marker of `version` and records it the way a
    /// finished background download does. Returns whether the marker is gone.
    fn record(
        cx: &mut TestAppContext,
        harness: &Harness,
        updates: &Entity<KotlinUpdates>,
        version: &str,
    ) -> bool {
        let entry = ManifestEntry {
            version: version.into(),
            ..harness.pin()
        };
        let marker = pending_marker_path(&harness.server_dir(), version);
        fs::create_dir_all(harness.server_dir()).unwrap();
        fs::write(&marker, &entry.sha256).unwrap();
        let checker = updates.read_with(cx, |updates, _| updates.checker.clone());
        block_on(checker.record_trial(&entry));
        !marker.exists()
    }

    fn trial_version(harness: &Harness) -> Option<String> {
        harness
            .cache()
            .read_state()
            .kotlin_lsp
            .trial
            .map(|trial| trial.version)
    }

    #[gpui::test]
    fn test_an_older_build_does_not_replace_a_newer_trial(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        let updates = harness.updates(cx);
        assert!(record(cx, &harness, &updates, "263.4.0"));
        assert!(record(cx, &harness, &updates, "263.3.0"));
        assert!(record(cx, &harness, &updates, "263.4.0"));
        assert_eq!(trial_version(&harness).as_deref(), Some("263.4.0"));
        assert_eq!(
            harness.download_lines(),
            [
                "download: kotlin-lsp 263.4.0 installed as the trial",
                "download: kotlin-lsp 263.3.0 not recorded: 263.4.0 is as new or newer",
                "download: kotlin-lsp 263.4.0 is already recorded",
            ]
        );
    }

    #[gpui::test]
    fn test_a_newer_build_replaces_an_older_trial(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        let updates = harness.updates(cx);
        assert!(record(cx, &harness, &updates, "263.3.0"));
        assert!(record(cx, &harness, &updates, "263.10.0"));
        assert_eq!(trial_version(&harness).as_deref(), Some("263.10.0"));
    }

    #[gpui::test]
    fn test_a_rejected_build_is_not_recorded(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        let updates = harness.updates(cx);
        let digest = harness.pin().sha256;
        block_on(harness.cache().with_state(|state| {
            state.kotlin_lsp.active = Some(BuildId {
                version: "263.1.0".into(),
                sha256: digest.clone(),
            });
            state.kotlin_lsp.rejected.push(BuildId {
                version: "263.5.0".into(),
                sha256: digest.to_uppercase(),
            });
        }))
        .unwrap();
        assert!(record(cx, &harness, &updates, "263.5.0"));
        assert_eq!(trial_version(&harness), None);
        assert_eq!(
            harness.download_lines(),
            ["download: kotlin-lsp 263.5.0 not recorded: it was rejected"]
        );
        // Older than the active build: not recorded either.
        assert!(record(cx, &harness, &updates, "263.0.9"));
        assert_eq!(trial_version(&harness), None);
    }

    #[test]
    fn test_bodies_over_the_caps_are_refused() {
        let big = vec![b'x'; MAX_SIGNATURE_BYTES + 1];
        let client = FakeHttpClient::create(move |_| {
            let big = big.clone();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(big))
                    .unwrap())
            }
        });
        let url = Url::parse(A).unwrap();
        let error = block_on(get_capped(
            client.as_ref(),
            &url,
            MAX_SIGNATURE_BYTES,
            "signature",
        ))
        .unwrap_err();
        assert!(format!("{error:#}").contains("more than"), "{error:#}");
    }

    /// A stalled download of build B beside an active build A.
    fn start_stalled_download(
        cx: &mut TestAppContext,
        harness: &Harness,
    ) -> (Entity<KotlinUpdates>, Arc<LockRegistry>, ManifestEntry) {
        harness.serve_builds(A, 3, &["263.2.0", "263.3.0"]);
        harness.server.serve(
            &archive_url("263.3.0"),
            Route::StallAfter(harness.archive.clone(), 64 * 1024),
        );
        set_user_url(cx, A);
        let locks = Arc::new(LockRegistry::new(Arc::new(cx.executor())));
        let updates = harness.updates_with(cx, locks.clone());
        advance(cx, STARTUP_DELAY);
        let active = harness.build(A, "263.2.0");
        crate::installed::tests::install(&harness.install_root(), &active);
        block_on(harness.cache().with_state(|state| {
            state.kotlin_lsp.active = Some(BuildId {
                version: active.version.clone(),
                sha256: active.sha256.clone(),
            })
        }))
        .unwrap();
        start(cx, &updates);
        let partial = harness.server_dir().join(".263.3.0.download");
        wait_until(cx, "the stalled download", || {
            fs::metadata(&partial).is_ok_and(|metadata| metadata.len() > 0)
        });
        (updates, locks, active)
    }

    #[gpui::test]
    fn test_download_progress_reaches_the_status_snapshot(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        harness.serve_builds(A, 3, &["263.3.0"]);
        set_user_url(cx, A);
        let updates = harness.updates(cx);
        advance(cx, STARTUP_DELAY);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let _observe = cx.update(|cx| {
            let seen = seen.clone();
            cx.observe(&updates, move |updates, cx| {
                seen.lock()
                    .unwrap()
                    .push(updates.read(cx).download_progress().cloned());
            })
        });
        let _subscribe = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&updates, move |_, event: &KotlinUpdatesEvent, _| {
                events.lock().unwrap().push(event.clone());
            })
        });
        start(cx, &updates);
        wait_until(cx, "the trial", || {
            harness.cache().read_state().kotlin_lsp.trial.is_some()
        });
        let seen: Vec<DownloadProgress> = seen.lock().unwrap().iter().flatten().cloned().collect();
        let last = seen.last().expect("progress was shown");
        assert_eq!(last.version, "263.3.0");
        assert_eq!(last.done, harness.archive.len() as u64);
        assert!(updates.read_with(cx, |updates, _| updates.download_progress().is_none()));
        assert_eq!(
            *events.lock().unwrap(),
            [KotlinUpdatesEvent::DownloadStarted {
                version: "263.3.0".into()
            }]
        );
    }

    #[gpui::test]
    fn test_cancelling_a_download_cleans_up_and_the_next_refresh_offers_it_again(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        let (updates, locks, active) = start_stalled_download(cx, &harness);
        assert!(updates.read_with(cx, |updates, _| updates.download_progress().is_some()));
        let cancelled = updates.update(cx, |updates, cx| updates.cancel_download(cx));
        assert!(cancelled);
        let reader = cx.clone();
        wait_until(cx, "the download to end", || {
            !updates.read_with(&reader, |updates, _| updates.downloading)
        });
        let dir = harness.server_dir();
        assert!(!dir.join(".263.3.0.download").exists());
        assert!(!dir.join(".263.3.0.staging").exists());
        assert!(!dir.join("263.3.0").exists());
        assert!(locks.try_version_lock(&dir, "263.3.0").unwrap().is_some());
        // The active build runs untouched and nothing was recorded.
        assert!(
            check_installed(
                &harness.install_root(),
                KotlinServerKind::KotlinLsp,
                &active
            )
            .is_some()
        );
        assert_eq!(harness.cache().read_state().kotlin_lsp.trial, None);
        assert!(updates.read_with(cx, |updates, _| updates.download_progress().is_none()));
        assert!(
            !updates.update(cx, |updates, cx| updates.cancel_download(cx)),
            "nothing is left to cancel"
        );

        // The next refresh offers the build again.
        harness.server.serve(
            &archive_url("263.3.0"),
            Route::Body(harness.archive.clone()),
        );
        refresh(cx, &updates);
        wait_until(cx, "the trial", || {
            harness.cache().read_state().kotlin_lsp.trial.is_some()
        });
        assert!(dir.join("263.3.0").exists());
    }

    #[gpui::test]
    async fn test_cancel_install_sets_the_flag_of_the_download(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let harness = Harness::new(cx);
        let (updates, _locks, _active) = start_stalled_download(cx, &harness);
        let flag = updates.read_with(cx, |updates, _| {
            updates.download.as_ref().unwrap().cancel.clone()
        });
        assert!(!flag.load(Ordering::SeqCst));
        cx.update(|cx| cx.set_global(GlobalKotlinUpdates(updates.clone())));
        let fs = ::fs::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [] as [&Path; 0], cx).await;
        assert!(cx.update(|cx| crate::cancel_installs(&project, cx)));
        assert!(flag.load(Ordering::SeqCst));
    }
}
