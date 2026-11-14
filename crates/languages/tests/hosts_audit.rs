//! Where the built-in language servers, debug adapters and Node.js are downloaded from.
//!
//! Every adapter is asked to download its server through a recording HTTP client and a fake
//! Node.js runtime that records npm commands. Each adapter's hosts must be the pinned ones, every
//! host must be on the allow list, and none may be a Zed or ACP host. A new adapter fails the test
//! until its hosts are pinned here (and listed in the README's Privacy section).

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use dap::{
    DapRegistry,
    adapters::{DapDelegate, DebugAdapterName, DebugTaskDefinition},
};
use gpui::{App, TestAppContext};
use http_client::{FakeHttpClient, HttpClient, Response};
use language::{
    BinaryStatus, LanguageRegistry, LanguageServerName, LanguageToolchainStore, LspAdapter,
    LspAdapterDelegate, Toolchain,
};
use lsp::LanguageServerBinary;
use node_runtime::{FakeNpmCall, FakeNpmLog, NPM_PRIVACY_ENV, NodeBinaryOptions, NodeRuntime};
use semver::Version;
use serde_json::json;
use settings::{SettingsStore, WorktreeId};
use util::rel_path::RelPath;

/// The hosts bongos may download from. GitHub's release CDN, which github.com redirects a
/// download to, is not here: the HTTP client follows that redirect itself.
const ALLOWED_HOSTS: [&str; 5] = [
    "api.github.com",
    "github.com",
    "nodejs.org",
    // npm's own default registry; bongos never names another one.
    "registry.npmjs.org",
    "pypi.org",
];

/// What script/bongos/check-hosts forbids in the product.
const FORBIDDEN: [&str; 6] = [
    "bongos.rocks",
    "zed.dev",
    "zed.industries",
    "agentclientprotocol.com",
    "MINIDUMP_ENDPOINT",
    "CIMD_URL",
];

const GITHUB: &[&str] = &["api.github.com", "github.com"];
const GITHUB_API: &[&str] = &["api.github.com"];
const GITHUB_DOWNLOAD: &[&str] = &["github.com"];
const NPM: &[&str] = &["registry.npmjs.org"];
const PYPI: &[&str] = &["pypi.org"];
const NODEJS: &[&str] = &["nodejs.org"];
/// Installed by a tool on the user's machine (pip, `go install`, gdb), which the test does not
/// have, so bongos itself contacts nothing.
const NONE: &[&str] = &[];

/// Each language server's download hosts.
const LANGUAGE_SERVER_HOSTS: [(&str, &[&str]); 19] = [
    ("basedpyright", NPM),
    ("bash-language-server", NPM),
    ("clangd", GITHUB),
    // The pinned vscode-eslint source archive; its dependencies then come from npm.
    ("eslint", GITHUB_DOWNLOAD),
    // The latest version from GitHub; the build is `go install` from the Go module proxy.
    ("gopls", GITHUB_API),
    ("json-language-server", NPM),
    ("package-version-server", GITHUB),
    // pip, from the user's Python.
    ("pylsp", NONE),
    ("pyright", NPM),
    ("ruff", GITHUB),
    ("rust-analyzer", GITHUB),
    ("tailwindcss-intellisense-css", NPM),
    ("tailwindcss-language-server", NPM),
    ("ty", GITHUB),
    ("typescript-language-server", NPM),
    ("vscode-css-language-server", NPM),
    ("vtsls", NPM),
    ("yaml-language-server", NPM),
    // Node.js itself, when there is no suitable system Node.js.
    ("Node.js", NODEJS),
];

/// Each debug adapter's download hosts.
const DEBUG_ADAPTER_HOSTS: [(&str, &[&str]); 5] = [
    ("CodeLLDB", GITHUB),
    // The latest version from PyPI; the wheel is fetched by the user's pip.
    ("Debugpy", PYPI),
    // The delve-shim-dap release; dlv itself is `go install`ed from the Go module proxy.
    ("Delve", GITHUB),
    ("GDB", NONE),
    ("JavaScript", GITHUB),
];

