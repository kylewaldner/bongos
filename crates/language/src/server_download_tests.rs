//! The blanket installer's cancel, cleanup and offline behavior, with a fake installer whose
//! download writes into a real directory.

use super::*;
use futures::AsyncReadExt as _;
use gpui::{Task, TestAppContext};
use http_client::{AsyncBody, FakeHttpClient, Response};
use std::sync::atomic::AtomicUsize;

const SERVER: &str = "test-server";

struct TestDelegate {
    http: Arc<dyn HttpClient>,
    container_dir: Arc<Path>,
    statuses: Mutex<Vec<BinaryStatus>>,
    cancel: Mutex<Arc<AtomicBool>>,
}

impl TestDelegate {
    fn new(container_dir: &Path, http: Arc<dyn HttpClient>) -> Arc<Self> {
        Arc::new(Self {
            http,
            container_dir: container_dir.into(),
            statuses: Mutex::default(),
            cancel: Mutex::default(),
        })
    }

    /// What `LspStore::cancel_server_download` does.
    fn cancel(&self) {
        self.cancel.lock().store(true, Ordering::SeqCst);
    }

    fn last_status(&self) -> Option<BinaryStatus> {
        self.statuses.lock().last().cloned()
    }
}

#[async_trait]
impl LspAdapterDelegate for TestDelegate {
    fn show_notification(&self, _: &str, _: &mut App) {}

    fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }

    fn worktree_id(&self) -> WorktreeId {
        WorktreeId::from_usize(1)
    }

    fn worktree_root_path(&self) -> &Path {
        &self.container_dir
    }

    fn resolve_relative_path(&self, path: PathBuf) -> PathBuf {
        path
    }

    fn update_status(&self, _: LanguageServerName, status: BinaryStatus) {
        self.statuses.lock().push(status);
    }

    fn registered_lsp_adapters(&self) -> Vec<Arc<dyn LspAdapter>> {
        Vec::new()
    }

    async fn language_server_download_dir(&self, _: &LanguageServerName) -> Option<Arc<Path>> {
        Some(self.container_dir.clone())
    }

    async fn npm_package_installed_version(&self, _: &str) -> Result<Option<(PathBuf, Version)>> {
        Ok(None)
    }

    async fn which(&self, _: &OsStr) -> Option<PathBuf> {
        None
    }

    async fn shell_env(&self) -> HashMap<String, String> {
        HashMap::default()
    }

    async fn read_text_file(&self, _: &RelPath) -> Result<String> {
        anyhow::bail!("no files")
    }

    async fn try_exec(&self, _: LanguageServerBinary) -> Result<()> {
        Ok(())
    }

    fn download_cancel_flag(&self, _: LanguageServerName) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        *self.cancel.lock() = flag.clone();
        flag
    }
}

/// Asks the HTTP client for the latest version, and downloads it into `<container>/<version>`:
/// first a `partial` file, then (unless `hang` is set, when it waits forever) the `server`.
#[derive(Default)]
struct TestInstaller {
    hang: Arc<AtomicBool>,
    downloads: Arc<AtomicUsize>,
}

impl TestInstaller {
    fn downloads(&self) -> usize {
        self.downloads.load(Ordering::SeqCst)
    }
}

fn binary(path: PathBuf) -> LanguageServerBinary {
    LanguageServerBinary {
        path,
        arguments: Vec::new(),
        env: None,
    }
}

#[async_trait(?Send)]
impl LspAdapter for TestInstaller {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static(SERVER)
    }
}

impl LspInstaller for TestInstaller {
    type BinaryVersion = String;

    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<String> {
        let mut response = delegate
            .http_client()
            .get("https://example.com/latest", AsyncBody::empty(), true)
            .await
            .context("checking the latest version")?;
        let mut version = String::new();
        response.body_mut().read_to_string(&mut version).await?;
        Ok(version)
    }

    fn check_if_version_installed(
        &self,
        version: &String,
        container_dir: &PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let server = container_dir.join(version).join("server");
        async move { server.exists().then(|| binary(server)) }
    }

    fn fetch_server_binary(
        &self,
        version: String,
        container_dir: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let hang = self.hang.clone();
        let downloads = self.downloads.clone();
        async move {
            downloads.fetch_add(1, Ordering::SeqCst);
            let dir = container_dir.join(version);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("partial"), "")?;
            if hang.load(Ordering::SeqCst) {
                futures::future::pending::<()>().await;
            }
            let server = dir.join("server");
            std::fs::write(&server, "")?;
            Ok(binary(server))
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        std::fs::read_dir(container_dir)
            .ok()?
            .filter_map(|entry| Some(entry.ok()?.path().join("server")))
            .find(|server| server.exists())
            .map(binary)
    }
}

/// A client that answers every request with `body`.
fn answers(body: &'static str) -> Arc<dyn HttpClient> {
    FakeHttpClient::create(move |_| async move {
        Ok(Response::builder()
            .status(200)
            .body(AsyncBody::from(body))
            .unwrap())
    })
}

