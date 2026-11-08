//! The one language server adapter for Kotlin. Whichever server a project resolves to (kotlin-lsp or
//! IntelliJ), Zed sees a server named `kotlin`, so switching is a restart of that one server.
//!
//! The adapter implements `DynLspInstaller` itself instead of `LspInstaller`: the blanket impl
//! caches the first binary it fetches and returns it on every later start, so a switch would start
//! the same server again.

use std::{path::Path, sync::Arc};

use anyhow::{Result, bail};
use async_trait::async_trait;
use futures::{FutureExt as _, lock::OwnedMutexGuard};
use gpui::{App, AppContext as _, AsyncApp};
use language::{
    DynLspInstaller, LanguageServerBinaryLocations, LspAdapter, LspAdapterDelegate, Toolchain,
};
use lsp::{LanguageServerBinary, LanguageServerBinaryOptions, LanguageServerName};
use settings::{KotlinServerKind, WorktreeId};

use crate::{
    KotlinDirs,
    installed::{check_installed, not_installed, server_name},
    manager::{ADAPTER_NAME, KotlinRegistry},
    manifest::{Manifest, current_platform},
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
                &root,
                delegate.worktree_id(),
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
    let manager = cx
        .try_global::<KotlinRegistry>()
        .and_then(|registry| registry.manager(worktree_root));
    if let Some(manager) = manager
        && let Some(server) = manager.update(cx, |manager, cx| manager.begin_start(worktree_id, cx))
    {
        return server;
    }
    resolve(&dirs.state_dir, worktree_root, worktree_id, cx).server
}

fn start_failed(worktree_root: &Path, worktree_id: WorktreeId, reason: String, cx: &mut App) {
    let manager = cx
        .try_global::<KotlinRegistry>()
        .and_then(|registry| registry.manager(worktree_root));
    if let Some(manager) = manager {
        manager.update(cx, |manager, cx| {
            manager.start_failed(worktree_id, reason, cx)
        });
    }
}

/// The command that starts the effective server for a worktree, or why it cannot start. Only the
/// pinned build counts as installed; installing a missing one is a later step.
pub async fn server_command(
    dirs: &KotlinDirs,
    manifest: &Manifest,
    worktree_root: &Path,
    worktree_id: WorktreeId,
    cx: &mut AsyncApp,
) -> Result<LanguageServerBinary> {
    let server = cx.update(|cx| begin_start(dirs, worktree_root, worktree_id, cx));
    let install_root = dirs.install_root.clone();
    let entry = current_platform()
        .ok_or_else(|| anyhow::anyhow!("no {} build for this platform", server_name(server)))
        .and_then(|platform| manifest.entry(server, platform));
    let binary = match entry {
        Ok(entry) => {
            cx.background_spawn(async move { check_installed(&install_root, server, &entry) })
                .await
        }
        Err(error) => {
            let reason = error.to_string();
            cx.update(|cx| start_failed(worktree_root, worktree_id, reason, cx));
            return Err(error);
        }
    };
    match binary {
        Some(binary) => Ok(binary),
        None => {
            let error = not_installed(server);
            let reason = error.to_string();
            cx.update(|cx| start_failed(worktree_root, worktree_id, reason, cx));
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, rc::Rc};

    use gpui::TestAppContext;
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

    #[gpui::test]
    async fn test_start_fails_when_the_server_is_not_installed(cx: &mut TestAppContext) {
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
        let manager = cx.new(|cx| KotlinProjectManager::new(Rc::new(NoHost), dirs.clone(), cx));
        manager.update(cx, |manager, cx| {
            manager.add_worktree(worktree(), root.clone(), cx)
        });
        let manifest = Manifest::embedded();
        let entry = manifest
            .entry(KotlinServerKind::KotlinLsp, current_platform().unwrap())
            .unwrap();

        let command = |cx: &mut TestAppContext| {
            let (dirs, manifest, root) = (dirs.clone(), manifest.clone(), root.clone());
            cx.update(|cx| {
                cx.spawn(async move |cx| {
                    server_command(&dirs, &manifest, &root, worktree(), cx).await
                })
            })
        };

        let error = command(cx).await.unwrap_err();
        assert_eq!(error.to_string(), "kotlin-lsp is not installed");
        let status = manager.read_with(cx, |manager, _| manager.status(worktree()).cloned());
        assert_eq!(
            status,
            Some(ServerStatus::Failed {
                reason: "kotlin-lsp is not installed".into()
            })
        );

        // A marker for another digest of the pinned version does not count as installed.
        let version_dir = dirs.install_root.join("kotlin-lsp").join(&entry.version);
        let launcher = version_dir.join(&entry.launcher);
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(&launcher, "#!/bin/sh\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let marker = |sha256: &str| {
            serde_json::to_string(&Marker {
                version: entry.version.clone(),
                sha256: sha256.into(),
            })
            .unwrap()
        };
        fs::write(version_dir.join(INSTALLED_MARKER), marker(&"0".repeat(64))).unwrap();
        let error = command(cx).await.unwrap_err();
        assert_eq!(error.to_string(), "kotlin-lsp is not installed");

        // Once the marker names the pinned version and digest, the same call returns the pinned
        // command, and the worktree is starting.
        fs::write(version_dir.join(INSTALLED_MARKER), marker(&entry.sha256)).unwrap();
        let binary = command(cx).await.unwrap();
        assert_eq!(binary.path, launcher);
        assert_eq!(
            binary.arguments,
            entry
                .args
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
        let status = manager.read_with(cx, |manager, _| manager.status(worktree()).cloned());
        assert_eq!(status, Some(ServerStatus::Starting));
    }

    #[gpui::test]
    async fn test_start_without_a_manager_resolves_from_settings(cx: &mut TestAppContext) {
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
        let task = cx.update(|cx| {
            cx.spawn(async move |cx| server_command(&dirs, &manifest, &root, worktree(), cx).await)
        });
        let error = task.await.unwrap_err();
        assert_eq!(error.to_string(), "kotlin-lsp is not installed");
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
