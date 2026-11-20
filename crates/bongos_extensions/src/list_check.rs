//! The release check of the curated language list: for every entry, fetch the pinned commit,
//! run the install preflight, build the extension and check the extension API version of the
//! built wasm against the range a Stable build loads. It reaches the network (git, and the
//! toolchain downloads of a build), so a developer runs it by hand before a release through
//! `script/bongos/check-language-list`; nothing at run time calls it.

use std::fmt;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use bongos_extensions_model::{
    ApiVersion, LanguageList, Layout, ListEntry, ManifestFacts, SourceUrl, check_after_clone,
    check_before_clone,
};
use extension::ExtensionManifest;
use extension::extension_builder::{CompileExtensionOptions, ExtensionBuilder};
use fs::Fs;
use release_channel::ReleaseChannel;

use crate::git::{GitRunner, stage};
use crate::host::{api_version, built_api_version};
use crate::installer::ToolProber;

/// Builds the extension in a folder.
#[async_trait]
pub trait ListBuilder: Send + Sync {
    /// Compiles the extension rooted at `root` and yields the extension API version of the built
    /// wasm, or `None` when the extension has no Rust part.
    async fn build(&self, root: &Path) -> Result<Option<ApiVersion>>;
}

/// Builds with the real [`ExtensionBuilder`], like the installer's host does.
pub struct RealListBuilder {
    pub builder: ExtensionBuilder,
    pub fs: Arc<dyn Fs>,
}

#[async_trait]
impl ListBuilder for RealListBuilder {
    async fn build(&self, root: &Path) -> Result<Option<ApiVersion>> {
        let mut manifest = ExtensionManifest::load(self.fs.clone(), root).await?;
        let id = manifest.id.clone();
        self.builder
            .compile_extension(
                root,
                &mut manifest,
                CompileExtensionOptions::dev(),
                self.fs.clone(),
            )
            .await?;
        built_api_version(&id, root)
    }
}

/// The extension API versions a Stable build loads.
pub fn stable_api_range() -> RangeInclusive<ApiVersion> {
    let range = extension_host::wasm_host::wit::wasm_api_version_range(ReleaseChannel::Stable);
    api_version(range.start())..=api_version(range.end())
}

/// Everything a check needs from outside.
pub struct CheckContext<'a> {
    pub git: &'a dyn GitRunner,
    pub tools: &'a dyn ToolProber,
    pub builder: &'a dyn ListBuilder,
    /// A folder for the fetched commits; every checkout is deleted after its entry is checked.
    pub scratch: &'a Path,
    pub api_range: RangeInclusive<ApiVersion>,
    pub schema_version_range: RangeInclusive<u32>,
    pub is_blocked_host: fn(&str) -> bool,
    pub is_suppressed: fn(&str) -> bool,
}

/// The result of checking one entry.
#[derive(Debug)]
pub struct EntryReport {
    pub id: String,
    /// The API version of the built wasm (`None`: no Rust part), or what went wrong.
    pub outcome: Result<Option<ApiVersion>, String>,
}

impl EntryReport {
    pub fn passed(&self) -> bool {
        self.outcome.is_ok()
    }
}

impl fmt::Display for EntryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.outcome {
            Ok(Some(api)) => write!(f, "ok    {}: built, extension API {api}", self.id),
            Ok(None) => write!(f, "ok    {}: built, no Rust part", self.id),
            Err(message) => write!(f, "FAIL  {}: {message}", self.id),
        }
    }
}

/// Checks every entry of `list`, in order, one report each. A failing entry never stops the rest.
pub async fn check_list(list: &LanguageList, cx: &CheckContext<'_>) -> Vec<EntryReport> {
    let mut reports = Vec::new();
    for entry in list.entries() {
        let outcome = check_entry(entry, cx)
            .await
            .map_err(|err| format!("{err:#}"));
        reports.push(EntryReport {
            id: entry.id.clone(),
            outcome,
        });
    }
    reports
}

async fn check_entry(entry: &ListEntry, cx: &CheckContext<'_>) -> Result<Option<ApiVersion>> {
    let url = SourceUrl::parse(&entry.repository)?;
    let probe = cx.tools.probe().await;
    check_before_clone(&probe, &url, cx.is_blocked_host)?;
    let layout = Layout::new(cx.scratch.to_path_buf());
    let staged = stage(cx.git, &layout, &url, &entry.commit, entry.path.as_deref()).await?;
    let facts = read_facts(staged.root(), staged.dir())?;
    check_after_clone(
        &probe,
        &facts,
        cx.is_blocked_host,
        cx.is_suppressed,
        |_| false,
        cx.schema_version_range.clone(),
        *cx.api_range.end(),
    )?;
    let built = cx
        .builder
        .build(staged.root())
        .await
        .context("the build failed")?;
    if let Some(api) = built
        && !cx.api_range.contains(&api)
    {
        return Err(anyhow!(
            "the built wasm uses extension API {api}, outside the Stable range {}..={}",
            cx.api_range.start(),
            cx.api_range.end()
        ));
    }
    Ok(built)
}

/// What the preflight needs from the extension's root `root` inside the checkout `checkout`.
fn read_facts(root: &Path, checkout: &Path) -> Result<ManifestFacts> {
    let manifest_path = root.join("extension.toml");
    let mut facts = ManifestFacts {
        has_extension_toml: manifest_path.is_file(),
        has_extension_json: root.join("extension.json").is_file(),
        ..ManifestFacts::default()
    };
    if facts.has_extension_toml {
        let text = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("could not read {}", manifest_path.display()))?;
        let manifest: toml::Table = text.parse().context("extension.toml is not valid TOML")?;
        facts.id = manifest
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("extension.toml has no `id`"))?
            .to_string();
        facts.schema_version = manifest
            .get("schema_version")
            .and_then(|v| v.as_integer())
            .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX));
        if let Some(grammars) = manifest.get("grammars").and_then(|v| v.as_table()) {
            for grammar in grammars.values() {
                if let Some(repository) = grammar.get("repository").and_then(|v| v.as_str()) {
                    facts.grammar_repositories.push(repository.to_string());
                }
            }
        }
    }
    facts.cargo_toml = std::fs::read_to_string(root.join("Cargo.toml")).ok();
    if facts.cargo_toml.is_some() && root != checkout {
        facts.workspace_toml = root
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(checkout))
            .filter_map(|dir| std::fs::read_to_string(dir.join("Cargo.toml")).ok())
            .find(|text| text.contains("[workspace"));
    }
    Ok(facts)
}

/// Reads the list a check runs on: the embedded list when `location` is `None`, else the file at
/// the path or `file://` URL.
pub fn load_list(location: Option<&str>) -> Result<LanguageList> {
    match location {
        None => LanguageList::embedded(),
        Some(location) => {
            let path = location.strip_prefix("file://").unwrap_or(location);
            LanguageList::load_from_file(Path::new(path))
        }
    }
}