/// Stands in for every tool on the user's machine the adapters look for. Only `go` and `dlv`
/// are "found", at a path that does not exist, so gopls and Delve go as far as their downloads.
fn which(command: &OsStr) -> Option<PathBuf> {
    ["go", "dlv"]
        .iter()
        .any(|tool| OsStr::new(tool) == command)
        .then(|| Path::new("/nonexistent-audit-tools").join(command))
}

#[derive(Clone, Default)]
struct Requests(Arc<Mutex<Vec<String>>>);

impl Requests {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// An HTTP client that records every URL. It answers a GitHub releases request with one release
/// that has every asset an adapter looks for, and everything else with a 404.
fn recording_http(requests: Requests) -> Arc<dyn HttpClient> {
    FakeHttpClient::create(move |request| {
        let url = request.uri().to_string();
        requests.0.lock().unwrap().push(url.clone());
        async move {
            let parsed = url::Url::parse(&url)?;
            let segments: Vec<_> = parsed.path_segments().into_iter().flatten().collect();
            if parsed.host_str() == Some("api.github.com")
                && let ["repos", owner, repo, "releases"] = segments.as_slice()
            {
                let body = github_releases(&format!("{owner}/{repo}"));
                return Ok(Response::builder().status(200).body(body.into())?);
            }
            Ok(Response::builder().status(404).body(Default::default())?)
        }
    })
}

fn github_releases(repo: &str) -> String {
    let tag = "v1.0.0";
    let arch = std::env::consts::ARCH;
    let codelldb_arch = if arch == "aarch64" { "arm64" } else { "x64" };
    let mut names = vec![
        format!("clangd-linux-{tag}.zip"),
        format!("clangd-mac-{tag}.zip"),
        format!("js-debug-dap-{tag}.tar.gz"),
        format!("codelldb-linux-{codelldb_arch}.vsix"),
        format!("codelldb-darwin-{codelldb_arch}.vsix"),
    ];
    for target in ["unknown-linux-gnu", "unknown-linux-musl", "apple-darwin"] {
        names.push(format!("rust-analyzer-{arch}-{target}.gz"));
        for stem in ["ruff", "ty", "package-version-server", "delve-shim-dap"] {
            names.push(format!("{stem}-{arch}-{target}.tar.gz"));
        }
    }
    let assets: Vec<_> = names
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "browser_download_url":
                    format!("https://github.com/{repo}/releases/download/{tag}/{name}"),
                "digest": null,
            })
        })
        .collect();
    json!([{
        "tag_name": tag,
        "prerelease": false,
        "assets": assets,
        "tarball_url": format!("https://api.github.com/repos/{repo}/tarball/{tag}"),
        "zipball_url": format!("https://api.github.com/repos/{repo}/zipball/{tag}"),
    }])
    .to_string()
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| panic!("a request to {url:?}, which has no host"))
}

/// The registry each npm command that contacts one would use: npm's own default, as long as the
/// command names no other. Also checks every command turns npm's own reporting off.
fn npm_hosts(calls: &[FakeNpmCall]) -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();
    for call in calls {
        assert!(
            !call.args.iter().any(|arg| arg.starts_with("--registry")),
            "an npm command names a registry: {call:?}"
        );
        assert!(
            !call
                .env
                .keys()
                .any(|name| name.to_lowercase().contains("registry")),
            "an npm command's environment names a registry: {call:?}"
        );
        for (name, value) in NPM_PRIVACY_ENV {
            assert_eq!(
                call.env.get(name).map(String::as_str),
                Some(value),
                "an npm command without {name}={value}: {call:?}"
            );
        }
        if matches!(
            call.subcommand.as_str(),
            "info" | "view" | "install" | "update" | "exec"
        ) {
            hosts.insert("registry.npmjs.org".to_string());
        }
    }
    hosts
}

struct AuditDelegate {
    http: Arc<dyn HttpClient>,
    root: PathBuf,
    node: NodeRuntime,
    fs: Arc<dyn fs::Fs>,
}

#[async_trait]
impl LspAdapterDelegate for AuditDelegate {
    fn show_notification(&self, _: &str, _: &mut App) {}

    fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }

    fn worktree_id(&self) -> WorktreeId {
        WorktreeId::from_usize(1)
    }

    fn worktree_root_path(&self) -> &Path {
        &self.root
    }

    fn resolve_relative_path(&self, path: PathBuf) -> PathBuf {
        self.root.join(path)
    }

    fn update_status(&self, _: LanguageServerName, _: BinaryStatus) {}

    fn registered_lsp_adapters(&self) -> Vec<Arc<dyn LspAdapter>> {
        Vec::new()
    }

    async fn language_server_download_dir(&self, name: &LanguageServerName) -> Option<Arc<Path>> {
        let dir = paths::languages_dir().join(name.0.as_ref());
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.into())
    }

    async fn npm_package_installed_version(&self, _: &str) -> Result<Option<(PathBuf, Version)>> {
        Ok(None)
    }

    async fn which(&self, command: &OsStr) -> Option<PathBuf> {
        which(command)
    }

    async fn shell_env(&self) -> HashMap<String, String> {
        HashMap::default()
    }

    async fn read_text_file(&self, _: &RelPath) -> Result<String> {
        anyhow::bail!("the audit worktree has no files")
    }

    async fn try_exec(&self, _: LanguageServerBinary) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl DapDelegate for AuditDelegate {
    fn worktree_id(&self) -> WorktreeId {
        WorktreeId::from_usize(1)
    }

    fn worktree_root_path(&self) -> &Path {
        &self.root
    }

    fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }

    fn node_runtime(&self) -> NodeRuntime {
        self.node.clone()
    }

    fn toolchain_store(&self) -> Arc<dyn LanguageToolchainStore> {
        Arc::new(NoToolchains)
    }

    fn fs(&self) -> Arc<dyn fs::Fs> {
        self.fs.clone()
    }

    fn output_to_console(&self, _: String) {}

    async fn which(&self, command: &OsStr) -> Option<PathBuf> {
        which(command)
    }

    async fn read_text_file(&self, _: &RelPath) -> Result<String> {
        anyhow::bail!("the audit worktree has no files")
    }

    async fn shell_env(&self) -> HashMap<String, String> {
        HashMap::default()
    }

    fn is_headless(&self) -> bool {
        false
    }
}

struct NoToolchains;

#[async_trait(?Send)]
impl LanguageToolchainStore for NoToolchains {
    async fn active_toolchain(
        self: Arc<Self>,
        _: WorktreeId,
        _: Arc<RelPath>,
        _: language::LanguageName,
        _: &mut gpui::AsyncApp,
    ) -> Option<Toolchain> {
        None
    }
}

fn init_languages(cx: &mut TestAppContext, node: NodeRuntime) -> Arc<LanguageRegistry> {
    let settings = cx.update(SettingsStore::test);
    cx.set_global(settings);
    let fs = fs::FakeFs::new(cx.executor());
    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    cx.update(|cx| languages::init(languages.clone(), fs, node, cx));
    languages
}

fn pinned(table: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
    table
        .iter()
        .map(|(name, hosts)| {
            let hosts = hosts.iter().map(|host| host.to_string()).collect();
            (name.to_string(), hosts)
        })
        .collect()
}