/// What a start of the server runs: the command lookup, then `binary_to_start`.
fn start(
    installer: &Arc<TestInstaller>,
    delegate: &Arc<TestDelegate>,
    cx: &mut TestAppContext,
) -> Task<Result<LanguageServerBinary>> {
    let installer = installer.clone();
    let delegate: Arc<dyn LspAdapterDelegate> = delegate.clone();
    cx.spawn(async move |cx| {
        let cache = Arc::new(futures::lock::Mutex::new(None)).lock_owned().await;
        let options = LanguageServerBinaryOptions {
            allow_path_lookup: false,
            allow_binary_download: true,
            pre_release: false,
        };
        let locations = installer
            .get_language_server_command(delegate, None, options, cache, cx.clone())
            .await;
        binary_to_start(locations, &cx).await
    })
}

fn entries(dir: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = dir_entry_names(dir).into_iter().collect();
    names.sort();
    names
}

#[gpui::test]
async fn test_cancelled_download_fails_leaves_nothing_and_the_next_start_downloads_again(
    cx: &mut TestAppContext,
) {
    let container = tempfile::tempdir().unwrap();
    let delegate = TestDelegate::new(container.path(), answers("1.0"));
    let installer = Arc::new(TestInstaller::default());
    installer.hang.store(true, Ordering::SeqCst);

    let first = start(&installer, &delegate, cx);
    cx.run_until_parked();
    assert_eq!(installer.downloads(), 1);
    assert_eq!(delegate.last_status(), Some(BinaryStatus::Downloading));
    assert!(container.path().join("1.0").join("partial").exists());

    delegate.cancel();
    cx.executor().advance_clock(DOWNLOAD_CANCEL_POLL_INTERVAL);
    cx.run_until_parked();
    let error = first.await.unwrap_err();
    assert_eq!(error.to_string(), "download cancelled");
    assert_eq!(
        delegate.last_status(),
        Some(BinaryStatus::Failed {
            error: "download cancelled".into()
        })
    );
    assert_eq!(
        entries(container.path()),
        Vec::<std::ffi::OsString>::new(),
        "the cancelled download left files behind"
    );

    installer.hang.store(false, Ordering::SeqCst);
    let second = start(&installer, &delegate, cx).await.unwrap();
    assert_eq!(installer.downloads(), 2);
    assert_eq!(second.path, container.path().join("1.0").join("server"));
}

#[gpui::test]
async fn test_cancel_during_the_version_check_stops_the_download(cx: &mut TestAppContext) {
    let container = tempfile::tempdir().unwrap();
    let (answer_tx, answer_rx) = futures::channel::oneshot::channel::<()>();
    let answer_rx = answer_rx.shared();
    let http = FakeHttpClient::create(move |_| {
        let answer_rx = answer_rx.clone();
        async move {
            answer_rx.await.ok();
            Ok(Response::builder()
                .status(200)
                .body(AsyncBody::from("1.0"))
                .unwrap())
        }
    });
    let delegate = TestDelegate::new(container.path(), http);
    let installer = Arc::new(TestInstaller::default());

    let task = start(&installer, &delegate, cx);
    cx.run_until_parked();
    assert_eq!(
        delegate.last_status(),
        Some(BinaryStatus::CheckingForUpdate)
    );
    delegate.cancel();
    answer_tx.send(()).unwrap();
    cx.run_until_parked();

    assert_eq!(task.await.unwrap_err().to_string(), "download cancelled");
    assert_eq!(installer.downloads(), 0);
    assert_eq!(entries(container.path()), Vec::<std::ffi::OsString>::new());
}

#[gpui::test]
async fn test_a_cached_server_starts_at_once_while_the_network_never_answers(
    cx: &mut TestAppContext,
) {
    let container = tempfile::tempdir().unwrap();
    let cached = container.path().join("0.9");
    std::fs::create_dir(&cached).unwrap();
    std::fs::write(cached.join("server"), "").unwrap();
    let http = FakeHttpClient::create(|_| futures::future::pending());
    let delegate = TestDelegate::new(container.path(), http);
    let installer = Arc::new(TestInstaller::default());

    let task = start(&installer, &delegate, cx);
    cx.run_until_parked();
    // No clock advance: the start is done without waiting for the version check.
    let binary = task.await.unwrap();
    assert_eq!(binary.path, cached.join("server"));
    assert_eq!(
        delegate.last_status(),
        Some(BinaryStatus::CheckingForUpdate)
    );
    assert_eq!(installer.downloads(), 0);
}

#[gpui::test]
async fn test_without_a_cache_a_failing_client_fails_with_its_cause(cx: &mut TestAppContext) {
    let container = tempfile::tempdir().unwrap();
    let http = FakeHttpClient::create(|_| async { Err(anyhow::anyhow!("network is unreachable")) });
    let delegate = TestDelegate::new(container.path(), http);
    let installer = Arc::new(TestInstaller::default());

    let error = start(&installer, &delegate, cx).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("network is unreachable"),
        "{error:#}"
    );
    match delegate.last_status() {
        Some(BinaryStatus::Failed { error }) => {
            assert!(error.contains("network is unreachable"), "{error}");
        }
        status => panic!("expected a failed status, got {status:?}"),
    }
    assert_eq!(installer.downloads(), 0);
}
