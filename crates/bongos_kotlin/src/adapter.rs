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
use gpui::{App, AppContext as _, AsyncApp};
use http_client::HttpClient;
use language::{
    DynLspInstaller, LanguageServerBinaryLocations, LspAdapter, LspAdapterDelegate, Toolchain,
};
use lsp::{LanguageServerBinary, LanguageServerBinaryOptions, LanguageServerName};
use settings::{KotlinServerKind, WorktreeId};

use crate::{
    KotlinDirs,
    install::{InstallCancelled, ProgressFn, spawn_ensure_installed},
    installed::{check_installed, not_installed, server_name},
    manager::{ADAPTER_NAME, EXPIRED_REASON, KotlinRegistry},
    manifest::{Manifest, ManifestEntry, current_platform},
    resolve::resolve,
};

pub struct KotlinAdapter {
    dirs: KotlinDirs,
    manifest: Arc<Manifest>,
}

impl KotlinAdapter {
    pub fn new(dirs: KotlinDirs, manifest: Arc<Manifest>) -> Self {
        Self { dirs, manifest }
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
                chrono::Utc::now().date_naive(),
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

/// The command that starts the effective server for a worktree, or why it cannot start.
///
/// An expired pin (decision D18) is neither installed nor started. An installed pin starts without
/// touching `http`. A missing kotlin-lsp is installed first, with `Installing` progress and the
/// manager's cancel flag; the IntelliJ server has no installer yet (step 9), so a missing one is
/// "not installed". `today` is the date the pin's `expires` is compared with.
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
    let entry = match current_platform()
        .ok_or_else(|| anyhow::anyhow!("no {} build for this platform", server_name(server)))
        .and_then(|platform| manifest.entry(server, platform))
    {
        Ok(entry) => entry,
        Err(error) => {
            let reason = error.to_string();
            cx.update(|cx| start_failed(worktree_root, worktree_id, reason, cx));
            return Err(error);
        }
    };

    let expired = cx.update(|cx| {
        let known = manager_of(worktree_root, cx)
            .is_some_and(|manager| manager.read(cx).is_expired(worktree_id, server));
        known || entry.is_expired(today)
    });
    if expired {
        cx.update(|cx| {
            if let Some(manager) = manager_of(worktree_root, cx) {
                manager.update(cx, |manager, cx| {
                    manager.mark_expired(worktree_id, server, cx)
                });
            }
        });
        bail!("{EXPIRED_REASON}");
    }

    let check = {
        let (install_root, entry) = (dirs.install_root.clone(), entry.clone());
        move || check_installed(&install_root, server, &entry)
    };
    if let Some(binary) = cx.background_spawn(async move { check() }).await {
        return Ok(binary);
    }
    if server != KotlinServerKind::KotlinLsp {
        let error = not_installed(server);
        let reason = error.to_string();
        cx.update(|cx| start_failed(worktree_root, worktree_id, reason, cx));
        return Err(error);
    }

    install_server(
        dirs,
        server,
        entry.clone(),
        http,
        worktree_root,
        worktree_id,
        cx,
    )
    .await?;
    check_installed(&dirs.install_root, server, &entry).ok_or_else(|| not_installed(server))
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
            .collect();
        siblings.sort();
        assert_eq!(siblings, ["1.2.3"], "no .download or .staging left");

        // Installed: the next start needs no network at all, and the failing client is never asked.
        let down = net_down();
        let again = fixture.command(cx, &down, today()).await.unwrap();
        assert_eq!(again.path, binary.path);
        assert_eq!(down.requests(), 0);
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
        assert!(!fixture.server_dir().join(".download").exists());
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
        assert!(fixture.server_dir().join(".download").exists());
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
        assert!(!fixture.server_dir().join(".download").exists());
        assert!(!fixture.server_dir().join(".staging").exists());
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

    #[gpui::test]
    async fn test_a_missing_intellij_server_is_not_installed_here(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = fixture(cx, Rc::new(NoHost), "2026-11-12");
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(r#"{ "kotlin": { "server": "intellij" } }"#, cx)
                    .unwrap();
            });
        });
        let manifest = Manifest::embedded();
        let net = net_down();
        let (dirs, root, http) = (
            fixture.dirs.clone(),
            fixture.root.clone(),
            net.client.clone(),
        );
        let error = cx
            .update(|cx| {
                cx.spawn(async move |cx| {
                    server_command(&dirs, &manifest, http, &root, worktree(), today(), cx).await
                })
            })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "intellij is not installed");
        assert_eq!(net.requests(), 0, "the IntelliJ installer is step 9");
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
        let manifest = Manifest::embedded();
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
        let manifest = Manifest::embedded();
        let kotlin_lsp = install(&dirs, &manifest, KotlinServerKind::KotlinLsp);
        let intellij = install(&dirs, &manifest, KotlinServerKind::Intellij);
        let manager = cx.new(|cx| KotlinProjectManager::new(Rc::new(NoHost), dirs.clone(), cx));
        manager.update(cx, |manager, cx| {
            manager.add_worktree(worktree(), root.clone(), cx)
        });

        let adapter = Arc::new(KotlinAdapter::new(dirs, Arc::new(manifest)));
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
}
