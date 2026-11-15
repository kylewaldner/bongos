use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use collections::HashMap;
use gpui::{BorrowAppContext as _, TestAppContext};
use http_client::{AsyncBody, FakeHttpClient, HttpClient, Response};
use language::{LanguageServerName, language_settings::AllLanguageSettings};
use settings::{Formatter, FormatterList, SettingsStore, WorktreeId};
use sha2::{Digest as _, Sha256};

use super::*;

const VERSION: &str = "0.0.2";

fn worktree() -> WorktreeId {
    WorktreeId::from_usize(1)
}

struct Net {
    requests: Arc<AtomicUsize>,
    hosts: Arc<Mutex<Vec<String>>>,
    client: Arc<dyn HttpClient>,
}

impl Net {
    fn serving(bytes: Vec<u8>) -> Self {
        let requests = Arc::new(AtomicUsize::new(0));
        let hosts = Arc::new(Mutex::new(Vec::new()));
        let client = FakeHttpClient::create({
            let (requests, hosts) = (requests.clone(), hosts.clone());
            move |request| {
                requests.fetch_add(1, Ordering::SeqCst);
                hosts
                    .lock()
                    .unwrap()
                    .push(request.uri().host().unwrap_or_default().to_string());
                let response = Response::builder()
                    .status(200)
                    .header("Content-Length", bytes.len())
                    .body(AsyncBody::from_reader(futures::io::Cursor::new(
                        bytes.clone(),
                    )))
                    .unwrap();
                async move { Ok(response) }
            }
        });
        Self {
            requests,
            hosts,
            client,
        }
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

struct Delegate {
    root: PathBuf,
    http: Arc<dyn HttpClient>,
    /// The commands `which` finds, by name.
    path: HashMap<String, PathBuf>,
    statuses: Mutex<Vec<BinaryStatus>>,
}

impl Delegate {
    fn new(net: &Net, path: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            root: PathBuf::from("/project"),
            http: net.client.clone(),
            path: path
                .iter()
                .map(|(name, found)| (name.to_string(), PathBuf::from(found)))
                .collect(),
            statuses: Mutex::default(),
        })
    }

    fn downloads(&self) -> usize {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|status| **status == BinaryStatus::Downloading)
            .count()
    }
}

#[async_trait]
impl LspAdapterDelegate for Delegate {
    fn show_notification(&self, _: &str, _: &mut App) {}
    fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }
    fn worktree_id(&self) -> WorktreeId {
        worktree()
    }
    fn worktree_root_path(&self) -> &Path {
        &self.root
    }
    fn resolve_relative_path(&self, path: PathBuf) -> PathBuf {
        self.root.join(path)
    }
    fn update_status(&self, _: LanguageServerName, status: BinaryStatus) {
        self.statuses.lock().unwrap().push(status);
    }
    fn registered_lsp_adapters(&self) -> Vec<Arc<dyn LspAdapter>> {
        Vec::new()
    }
    async fn language_server_download_dir(&self, _: &LanguageServerName) -> Option<Arc<Path>> {
        None
    }
    async fn npm_package_installed_version(
        &self,
        _: &str,
    ) -> Result<Option<(PathBuf, semver::Version)>> {
        Ok(None)
    }
    async fn which(&self, command: &OsStr) -> Option<PathBuf> {
        self.path.get(command.to_str()?).cloned()
    }
    async fn shell_env(&self) -> HashMap<String, String> {
        Default::default()
    }
    async fn read_text_file(&self, _: &RelPath) -> Result<String> {
        bail!("no files")
    }
    async fn try_exec(&self, _: LanguageServerBinary) -> Result<()> {
        Ok(())
    }
    fn download_cancel_flag(&self, _: LanguageServerName) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }
}

/// A small terraform-ls archive holding the launcher.
fn archive() -> Vec<u8> {
    use async_zip::{Compression, ZipEntryBuilder, base::write::ZipFileWriter};
    futures::executor::block_on(async {
        let mut writer = ZipFileWriter::new(Vec::new());
        let entry = ZipEntryBuilder::new("terraform-ls".into(), Compression::Deflate)
            .unix_permissions(0o755);
        writer
            .write_entry_whole(entry, b"#!/bin/sh\n")
            .await
            .unwrap();
        writer.close().await.unwrap()
    })
}

