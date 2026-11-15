use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use collections::HashMap;
use gpui::{BorrowAppContext as _, TestAppContext};
use http_client::{AsyncBody, FakeHttpClient, HttpClient, Response};
use language::LanguageServerName;
use settings::{SettingsStore, WorktreeId};

use super::*;

fn worktree() -> WorktreeId {
    WorktreeId::from_usize(1)
}

/// A project fixture the language tests share with the smoke scenarios.
fn project(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../script/bongos/fixtures/languages")
        .join(name)
}

/// A request counter around a fake client that answers each request with `respond`.
struct Net {
    requests: Arc<AtomicUsize>,
    hosts: Arc<Mutex<Vec<String>>>,
    client: Arc<dyn HttpClient>,
}

impl Net {
    fn new(respond: impl Fn() -> Result<Response<AsyncBody>> + Send + Sync + 'static) -> Self {
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
                let response = respond();
                async move { response }
            }
        });
        Self {
            requests,
            hosts,
            client,
        }
    }

    fn serving(bytes: Vec<u8>) -> Self {
        Self::new(move || {
            Ok(Response::builder()
                .status(200)
                .header("Content-Length", bytes.len())
                .body(AsyncBody::from_reader(futures::io::Cursor::new(
                    bytes.clone(),
                )))
                .unwrap())
        })
    }

    fn down() -> Self {
        Self::new(|| Err(anyhow!("network is down")))
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

struct Delegate {
    root: PathBuf,
    http: Arc<dyn HttpClient>,
    statuses: Mutex<Vec<BinaryStatus>>,
    progress: Mutex<Vec<(u64, Option<u64>)>>,
}

impl Delegate {
    fn new(root: &Path, net: &Net) -> Arc<Self> {
        Arc::new(Self {
            root: root.to_path_buf(),
            http: net.client.clone(),
            statuses: Mutex::default(),
            progress: Mutex::default(),
        })
    }

    fn statuses(&self) -> Vec<BinaryStatus> {
        self.statuses.lock().unwrap().clone()
    }

    fn downloads(&self) -> usize {
        self.statuses()
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
        path
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
    async fn which(&self, _: &OsStr) -> Option<PathBuf> {
        None
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
    fn report_download_progress(&self, _: LanguageServerName, done: u64, total: Option<u64>) {
        self.progress.lock().unwrap().push((done, total));
    }
    fn download_cancel_flag(&self, _: LanguageServerName) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }
}

const VERSION: &str = "1.2.3";

/// A small jdtls archive: the Python launcher, a launcher jar and the platform configurations.
fn archive() -> Vec<u8> {
    use futures::AsyncReadExt as _;
    futures::executor::block_on(async {
        let mut builder = async_tar::Builder::new(Vec::new());
        for (path, mode) in [
            ("bin/jdtls", 0o644),
            ("plugins/org.eclipse.equinox.launcher_1.0.0.jar", 0o644),
            (
                "plugins/org.eclipse.equinox.launcher.gtk.linux.x86_64_1.jar",
                0o644,
            ),
            ("config_linux/config.ini", 0o644),
            ("config_linux_arm/config.ini", 0o644),
            ("config_mac/config.ini", 0o644),
            ("config_mac_arm/config.ini", 0o644),
        ] {
            let mut header = async_tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_size(2);
            header.set_mode(mode);
            header.set_cksum();
            builder.append(&header, &b"x\n"[..]).await.unwrap();
        }
        let tar = builder.into_inner().await.unwrap();
        let mut encoder = async_compression::futures::bufread::GzipEncoder::new(
            futures::io::BufReader::new(futures::io::Cursor::new(tar)),
        );
        let mut gzipped = Vec::new();
        encoder.read_to_end(&mut gzipped).await.unwrap();
        gzipped
    })
}

fn manifest_for(archive: &[u8]) -> Manifest {
    let digest: String = Sha256::digest(archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Manifest::parse(&format!(
        r#"
[jdtls]
version = "{VERSION}"

[jdtls.platform.{platform}]
url = "http://127.0.0.1:1/jdtls.tar.gz"
sha256 = "{digest}"
archive = "tar.gz"
strip_components = 0
launcher = "bin/jdtls"
"#,
        platform = current_platform().unwrap(),
    ))
    .unwrap()
}

fn write_jdk(path: &Path, version: &str) -> PathBuf {
    fs::create_dir_all(path.join("bin")).unwrap();
    fs::write(
        path.join("release"),
        format!("JAVA_VERSION=\"{version}\"\n"),
    )
    .unwrap();
    fs::write(path.join("bin/java"), "#!/bin/sh\n").unwrap();
    path.to_path_buf()
}

struct Fixture {
    temp: tempfile::TempDir,
    adapter: Arc<JdtlsAdapter>,
    archive: Vec<u8>,
}

impl Fixture {
    fn languages(&self) -> PathBuf {
        self.temp.path().join("languages")
    }

    fn jdks(&self) -> PathBuf {
        self.temp.path().join("jdks")
    }

    fn install_dir(&self) -> PathBuf {
        self.languages().join("java/jdtls").join(VERSION)
    }
}

/// An adapter on the small archive, whose JDK discovery sees only `<temp>/jdks/*` and `java_home`.
fn setup(cx: &mut TestAppContext, java_home: Option<&str>) -> Fixture {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        JavaSettings::register(cx);
    });
    let temp = tempfile::tempdir().unwrap();
    let archive = archive();
    let jdks = temp.path().join("jdks");
    let env = JdkEnvironment {
        java_home: java_home.map(|name| jdks.join(name).into_os_string()),
        candidates: vec![format!("{}/*", jdks.display())],
    };
    let adapter = Arc::new(JdtlsAdapter::new(
        temp.path().join("languages"),
        Arc::new(manifest_for(&archive)),
        env,
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

fn notices(cx: &mut TestAppContext) -> Vec<bongos_language_status::Notice> {
    cx.update(|cx| LanguageNotices::list(cx).read(cx).notices().to_vec())
}

fn args(binary: &LanguageServerBinary) -> Vec<String> {
    binary
        .arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let index = args.iter().position(|argument| argument == flag).unwrap();
    &args[index + 1]
}

#[gpui::test]
async fn test_java_a_first_start_installs_and_a_second_makes_no_request(cx: &mut TestAppContext) {
    let fixture = setup(cx, None);
    write_jdk(&fixture.jdks().join("jdk-25"), "25.0.1");
    let root = project("java-gradle");

    let net = Net::serving(fixture.archive.clone());
    let delegate = Delegate::new(&root, &net);
    let first = start(cx, &fixture, &delegate).await.unwrap();
    assert_eq!(net.requests(), 1);
    assert_eq!(delegate.downloads(), 1);
    assert!(fixture.install_dir().join("plugins").is_dir());
    assert_eq!(
        first.path,
        fixture.jdks().join("jdk-25").join("bin").join("java")
    );

    let down = Net::down();
    let delegate = Delegate::new(&root, &down);
    let second = start(cx, &fixture, &delegate).await.unwrap();
    assert_eq!(down.requests(), 0);
    assert_eq!(delegate.downloads(), 0);
    assert_eq!(args(&second), args(&first));
}

#[gpui::test]
async fn test_java_the_command_uses_the_server_jdk_and_the_pinned_arguments(
    cx: &mut TestAppContext,
) {
    let fixture = setup(cx, None);
    write_jdk(&fixture.jdks().join("jdk-21"), "21.0.2");
    let newest = write_jdk(&fixture.jdks().join("jdk-26"), "26.0.1");
    let root = project("java-maven");
    let net = Net::serving(fixture.archive.clone());
    let delegate = Delegate::new(&root, &net);

    let binary = start(cx, &fixture, &delegate).await.unwrap();
    assert_eq!(binary.path, newest.join("bin/java"));
    let args = args(&binary);
    for pinned in [
        "-Declipse.application=org.eclipse.jdt.ls.core.id1",
        "-Dosgi.bundles.defaultStartLevel=4",
        "-Declipse.product=org.eclipse.jdt.ls.core.product",
        HEAP_LIMIT,
        "-Djava.import.generatesMetadataFilesAtProjectRoot=false",
        "-Djdk.xml.maxGeneralEntitySizeLimit=0",
        "-Djdk.xml.totalEntitySizeLimit=0",
    ] {
        assert!(args.iter().any(|argument| argument == pinned), "{pinned}");
    }
    let install = fixture.install_dir();
    assert_eq!(
        after(&args, "-jar"),
        install
            .join("plugins/org.eclipse.equinox.launcher_1.0.0.jar")
            .to_str()
            .unwrap()
    );
    let config = config_dir_name(current_platform().unwrap()).unwrap();
    assert_eq!(
        after(&args, "-configuration"),
        install.join(config).to_str().unwrap()
    );

    let data = PathBuf::from(after(&args, "-data"));
    let canonical = fs::canonicalize(&root).unwrap();
    let digest: String = Sha256::digest(canonical.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        data,
        fixture
            .languages()
            .join("java/workspaces")
            .join(&digest[..16])
    );
    assert!(
        !data.starts_with(&root),
        "the workspace is outside the project"
    );
    // The last argument is the data directory: nothing follows it, and no pinned extras exist.
    assert_eq!(args.last().unwrap(), data.to_str().unwrap());
}

#[test]
fn test_java_every_platform_has_a_configuration_directory() {
    assert_eq!(config_dir_name("linux-x64").unwrap(), "config_linux");
    assert_eq!(config_dir_name("macos-x64").unwrap(), "config_mac");
    assert_eq!(config_dir_name("linux-arm64").unwrap(), "config_linux_arm");
    assert_eq!(config_dir_name("macos-arm64").unwrap(), "config_mac_arm");
    assert!(config_dir_name("windows-x64").is_err());
}

#[gpui::test]
async fn test_java_a_user_server_jdk_wins_over_the_detected_one(cx: &mut TestAppContext) {
    let fixture = setup(cx, Some("jdk-26"));
    write_jdk(&fixture.jdks().join("jdk-26"), "26.0.1");
    let mine = write_jdk(&fixture.temp.path().join("mine"), "25.0.1");
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(&json!({ "java": { "server_jdk": mine } }).to_string(), cx)
                .unwrap();
        });
    });
    let net = Net::serving(fixture.archive.clone());
    let delegate = Delegate::new(&project("java-gradle"), &net);
    let binary = start(cx, &fixture, &delegate).await.unwrap();
    assert_eq!(binary.path, mine.join("bin/java"));
}

