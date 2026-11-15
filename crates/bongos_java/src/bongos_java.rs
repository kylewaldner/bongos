//! The Java language server adapter: Eclipse JDT Language Server (`jdtls`), installed from the
//! pinned archive and launched through `java` directly (the archive's `bin/jdtls` needs Python).
//!
//! Two JDKs are involved and chosen apart. The server JDK runs jdtls itself and must be new enough
//! for the pinned build (`bongos_kotlin::server_jdk`); none is a start failure with the exact cause
//! and a notice. The import JDK is the one `bongos_kotlin::detect` picks for the project (Gradle
//! toolchain, Maven level, `JAVA_HOME`, candidates) and reaches jdtls through its initialization
//! options. Failing to find an import JDK never fails a start: jdtls then uses its own default.
//!
//! jdtls writes its workspace under `<languages dir>/java/workspaces/<hash of the worktree root>`,
//! never inside the project.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use bongos_kotlin::{
    InstallCancelled, JdkInputs, LockRegistry, Manifest, ManifestEntry, ServerJdkChoice,
    current_platform, default_candidate_patterns, detect,
    pinned::{self, JDTLS},
    server_jdk,
};
use bongos_language_status::LanguageNotices;
use futures::{FutureExt as _, lock::OwnedMutexGuard};
use gpui::{App, AppContext as _, AsyncApp};
use language::{
    BinaryStatus, DynLspInstaller, LanguageName, LanguageRegistry, LanguageServerBinaryLocations,
    LspAdapter, LspAdapterDelegate, Toolchain,
};
use lsp::{LanguageServerBinary, LanguageServerBinaryOptions, LanguageServerName};
use serde_json::{Value, json};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsLocation};
use sha2::{Digest as _, Sha256};
use util::rel_path::RelPath;

mod show_jdk;
#[cfg(test)]
mod tests;

pub use show_jdk::{SHOW_JDK_TITLE, ShowJdk, ShowJdkRequest};

/// The adapter's language server name.
pub const ADAPTER_NAME: &str = "jdtls";

/// The language the adapter serves, and the language its notices carry.
pub const LANGUAGE: &str = "Java";

/// The oldest JDK the pinned jdtls runs on (recorded beside the pin in `servers.toml`).
pub const MIN_SERVER_JDK: u32 = 25;

/// The key of the notice posted when no JDK can run jdtls.
pub const NO_JDK_NOTICE_KEY: &str = "server-jdk";

/// The heap limit passed to jdtls.
pub const HEAP_LIMIT: &str = "-Xmx1G";

/// Where bongos keeps what the Java adapter installs and reads.
#[derive(Clone, Debug)]
pub struct JavaDirs {
    /// `paths::languages_dir()`: jdtls installs under `java/jdtls/<version>` and its workspaces
    /// live under `java/workspaces`.
    pub languages_dir: PathBuf,
    /// Holds the user's `kotlin-server-manifest.toml`, which may replace the jdtls pin.
    pub config_dir: PathBuf,
}

/// The merged `java` settings. Read them with a `SettingsLocation` so a project's
/// `.bongos/settings.json` applies over the user settings. A relative path resolves against the
/// worktree root.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct JavaSettings {
    /// The JDK jdtls itself runs on.
    pub server_jdk: Option<PathBuf>,
    /// The JDK used for project import and builds.
    pub jdk_home: Option<PathBuf>,
}

impl Settings for JavaSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let java = content
            .project
            .java
            .clone()
            .expect("default.json defines `java`");
        Self {
            server_jdk: java.server_jdk.map(PathBuf::from),
            jdk_home: java.jdk_home.map(PathBuf::from),
        }
    }
}

/// What JDK discovery reads from the machine, so a test never sees the real one.
#[derive(Clone, Debug, Default)]
pub struct JdkEnvironment {
    pub java_home: Option<OsString>,
    pub candidates: Vec<String>,
}

impl JdkEnvironment {
    pub fn from_process() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self {
            java_home: std::env::var_os("JAVA_HOME"),
            candidates: default_candidate_patterns(home.as_deref()),
        }
    }
}

pub struct JdtlsAdapter {
    languages_dir: PathBuf,
    manifest: Arc<Manifest>,
    platform: Option<&'static str>,
    env: JdkEnvironment,
}

impl JdtlsAdapter {
    pub fn new(languages_dir: PathBuf, manifest: Arc<Manifest>, env: JdkEnvironment) -> Self {
        Self {
            languages_dir,
            manifest,
            platform: current_platform(),
            env,
        }
    }

    fn jdk_inputs(&self, root: &Path, jdk_home: Option<PathBuf>) -> JdkInputs {
        JdkInputs {
            setting: jdk_home,
            setting_name: "java.jdk_home",
            project_settings: Some(root.join(".bongos").join("settings.json")),
            root: root.to_path_buf(),
            java_home: self.env.java_home.clone(),
            candidates: self.env.candidates.clone(),
        }
    }

