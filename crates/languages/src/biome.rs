//! Biome: the project's own `@biomejs/biome` package, run as `biome lsp-proxy`.
//!
//! Biome is never downloaded. It runs only where the project has a `biome.json` or `biome.jsonc`
//! and has installed the package; [`BiomePolicy`] keeps ESLint and Prettier out of the way there.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use collections::HashMap;
use gpui::{App, AsyncApp};
use language::{LanguageName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{LanguageServerBinary, LanguageServerName};
use node_runtime::NodeRuntime;
use project::{Fs, LanguageServerPolicy, PolicyWorktree};
use util::rel_path::RelPath;

use crate::{
    eslint::EsLintLspAdapter,
    package_json::{PackageJsonData, detect_lockfiles},
};

/// The languages Biome formats and lints.
pub const BIOME_LANGUAGES: [&str; 6] = ["TSX", "TypeScript", "JavaScript", "JSON", "JSONC", "CSS"];

const CONFIG_FILE_NAMES: [&str; 2] = ["biome.json", "biome.jsonc"];
const SERVER_PATH: &str = "node_modules/@biomejs/biome/bin/biome";

pub struct BiomeLspAdapter {
    node: NodeRuntime,
    fs: Arc<dyn Fs>,
}

impl BiomeLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("biome");

    pub fn new(node: NodeRuntime, fs: Arc<dyn Fs>) -> Self {
        Self { node, fs }
    }
}

async fn binary(node: &NodeRuntime, script: PathBuf) -> Result<LanguageServerBinary> {
    Ok(LanguageServerBinary {
        path: node.binary_path().await?,
        env: None,
        arguments: vec![script.into(), "lsp-proxy".into()],
    })
}

/// The project's Biome script, searching from `package_dir` up to `worktree_root`.
pub(crate) async fn find_biome(
    fs: &dyn Fs,
    package_dir: &Path,
    worktree_root: &Path,
) -> Option<PathBuf> {
    for dir in package_dir
        .ancestors()
        .take_while(|dir| dir.starts_with(worktree_root))
    {
        let candidate = dir.join(SERVER_PATH);
        if fs.is_file(&candidate).await {
            return Some(candidate);
        }
    }
    None
}

/// The text shown when Biome is configured but the project has not installed it.
pub(crate) async fn not_installed_message(fs: &dyn Fs, worktree_root: &Path) -> String {
    let mut data = PackageJsonData::default();
    data.lockfiles = detect_lockfiles(fs, &[worktree_root.to_path_buf()]).await;
    if let Ok(text) = fs.load(&worktree_root.join("package.json")).await {
        if let Ok(json) =
            serde_json_lenient::from_str::<HashMap<String, serde_json_lenient::Value>>(&text)
        {
            let lockfiles = std::mem::take(&mut data.lockfiles);
            data = PackageJsonData::new(worktree_root.join("package.json").into(), json);
            data.lockfiles = lockfiles;
        }
    }
    format!(
        "Biome is configured but not installed; run {} install",
        data.resolved_package_manager()
    )
}

impl LspInstaller for BiomeLspAdapter {
    type BinaryVersion = PathBuf;

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let root = delegate.worktree_root_path();
        let script = find_biome(self.fs.as_ref(), root, root).await?;
        binary(&self.node, script).await.ok()
    }

    /// Biome is never downloaded: a project without it gets the "not installed" text.
    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        let root = delegate.worktree_root_path();
        match find_biome(self.fs.as_ref(), root, root).await {
            Some(script) => Ok(script),
            None => Err(anyhow!(not_installed_message(self.fs.as_ref(), root).await)),
        }
    }

    fn fetch_server_binary(
        &self,
        script: Self::BinaryVersion,
        _: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + std::future::Future<Output = Result<LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        async move { binary(&node, script).await }
    }

    async fn cached_server_binary(
        &self,
        _: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        None
    }
}

#[async_trait(?Send)]
impl LspAdapter for BiomeLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }
}

/// Allows Biome where the project configures it, and then refuses ESLint unless the project
/// configures ESLint too. Makes Biome the "auto" formatter there.
pub struct BiomePolicy;

