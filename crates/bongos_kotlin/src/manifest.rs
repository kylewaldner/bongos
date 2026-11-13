//! The pinned server manifest (`servers.toml`): which build of each server bongos installs, where
//! it comes from and the SHA-256 it must have. The manifest is embedded in the binary. A debug
//! build can read another one from `BONGOS_KOTLIN_SERVERS` (decision D7), which tests use to install
//! fake servers from loopback; a release build never looks at the variable.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use chrono::NaiveDate;
use serde::Deserialize;
use settings::KotlinServerKind;

use crate::installed::server_name;

/// The manifest compiled into the binary.
pub const EMBEDDED: &str = include_str!("../servers.toml");

/// Names a manifest file that replaces the embedded one, in debug builds only.
pub const OVERRIDE_VAR: &str = "BONGOS_KOTLIN_SERVERS";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum ArchiveKind {
    #[serde(rename = "tar.gz")]
    TarGz,
    /// A zip file. JetBrains' `.sit` files are zips.
    #[serde(rename = "zip")]
    Zip,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlatformPin {
    url: String,
    sha256: String,
    archive: ArchiveKind,
    /// Leading directories of the archive to drop. Each must be the only entry of its parent.
    #[serde(default)]
    strip_components: usize,
    /// Path of the launcher inside the extracted archive.
    launcher: String,
    /// Path of the bundled Java runtime inside the extracted archive.
    #[serde(default)]
    runtime: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerPin {
    version: String,
    /// The day the build was made. Early-access builds expire shortly after.
    #[serde(default)]
    built: Option<NaiveDate>,
    /// The last day the build runs.
    #[serde(default)]
    expires: Option<NaiveDate>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    platform: BTreeMap<String, PlatformPin>,
}

/// One server on one platform: everything `install` and `check_installed` need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    pub version: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the archive at `url`.
    pub sha256: String,
    pub archive: ArchiveKind,
    pub strip_components: usize,
    pub launcher: String,
    pub runtime: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub built: Option<NaiveDate>,
    pub expires: Option<NaiveDate>,
}

impl ManifestEntry {
    /// Whether the build no longer runs on `today`: `expires` is the last day it does.
    pub fn is_expired(&self, today: NaiveDate) -> bool {
        self.expires.is_some_and(|expires| today > expires)
    }
}

#[derive(Clone, Debug)]
pub struct Manifest {
    servers: BTreeMap<String, ServerPin>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let servers: BTreeMap<String, ServerPin> =
            toml::from_str(text).context("parsing the Kotlin server manifest")?;
        for (name, server) in &servers {
            ensure!(
                is_plain_name(&server.version),
                "{name}: version {:?} is not a plain directory name",
                server.version
            );
            ensure!(
                !server.platform.is_empty(),
                "{name}: no platforms are pinned"
            );
            for (platform, pin) in &server.platform {
                ensure!(
                    is_sha256_hex(&pin.sha256),
                    "{name} on {platform}: sha256 must be 64 hexadecimal characters, got {:?}",
                    pin.sha256
                );
                ensure!(!pin.url.is_empty(), "{name} on {platform}: url is empty");
                ensure!(
                    is_relative_path(&pin.launcher),
                    "{name} on {platform}: launcher {:?} is not a relative path inside the archive",
                    pin.launcher
                );
            }
        }
        Ok(Self { servers })
    }

    /// The manifest compiled into the binary.
    pub fn embedded() -> Self {
        Self::parse(EMBEDDED).expect("servers.toml is valid (a test parses it)")
    }

    /// The embedded manifest, or in a debug build (`allow_override`, from
    /// `cfg!(debug_assertions)`) the file `BONGOS_KOTLIN_SERVERS` names.
    pub fn load(allow_override: bool) -> Result<Self> {
        Self::load_with(allow_override, std::env::var_os(OVERRIDE_VAR))
    }

    fn load_with(allow_override: bool, override_path: Option<OsString>) -> Result<Self> {
        match override_path {
            Some(path) if allow_override => {
                let path = PathBuf::from(path);
                log::warn!(
                    "using the Kotlin server manifest {} from {OVERRIDE_VAR}",
                    path.display()
                );
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                Self::parse(&text).with_context(|| path.display().to_string())
            }
            _ => Ok(Self::embedded()),
        }
    }

    /// The arguments and environment `server` starts with, whatever build runs.
    pub(crate) fn args_and_env(
        &self,
        server: KotlinServerKind,
    ) -> Option<(Vec<String>, BTreeMap<String, String>)> {
        let pin = self.servers.get(server_name(server))?;
        Some((pin.args.clone(), pin.env.clone()))
    }

    /// The pin of `server` for `platform` (`current_platform()` in the app).
    pub fn entry(&self, server: KotlinServerKind, platform: &str) -> Result<ManifestEntry> {
        let name = server_name(server);
        let pin = self
            .servers
            .get(name)
            .ok_or_else(|| anyhow!("{name} is not in the server manifest"))?;
        let Some(platform_pin) = pin.platform.get(platform) else {
            bail!("{name} has no build for {platform}");
        };
        Ok(ManifestEntry {
            version: pin.version.clone(),
            url: platform_pin.url.clone(),
            sha256: platform_pin.sha256.to_ascii_lowercase(),
            archive: platform_pin.archive,
            strip_components: platform_pin.strip_components,
            launcher: platform_pin.launcher.clone(),
            runtime: platform_pin.runtime.clone(),
            args: pin.args.clone(),
            env: pin.env.clone(),
            built: pin.built,
            expires: pin.expires,
        })
    }
}