    /// The command for one start: the server JDK, the install (downloaded with progress and
    /// cancel through the delegate when missing) and the workspace. A failure is also the
    /// delegate's `Failed` status, so the status line shows its first line.
    async fn start(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<LanguageServerBinary> {
        let root = delegate.worktree_root_path().to_path_buf();
        let worktree_id = delegate.worktree_id();
        let name = LanguageServerName::new_static(ADAPTER_NAME);
        let fail = |message: String| {
            delegate.update_status(
                name.clone(),
                BinaryStatus::Failed {
                    error: message.clone(),
                },
            );
            anyhow!(message)
        };

        let java = cx.update(|cx| {
            JavaSettings::get(
                Some(SettingsLocation {
                    worktree_id,
                    path: RelPath::empty(),
                }),
                cx,
            )
            .clone()
        });
        let inputs = self.jdk_inputs(&root, java.jdk_home);
        let server = server_jdk(&inputs, java.server_jdk.as_deref(), MIN_SERVER_JDK);
        let jdk = match server.choice {
            ServerJdkChoice::Found(jdk) => {
                cx.update(|cx| {
                    LanguageNotices::clear(worktree_id, LANGUAGE, NO_JDK_NOTICE_KEY, cx)
                });
                jdk
            }
            ServerJdkChoice::Unavailable(cause) => {
                let text = format!(
                    "{cause}. Install a JDK {MIN_SERVER_JDK} or newer, or set java.server_jdk."
                );
                cx.update(|cx| {
                    LanguageNotices::report(worktree_id, LANGUAGE, NO_JDK_NOTICE_KEY, &text, cx)
                });
                return Err(fail(cause));
            }
        };

        let Some(platform) = self.platform else {
            return Err(fail("jdtls has no build for this platform".into()));
        };
        let entry = match pinned::entry(&self.manifest, JDTLS.name, platform) {
            Ok(entry) => entry,
            Err(error) => return Err(fail(format!("{error:#}"))),
        };
        let install = if pinned::check_installed(JDTLS, &entry, &self.languages_dir).is_some() {
            JDTLS.version_dir(&self.languages_dir, &entry.version)
        } else {
            self.download(delegate, &entry, cx).await.map_err(fail)?
        };

        let workspace = workspace_dir(&self.languages_dir, &root);
        jdtls_command(&jdk.home, &install, &entry, platform, &workspace)
            .map_err(|error| fail(format!("{error:#}")))
    }

    /// Installs the pin, reporting progress to the delegate. The error is the status text.
    async fn download(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        entry: &ManifestEntry,
        cx: &mut AsyncApp,
    ) -> std::result::Result<PathBuf, String> {
        let name = LanguageServerName::new_static(ADAPTER_NAME);
        delegate.update_status(name.clone(), BinaryStatus::Downloading);
        let cancel = delegate.download_cancel_flag(name.clone());
        let locks = cx.update(|cx| LockRegistry::global(cx));
        let task = cx.update(|cx| {
            let delegate = delegate.clone();
            let http = delegate.http_client();
            let entry = entry.clone();
            let languages_dir = self.languages_dir.clone();
            cx.background_spawn(async move {
                let progress = {
                    let delegate = delegate.clone();
                    move |done, total| {
                        delegate.report_download_progress(
                            LanguageServerName::new_static(ADAPTER_NAME),
                            done,
                            total,
                        )
                    }
                };
                pinned::ensure_installed(
                    &locks,
                    JDTLS,
                    &entry,
                    &languages_dir,
                    http.as_ref(),
                    &progress,
                    &cancel,
                )
                .await
            })
        });
        match task.await {
            Ok(installed) => Ok(installed.dir),
            Err(error) if error.downcast_ref::<InstallCancelled>().is_some() => {
                Err(error.to_string())
            }
            Err(error) => Err(format!("could not download jdtls\n{error:#}")),
        }
    }
}

#[async_trait(?Send)]
impl LspAdapter for JdtlsAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static(ADAPTER_NAME)
    }

    /// The import JDK for jdtls: one default runtime and the Gradle Java home. No import JDK (a
    /// Gradle toolchain nothing satisfies, or no JDK at all) sends neither, and jdtls picks its
    /// own; the start never fails for it.
    async fn initialization_options(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Option<Value>> {
        let root = delegate.worktree_root_path().to_path_buf();
        let worktree_id = delegate.worktree_id();
        let jdk_home = cx.update(|cx| {
            JavaSettings::get(
                Some(SettingsLocation {
                    worktree_id,
                    path: RelPath::empty(),
                }),
                cx,
            )
            .jdk_home
            .clone()
        });
        let detection = detect(&self.jdk_inputs(&root, jdk_home));
        Ok(Some(initialization_options(detection.jdk())))
    }
}

#[async_trait(?Send)]
impl DynLspInstaller for JdtlsAdapter {
    async fn try_fetch_server_binary(
        &self,
        _delegate: &Arc<dyn LspAdapterDelegate>,
        _container_dir: PathBuf,
        _pre_release: bool,
        _cx: &mut AsyncApp,
    ) -> Result<LanguageServerBinary> {
        bail!("not used by bongos_java")
    }