#[gpui::test]
async fn test_java_runtimes_carry_the_import_jdk_on_the_gradle_fixture(cx: &mut TestAppContext) {
    // The Gradle fixture declares no toolchain, so `JAVA_HOME` is the import JDK; the server runs
    // on another JDK.
    let fixture = setup(cx, Some("jdk-21"));
    let import = write_jdk(&fixture.jdks().join("jdk-21"), "21.0.2");
    write_jdk(&fixture.jdks().join("jdk-25"), "25.0.1");
    let net = Net::down();
    let delegate = Delegate::new(&project("java-gradle"), &net);

    let options = options(cx, &fixture, &delegate).await.unwrap().unwrap();
    let java = &options["settings"]["java"];
    assert_eq!(
        java["configuration"]["runtimes"],
        json!([{ "name": "JavaSE-21", "path": import, "default": true }])
    );
    assert_eq!(java["import"]["gradle"]["java"]["home"], json!(import));
    assert_eq!(
        java["import"]["generatesMetadataFilesAtProjectRoot"],
        json!(false)
    );
}

#[gpui::test]
async fn test_java_runtimes_carry_the_import_jdk_on_the_maven_fixture(cx: &mut TestAppContext) {
    // The Maven fixture compiles at level 17, which a 17 JDK matches over the newer ones.
    let fixture = setup(cx, None);
    let import = write_jdk(&fixture.jdks().join("jdk-17"), "17.0.9");
    write_jdk(&fixture.jdks().join("jdk-25"), "25.0.1");
    let net = Net::down();
    let delegate = Delegate::new(&project("java-maven"), &net);

    let options = options(cx, &fixture, &delegate).await.unwrap().unwrap();
    let java = &options["settings"]["java"];
    assert_eq!(
        java["configuration"]["runtimes"],
        json!([{ "name": "JavaSE-17", "path": import, "default": true }])
    );
    assert_eq!(java["import"]["gradle"]["java"]["home"], json!(import));
}

