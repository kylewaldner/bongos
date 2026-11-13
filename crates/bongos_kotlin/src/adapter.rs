//! The one language server adapter for Kotlin. Whichever server a project resolves to (kotlin-lsp or
//! IntelliJ), Zed sees a server named `kotlin`, so switching is a restart of that one server.
//!
//! The adapter implements `DynLspInstaller` itself instead of `LspInstaller`: the blanket impl
//! caches the first binary it fetches and returns it on every later start, so a switch would start
//! the same server again.

use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono::NaiveDate;
use futures::{FutureExt as _, StreamExt as _, lock::OwnedMutexGuard};
use gpui::{App, AppContext as _, AsyncApp, WeakEntity};
use http_client::HttpClient;
use language::{
    DynLspInstaller, LanguageServerBinaryLocations, LspAdapter, LspAdapterDelegate, Toolchain,
};
use lsp::{LanguageServerBinary, LanguageServerBinaryOptions, LanguageServerName};
use settings::{KotlinServerKind, Settings as _, WorktreeId};
use util::rel_path::RelPath;

use crate::{
    KotlinDirs, KotlinSettings,
    install::{InstallCancelled, ProgressFn, spawn_ensure_installed},
    installed::{check_installed, not_installed, server_name, version_dir},
    intellij::{LICENSE_TIMEOUT, bundle_eula_hash, eula_accepted, eula_argument, license_check},
    jdk,
    license::fallback_for,
    manager::{
        ADAPTER_NAME, EXPIRED_REASON, FallbackReason, KotlinProjectManager, KotlinRegistry,
        RejectedTrials, TrialLaunch,
    },
    manifest::{Manifest, ManifestEntry, current_platform},
    manifest_cache::{BuildId, ServerState, read_for_start},
    remote_manifest::{TrustedKey, trusted_keys},
    resolve::resolve,
    updates::KotlinUpdates,
    versions::{Resolution, resolve_build},
};

pub struct KotlinAdapter {
    dirs: KotlinDirs,
    manifest: Arc<Manifest>,
    /// Tests pin the date the expiry check sees; production reads the clock.
    #[cfg(test)]
    pinned_today: Option<NaiveDate>,
}

impl KotlinAdapter {
    pub fn new(dirs: KotlinDirs, manifest: Arc<Manifest>) -> Self {
        Self {
            dirs,
            manifest,
            #[cfg(test)]
            pinned_today: None,
        }
    }

    fn today(&self) -> NaiveDate {
        #[cfg(test)]
        if let Some(today) = self.pinned_today {
            return today;
        }
        chrono::Utc::now().date_naive()
    }
}

#[async_trait(?Send)]
impl LspAdapter for KotlinAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static(ADAPTER_NAME)
    }
}

#[async_trait(?Send)]
impl DynLspInstaller for KotlinAdapter {
    async fn try_fetch_server_binary(
        &self,
        _delegate: &Arc<dyn LspAdapterDelegate>,
        _container_dir: std::path::PathBuf,
        _pre_release: bool,
        _cx: &mut AsyncApp,
    ) -> Result<LanguageServerBinary> {
        bail!("not used by bongos_kotlin")
    }

    fn get_language_server_command(
        self: Arc<Self>,
        delegate: Arc<dyn LspAdapterDelegate>,
        _toolchains: Option<Toolchain>,
        _binary_options: LanguageServerBinaryOptions,
        _cached_binary: OwnedMutexGuard<Option<(bool, LanguageServerBinary)>>,
        mut cx: AsyncApp,
    ) -> LanguageServerBinaryLocations {
        async move {
            let root = delegate.worktree_root_path().to_path_buf();
            let result = server_command(
                &self.dirs,
                &self.manifest,
                delegate.http_client(),
                &root,
                delegate.worktree_id(),
                self.today(),
                &mut cx,
            )
            .await;
            (result, None)
        }
        .boxed_local()
    }
}

/// The server a project runs now: its manager's answer when it has one (which also marks the
/// worktree `starting`), else the plain resolution.
fn begin_start(
    dirs: &KotlinDirs,
    worktree_root: &Path,
    worktree_id: WorktreeId,
    cx: &mut App,
) -> KotlinServerKind {
    if let Some(manager) = manager_of(worktree_root, cx)
        && let Some(server) = manager.update(cx, |manager, cx| manager.begin_start(worktree_id, cx))
    {
        return server;
    }
    resolve(&dirs.state_dir, worktree_root, worktree_id, cx).server
}

fn start_failed(worktree_root: &Path, worktree_id: WorktreeId, reason: String, cx: &mut App) {
    if let Some(manager) = manager_of(worktree_root, cx) {
        manager.update(cx, |manager, cx| {
            manager.start_failed(worktree_id, reason, cx)
        });
    }
}

fn manager_of(
    worktree_root: &Path,
    cx: &App,
) -> Option<gpui::Entity<crate::manager::KotlinProjectManager>> {
    cx.try_global::<KotlinRegistry>()
        .and_then(|registry| registry.manager(worktree_root))
}

/// The message of an IntelliJ server that is not installed and may not be downloaded (D16).
pub const INTELLIJ_DOWNLOAD_HINT: &str = "intellij is not installed: set kotlin.intellij.download to true to let bongos download it, or point lsp.kotlin.binary.path at a server you installed by hand";

/// Why a start of the IntelliJ server did not happen, when kotlin-lsp can run instead.
struct Refusal {
    reason: FallbackReason,
    /// For `kotlin.log`.
    detail: String,
    error: anyhow::Error,
}