#[gpui::test]
async fn hosts_audit_every_download_goes_to_a_pinned_listed_host(cx: &mut TestAppContext) {
    let data_dir = tempfile::tempdir().unwrap();
    paths::set_custom_data_dir(data_dir.path().to_str().unwrap());
    let worktree = tempfile::tempdir().unwrap();
    cx.executor().allow_parking();

    let requests = Requests::default();
    let http = recording_http(requests.clone());
    let npm = FakeNpmLog::default();
    let node = NodeRuntime::fake(http.clone(), npm.clone());
    let languages = init_languages(cx, node.clone());
    let audit = Arc::new(AuditDelegate {
        http: http.clone(),
        root: worktree.path().to_path_buf(),
        node: node.clone(),
        fs: Arc::new(fs::RealFs::new(None, cx.executor())),
    });
    let lsp_delegate: Arc<dyn LspAdapterDelegate> = audit.clone();
    let dap_delegate: Arc<dyn DapDelegate> = audit;
    let mut all_urls = Vec::new();
    let take_hosts = |all_urls: &mut Vec<String>| {
        let urls = requests.take();
        let mut hosts: BTreeSet<String> = urls.iter().map(|url| host_of(url)).collect();
        hosts.extend(npm_hosts(&npm.take()));
        all_urls.extend(urls);
        hosts
    };

    // Language servers, including those registered only for the `language_servers` setting.
    let mut adapters = BTreeMap::new();
    for adapter in languages.all_lsp_adapters() {
        adapters.insert(adapter.name.0.to_string(), adapter);
    }
    for name in languages.available_lsp_adapter_names() {
        let adapter = languages.load_available_lsp_adapter(&name).unwrap();
        adapters.insert(name.0.to_string(), adapter);
    }
    let mut found = BTreeMap::new();
    for (name, adapter) in adapters {
        let container_dir = paths::languages_dir().join(&name);
        std::fs::create_dir_all(&container_dir).unwrap();
        // Most downloads fail on the fake's 404; only the requests matter.
        adapter
            .adapter
            .try_fetch_server_binary(&lsp_delegate, container_dir, false, &mut cx.to_async())
            .await
            .ok();
        found.insert(name, take_hosts(&mut all_urls));
    }

    // bongos's own Node.js, used when there is no suitable system Node.js.
    let options = NodeBinaryOptions {
        allow_path_lookup: false,
        allow_binary_download: true,
        use_paths: None,
    };
    let (_options_tx, options_rx) = watch::channel(Some(options));
    let managed_node = NodeRuntime::new(http.clone(), None, options_rx);
    managed_node.binary_path().await.ok();
    found.insert("Node.js".to_string(), take_hosts(&mut all_urls));
    assert_eq!(found, pinned(&LANGUAGE_SERVER_HOSTS));

    // Debug adapters.
    cx.update(dap_adapters::init);
    let registry = cx.update(|cx| DapRegistry::global(cx).clone());
    let names: Vec<DebugAdapterName> = registry.enumerate_adapters();
    let mut found = BTreeMap::new();
    for name in names {
        if name.as_ref() == "fake-adapter" {
            continue;
        }
        let adapter = registry.adapter(&name).unwrap();
        let definition = DebugTaskDefinition {
            label: "audit".into(),
            adapter: name.clone(),
            config: json!({ "request": "launch", "program": "main" }),
            tcp_connection: None,
        };
        adapter
            .get_binary(
                &dap_delegate,
                &definition,
                None,
                None,
                None,
                &mut cx.to_async(),
            )
            .await
            .ok();
        found.insert(name.to_string(), take_hosts(&mut all_urls));
    }
    assert_eq!(found, pinned(&DEBUG_ADAPTER_HOSTS));

    assert!(!all_urls.is_empty());
    for url in &all_urls {
        let host = host_of(url);
        assert!(
            ALLOWED_HOSTS.contains(&host.as_str()),
            "{url} is not on an allowed host"
        );
        for forbidden in FORBIDDEN {
            assert!(!url.contains(forbidden), "{url} contains {forbidden}");
        }
    }
}

#[gpui::test]
async fn hosts_audit_yaml_language_server_starts_with_telemetry_off(cx: &mut TestAppContext) {
    let http = recording_http(Requests::default());
    let node = NodeRuntime::fake(http.clone(), FakeNpmLog::default());
    let languages = init_languages(cx, node.clone());
    let worktree = tempfile::tempdir().unwrap();
    let delegate: Arc<dyn LspAdapterDelegate> = Arc::new(AuditDelegate {
        http,
        root: worktree.path().to_path_buf(),
        node,
        fs: Arc::new(fs::RealFs::new(None, cx.executor())),
    });
    let yaml = languages
        .all_lsp_adapters()
        .into_iter()
        .find(|adapter| adapter.name.0.as_ref() == "yaml-language-server")
        .unwrap();

    let configuration = yaml
        .adapter
        .clone()
        .workspace_configuration(&delegate, None, None, &mut cx.to_async())
        .await
        .unwrap();

    assert_eq!(
        configuration["redhat"]["telemetry"]["enabled"],
        json!(false)
    );
}