fn manifest_for(archive: &[u8]) -> Manifest {
    let digest: String = Sha256::digest(archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Manifest::parse(&format!(
        r#"
[terraform-ls]
version = "{VERSION}"
args = ["serve"]

[terraform-ls.env]
CHECKPOINT_DISABLE = "0"

[terraform-ls.platform.{platform}]
url = "https://releases.hashicorp.com/terraform-ls/{VERSION}/terraform-ls.zip"
sha256 = "{digest}"
archive = "zip"
strip_components = 0
launcher = "terraform-ls"
"#,
        platform = current_platform().unwrap(),
    ))
    .unwrap()
}

struct Fixture {
    temp: tempfile::TempDir,
    adapter: Arc<TerraformLsAdapter>,
    archive: Vec<u8>,
}

fn setup(cx: &mut TestAppContext) -> Fixture {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        TerraformSettings::register(cx);
        AllLanguageSettings::register(cx);
    });
    let temp = tempfile::tempdir().unwrap();
    let archive = archive();
    let adapter = Arc::new(TerraformLsAdapter::new(
        temp.path().join("languages"),
        Arc::new(manifest_for(&archive)),
    ));
    Fixture {
        temp,
        adapter,
        archive,
    }
}

fn start(
    cx: &mut TestAppContext,
    fixture: &Fixture,
    delegate: &Arc<Delegate>,
) -> gpui::Task<Result<LanguageServerBinary>> {
    let adapter = fixture.adapter.clone();
    let delegate: Arc<dyn LspAdapterDelegate> = delegate.clone();
    cx.update(|cx| cx.spawn(async move |cx| adapter.start(&delegate, cx).await))
}

fn options(
    cx: &mut TestAppContext,
    fixture: &Fixture,
    delegate: &Arc<Delegate>,
) -> gpui::Task<Result<Option<Value>>> {
    let adapter = fixture.adapter.clone();
    let delegate: Arc<dyn LspAdapterDelegate> = delegate.clone();
    cx.update(|cx| cx.spawn(async move |cx| adapter.initialization_options(&delegate, cx).await))
}

fn set_binary(cx: &mut TestAppContext, binary: &str) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(
                    &json!({ "terraform": { "binary": binary } }).to_string(),
                    cx,
                )
                .unwrap();
        });
    });
}

fn terraform_path(options: Option<Value>) -> Option<String> {
    options?["terraform"]["path"].as_str().map(str::to_string)
}

fn notices(cx: &mut TestAppContext) -> Vec<bongos_language_status::Notice> {
    cx.update(|cx| LanguageNotices::list(cx).read(cx).notices().to_vec())
}

#[gpui::test]
async fn test_terraform_a_first_start_installs_and_serves_with_checkpoint_disabled(
    cx: &mut TestAppContext,
) {
    let fixture = setup(cx);
    let net = Net::serving(fixture.archive.clone());
    let delegate = Delegate::new(&net, &[]);

    let first = start(cx, &fixture, &delegate).await.unwrap();
    assert_eq!(net.requests(), 1);
    assert_eq!(*net.hosts.lock().unwrap(), ["releases.hashicorp.com"]);
    assert_eq!(delegate.downloads(), 1);
    let install = fixture.temp.path().join("languages/terraform/terraform-ls");
    assert!(install.join(VERSION).join("terraform-ls").is_file());
    assert_eq!(first.arguments, [OsStr::new("serve")]);
    // Whatever the pin says, the server never checks in with HashiCorp.
    assert_eq!(
        first.env.as_ref().unwrap().get(CHECKPOINT_DISABLE),
        Some(&"1".to_string())
    );

    // A second start installs nothing.
    let second = start(cx, &fixture, &delegate).await.unwrap();
    assert_eq!(net.requests(), 1);
    assert_eq!(second.path, first.path);
}