/// A start that failed: with a `Refusal` the IntelliJ server falls back to kotlin-lsp.
enum StartError {
    Refused(Refusal),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for StartError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

fn refused(reason: FallbackReason, detail: String) -> StartError {
    StartError::Refused(Refusal {
        reason,
        detail,
        error: anyhow::anyhow!(reason.text()),
    })
}

/// The command that starts the effective server for a worktree, or why it cannot start.
///
/// An expired pin (decision D18) is neither installed nor started. An installed pin starts without
/// touching `http`. A missing kotlin-lsp is installed first, with `Installing` progress and the
/// manager's cancel flag. A missing IntelliJ server is installed the same way only when
/// `kotlin.intellij.download` is true (D16); otherwise it is "intellij is not installed" and
/// nothing is requested. An installed IntelliJ server then needs the user's EULA acceptance
/// (D9) and a passing `license status` (D10), and starts with `--eula=<hash>`.
///
/// An IntelliJ start that fails its install, its EULA or its license check, or whose build
/// expired, falls back to kotlin-lsp (the fallback table): the manager records the fallback and
/// this same start returns kotlin-lsp. Without a manager the failure is the error. `today` is the
/// date the pin's `expires` is compared with.
pub async fn server_command(
    dirs: &KotlinDirs,
    manifest: &Manifest,
    http: Arc<dyn HttpClient>,
    worktree_root: &Path,
    worktree_id: WorktreeId,
    today: NaiveDate,
    cx: &mut AsyncApp,
) -> Result<LanguageServerBinary> {
    let server = cx.update(|cx| begin_start(dirs, worktree_root, worktree_id, cx));
    let refusal = match start_command(
        dirs,
        manifest,
        http.clone(),
        worktree_root,
        worktree_id,
        today,
        server,
        cx,
    )
    .await
    {
        Ok(binary) => return Ok(binary),
        Err(StartError::Failed(error)) => return Err(error),
        Err(StartError::Refused(refusal)) => refusal,
    };
    let fell_back = cx.update(|cx| {
        manager_of(worktree_root, cx).is_some_and(|manager| {
            manager.update(cx, |manager, cx| {
                manager.intellij_unavailable(worktree_id, refusal.reason, &refusal.detail, cx)
            })
        })
    });
    if !fell_back {
        cx.update(|cx| {
            log_line(worktree_root, &refusal.detail, cx);
            start_failed(worktree_root, worktree_id, refusal.reason.text().into(), cx)
        });
        return Err(refusal.error);
    }
    match start_command(
        dirs,
        manifest,
        http,
        worktree_root,
        worktree_id,
        today,
        KotlinServerKind::KotlinLsp,
        cx,
    )
    .await
    {
        Ok(binary) => Ok(binary),
        Err(StartError::Failed(error)) => Err(error),
        Err(StartError::Refused(refusal)) => Err(refusal.error),
    }
}

/// The command for `server`, which the start has already chosen.
#[allow(clippy::too_many_arguments)]
async fn start_command(
    dirs: &KotlinDirs,
    manifest: &Manifest,
    http: Arc<dyn HttpClient>,
    worktree_root: &Path,
    worktree_id: WorktreeId,
    today: NaiveDate,
    server: KotlinServerKind,
    cx: &mut AsyncApp,
) -> Result<LanguageServerBinary, StartError> {
    let (platform, pin) = match current_platform()
        .ok_or_else(|| anyhow::anyhow!("no {} build for this platform", server_name(server)))
        .and_then(|platform| Ok((platform, manifest.entry(server, platform)?)))
    {
        Ok(found) => found,
        Err(error) => {
            let reason = error.to_string();
            cx.update(|cx| start_failed(worktree_root, worktree_id, reason, cx));
            return Err(error.into());
        }
    };

    // kotlin-lsp may run a downloaded build instead of the pin (`versions.rs`); IntelliJ runs its pin.
    let mut trial_fallback = None;
    let resolution = match server {
        KotlinServerKind::KotlinLsp => {
            // From now on the background check may download a newer build for the next start.
            cx.update(KotlinUpdates::start_requested);
            let (manifest_url, keys, rejected) = cx.update(|cx| {
                (
                    KotlinSettings::get_global(cx).manifest_url_for_fetch(),
                    manifest_keys(cx),
                    RejectedTrials::builds(cx),
                )
            });
            let (install_root, pin) = (dirs.install_root.clone(), pin.clone());
            let choice = cx
                .background_spawn(async move {
                    kotlin_lsp_choice(
                        &install_root,
                        manifest_url,
                        &keys,
                        &rejected,
                        platform,
                        pin,
                        today,
                    )
                })
                .await;
            trial_fallback = choice.fallback;
            let resolution = choice.resolution;
            let text = match &resolution {
                Resolution::Trial(entry) => Some(format!("kotlin-lsp {} (trial)", entry.version)),
                Resolution::Active(entry) => {
                    Some(format!("kotlin-lsp {} (downloaded)", entry.version))
                }
                Resolution::Pin(_) | Resolution::Expired => None,
            };
            if let Some(text) = text {
                cx.update(|cx| log_line(worktree_root, &text, cx));
            }
            resolution
        }
        KotlinServerKind::Intellij => Resolution::Pin(pin.clone()),
    };
    let pin_version = pin.version.clone();
    let entry = match &resolution {
        Resolution::Trial(entry) | Resolution::Active(entry) | Resolution::Pin(entry) => {
            entry.clone()
        }
        Resolution::Expired => pin,
    };

    let expired = cx.update(|cx| {
        let known = manager_of(worktree_root, cx)
            .is_some_and(|manager| manager.read(cx).is_expired(worktree_id, server));
        known || resolution == Resolution::Expired || entry.is_expired(today)
    });
    if expired && server == KotlinServerKind::Intellij {
        return Err(refused(
            FallbackReason::Expired,
            format!("intellij {} expired", entry.version),
        ));
    }
    if expired {
        cx.update(|cx| {
            if let Some(manager) = manager_of(worktree_root, cx) {
                manager.update(cx, |manager, cx| {
                    manager.mark_expired(worktree_id, server, cx)
                });
            }
        });
        return Err(anyhow::anyhow!("{EXPIRED_REASON}").into());
    }

    // The import JDK (D11). A Gradle toolchain no JDK satisfies fails the start before anything is
    // downloaded.
    let inputs = cx.update(|cx| jdk::inputs(worktree_root, worktree_id, cx));
    let detection = cx
        .background_spawn(async move { jdk::detect(&inputs) })
        .await;
    if let Some(reason) = detection.failure_reason() {
        cx.update(|cx| {
            log_line(worktree_root, &format!("jdk: {reason}"), cx);
            start_failed(worktree_root, worktree_id, reason.clone(), cx)
        });
        return Err(anyhow::anyhow!(reason).into());
    }
    cx.update(|cx| {
        let text = match detection.jdk() {
            Some(jdk) => format!("jdk {} ({})", jdk.home.display(), jdk.reason),
            None => "jdk: none, the server's bundled runtime imports".to_string(),
        };
        log_line(worktree_root, &text, cx);
    });

    let check = {
        let (install_root, entry) = (dirs.install_root.clone(), entry.clone());
        move || check_installed(&install_root, server, &entry)
    };
    let binary = match cx.background_spawn(async move { check() }).await {
        Some(binary) => binary,
        // A start installs only the pin; a downloaded build that went missing is not installed again.
        None if !matches!(resolution, Resolution::Pin(_)) => {
            let error = not_installed(server);
            let reason = error.to_string();
            cx.update(|cx| start_failed(worktree_root, worktree_id, reason, cx));
            return Err(error.into());
        }
        None => {
            if server == KotlinServerKind::Intellij && !intellij_download_allowed(worktree_id, cx) {
                let reason = INTELLIJ_DOWNLOAD_HINT.to_string();
                cx.update(|cx| {
                    log_line(
                        worktree_root,
                        "intellij download is off (kotlin.intellij.download)",
                        cx,
                    );
                    start_failed(worktree_root, worktree_id, reason, cx)
                });
                return Err(anyhow::anyhow!(INTELLIJ_DOWNLOAD_HINT).into());
            }
            let installed = install_server(
                dirs,
                server,
                entry.clone(),
                http,
                worktree_root,
                worktree_id,
                cx,
            )
            .await
            .and_then(|()| {
                check_installed(&dirs.install_root, server, &entry)
                    .ok_or_else(|| not_installed(server))
            });
            match installed {
                Ok(binary) => binary,
                Err(error) if server == KotlinServerKind::Intellij => {
                    return Err(refused(
                        FallbackReason::InstallFailed,
                        format!("install error: {error:#}"),
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    let binary = with_java_home(binary, &detection);
    match server {
        KotlinServerKind::KotlinLsp => {
            // The launched mark (D8): only now, with the JDK, expiry and install checks passed,
            // can this start fail the trial.
            if let Resolution::Trial(trial) = &resolution {
                let launch = TrialLaunch {
                    build: BuildId {
                        version: trial.version.clone(),
                        sha256: trial.sha256.clone(),
                    },
                    fallback: trial_fallback.unwrap_or_else(|| "the previous build".into()),
                    pin_version: pin_version.clone(),
                };
                cx.update(|cx| {
                    if let Some(manager) = manager_of(worktree_root, cx) {
                        manager.update(cx, |manager, cx| {
                            manager.trial_launched(worktree_id, launch, cx)
                        });
                    }
                });
            }
            Ok(binary)
        }
        KotlinServerKind::Intellij => intellij_gates(dirs, &entry, binary, worktree_root, cx).await,
    }
}

/// The keys a cached manifest must verify with: the release keys (plus the debug-only override),
/// or the keys a test set.
#[cfg(not(test))]
fn manifest_keys(_: &App) -> Vec<TrustedKey> {
    trusted_keys(true)
}

#[cfg(test)]
fn manifest_keys(cx: &App) -> Vec<TrustedKey> {
    cx.try_global::<ManifestKeys>()
        .map(|keys| keys.0.clone())
        .unwrap_or_else(|| trusted_keys(true))
}

/// Replaces the trusted manifest keys of starts, for tests.
#[cfg(test)]
pub struct ManifestKeys(pub Vec<TrustedKey>);

#[cfg(test)]
impl gpui::Global for ManifestKeys {}

/// What a kotlin-lsp start runs, and for a trial what runs once it is rejected.
struct KotlinLspChoice {
    resolution: Resolution,
    /// For a trial: the next choice, as the rejection notice names it.
    fallback: Option<String>,
}

/// The kotlin-lsp build a start runs. Without a manifest URL nothing is read and the pin runs, as
/// before any manifest was cached; with one, `cache.json` and `state.json` are read as files, with
/// no lock, and no download is waited for. `rejected` (the trials rejected in this run) counts as
/// rejected even when `state.json` does not list it yet.
fn kotlin_lsp_choice(
    install_root: &Path,
    manifest_url: Option<http_client::Url>,
    keys: &[TrustedKey],
    rejected: &[BuildId],
    platform: &str,
    pin: ManifestEntry,
    today: NaiveDate,
) -> KotlinLspChoice {
    let Some(manifest_url) = manifest_url else {
        return KotlinLspChoice {
            resolution: resolve_build(&pin, &[], &ServerState::default(), |_| false, today),
            fallback: None,
        };
    };
    let (cached, mut state) = read_for_start(install_root, &manifest_url, keys);
    for build in rejected {
        if !state.kotlin_lsp.rejected.contains(build) {
            state.kotlin_lsp.rejected.push(build.clone());
        }
    }
    let entries: Vec<ManifestEntry> = cached
        .iter()
        .flat_map(|cached| &cached.manifest.kotlin_lsp)
        .filter_map(|build| build.entry(platform).cloned())
        .collect();
    let installed = |entry: &ManifestEntry| {
        check_installed(install_root, KotlinServerKind::KotlinLsp, entry).is_some()
    };
    let resolution = resolve_build(&pin, &entries, &state, installed, today);
    let fallback = match &resolution {
        Resolution::Trial(trial) => {
            state.kotlin_lsp.rejected.push(BuildId {
                version: trial.version.clone(),
                sha256: trial.sha256.clone(),
            });
            let next = match resolve_build(&pin, &entries, &state, installed, today) {
                Resolution::Trial(next) | Resolution::Active(next) | Resolution::Pin(next) => next,
                Resolution::Expired => pin.clone(),
            };
            Some(format!("kotlin-lsp {}", next.version))
        }
        _ => None,
    };
    KotlinLspChoice {
        resolution,
        fallback,
    }
}

/// The binary with the import JDK as `JAVA_HOME`; unchanged when the bundled runtime imports.
fn with_java_home(
    mut binary: LanguageServerBinary,
    detection: &jdk::Detection,
) -> LanguageServerBinary {
    if let Some(jdk) = detection.jdk() {
        binary
            .env
            .get_or_insert_default()
            .insert("JAVA_HOME".into(), jdk.home.to_string_lossy().into_owned());
    }
    binary
}

fn intellij_download_allowed(worktree_id: WorktreeId, cx: &mut AsyncApp) -> bool {
    cx.update(|cx| {
        KotlinSettings::get(
            Some(settings::SettingsLocation {
                worktree_id,
                path: RelPath::empty(),
            }),
            cx,
        )
        .intellij_download
    })
}

fn log_line(worktree_root: &Path, text: &str, cx: &App) {
    if let Some(manager) = manager_of(worktree_root, cx) {
        manager.read(cx).log().line(text);
    }
}

/// The checks an installed IntelliJ server passes before it starts (D9, then D10): the user has
/// accepted this bundle's EULA, and `license status` says licensed. Returns the binary with
/// `--eula=<hash>` added; a failure is the fallback it causes.
async fn intellij_gates(
    dirs: &KotlinDirs,
    entry: &ManifestEntry,
    mut binary: LanguageServerBinary,
    worktree_root: &Path,
    cx: &mut AsyncApp,
) -> Result<LanguageServerBinary, StartError> {
    let bundle = version_dir(
        &dirs.install_root,
        KotlinServerKind::Intellij,
        &entry.version,
    );
    let hash = match cx
        .background_spawn(async move { bundle_eula_hash(&bundle) })
        .await
    {
        Ok(hash) => hash,
        // A bundle without its agreement is a broken install.
        Err(error) => {
            return Err(refused(
                FallbackReason::InstallFailed,
                format!("eula error: {error:#}"),
            ));
        }
    };
    let state_dir = dirs.state_dir.clone();
    let accepted = {
        let hash = hash.clone();
        cx.background_spawn(async move { eula_accepted(&state_dir, &hash) })
            .await
    };
    if !accepted {
        return Err(refused(
            FallbackReason::EulaNotAccepted,
            format!("eula {hash} is not accepted"),
        ));
    }

    let timeout = cx.background_executor().timer(LICENSE_TIMEOUT);
    let outcome = match license_check(&binary, timeout).await {
        Ok(outcome) => outcome,
        Err(error) => {
            return Err(refused(
                FallbackReason::LicenseCheckFailed,
                format!("license check error: {error:#}"),
            ));
        }
    };
    if let Some(reason) = fallback_for(outcome) {
        return Err(refused(reason, format!("license {outcome:?}")));
    }
    cx.update(|cx| log_line(worktree_root, "license licensed", cx));
    binary.arguments.push(eula_argument(&hash));
    Ok(binary)
}

/// Ends the install in the manager if the start future is dropped before it finished, so the
/// status does not stay `Installing` until the next start.
struct InstallGuard {
    manager: WeakEntity<KotlinProjectManager>,
    worktree_id: WorktreeId,
    cancel: Arc<AtomicBool>,
    cx: AsyncApp,
    armed: bool,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (manager, worktree_id, cancel) =
            (self.manager.clone(), self.worktree_id, self.cancel.clone());
        // Not inline: the drop may happen inside an update of the app.
        self.cx
            .spawn(async move |cx| {
                manager
                    .update(cx, |manager, cx| {
                        manager.abandon_install(worktree_id, &cancel, cx)
                    })
                    .ok();
            })
            .detach();
    }
}

/// Installs `server` with progress in the manager's status. The outcome is also the status: on an
/// error `Failed`, which for a cancel is exactly "install cancelled".
async fn install_server(
    dirs: &KotlinDirs,
    server: KotlinServerKind,
    entry: ManifestEntry,
    http: Arc<dyn HttpClient>,
    worktree_root: &Path,
    worktree_id: WorktreeId,
    cx: &mut AsyncApp,
) -> Result<()> {
    let manager = cx.update(|cx| manager_of(worktree_root, cx));
    let cancel = match &manager {
        Some(manager) => {
            cx.update(|cx| manager.update(cx, |manager, cx| manager.begin_install(worktree_id, cx)))
        }
        None => Arc::new(AtomicBool::new(false)),
    };
    let mut guard = manager.as_ref().map(|manager| InstallGuard {
        manager: manager.downgrade(),
        worktree_id,
        cancel: cancel.clone(),
        cx: cx.clone(),
        armed: true,
    });
    let (sender, mut updates) = futures::channel::mpsc::unbounded();
    let progress: Arc<ProgressFn> = Arc::new(move |done, total| {
        sender.unbounded_send((done, total)).ok();
    });
    let dir = dirs.install_root.join(server_name(server));
    let mut task = cx
        .update(|cx| spawn_ensure_installed(server, entry, dir, http, progress, cancel, cx))
        .fuse();
    let result = loop {
        futures::select_biased! {
            update = updates.select_next_some() => {
                let (done, total) = update;
                if let Some(manager) = &manager {
                    cx.update(|cx| {
                        manager.update(cx, |manager, cx| {
                            manager.install_progress(worktree_id, done, total, cx)
                        })
                    });
                }
            }
            result = task => break result,
        }
    };

    if let Some(guard) = &mut guard {
        guard.armed = false;
    }
    let outcome = match &result {
        Ok(_) => Ok(()),
        Err(error) if error.downcast_ref::<InstallCancelled>().is_some() => Err(error.to_string()),
        Err(error) => Err(format!("install failed: {error}")),
    };
    if let Some(manager) = &manager {
        cx.update(|cx| {
            manager.update(cx, |manager, cx| {
                if let Err(error) = &result {
                    manager.log().line(&format!("install error: {error:#}"));
                }
                manager.end_install(worktree_id, outcome.clone(), cx)
            })
        });
    }
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        rc::Rc,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use gpui::{BorrowAppContext as _, TestAppContext};
    use settings::{Settings as _, SettingsStore};

    use super::*;
    use crate::{
        INSTALLED_MARKER, KotlinProjectManager, KotlinSettings, ServerStatus, installed::Marker,
        manager::KotlinHost,
    };

    struct NoHost;

    impl KotlinHost for NoHost {
        fn restart(&self, _: WorktreeId, _: Option<lsp::LanguageServerId>, _: &mut App) {}
    }

    fn worktree() -> WorktreeId {
        WorktreeId::from_usize(1)
    }

    /// The embedded manifest with every expiry moved far ahead: the adapter reads the real
    /// clock, and the pinned build's expiry must not decide these tests.
    fn unexpired_embedded() -> Manifest {
        let text = crate::manifest::EMBEDDED
            .lines()
            .map(|line| {
                if line.starts_with("expires = ") {
                    "expires = \"2999-01-01\"".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Manifest::parse(&text).unwrap()
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 7).unwrap()
    }

    /// A client that counts its requests and answers each with `respond`.
    struct Net {
        requests: Arc<AtomicUsize>,
        client: Arc<dyn HttpClient>,
    }

    impl Net {
        fn requests(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    fn net_with(
        respond: impl Fn() -> Result<http_client::Response<http_client::AsyncBody>>
        + Send
        + Sync
        + 'static,
    ) -> Net {
        let requests = Arc::new(AtomicUsize::new(0));
        let client = http_client::FakeHttpClient::create({
            let requests = requests.clone();
            move |_| {
                requests.fetch_add(1, Ordering::SeqCst);
                let response = respond();
                async move { response }
            }
        });
        Net { requests, client }
    }

    /// Serves `bytes` for every request.
    fn net_serving(bytes: Vec<u8>) -> Net {
        net_with(move || {
            Ok(http_client::Response::builder()
                .status(200)
                .header("Content-Length", bytes.len())
                .body(http_client::AsyncBody::from_reader(
                    futures::io::Cursor::new(bytes.clone()),
                ))
                .unwrap())
        })
    }

    /// Answers every request with an error, as a machine with no network does.
    fn net_down() -> Net {
        net_with(|| Err(anyhow::anyhow!("network is down")))
    }

    /// A body that never ends: a megabyte and a half at once, then a chunk every few milliseconds.
    /// The pauses return `Pending` so that the foreground can run between chunks; `Content-Length`
    /// promises more than the test will read.
    struct Endless {
        sent: usize,
        paused: bool,
    }

    impl futures::AsyncRead for Endless {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buffer: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.sent > 3 * 512 * 1024 && !std::mem::replace(&mut self.paused, true) {
                let waker = cx.waker().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    waker.wake();
                });
                return std::task::Poll::Pending;
            }
            self.paused = false;
            let length = buffer.len().min(16 * 1024);
            buffer[..length].fill(7);
            self.sent += length;
            std::task::Poll::Ready(Ok(length))
        }
    }

    fn net_endless() -> Net {
        net_with(|| {
            Ok(http_client::Response::builder()
                .status(200)
                .header("Content-Length", 1u64 << 30)
                .body(http_client::AsyncBody::from_reader(Endless {
                    sent: 0,
                    paused: false,
                }))
                .unwrap())
        })
    }

    struct Fixture {
        dirs: KotlinDirs,
        root: PathBuf,
        manifest: Manifest,
        entry: ManifestEntry,
        archive: Vec<u8>,
        manager: gpui::Entity<KotlinProjectManager>,
        _temp: tempfile::TempDir,
    }

    const PINNED_ARGS: [&str; 3] = ["--stdio", "--data-sharing=none", "--region=americas"];

    /// A kotlin-lsp pin for a small generated archive, expiring on `expires`.
    fn fixture(cx: &mut TestAppContext, host: Rc<dyn KotlinHost>, expires: &str) -> Fixture {
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
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
        let archive =
            crate::install::tests::tar_gz(&crate::install::tests::files(2 * 1024 * 1024, true));
        let manifest = Manifest::parse(&format!(
            r#"
[kotlin-lsp]
version = "1.2.3"
expires = "{expires}"
args = {args:?}

[kotlin-lsp.env]
INTELLIJ_UPDATES_ENABLED = "false"

[kotlin-lsp.platform.{platform}]
url = "http://127.0.0.1:1/kotlin-server.tar.gz"
sha256 = "{sha}"
archive = "tar.gz"
strip_components = 1
launcher = "bin/launcher"
"#,
            args = PINNED_ARGS,
            platform = current_platform().unwrap(),
            sha = crate::install::tests::sha256(&archive),
        ))
        .unwrap();
        let entry = manifest
            .entry(KotlinServerKind::KotlinLsp, current_platform().unwrap())
            .unwrap();
        let manager = cx.new(|cx| KotlinProjectManager::new(host, dirs.clone(), cx));
        manager.update(cx, |manager, cx| {
            manager.add_worktree(worktree(), root.clone(), cx)
        });
        Fixture {
            dirs,
            root,
            manifest,
            entry,
            archive,
            manager,
            _temp: temp,
        }
    }

    impl Fixture {
        fn command(
            &self,
            cx: &mut TestAppContext,
            net: &Net,
            today: NaiveDate,
        ) -> gpui::Task<Result<LanguageServerBinary>> {
            let (dirs, manifest, root) =
                (self.dirs.clone(), self.manifest.clone(), self.root.clone());
            let http = net.client.clone();
            cx.update(|cx| {
                cx.spawn(async move |cx| {
                    server_command(&dirs, &manifest, http, &root, worktree(), today, cx).await
                })
            })
        }

        fn status(&self, cx: &mut TestAppContext) -> Option<ServerStatus> {
            self.manager
                .read_with(cx, |manager, _| manager.status(worktree()).cloned())
        }

        fn server_dir(&self) -> PathBuf {
            self.dirs.install_root.join("kotlin-lsp")
        }

        fn launcher(&self) -> PathBuf {
            self.server_dir().join("1.2.3").join("bin/launcher")
        }
    }

    /// Runs foreground work until `condition` holds; the installer runs on other threads.
    fn wait_until(
        cx: &mut TestAppContext,
        what: &str,
        mut condition: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        for _ in 0..2000 {
            cx.run_until_parked();
            if condition(cx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    #[gpui::test]
    async fn test_a_missing_server_is_installed_before_it_starts(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let net = net_serving(fixture.archive.clone());

        let binary = fixture.command(cx, &net, today()).await.unwrap();
        assert_eq!(net.requests(), 1);
        assert_eq!(binary.path, fixture.launcher());
        assert!(binary.path.is_file());
        // The launcher arguments and environment are the pin's, data sharing off included.
        assert_eq!(
            binary.arguments,
            PINNED_ARGS.map(std::ffi::OsString::from).to_vec()
        );
        assert_eq!(
            binary
                .env
                .as_ref()
                .and_then(|env| env.get("INTELLIJ_UPDATES_ENABLED"))
                .map(String::as_str),
            Some("false")
        );
        assert_eq!(fixture.status(cx), Some(ServerStatus::Starting));
        let log = fs::read_to_string(fixture.dirs.logs_dir.join("kotlin.log")).unwrap();
        assert!(log.contains("install "), "{log}");
        assert!(log.contains(" installing "), "{log}");
        let mut siblings: Vec<_> = fs::read_dir(fixture.server_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".lock"))
            .collect();
        siblings.sort();
        assert_eq!(siblings, ["1.2.3"], "no .download or .staging left");

        // Installed: the next start needs no network at all, and the failing client is never asked.
        let down = net_down();
        let again = fixture.command(cx, &down, today()).await.unwrap();
        assert_eq!(again.path, binary.path);
        assert_eq!(down.requests(), 0);
    }

    #[gpui::test]
    async fn test_the_import_jdk_becomes_java_home(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let net = net_serving(fixture.archive.clone());
        fs::create_dir_all(fixture.root.join("tools/jdk")).unwrap();
        fs::write(
            fixture.root.join("tools/jdk/release"),
            "JAVA_VERSION=\"21.0.2\"\n",
        )
        .unwrap();
        // A relative `jdk_home` resolves against the worktree root.
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(r#"{ "kotlin": { "jdk_home": "tools/jdk" } }"#, cx)
                    .unwrap();
            });
        });

        let binary = fixture.command(cx, &net, today()).await.unwrap();
        let env = binary.env.as_ref().unwrap();
        assert_eq!(
            env.get("JAVA_HOME").map(String::as_str),
            Some(fixture.root.join("tools/jdk").to_str().unwrap())
        );
        // The pin's own variables stay.
        assert_eq!(
            env.get("INTELLIJ_UPDATES_ENABLED").map(String::as_str),
            Some("false")
        );
        let log = fs::read_to_string(fixture.dirs.logs_dir.join("kotlin.log")).unwrap();
        assert!(log.contains("kotlin.jdk_home in settings"), "{log}");
    }

    #[gpui::test]
    async fn test_an_unmet_toolchain_fails_before_any_download(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let net = net_serving(fixture.archive.clone());
        // No machine has a JDK 99.
        fs::write(
            fixture.root.join("build.gradle.kts"),
            "kotlin { jvmToolchain(99) }",
        )
        .unwrap();

        let error = fixture.command(cx, &net, today()).await.unwrap_err();
        assert_eq!(error.to_string(), "no JDK for Gradle import (toolchain 99)");
        assert_eq!(net.requests(), 0);
        assert_eq!(
            fixture.status(cx),
            Some(ServerStatus::Failed {
                reason: "no JDK for Gradle import (toolchain 99)".into()
            })
        );
    }

    /// The manager's host starts whatever `server_command` returns, as Zed does after `restart`.
    /// Its fields are filled in once the fixture exists.
    #[derive(Default)]
    struct StartingHost {
        fixture: std::cell::RefCell<Option<(KotlinDirs, Manifest, PathBuf)>>,
        http: std::cell::RefCell<Option<Arc<dyn HttpClient>>>,
        started: Rc<std::cell::RefCell<Vec<PathBuf>>>,
        manager: std::cell::RefCell<Option<gpui::WeakEntity<KotlinProjectManager>>>,
    }

    impl KotlinHost for StartingHost {
        fn restart(&self, worktree_id: WorktreeId, _: Option<lsp::LanguageServerId>, cx: &mut App) {
            let (dirs, manifest, root) = self.fixture.borrow().clone().unwrap();
            let http = self.http.borrow().clone().unwrap();
            let started = self.started.clone();
            let manager = self.manager.borrow().clone().unwrap();
            cx.spawn(async move |cx| {
                let command =
                    server_command(&dirs, &manifest, http, &root, worktree_id, today(), cx).await;
                if let Ok(binary) = command {
                    started.borrow_mut().push(binary.path);
                    manager
                        .update(cx, |manager, cx| {
                            manager.server_added(lsp::LanguageServerId(1), worktree_id, cx)
                        })
                        .ok();
                }
            })
            .detach();
        }
    }

    #[gpui::test]
    async fn test_the_host_starts_the_installed_server(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let host = Rc::new(StartingHost::default());
        let fixture = fixture(cx, host.clone(), "2026-11-12");
        let net = net_serving(fixture.archive.clone());
        *host.fixture.borrow_mut() = Some((
            fixture.dirs.clone(),
            fixture.manifest.clone(),
            fixture.root.clone(),
        ));
        *host.http.borrow_mut() = Some(net.client.clone());
        *host.manager.borrow_mut() = Some(fixture.manager.downgrade());

        fixture
            .manager
            .update(cx, |manager, cx| manager.restart_server(worktree(), cx));
        wait_until(cx, "the server to start", |_| {
            !host.started.borrow().is_empty()
        });
        cx.run_until_parked();
        assert_eq!(*host.started.borrow(), [fixture.launcher()]);
        assert_eq!(fixture.status(cx), Some(ServerStatus::Ready));
        assert_eq!(net.requests(), 1);
    }

    #[gpui::test]
    async fn test_an_installed_server_starts_when_the_network_fails(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        fixture
            .command(cx, &net_serving(fixture.archive.clone()), today())
            .await
            .unwrap();

        let down = net_down();
        let binary = fixture.command(cx, &down, today()).await.unwrap();
        assert_eq!(binary.path, fixture.launcher());
        assert_eq!(down.requests(), 0);
        assert_eq!(fixture.status(cx), Some(ServerStatus::Starting));

        // Without the install the same network failure fails the start, naming the install.
        fs::remove_dir_all(fixture.server_dir()).unwrap();
        let error = fixture.command(cx, &down, today()).await.unwrap_err();
        assert!(error.to_string().contains("downloading"), "{error:#}");
        assert_eq!(down.requests(), 1);
        let Some(ServerStatus::Failed { reason }) = fixture.status(cx) else {
            panic!("expected Failed, got {:?}", fixture.status(cx));
        };
        assert!(reason.starts_with("install failed: "), "{reason}");
        assert!(!fixture.server_dir().join(".1.2.3.download").exists());
    }

    #[gpui::test]
    async fn test_cancelling_an_install_fails_it_and_the_next_start_retries(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let endless = net_endless();
        let task = fixture.command(cx, &endless, today());

        wait_until(cx, "download progress", |cx| {
            matches!(
                fixture.status(cx),
                Some(ServerStatus::Installing { done, .. }) if done >= crate::PROGRESS_STEP
            )
        });
        assert!(fixture.server_dir().join(".1.2.3.download").exists());
        assert!(
            fixture
                .manager
                .update(cx, |manager, _| manager.cancel_install(worktree()))
        );

        let error = task.await.unwrap_err();
        assert_eq!(error.to_string(), "install cancelled");
        assert_eq!(
            fixture.status(cx),
            Some(ServerStatus::Failed {
                reason: "install cancelled".into()
            })
        );
        assert!(!fixture.server_dir().join(".1.2.3.download").exists());
        assert!(!fixture.server_dir().join(".1.2.3.staging").exists());
        assert!(!fixture.server_dir().join("1.2.3").exists());

        // The next start begins again from zero and succeeds.
        let good = net_serving(fixture.archive.clone());
        let binary = fixture.command(cx, &good, today()).await.unwrap();
        assert_eq!(binary.path, fixture.launcher());
        assert_eq!(good.requests(), 1);
        assert_eq!(fixture.status(cx), Some(ServerStatus::Starting));
    }

    #[gpui::test]
    async fn test_two_projects_starting_at_once_install_once(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let second_root = fixture.root.parent().unwrap().join("project2");
        fs::create_dir_all(&second_root).unwrap();
        fixture.manager.update(cx, |manager, cx| {
            manager.add_worktree(WorktreeId::from_usize(2), second_root.clone(), cx)
        });
        let net = net_serving(fixture.archive.clone());

        let first = fixture.command(cx, &net, today());
        let second = {
            let (dirs, manifest, http) = (
                fixture.dirs.clone(),
                fixture.manifest.clone(),
                net.client.clone(),
            );
            cx.update(|cx| {
                cx.spawn(async move |cx| {
                    server_command(
                        &dirs,
                        &manifest,
                        http,
                        &second_root,
                        WorktreeId::from_usize(2),
                        today(),
                        cx,
                    )
                    .await
                })
            })
        };
        let (first, second) = futures::future::join(first, second).await;
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first.path, second.path);
        assert_eq!(
            net.requests(),
            1,
            "the second start downloaded the server again"
        );
        let installed = crate::check_installed(
            &fixture.dirs.install_root,
            KotlinServerKind::KotlinLsp,
            &fixture.entry,
        );
        assert!(installed.is_some());
    }

    #[gpui::test]
    async fn test_an_expired_pin_is_not_installed_or_started(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let net = net_serving(fixture.archive.clone());
        let expired_status = Some(ServerStatus::Failed {
            reason: "server expired, update bongos".into(),
        });

        // The day after `expires`: failed, with no download and nothing on disk.
        let day_after = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap();
        let error = fixture.command(cx, &net, day_after).await.unwrap_err();
        assert_eq!(error.to_string(), "server expired, update bongos");
        assert_eq!(fixture.status(cx), expired_status);
        assert_eq!(net.requests(), 0);
        assert!(!fixture.server_dir().exists());
    }

    /// With `kotlin.manifest_url` empty, a start reads no cache or state and runs the pin, even when
    /// `state.json` names an installed downloaded build.
    #[gpui::test]
    async fn test_without_a_manifest_url_the_pin_starts(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2027-01-01");
        let downloaded = ManifestEntry {
            version: "9.9.9".into(),
            ..fixture.entry.clone()
        };
        crate::installed::tests::install(&fixture.dirs.install_root, &downloaded);
        let manifest_dir = fixture.dirs.install_root.join(crate::locks::MANIFEST_DIR);
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(
            manifest_dir.join(crate::manifest_cache::STATE_FILE),
            serde_json::json!({
                "version": 1,
                "kotlin-lsp": {
                    "active": { "version": "9.9.9", "sha256": downloaded.sha256 },
                    "trial": { "version": "9.9.9", "sha256": downloaded.sha256 },
                },
            })
            .to_string(),
        )
        .unwrap();
        let net = net_serving(fixture.archive.clone());

        let binary = fixture.command(cx, &net, today()).await.unwrap();
        assert_eq!(binary.path, fixture.launcher());
        assert_eq!(net.requests(), 1);
    }

    /// `expires` is the last day the build runs; the day after, even an installed server is not
    /// started.
    #[gpui::test]
    async fn test_expires_is_the_last_day_the_build_runs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let net = net_serving(fixture.archive.clone());
        let last_day = NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
        let day_after = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap();

        fixture.command(cx, &net, last_day).await.unwrap();
        assert_eq!(fixture.status(cx), Some(ServerStatus::Starting));
        assert_eq!(net.requests(), 1);

        let error = fixture.command(cx, &net, day_after).await.unwrap_err();
        assert_eq!(error.to_string(), "server expired, update bongos");
        assert_eq!(
            fixture.status(cx),
            Some(ServerStatus::Failed {
                reason: "server expired, update bongos".into()
            })
        );
        assert_eq!(net.requests(), 1);
    }

    /// Exit code 7 or the startup message marks the server expired for the run, even when the date
    /// says the build is still good (the machine's clock may be wrong).
    #[gpui::test]
    async fn test_a_server_that_reported_expiry_is_not_started_again(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let net = net_serving(fixture.archive.clone());
        fixture.command(cx, &net, today()).await.unwrap();

        fixture.manager.update(cx, |manager, cx| {
            manager.startup_stderr(worktree(), "This build of kotlin-server has expired.\n", cx)
        });
        let down = net_down();
        let error = fixture.command(cx, &down, today()).await.unwrap_err();
        assert_eq!(error.to_string(), "server expired, update bongos");
        assert_eq!(
            fixture.status(cx),
            Some(ServerStatus::Failed {
                reason: "server expired, update bongos".into()
            })
        );
        assert_eq!(down.requests(), 0);
    }

    fn set_user_settings(cx: &mut TestAppContext, json: &str) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.set_user_settings(json, cx).unwrap();
            });
        });
    }

    const INTELLIJ_OFF: &str = r#"{ "kotlin": { "server": "intellij" } }"#;
    const INTELLIJ_ON: &str =
        r#"{ "kotlin": { "server": "intellij", "intellij": { "download": true } } }"#;
    const EULA: &str = "FAKE JETBRAINS USER AGREEMENT\nVersion 2.0\n";

    /// An IntelliJ pin for a small archive: a `EULA.txt` and a launcher that records that it ran
    /// and exits with the code in the `license-exit` file next to the state directory.
    fn intellij_fixture(cx: &mut TestAppContext) -> Fixture {
        let mut fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let scratch = fixture.dirs.state_dir.parent().unwrap().to_path_buf();
        let launcher = format!(
            "#!/bin/sh\necho ran >> '{ran}'\nexit $(cat '{exit}')\n",
            ran = scratch.join("license-ran").display(),
            exit = scratch.join("license-exit").display(),
        );
        fixture.archive = crate::install::tests::tar_gz(&[
            (
                "server-1.2.3/EULA.txt".to_string(),
                0o644,
                EULA.as_bytes().to_vec(),
            ),
            (
                "server-1.2.3/bin/launcher".to_string(),
                0o644,
                launcher.into_bytes(),
            ),
        ]);
        fixture.manifest = Manifest::parse(&format!(
            r#"
[kotlin-lsp]
version = "1.2.3"
expires = "2026-11-12"
args = {args:?}

[kotlin-lsp.platform.{platform}]
url = "http://127.0.0.1:1/kotlin-server.tar.gz"
sha256 = "{lsp_sha}"
archive = "tar.gz"
strip_components = 1
launcher = "bin/launcher"

[intellij]
version = "1.2.3"
args = {args:?}

[intellij.env]
INTELLIJ_UPDATES_ENABLED = "false"

[intellij.platform.{platform}]
url = "http://127.0.0.1:1/intellij-server.tar.gz"
sha256 = "{sha}"
archive = "tar.gz"
strip_components = 1
launcher = "bin/launcher"
"#,
            args = PINNED_ARGS,
            platform = current_platform().unwrap(),
            sha = crate::install::tests::sha256(&fixture.archive),
            lsp_sha = "0".repeat(64),
        ))
        .unwrap();
        fixture.entry = fixture
            .manifest
            .entry(KotlinServerKind::Intellij, current_platform().unwrap())
            .unwrap();
        fixture.set_license_exit(0);
        set_user_settings(cx, INTELLIJ_OFF);
        fixture
    }

    impl Fixture {
        fn scratch(&self) -> PathBuf {
            self.dirs.state_dir.parent().unwrap().to_path_buf()
        }

        fn set_license_exit(&self, code: i32) {
            fs::write(self.scratch().join("license-exit"), code.to_string()).unwrap();
        }

        /// How many times the launcher ran (`license status` runs it).
        fn license_runs(&self) -> usize {
            fs::read_to_string(self.scratch().join("license-ran"))
                .map(|text| text.lines().count())
                .unwrap_or(0)
        }

        fn intellij_dir(&self) -> PathBuf {
            self.dirs.install_root.join("intellij")
        }

        /// kotlin-lsp, installed, for the start that falls back to it.
        fn preinstall_kotlin_lsp(&self) -> PathBuf {
            install(&self.dirs, &self.manifest, KotlinServerKind::KotlinLsp)
        }

        fn fallback(&self, cx: &mut TestAppContext) -> Option<FallbackReason> {
            self.manager.read_with(cx, |manager, _| {
                manager
                    .worktree(worktree())
                    .and_then(|state| state.fallback.as_ref())
                    .map(|fallback| fallback.kind)
            })
        }

        /// `kotlin: restart server`: the fallback ends and the next start tries IntelliJ again.
        fn end_fallback(&self, cx: &mut TestAppContext) {
            self.manager
                .update(cx, |manager, cx| manager.restart_server(worktree(), cx));
        }
    }

    #[gpui::test]
    async fn test_a_missing_intellij_server_is_not_downloaded_unless_the_setting_is_on(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let fixture = intellij_fixture(cx);
        let net = net_serving(fixture.archive.clone());

        // Off (the default): "intellij is not installed", the way out in the message, no request.
        let error = fixture.command(cx, &net, today()).await.unwrap_err();
        assert_eq!(error.to_string(), INTELLIJ_DOWNLOAD_HINT);
        assert!(error.to_string().contains("kotlin.intellij.download"));
        assert!(error.to_string().contains("lsp.kotlin.binary.path"));
        assert_eq!(
            fixture.status(cx),
            Some(ServerStatus::Failed {
                reason: INTELLIJ_DOWNLOAD_HINT.into()
            })
        );
        assert_eq!(
            net.requests(),
            0,
            "the download started with the setting off"
        );
        assert!(!fixture.intellij_dir().exists());

        // Not installed with the download off is no fallback: there is nothing to fall back from.
        assert_eq!(fixture.fallback(cx), None);

        // On: the download starts, the bundle is installed, then the EULA gate stops IntelliJ and
        // the same start runs kotlin-lsp.
        let kotlin_lsp = fixture.preinstall_kotlin_lsp();
        set_user_settings(cx, INTELLIJ_ON);
        let binary = fixture.command(cx, &net, today()).await.unwrap();
        assert_eq!(binary.path, kotlin_lsp);
        assert_eq!(fixture.fallback(cx), Some(FallbackReason::EulaNotAccepted));
        assert_eq!(net.requests(), 1);
        assert!(fixture.intellij_dir().join("1.2.3/EULA.txt").is_file());
    }

    #[gpui::test]
    async fn test_a_project_can_turn_the_intellij_download_on(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = intellij_fixture(cx);
        let net = net_serving(fixture.archive.clone());
        fixture.preinstall_kotlin_lsp();
        // Off for the user, on in the project's own settings.
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        worktree(),
                        settings::LocalSettingsPath::InWorktree(RelPath::empty_arc()),
                        settings::LocalSettingsKind::Settings,
                        Some(r#"{ "kotlin": { "intellij": { "download": true } } }"#),
                        cx,
                    )
                    .unwrap();
            });
        });
        fixture.command(cx, &net, today()).await.unwrap();
        assert_eq!(net.requests(), 1);
    }

    /// A start that the IntelliJ gates refuse for `reason`: it falls back and returns kotlin-lsp.
    /// Ends the fallback afterwards, as `kotlin: restart server` does.
    async fn falls_back(
        fixture: &Fixture,
        net: &Net,
        kotlin_lsp: &Path,
        reason: FallbackReason,
        cx: &mut TestAppContext,
    ) {
        let binary = fixture.command(cx, net, today()).await.unwrap();
        assert_eq!(binary.path, kotlin_lsp, "{reason:?}");
        assert_eq!(fixture.fallback(cx), Some(reason));
        assert_eq!(fixture.status(cx), Some(ServerStatus::Starting));
        fixture.end_fallback(cx);
    }

    #[gpui::test]
    async fn test_intellij_starts_only_after_the_eula_and_the_license_check(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let fixture = intellij_fixture(cx);
        set_user_settings(cx, INTELLIJ_ON);
        let net = net_serving(fixture.archive.clone());
        let kotlin_lsp = fixture.preinstall_kotlin_lsp();
        // Installed, not accepted: no IntelliJ start, and `license status` was not run.
        falls_back(
            &fixture,
            &net,
            &kotlin_lsp,
            FallbackReason::EulaNotAccepted,
            cx,
        )
        .await;
        assert_eq!(fixture.license_runs(), 0);

        // Accepting another agreement's hash does not count.
        crate::accept_eula(&fixture.dirs.state_dir, &crate::eula_hash(b"another text")).unwrap();
        falls_back(
            &fixture,
            &net,
            &kotlin_lsp,
            FallbackReason::EulaNotAccepted,
            cx,
        )
        .await;
        assert_eq!(fixture.license_runs(), 0);

        // Accepted but unlicensed, then offline: the fallback names the reason.
        let hash = crate::eula_hash(EULA.as_bytes());
        crate::accept_eula(&fixture.dirs.state_dir, &hash).unwrap();
        fixture.set_license_exit(2);
        falls_back(&fixture, &net, &kotlin_lsp, FallbackReason::Unlicensed, cx).await;
        fixture.set_license_exit(3);
        falls_back(
            &fixture,
            &net,
            &kotlin_lsp,
            FallbackReason::LicenseCheckFailed,
            cx,
        )
        .await;
        assert_eq!(fixture.license_runs(), 2);
        let log = fs::read_to_string(fixture.dirs.logs_dir.join("kotlin.log")).unwrap();
        assert!(
            log.contains("intellij -> kotlin-lsp: IntelliJ unlicensed (license Unlicensed)"),
            "{log}"
        );

        // Licensed: the pinned arguments and the accepted hash. The installed server needs no
        // download, so the setting can be off again.
        fixture.set_license_exit(0);
        set_user_settings(cx, INTELLIJ_OFF);
        let binary = fixture.command(cx, &net, today()).await.unwrap();
        let mut expected: Vec<std::ffi::OsString> =
            PINNED_ARGS.map(std::ffi::OsString::from).to_vec();
        expected.push(format!("--eula={hash}").into());
        assert_eq!(binary.arguments, expected);
        assert_eq!(
            binary.path,
            fixture.intellij_dir().join("1.2.3/bin/launcher")
        );
        assert_eq!(fixture.status(cx), Some(ServerStatus::Starting));
        assert_eq!(fixture.fallback(cx), None);
        assert_eq!(fixture.license_runs(), 3);
        assert_eq!(net.requests(), 1, "only the first start downloaded");
        let log = fs::read_to_string(fixture.dirs.logs_dir.join("kotlin.log")).unwrap();
        assert!(log.contains("license licensed"), "{log}");
    }

    #[gpui::test]
    async fn test_a_failed_intellij_install_falls_back(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = intellij_fixture(cx);
        let kotlin_lsp = fixture.preinstall_kotlin_lsp();
        set_user_settings(cx, INTELLIJ_ON);
        let down = net_down();
        let binary = fixture.command(cx, &down, today()).await.unwrap();
        assert_eq!(binary.path, kotlin_lsp);
        assert_eq!(fixture.fallback(cx), Some(FallbackReason::InstallFailed));
        assert_eq!(fixture.status(cx), Some(ServerStatus::Starting));
        assert_eq!(down.requests(), 1);
        let log = fs::read_to_string(fixture.dirs.logs_dir.join("kotlin.log")).unwrap();
        assert!(
            log.contains("IntelliJ install failed (install error: "),
            "{log}"
        );
    }

    /// kotlin-lsp fails while it stands in for IntelliJ: no server at all, and the status says why
    /// for both.
    #[gpui::test]
    async fn test_kotlin_lsp_failing_after_a_fallback_is_both_failed(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = intellij_fixture(cx);
        set_user_settings(cx, INTELLIJ_ON);
        // The IntelliJ bundle downloads; kotlin-lsp gets the same bytes, which fail its digest.
        let net = net_serving(fixture.archive.clone());
        fixture.command(cx, &net, today()).await.unwrap_err();
        assert_eq!(fixture.fallback(cx), Some(FallbackReason::EulaNotAccepted));
        let Some(ServerStatus::BothFailed {
            intellij,
            kotlin_lsp,
        }) = fixture.status(cx)
        else {
            panic!("expected BothFailed, got {:?}", fixture.status(cx));
        };
        assert_eq!(intellij, "IntelliJ EULA not accepted");
        assert!(kotlin_lsp.starts_with("install failed: "), "{kotlin_lsp}");
        assert_eq!(net.requests(), 2);
    }

    /// Starts an install that never ends, drops the start, and returns the status afterwards.
    async fn status_after_dropping_a_start_mid_install(
        cx: &mut TestAppContext,
        fixture: &Fixture,
    ) -> Option<ServerStatus> {
        let endless = net_endless();
        let task = fixture.command(cx, &endless, today());
        wait_until(cx, "download progress", |cx| {
            matches!(fixture.status(cx), Some(ServerStatus::Installing { .. }))
        });
        drop(task);
        cx.run_until_parked();
        fixture.status(cx)
    }

    #[gpui::test]
    async fn test_dropping_a_kotlin_lsp_start_mid_install_ends_the_install(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let status = status_after_dropping_a_start_mid_install(cx, &fixture).await;
        assert_eq!(
            status,
            Some(ServerStatus::Failed {
                reason: "install interrupted".into()
            })
        );
        // The next start installs from zero.
        let good = net_serving(fixture.archive.clone());
        fixture.command(cx, &good, today()).await.unwrap();
        assert_eq!(fixture.status(cx), Some(ServerStatus::Starting));
    }

    #[gpui::test]
    async fn test_dropping_an_intellij_start_mid_install_ends_the_install(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = intellij_fixture(cx);
        set_user_settings(cx, INTELLIJ_ON);
        let status = status_after_dropping_a_start_mid_install(cx, &fixture).await;
        assert_eq!(
            status,
            Some(ServerStatus::Failed {
                reason: "install interrupted".into()
            })
        );
    }

    #[gpui::test]
    async fn test_a_dropped_start_does_not_end_a_newer_install(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        let endless = net_endless();
        let first = fixture.command(cx, &endless, today());
        wait_until(cx, "the first download", |cx| {
            matches!(fixture.status(cx), Some(ServerStatus::Installing { .. }))
        });
        // The manager moves to a second install; the first start's guard must leave it alone.
        let second_cancel = fixture
            .manager
            .update(cx, |manager, cx| manager.begin_install(worktree(), cx));
        drop(first);
        cx.run_until_parked();
        assert!(matches!(
            fixture.status(cx),
            Some(ServerStatus::Installing { .. })
        ));
        assert!(!second_cancel.load(Ordering::SeqCst));
    }

    #[gpui::test]
    async fn test_start_without_a_manager_installs_from_the_pin(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            KotlinSettings::register(cx);
        });
        let temp = tempfile::tempdir().unwrap();
        let dirs = KotlinDirs {
            state_dir: temp.path().join("state"),
            logs_dir: temp.path().join("logs"),
            install_root: temp.path().join("languages"),
        };
        let root = temp.path().to_path_buf();
        let manifest = unexpired_embedded();
        let net = net_with(|| {
            Ok(http_client::Response::builder()
                .status(404)
                .body(http_client::AsyncBody::empty())
                .unwrap())
        });
        let http = net.client.clone();
        let task = cx.update(|cx| {
            cx.spawn(async move |cx| {
                server_command(&dirs, &manifest, http, &root, worktree(), today(), cx).await
            })
        });
        let error = task.await.unwrap_err();
        assert!(error.to_string().contains("404"), "{error:#}");
        assert_eq!(net.requests(), 1);
    }

    struct StubDelegate {
        root: std::path::PathBuf,
    }

    #[async_trait]
    impl LspAdapterDelegate for StubDelegate {
        fn show_notification(&self, _: &str, _: &mut App) {}
        fn http_client(&self) -> Arc<dyn http_client::HttpClient> {
            http_client::FakeHttpClient::with_404_response()
        }
        fn worktree_id(&self) -> WorktreeId {
            worktree()
        }
        fn worktree_root_path(&self) -> &Path {
            &self.root
        }
        fn resolve_relative_path(&self, path: std::path::PathBuf) -> std::path::PathBuf {
            path
        }
        fn update_status(&self, _: LanguageServerName, _: language::BinaryStatus) {}
        fn registered_lsp_adapters(&self) -> Vec<Arc<dyn LspAdapter>> {
            Vec::new()
        }
        async fn language_server_download_dir(&self, _: &LanguageServerName) -> Option<Arc<Path>> {
            None
        }
        async fn npm_package_installed_version(
            &self,
            _: &str,
        ) -> Result<Option<(std::path::PathBuf, semver::Version)>> {
            Ok(None)
        }
        async fn which(&self, _: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
            None
        }
        async fn shell_env(&self) -> collections::HashMap<String, String> {
            Default::default()
        }
        async fn read_text_file(&self, _: &util::rel_path::RelPath) -> Result<String> {
            bail!("no files")
        }
        async fn try_exec(&self, _: LanguageServerBinary) -> Result<()> {
            Ok(())
        }
    }

    /// Lays out the pinned build of `server` the way the installer leaves it; returns its launcher.
    fn install(dirs: &KotlinDirs, manifest: &Manifest, server: KotlinServerKind) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let entry = manifest.entry(server, current_platform().unwrap()).unwrap();
        let dir = dirs
            .install_root
            .join(server_name(server))
            .join(&entry.version);
        let launcher = dir.join(&entry.launcher);
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(&launcher, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join(crate::EULA_FILE), EULA).unwrap();
        let marker = Marker {
            version: entry.version,
            sha256: entry.sha256,
        };
        fs::write(
            dir.join(INSTALLED_MARKER),
            serde_json::to_string(&marker).unwrap(),
        )
        .unwrap();
        launcher
    }

    /// The blanket installer would cache the first binary and start it again after a switch.
    #[gpui::test]
    async fn test_every_start_resolves_the_server_again(cx: &mut TestAppContext) {
        // The license check runs a real process under a timer of the test clock.
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
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
        let manifest = unexpired_embedded();
        let kotlin_lsp = install(&dirs, &manifest, KotlinServerKind::KotlinLsp);
        let intellij = install(&dirs, &manifest, KotlinServerKind::Intellij);
        // The user accepted the bundle's agreement; the launcher exits 0 for `license status`.
        crate::accept_eula(&dirs.state_dir, &crate::eula_hash(EULA.as_bytes())).unwrap();
        let manager = cx.new(|cx| KotlinProjectManager::new(Rc::new(NoHost), dirs.clone(), cx));
        manager.update(cx, |manager, cx| {
            manager.add_worktree(worktree(), root.clone(), cx)
        });

        let mut adapter = KotlinAdapter::new(dirs, Arc::new(manifest));
        adapter.pinned_today = Some(today());
        let adapter = Arc::new(adapter);
        let cached = Arc::new(futures::lock::Mutex::new(None));
        let start = |cx: &mut TestAppContext| {
            let (adapter, cached, root) = (adapter.clone(), cached.clone(), root.clone());
            cx.update(|cx| {
                cx.spawn(async move |cx| {
                    let delegate: Arc<dyn LspAdapterDelegate> = Arc::new(StubDelegate { root });
                    let guard = cached.lock_owned().await;
                    let options = LanguageServerBinaryOptions {
                        allow_path_lookup: true,
                        allow_binary_download: true,
                        pre_release: false,
                    };
                    adapter
                        .get_language_server_command(delegate, None, options, guard, cx.clone())
                        .await
                        .0
                })
            })
        };

        assert_eq!(start(cx).await.unwrap().path, kotlin_lsp);
        manager
            .update(cx, |manager, cx| {
                manager.switch_server(worktree(), Some(KotlinServerKind::Intellij), cx)
            })
            .unwrap();
        assert_eq!(start(cx).await.unwrap().path, intellij);
        manager
            .update(cx, |manager, cx| {
                manager.switch_server(worktree(), None, cx)
            })
            .unwrap();
        assert_eq!(start(cx).await.unwrap().path, kotlin_lsp);
    }

    const MANIFEST_URL: &str = "https://manifests.example.com/kotlin/servers.json";

    fn build_id(entry: &ManifestEntry) -> BuildId {
        BuildId {
            version: entry.version.clone(),
            sha256: entry.sha256.clone(),
        }
    }

    /// A verified cached manifest listing an active build (9.0.0) and a pending trial (9.1.0), both
    /// installed and named in `state.json`, with `kotlin.manifest_url` set and the test key trusted.
    /// Returns the active build and the trial.
    fn seed_trial(cx: &mut TestAppContext, fixture: &Fixture) -> (ManifestEntry, ManifestEntry) {
        use crate::manifest_cache::{
            Claim, ManifestCache,
            tests::{key_pair, now, release_keys, signed_with},
        };
        set_user_settings(
            cx,
            &format!(r#"{{ "kotlin": {{ "manifest_url": "{MANIFEST_URL}" }} }}"#),
        );
        cx.update(|cx| cx.set_global(ManifestKeys(release_keys())));
        let platform = current_platform().unwrap();
        let builds: Vec<serde_json::Value> = [("9.0.0", "a"), ("9.1.0", "b")]
            .iter()
            .map(|(version, digit)| {
                serde_json::json!({
                    "version": version, "built": "2026-11-01", "expires": "2027-01-01",
                    "requires": 1,
                    "platform": { platform: {
                        "url": format!("https://download.jetbrains.com/kotlin-server-{version}.tar.gz"),
                        "sha256": digit.repeat(64), "archive": "tar.gz", "strip_components": 1,
                        "launcher": "bin/launcher"
                    } }
                })
            })
            .collect();
        let servers = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "kind": "bongos-kotlin-servers", "serial": 1,
            "published": "2026-11-01", "kotlin-lsp": builds
        }))
        .unwrap();
        let url = http_client::Url::parse(MANIFEST_URL).unwrap();
        let cache = ManifestCache::new(
            &fixture.dirs.install_root,
            crate::locks::tests::thread_registry(),
        );
        let Claim::Claimed(lease) = futures::executor::block_on(cache.claim_fetch(
            now(),
            std::time::Duration::from_secs(60),
            true,
        ))
        .unwrap() else {
            panic!("no claim");
        };
        futures::executor::block_on(cache.finish_fetch(
            lease,
            now(),
            Ok(signed_with(servers, "release-1", &key_pair(7))),
            &url,
            &release_keys(),
        ))
        .unwrap();
        let cached = cache.load(&url, &release_keys()).unwrap();
        let entries: Vec<ManifestEntry> = cached
            .manifest
            .kotlin_lsp
            .iter()
            .filter_map(|build| build.entry(platform).cloned())
            .collect();
        let (active, trial) = (entries[0].clone(), entries[1].clone());
        for entry in [&active, &trial] {
            crate::installed::tests::install(&fixture.dirs.install_root, entry);
        }
        futures::executor::block_on(cache.with_state(|state| {
            state.kotlin_lsp.active = Some(build_id(&active));
            state.kotlin_lsp.trial = Some(build_id(&trial));
        }))
        .unwrap();
        (active, trial)
    }

    impl Fixture {
        fn builds(&self) -> crate::manifest_cache::KotlinLspBuilds {
            crate::manifest_cache::ManifestCache::new(
                &self.dirs.install_root,
                crate::locks::tests::thread_registry(),
            )
            .read_state()
            .kotlin_lsp
        }

        fn launcher_of(&self, entry: &ManifestEntry) -> PathBuf {
            self.server_dir().join(&entry.version).join("bin/launcher")
        }

        fn launched_trial(&self, cx: &mut TestAppContext) -> Option<BuildId> {
            self.manager
                .read_with(cx, |manager, _| manager.launched_trial(worktree()).cloned())
        }

        fn trial_notices(&self, cx: &mut TestAppContext) -> Rc<std::cell::RefCell<Vec<String>>> {
            let notices = Rc::new(std::cell::RefCell::new(Vec::new()));
            cx.update(|cx| {
                let notices = notices.clone();
                cx.subscribe(
                    &self.manager,
                    move |_, event: &crate::manager::KotlinEvent, _| {
                        if let crate::manager::KotlinEvent::TrialFailed { message, .. } = event {
                            notices.borrow_mut().push(message.clone());
                        }
                    },
                )
                .detach();
            });
            notices
        }
    }

    /// A start that fails before the trial's process is launched (no JDK satisfies the Gradle
    /// toolchain) is a project problem: the trial is not rejected, no notice shows, and a later
    /// start whose JDK resolves runs the trial.
    #[gpui::test]
    async fn test_a_jdk_failure_before_launch_leaves_the_trial_pending(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2027-01-01");
        let (_, trial) = seed_trial(cx, &fixture);
        let notices = fixture.trial_notices(cx);
        let gradle = fixture.root.join("build.gradle.kts");
        fs::write(&gradle, "kotlin { jvmToolchain(99) }").unwrap();
        let down = net_down();

        let error = fixture.command(cx, &down, today()).await.unwrap_err();
        assert_eq!(error.to_string(), "no JDK for Gradle import (toolchain 99)");
        assert_eq!(
            fixture.status(cx),
            Some(ServerStatus::Failed {
                reason: "no JDK for Gradle import (toolchain 99)".into()
            })
        );
        assert_eq!(fixture.launched_trial(cx), None);
        // Zed reports the refused start as a start failure too; it is still not the trial's.
        fixture.manager.update(cx, |manager, cx| {
            manager.server_start_failed(worktree(), "no JDK for Gradle import (toolchain 99)", cx)
        });
        cx.run_until_parked();
        assert_eq!(fixture.builds().trial, Some(build_id(&trial)));
        assert!(fixture.builds().rejected.is_empty());
        assert!(notices.borrow().is_empty());

        fs::remove_file(&gradle).unwrap();
        let binary = fixture.command(cx, &down, today()).await.unwrap();
        assert_eq!(binary.path, fixture.launcher_of(&trial));
        assert_eq!(fixture.launched_trial(cx), Some(build_id(&trial)));
        assert_eq!(down.requests(), 0);
    }

    /// A trial that exits 2 s after it was added is rejected and the worktree restarts with the
    /// previous (active) build. The rejected build is not started again: neither by a later start
    /// in this run nor after a relaunch, which reads `rejected` from `state.json`.
    #[gpui::test]
    async fn test_a_rejected_trial_restarts_the_previous_build_and_is_never_retried(
        cx: &mut TestAppContext,
    ) {
        use std::os::unix::process::ExitStatusExt as _;
        cx.executor().allow_parking();
        let host = Rc::new(StartingHost::default());
        let fixture = fixture(cx, host.clone(), "2027-01-01");
        let (active, trial) = seed_trial(cx, &fixture);
        let down = net_down();
        *host.fixture.borrow_mut() = Some((
            fixture.dirs.clone(),
            fixture.manifest.clone(),
            fixture.root.clone(),
        ));
        *host.http.borrow_mut() = Some(down.client.clone());
        *host.manager.borrow_mut() = Some(fixture.manager.downgrade());
        let notices = fixture.trial_notices(cx);

        let binary = fixture.command(cx, &down, today()).await.unwrap();
        assert_eq!(binary.path, fixture.launcher_of(&trial));
        let server_id = lsp::LanguageServerId(1);
        fixture.manager.update(cx, |manager, cx| {
            manager.server_added(server_id, worktree(), cx)
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(2));
        fixture.manager.update(cx, |manager, cx| {
            manager.server_terminated(
                server_id,
                Some(std::process::ExitStatus::from_raw(1 << 8)),
                cx,
            )
        });
        wait_until(cx, "the restart", |_| !host.started.borrow().is_empty());
        cx.run_until_parked();
        assert_eq!(*host.started.borrow(), [fixture.launcher_of(&active)]);
        assert_eq!(fixture.status(cx), Some(ServerStatus::Ready));
        assert_eq!(
            *notices.borrow(),
            ["Kotlin: kotlin-lsp 9.1.0 failed its first start; using kotlin-lsp 9.0.0 again"]
        );
        let builds = fixture.builds();
        assert_eq!(builds.rejected, [build_id(&trial)]);
        assert_eq!(builds.active, Some(build_id(&active)));
        assert_eq!(builds.trial, None);

        // A restart in this run.
        let binary = fixture.command(cx, &down, today()).await.unwrap();
        assert_eq!(binary.path, fixture.launcher_of(&active));
        assert_eq!(fixture.launched_trial(cx), None);

        // A relaunch: nothing in memory, a new manager, the trial named again in `state.json`
        // (an older process recorded it); `rejected` still keeps it from running.
        cx.update(|cx| cx.set_global(RejectedTrials::default()));
        futures::executor::block_on(
            crate::manifest_cache::ManifestCache::new(
                &fixture.dirs.install_root,
                crate::locks::tests::thread_registry(),
            )
            .with_state(|state| state.kotlin_lsp.trial = Some(build_id(&trial))),
        )
        .unwrap();
        let relaunched =
            cx.new(|cx| KotlinProjectManager::new(Rc::new(NoHost), fixture.dirs.clone(), cx));
        relaunched.update(cx, |manager, cx| {
            manager.add_worktree(worktree(), fixture.root.clone(), cx)
        });
        let binary = fixture.command(cx, &down, today()).await.unwrap();
        assert_eq!(binary.path, fixture.launcher_of(&active));
        assert!(relaunched.read_with(cx, |manager, _| {
            manager.launched_trial(worktree()).is_none()
        }));
        assert_eq!(notices.borrow().len(), 1);
        assert_eq!(down.requests(), 0);
    }

    /// An expired pin stops the worktree; a verified newer build is then downloaded and recorded,
    /// and the worktree restarts and runs it as a trial. The expired status is never shown again.
    #[gpui::test]
    async fn test_an_expired_pin_then_a_newer_build_restarts_with_the_trial(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let host = Rc::new(StartingHost::default());
        // The pin expired the day before `today()`.
        let fixture = fixture(cx, host.clone(), "2026-11-06");
        let down = net_down();
        *host.fixture.borrow_mut() = Some((
            fixture.dirs.clone(),
            fixture.manifest.clone(),
            fixture.root.clone(),
        ));
        *host.http.borrow_mut() = Some(down.client.clone());
        *host.manager.borrow_mut() = Some(fixture.manager.downgrade());

        let error = fixture.command(cx, &down, today()).await.unwrap_err();
        assert_eq!(error.to_string(), EXPIRED_REASON);
        let expired = Some(ServerStatus::Failed {
            reason: EXPIRED_REASON.into(),
        });
        assert_eq!(fixture.status(cx), expired);

        // The newer build is downloaded and recorded as the trial (and the manifest is cached).
        let (_, trial) = seed_trial(cx, &fixture);
        let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        let _observe = cx.update(|cx| {
            let seen = seen.clone();
            cx.observe(&fixture.manager, move |manager, cx| {
                seen.borrow_mut()
                    .push(manager.read(cx).status(worktree()).cloned());
            })
        });
        fixture.manager.update(cx, |manager, cx| {
            manager.newer_build_ready(&trial.version, cx)
        });
        wait_until(cx, "the restart", |_| !host.started.borrow().is_empty());
        cx.run_until_parked();

        assert_eq!(*host.started.borrow(), [fixture.launcher_of(&trial)]);
        assert_eq!(fixture.launched_trial(cx), Some(build_id(&trial)));
        assert_eq!(fixture.status(cx), Some(ServerStatus::Ready));
        assert!(!seen.borrow().contains(&expired), "{:?}", seen.borrow());
        assert!(!fixture.manager.read_with(cx, |manager, _| {
            manager.is_expired(worktree(), KotlinServerKind::KotlinLsp)
        }));
    }
}