fn has_any(worktree: &PolicyWorktree, names: &[&str]) -> bool {
    names.iter().any(|name| {
        RelPath::unix(name)
            .map(|path| (worktree.has_entry)(path))
            .unwrap_or(false)
    })
}

impl BiomePolicy {
    fn biome_configured(worktree: &PolicyWorktree) -> bool {
        has_any(worktree, &CONFIG_FILE_NAMES)
    }

    fn eslint_configured(worktree: &PolicyWorktree) -> bool {
        has_any(worktree, EsLintLspAdapter::FLAT_CONFIG_FILE_NAMES_V10)
            || has_any(worktree, EsLintLspAdapter::LEGACY_CONFIG_FILE_NAMES)
    }
}

impl LanguageServerPolicy for BiomePolicy {
    fn allows(
        &self,
        worktree: &PolicyWorktree,
        _language: &LanguageName,
        server: &LanguageServerName,
        _cx: &App,
    ) -> bool {
        match server.0.as_ref() {
            "biome" => Self::biome_configured(worktree),
            "eslint" => !Self::biome_configured(worktree) || Self::eslint_configured(worktree),
            _ => true,
        }
    }

    fn formatter_server(
        &self,
        worktree: &PolicyWorktree,
        language: &LanguageName,
        _cx: &App,
    ) -> Option<LanguageServerName> {
        (BIOME_LANGUAGES.contains(&language.as_ref()) && Self::biome_configured(worktree))
            .then(|| BiomeLspAdapter::SERVER_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::WorktreeId;

    fn decide(files: &[&str], cx: &App) -> (bool, bool, Option<LanguageServerName>) {
        let has_entry = |path: &RelPath| files.contains(&path.as_unix_str());
        let worktree = PolicyWorktree {
            id: WorktreeId::from_usize(0),
            abs_path: Path::new("/project"),
            has_entry: &has_entry,
        };
        let language = LanguageName::new_static("TypeScript");
        let allows = |name: &'static str| {
            BiomePolicy.allows(
                &worktree,
                &language,
                &LanguageServerName::new_static(name),
                cx,
            )
        };
        (
            allows("biome"),
            allows("eslint"),
            BiomePolicy.formatter_server(&worktree, &language, cx),
        )
    }

    #[gpui::test]
    fn test_biome_config_allows_biome_and_refuses_eslint(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let (biome, eslint, formatter) = decide(&["biome.json"], cx);
            assert!(biome && !eslint);
            assert_eq!(formatter, Some(LanguageServerName::new_static("biome")));
            let (biome, _, _) = decide(&["biome.jsonc"], cx);
            assert!(biome);
        });
    }

    #[gpui::test]
    fn test_both_configs_keep_eslint(cx: &mut TestAppContext) {
        cx.update(|cx| {
            for eslint_config in ["eslint.config.js", ".eslintrc.json"] {
                let (biome, eslint, _) = decide(&["biome.json", eslint_config], cx);
                assert!(biome && eslint);
            }
        });
    }

    #[gpui::test]
    fn test_no_config_keeps_stock_servers(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let (biome, eslint, formatter) = decide(&["package.json"], cx);
            assert!(!biome && eslint);
            assert_eq!(formatter, None);
        });
    }

    #[gpui::test]
    async fn test_not_installed_names_the_package_manager(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({ "biome.json": "{}", "package.json": "{}", "pnpm-lock.yaml": "" }),
        )
        .await;
        let message = not_installed_message(fs.as_ref(), Path::new("/project")).await;
        assert_eq!(
            message,
            "Biome is configured but not installed; run pnpm install"
        );
        assert!(
            find_biome(fs.as_ref(), Path::new("/project"), Path::new("/project"))
                .await
                .is_none()
        );

        fs.insert_tree(
            "/project/node_modules/@biomejs/biome/bin",
            json!({ "biome": "" }),
        )
        .await;
        fs.insert_tree("/project/packages/app", json!({})).await;
        let found = find_biome(
            fs.as_ref(),
            Path::new("/project/packages/app"),
            Path::new("/project"),
        )
        .await;
        assert_eq!(found, Some(PathBuf::from("/project").join(SERVER_PATH)));
    }
}
