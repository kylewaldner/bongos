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
    installed::{find_installed, not_installed},
    manager::{ADAPTER_NAME, KotlinRegistry},
    resolve::resolve,
};

pub struct KotlinAdapter {
    dirs: KotlinDirs,
}

impl KotlinAdapter {
    pub fn new(dirs: KotlinDirs) -> Self {
        Self { dirs }
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
            let result = server_command(&self.dirs, &root, delegate.worktree_id(), &mut cx).await;
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

/// The command that starts the effective server for a worktree, or why it cannot start.
pub async fn server_command(
    dirs: &KotlinDirs,
    worktree_root: &Path,
    worktree_id: WorktreeId,
    cx: &mut AsyncApp,
) -> Result<LanguageServerBinary> {
    let server = cx.update(|cx| begin_start(dirs, worktree_root, worktree_id, cx));
    let install_root = dirs.install_root.clone();
    let binary = cx
        .background_spawn(async move { find_installed(&install_root, server) })
        .await;
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
    use std::{fs, rc::Rc};

    use gpui::TestAppContext;
    use settings::{Settings as _, SettingsStore};

    use super::*;
    use crate::{
        INSTALLED_MARKER, KotlinProjectManager, KotlinSettings, ServerStatus, manager::KotlinHost,
    };

    struct NoHost;

    impl KotlinHost for NoHost {
        fn restart(&self, _: WorktreeId, _: &mut App) {}
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

        let command = |cx: &mut TestAppContext| {
            let (dirs, root) = (dirs.clone(), root.clone());
            cx.update(|cx| {
                cx.spawn(async move |cx| server_command(&dirs, &root, worktree(), cx).await)
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

        // Once the server's marker and launcher exist, the same call returns its command, and the
        // worktree is starting.
        let version_dir = dirs.install_root.join("kotlin-lsp").join("1.0");
        fs::create_dir_all(version_dir.join("bin")).unwrap();
        fs::write(version_dir.join("bin/kotlin-lsp"), "#!/bin/sh\n").unwrap();
        fs::write(
            version_dir.join(INSTALLED_MARKER),
            r#"{ "launcher": "bin/kotlin-lsp", "args": ["--stdio"] }"#,
        )
        .unwrap();
        let binary = command(cx).await.unwrap();
        assert_eq!(binary.path, version_dir.join("bin/kotlin-lsp"));
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
        let task = cx.update(|cx| {
            cx.spawn(async move |cx| server_command(&dirs, &root, worktree(), cx).await)
        });
        let error = task.await.unwrap_err();
        assert_eq!(error.to_string(), "kotlin-lsp is not installed");
    }
}