    fn get_language_server_command(
        self: Arc<Self>,
        delegate: Arc<dyn LspAdapterDelegate>,
        _toolchains: Option<Toolchain>,
        _binary_options: LanguageServerBinaryOptions,
        _cached_binary: OwnedMutexGuard<Option<(bool, LanguageServerBinary)>>,
        mut cx: AsyncApp,
    ) -> LanguageServerBinaryLocations {
        async move { (self.start(&delegate, &mut cx).await, None) }.boxed_local()
    }
}

/// The initialization options for an import JDK, or the bare minimum without one.
pub fn initialization_options(import_jdk: Option<&bongos_kotlin::Jdk>) -> Value {
    let mut java = json!({
        // Without this jdtls writes `.project` and `.classpath` next to the build files.
        "import": { "generatesMetadataFilesAtProjectRoot": false },
    });
    if let Some(jdk) = import_jdk {
        let home = jdk.home.to_string_lossy().into_owned();
        java["import"]["gradle"] = json!({ "java": { "home": home } });
        if let Some(major) = jdk.major {
            java["configuration"] = json!({
                "runtimes": [{ "name": runtime_name(major), "path": home, "default": true }],
            });
        }
    }
    json!({ "settings": { "java": java } })
}

/// The execution environment name jdtls knows a JDK by: `JavaSE-21`, `JavaSE-1.8`.
fn runtime_name(major: u32) -> String {
    if major <= 8 {
        format!("JavaSE-1.{major}")
    } else {
        format!("JavaSE-{major}")
    }
}

/// The directory of the install's configuration for `platform`.
pub fn config_dir_name(platform: &str) -> Result<&'static str> {
    Ok(match platform {
        "linux-x64" => "config_linux",
        "linux-arm64" => "config_linux_arm",
        "macos-x64" => "config_mac",
        "macos-arm64" => "config_mac_arm",
        other => bail!("jdtls has no configuration for {other}"),
    })
}

/// jdtls's data directory for a worktree: `<languages dir>/java/workspaces/<16 hex digits of the
/// SHA-256 of the canonical worktree root>`. Outside the project, so jdtls writes nothing into it.
pub fn workspace_dir(languages_dir: &Path, worktree_root: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.into());
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let hex: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    languages_dir.join("java").join("workspaces").join(hex)
}

/// The launcher jar the install holds: `plugins/org.eclipse.equinox.launcher_<version>.jar`.
fn launcher_jar(install: &Path) -> Result<PathBuf> {
    let plugins = install.join("plugins");
    let mut jars: Vec<PathBuf> = std::fs::read_dir(&plugins)
        .with_context(|| format!("reading {}", plugins.display()))?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("org.eclipse.equinox.launcher_") && name.ends_with(".jar")
                })
        })
        .collect();
    jars.sort();
    jars.pop()
        .ok_or_else(|| anyhow!("jdtls has no launcher jar in {}", plugins.display()))
}

/// The command that starts jdtls from `install` on the JDK at `java_home`.
pub fn jdtls_command(
    java_home: &Path,
    install: &Path,
    entry: &ManifestEntry,
    platform: &str,
    workspace: &Path,
) -> Result<LanguageServerBinary> {
    let mut arguments: Vec<OsString> = [
        "-Declipse.application=org.eclipse.jdt.ls.core.id1",
        "-Dosgi.bundles.defaultStartLevel=4",
        "-Declipse.product=org.eclipse.jdt.ls.core.product",
        HEAP_LIMIT,
        "--add-modules=ALL-SYSTEM",
        "--add-opens",
        "java.base/java.util=ALL-UNNAMED",
        "--add-opens",
        "java.base/java.lang=ALL-UNNAMED",
        "-jar",
    ]
    .map(OsString::from)
    .into();
    arguments.push(launcher_jar(install)?.into_os_string());
    arguments.push("-configuration".into());
    arguments.push(install.join(config_dir_name(platform)?).into_os_string());
    arguments.push("-data".into());
    arguments.push(workspace.as_os_str().to_owned());
    arguments.extend(entry.args.iter().map(OsString::from));
    let env = (!entry.env.is_empty()).then(|| {
        entry
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    });
    Ok(LanguageServerBinary {
        path: java_home.join("bin").join("java"),
        arguments,
        env,
    })
}

/// Registers the `jdtls` adapter for Java and the `java` settings.
pub fn init(languages: &LanguageRegistry, dirs: JavaDirs, cx: &mut App) {
    JavaSettings::register(cx);
    // Kotlin reports a malformed user manifest; this reads the same file and falls back to the
    // embedded pins without a second report.
    let manifest = Manifest::load_for_app(cfg!(debug_assertions), &dirs.config_dir).manifest;
    languages.register_lsp_adapter(
        LanguageName::new_static(LANGUAGE),
        Arc::new(JdtlsAdapter::new(
            dirs.languages_dir,
            Arc::new(manifest),
            JdkEnvironment::from_process(),
        )),
    );
}