#[gpui::test]
async fn test_java_an_import_jdk_nothing_satisfies_does_not_fail_the_start(
    cx: &mut TestAppContext,
) {
    let fixture = setup(cx, None);
    write_jdk(&fixture.jdks().join("jdk-25"), "25.0.1");
    let project = fixture.temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("build.gradle.kts"),
        "java {\n  toolchain {\n    languageVersion.set(JavaLanguageVersion.of(11))\n  }\n}\n",
    )
    .unwrap();
    let net = Net::serving(fixture.archive.clone());
    let delegate = Delegate::new(&project, &net);

    // The toolchain is unmet: no runtime is sent, the server still starts.
    let options = options(cx, &fixture, &delegate).await.unwrap().unwrap();
    assert!(options["settings"]["java"].get("configuration").is_none());
    assert!(
        options["settings"]["java"]["import"]
            .get("gradle")
            .is_none()
    );
    start(cx, &fixture, &delegate).await.unwrap();
    assert!(
        !delegate
            .statuses()
            .iter()
            .any(|status| matches!(status, BinaryStatus::Failed { .. }))
    );
}

#[gpui::test]
async fn test_java_no_jdk_at_the_minimum_is_a_failure_with_a_notice(cx: &mut TestAppContext) {
    let fixture = setup(cx, None);
    write_jdk(&fixture.jdks().join("jdk-17"), "17.0.9");
    let net = Net::serving(fixture.archive.clone());
    let delegate = Delegate::new(&project("java-gradle"), &net);

    let error = start(cx, &fixture, &delegate).await.unwrap_err();
    assert_eq!(error.to_string(), "jdtls needs JDK 25 or newer (found 17)");
    assert_eq!(net.requests(), 0, "nothing is downloaded without a JDK");
    assert_eq!(
        delegate.statuses().last(),
        Some(&BinaryStatus::Failed {
            error: "jdtls needs JDK 25 or newer (found 17)".into()
        })
    );
    let posted = notices(cx);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].language, "Java");
    assert_eq!(posted[0].worktree_id, worktree());
    assert!(
        posted[0]
            .text
            .starts_with("jdtls needs JDK 25 or newer (found 17)"),
        "{}",
        posted[0].text
    );

    // Installing a JDK 25 clears the notice at the next start.
    write_jdk(&fixture.jdks().join("jdk-25"), "25.0.1");
    start(cx, &fixture, &delegate).await.unwrap();
    assert!(notices(cx).is_empty());
}