/// True when `name` is one plain path component, so joining it cannot leave its parent.
pub(crate) fn is_plain_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

pub(crate) fn is_relative_path(path: &str) -> bool {
    let mut components = Path::new(path).components().peekable();
    components.peek().is_some() && components.all(|part| matches!(part, Component::Normal(_)))
}

pub(crate) fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The manifest key of the machine bongos runs on, or `None` where no build is pinned.
pub fn current_platform() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x64")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("linux-arm64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("macos-arm64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("macos-x64")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLATFORMS: [&str; 4] = ["linux-x64", "linux-arm64", "macos-arm64", "macos-x64"];

    #[test]
    fn test_embedded_manifest_pins_both_servers_on_every_platform() {
        let manifest = Manifest::embedded();
        for server in [KotlinServerKind::KotlinLsp, KotlinServerKind::Intellij] {
            for platform in PLATFORMS {
                let entry = manifest.entry(server, platform).unwrap();
                assert!(entry.url.starts_with("https://download.jetbrains.com/"));
                assert!(is_sha256_hex(&entry.sha256), "{server:?} {platform}");
                assert_eq!(entry.launcher, "bin/intellij-server");
                assert_eq!(entry.strip_components, 1);
                assert!(entry.args.contains(&"--stdio".to_string()));
                assert_eq!(
                    entry
                        .env
                        .get("INTELLIJ_UPDATES_ENABLED")
                        .map(String::as_str),
                    Some("false")
                );
                assert!(
                    entry.url.contains(&entry.version),
                    "{server:?} {platform}: url names another version"
                );
                let expected = if platform.starts_with("macos") {
                    ArchiveKind::Zip
                } else {
                    ArchiveKind::TarGz
                };
                assert_eq!(entry.archive, expected);
            }
        }
    }

    #[test]
    fn test_kotlin_lsp_pin_records_build_and_expiry() {
        let entry = Manifest::embedded()
            .entry(KotlinServerKind::KotlinLsp, "linux-x64")
            .unwrap();
        let (built, expires) = (entry.built.unwrap(), entry.expires.unwrap());
        assert!(built < expires);
        assert_eq!((expires - built).num_days(), 29);
    }

    #[test]
    fn test_unknown_platform_and_bad_digest_are_errors() {
        let manifest = Manifest::embedded();
        let error = manifest
            .entry(KotlinServerKind::KotlinLsp, "plan9-mips")
            .unwrap_err();
        assert_eq!(error.to_string(), "kotlin-lsp has no build for plan9-mips");

        let pinned_digest = manifest
            .entry(KotlinServerKind::KotlinLsp, "linux-x64")
            .unwrap()
            .sha256;
        let bad = EMBEDDED.replacen(&pinned_digest, "", 1);
        let error = Manifest::parse(&bad).unwrap_err();
        assert!(format!("{error:#}").contains("sha256 must be 64 hexadecimal"));
    }

    /// The kotlin-lsp version in `servers.toml`, so a refreshed pin does not break these tests.
    fn pinned_version() -> String {
        Manifest::embedded()
            .entry(KotlinServerKind::KotlinLsp, "linux-x64")
            .unwrap()
            .version
    }

    fn fake_manifest_file(dir: &std::path::Path) -> OsString {
        let path = dir.join("servers.toml");
        let text = EMBEDDED.replace(&pinned_version(), "0.0.1");
        fs::write(&path, text).unwrap();
        path.into_os_string()
    }

    #[test]
    fn test_override_is_read_only_when_allowed() {
        let temp = tempfile::tempdir().unwrap();
        let path = fake_manifest_file(temp.path());
        let version = |manifest: Manifest| {
            manifest
                .entry(KotlinServerKind::KotlinLsp, "linux-x64")
                .unwrap()
                .version
        };
        // Allowed (a debug build): the file wins over the embedded manifest.
        assert_eq!(
            version(Manifest::load_with(true, Some(path.clone())).unwrap()),
            "0.0.1"
        );
        // Not allowed (a release build): the variable is ignored, even though the file exists.
        assert_eq!(
            version(Manifest::load_with(false, Some(path)).unwrap()),
            pinned_version()
        );
        // Allowed but unset: embedded.
        assert_eq!(
            version(Manifest::load_with(true, None).unwrap()),
            pinned_version()
        );
        // Allowed and unreadable: an error, not a silent fallback.
        let missing = temp.path().join("missing.toml").into_os_string();
        assert!(Manifest::load_with(true, Some(missing)).is_err());
    }
}
