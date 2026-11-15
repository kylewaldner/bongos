//! The Terraform language server adapter: HashiCorp's `terraform-ls`, installed from the pinned
//! archive and started with `serve`. It serves Terraform and Terraform Vars.
//!
//! The `terraform` (or OpenTofu) binary terraform-ls drives is chosen apart: the `terraform.binary`
//! setting, else `terraform` on the worktree's PATH, else `tofu`. It reaches the server as the
//! `terraform.path` initialization option. With none found the server still starts (completion and
//! syntax need no binary) and a notice says so; formatting then fails inside the server.
//!
//! Privacy: the server's environment always carries `CHECKPOINT_DISABLE=1`, which stops
//! HashiCorp's version check (and so every `terraform` it runs). The only host bongos itself
//! contacts is `releases.hashicorp.com`, for the pinned download. One request is not ours to
//! stop: terraform-ls asks Algolia for module-source completion when the user completes a
//! `source` or `version` in a `module` block. That is user-triggered and documented here; the
//! adapter does not filter those completion items.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use bongos_kotlin::{
    InstallCancelled, LockRegistry, Manifest, ManifestEntry, current_platform,
    pinned::{self, TERRAFORM_LS},
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
use util::rel_path::RelPath;

#[cfg(test)]
mod tests;

/// The adapter's language server name.
pub const ADAPTER_NAME: &str = "terraform-ls";

/// The languages the adapter serves, and the languages its notices carry.
pub const LANGUAGES: [&str; 2] = ["Terraform", "Terraform Vars"];

/// The key of the notice posted when neither `terraform` nor `tofu` can be found.
pub const NO_BINARY_NOTICE_KEY: &str = "terraform-binary";

/// The environment variable that stops HashiCorp's version check.
pub const CHECKPOINT_DISABLE: &str = "CHECKPOINT_DISABLE";

/// Where bongos keeps what the Terraform adapter installs and reads.
#[derive(Clone, Debug)]
pub struct TerraformDirs {
    /// `paths::languages_dir()`: terraform-ls installs under `terraform/terraform-ls/<version>`.
    pub languages_dir: PathBuf,
    /// Holds the user's `kotlin-server-manifest.toml`, which may replace the terraform-ls pin.
    pub config_dir: PathBuf,
}

/// The merged `terraform` settings. Read them with a `SettingsLocation` so a project's
/// `.bongos/settings.json` applies over the user settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct TerraformSettings {
    /// The `terraform` or `tofu` binary terraform-ls runs.
    pub binary: Option<String>,
}

impl Settings for TerraformSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let terraform = content
            .project
            .terraform
            .clone()
            .expect("default.json defines `terraform`");
        Self {
            binary: terraform.binary.filter(|binary| !binary.is_empty()),
        }
    }
}

pub struct TerraformLsAdapter {
    languages_dir: PathBuf,
    manifest: Arc<Manifest>,
    platform: Option<&'static str>,
}

impl TerraformLsAdapter {
    pub fn new(languages_dir: PathBuf, manifest: Arc<Manifest>) -> Self {
        Self {
            languages_dir,
            manifest,
            platform: current_platform(),
        }
    }

    /// The command for one start: the install (downloaded with progress and cancel through the
    /// delegate when missing) with `CHECKPOINT_DISABLE=1` in its environment. A failure is also
    /// the delegate's `Failed` status.
    async fn start(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<LanguageServerBinary> {
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
        let Some(platform) = self.platform else {
            return Err(fail("terraform-ls has no build for this platform".into()));
        };
        let entry = match pinned::entry(&self.manifest, TERRAFORM_LS.name, platform) {
            Ok(entry) => entry,
            Err(error) => return Err(fail(format!("{error:#}"))),
        };
        let binary = match pinned::check_installed(TERRAFORM_LS, &entry, &self.languages_dir) {
            Some(binary) => binary,
            None => {
                self.download(delegate, &entry, cx).await.map_err(fail)?;
                pinned::check_installed(TERRAFORM_LS, &entry, &self.languages_dir)
                    .ok_or_else(|| fail("terraform-ls did not install correctly".into()))?
            }
        };
        Ok(with_checkpoint_disabled(binary))
    }

    /// Installs the pin, reporting progress to the delegate. The error is the status text.
    async fn download(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        entry: &ManifestEntry,
        cx: &mut AsyncApp,
    ) -> std::result::Result<(), String> {
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
                    TERRAFORM_LS,
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
            Ok(_) => Ok(()),
            Err(error) if error.downcast_ref::<InstallCancelled>().is_some() => {
                Err(error.to_string())
            }
            Err(error) => Err(format!("could not download terraform-ls\n{error:#}")),
        }
    }
}