#[gpui::test]
async fn test_terraform_the_setting_beats_terraform_beats_tofu(cx: &mut TestAppContext) {
    let fixture = setup(cx);
    let net = Net::serving(fixture.archive.clone());
    let both = Delegate::new(
        &net,
        &[("terraform", "/bin/terraform"), ("tofu", "/bin/tofu")],
    );
    let path = terraform_path(options(cx, &fixture, &both).await.unwrap());
    assert_eq!(path.as_deref(), Some("/bin/terraform"));

    set_binary(cx, "/opt/mine/terraform");
    let path = terraform_path(options(cx, &fixture, &both).await.unwrap());
    assert_eq!(path.as_deref(), Some("/opt/mine/terraform"));

    // A bare name resolves on PATH, and a relative path against the worktree root.
    set_binary(cx, "tofu");
    let path = terraform_path(options(cx, &fixture, &both).await.unwrap());
    assert_eq!(path.as_deref(), Some("/bin/tofu"));
    set_binary(cx, "tools/terraform");
    let path = terraform_path(options(cx, &fixture, &both).await.unwrap());
    assert_eq!(path.as_deref(), Some("/project/tools/terraform"));
}

#[gpui::test]
async fn test_terraform_only_tofu_passes_tofu(cx: &mut TestAppContext) {
    let fixture = setup(cx);
    let net = Net::serving(fixture.archive.clone());
    let delegate = Delegate::new(&net, &[("tofu", "/usr/local/bin/tofu")]);
    let path = terraform_path(options(cx, &fixture, &delegate).await.unwrap());
    assert_eq!(path.as_deref(), Some("/usr/local/bin/tofu"));
    assert!(notices(cx).is_empty());
}

#[gpui::test]
async fn test_terraform_no_binary_posts_the_notice_and_the_server_still_starts(
    cx: &mut TestAppContext,
) {
    let fixture = setup(cx);
    let net = Net::serving(fixture.archive.clone());
    let delegate = Delegate::new(&net, &[]);

    let sent = options(cx, &fixture, &delegate).await.unwrap();
    assert_eq!(terraform_path(sent), None);
    let posted = notices(cx);
    let languages: Vec<&str> = posted
        .iter()
        .map(|notice| notice.language.as_str())
        .collect();
    assert_eq!(languages, LANGUAGES);
    assert!(
        posted
            .iter()
            .all(|notice| notice.key == NO_BINARY_NOTICE_KEY && notice.text == NO_BINARY_TEXT)
    );

    start(cx, &fixture, &delegate).await.unwrap();
    assert!(
        !delegate
            .statuses
            .lock()
            .unwrap()
            .iter()
            .any(|status| matches!(status, BinaryStatus::Failed { .. }))
    );

    // Finding a binary later clears the notice.
    let found = Delegate::new(&net, &[("terraform", "/bin/terraform")]);
    options(cx, &fixture, &found).await.unwrap();
    assert!(notices(cx).is_empty());
}

#[gpui::test]
async fn test_terraform_formatting_goes_to_the_language_server(cx: &mut TestAppContext) {
    let _fixture = setup(cx);
    for language in LANGUAGES {
        let name = LanguageName::new_static(language);
        cx.update(|cx| {
            let settings = AllLanguageSettings::get_global(cx).language(None, Some(&name), cx);
            assert!(
                matches!(
                    &settings.formatter,
                    FormatterList::Single(Formatter::LanguageServer(_))
                ),
                "{language}: {:?}",
                settings.formatter
            );
            assert_eq!(settings.language_servers, ["terraform-ls", "..."]);
        });
    }
}

#[test]
fn test_terraform_the_download_host_is_allowed_by_the_host_guard() {
    // The only host bongos contacts for terraform-ls; the guard blocks Zed hosts only.
    assert!(!bongos_net::is_blocked_host("releases.hashicorp.com"));
    assert!(bongos_net::is_blocked_host("zed.dev"));
    let manifest = Manifest::embedded();
    let entry = pinned::entry(&manifest, TERRAFORM_LS.name, "linux-x64").unwrap();
    assert!(entry.url.starts_with("https://releases.hashicorp.com/"));
}

#[test]
fn test_terraform_the_checkpoint_variable_overrides_a_manifest_value() {
    let binary = LanguageServerBinary {
        path: PathBuf::from("terraform-ls"),
        arguments: vec![],
        env: Some(
            [(CHECKPOINT_DISABLE.to_string(), "0".to_string())]
                .into_iter()
                .collect(),
        ),
    };
    let env = with_checkpoint_disabled(binary).env.unwrap();
    assert_eq!(env.get(CHECKPOINT_DISABLE), Some(&"1".to_string()));
}