#[gpui::test]
async fn test_java_a_download_failure_names_jdtls_and_only_the_eclipse_host_is_asked(
    cx: &mut TestAppContext,
) {
    // The embedded pin, so the host is the one a release build contacts.
    let mut fixture = setup(cx, None);
    fixture.adapter = Arc::new(JdtlsAdapter::new(
        fixture.languages(),
        Arc::new(Manifest::embedded()),
        JdkEnvironment {
            java_home: None,
            candidates: vec![format!("{}/*", fixture.jdks().display())],
        },
    ));
    write_jdk(&fixture.jdks().join("jdk-25"), "25.0.1");
    let net = Net::down();
    let delegate = Delegate::new(&project("java-gradle"), &net);

    let error = start(cx, &fixture, &delegate).await.unwrap_err();
    assert!(
        error.to_string().starts_with("could not download jdtls"),
        "{error:#}"
    );
    let Some(BinaryStatus::Failed { error }) = delegate.statuses().last().cloned() else {
        panic!("not failed: {:?}", delegate.statuses());
    };
    assert!(error.starts_with("could not download jdtls"), "{error}");
    assert!(!fixture.install_dir().exists());

    let hosts = net.hosts.lock().unwrap().clone();
    assert_eq!(hosts, ["download.eclipse.org"]);
    for host in &hosts {
        for forbidden in [
            "zed.dev",
            "zed.industries",
            "bongos.rocks",
            "agentclientprotocol.com",
        ] {
            assert!(!host.ends_with(forbidden), "{host}");
        }
    }
}