/// `binary` with `CHECKPOINT_DISABLE=1` forced into its environment, whatever a user manifest says.
fn with_checkpoint_disabled(mut binary: LanguageServerBinary) -> LanguageServerBinary {
    let mut env = binary.env.take().unwrap_or_default();
    env.insert(CHECKPOINT_DISABLE.to_string(), "1".to_string());
    binary.env = Some(env);
    binary
}

/// The `terraform` binary terraform-ls runs: the setting (a bare name is looked up on PATH, a
/// path resolves against the worktree root), else `terraform` on PATH, else `tofu`.
pub async fn choose_binary(
    delegate: &Arc<dyn LspAdapterDelegate>,
    setting: Option<&str>,
) -> Option<PathBuf> {
    if let Some(setting) = setting {
        if Path::new(setting).components().count() > 1 || Path::new(setting).is_absolute() {
            return Some(delegate.resolve_relative_path(PathBuf::from(setting)));
        }
        if let Some(found) = delegate.which(OsStr::new(setting)).await {
            return Some(found);
        }
        return Some(PathBuf::from(setting));
    }
    for candidate in ["terraform", "tofu"] {
        if let Some(found) = delegate.which(OsStr::new(candidate)).await {
            return Some(found);
        }
    }
    None
}

/// The text of the notice for a start with no `terraform` or `tofu`.
pub const NO_BINARY_TEXT: &str = "Neither terraform nor tofu was found. Completion and syntax work, but formatting and validation need one: install terraform or OpenTofu, or set terraform.binary.";

/// The initialization options naming the binary, or none without one.
pub fn initialization_options(binary: Option<&Path>) -> Value {
    match binary {
        Some(path) => json!({ "terraform": { "path": path.to_string_lossy() } }),
        None => json!({}),
    }
}

#[async_trait(?Send)]
impl LspAdapter for TerraformLsAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static(ADAPTER_NAME)
    }

    /// The `terraform.path` option. Finding no binary posts the notice and sends no path; the
    /// server still starts.
    async fn initialization_options(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Option<Value>> {
        let worktree_id = delegate.worktree_id();
        let setting = cx.update(|cx| {
            TerraformSettings::get(
                Some(SettingsLocation {
                    worktree_id,
                    path: RelPath::empty(),
                }),
                cx,
            )
            .binary
            .clone()
        });
        let binary = choose_binary(delegate, setting.as_deref()).await;
        cx.update(|cx| {
            for language in LANGUAGES {
                match &binary {
                    Some(_) => {
                        LanguageNotices::clear(worktree_id, language, NO_BINARY_NOTICE_KEY, cx)
                    }
                    None => LanguageNotices::report(
                        worktree_id,
                        language,
                        NO_BINARY_NOTICE_KEY,
                        NO_BINARY_TEXT,
                        cx,
                    ),
                }
            }
        });
        Ok(Some(initialization_options(binary.as_deref())))
    }
}

#[async_trait(?Send)]
impl DynLspInstaller for TerraformLsAdapter {
    async fn try_fetch_server_binary(
        &self,
        _delegate: &Arc<dyn LspAdapterDelegate>,
        _container_dir: PathBuf,
        _pre_release: bool,
        _cx: &mut AsyncApp,
    ) -> Result<LanguageServerBinary> {
        bail!("not used by bongos_terraform")
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

/// Registers the `terraform-ls` adapter for Terraform and Terraform Vars and the `terraform`
/// settings.
pub fn init(languages: &LanguageRegistry, dirs: TerraformDirs, cx: &mut App) {
    TerraformSettings::register(cx);
    // Kotlin reports a malformed user manifest; this reads the same file and falls back to the
    // embedded pins without a second report.
    let manifest = Manifest::load_for_app(cfg!(debug_assertions), &dirs.config_dir).manifest;
    let adapter = Arc::new(TerraformLsAdapter::new(
        dirs.languages_dir,
        Arc::new(manifest),
    ));
    for language in LANGUAGES {
        languages.register_lsp_adapter(LanguageName::new_static(language), adapter.clone());
    }
}
