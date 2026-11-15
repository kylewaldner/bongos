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

/// The optional user-maintained manifest in the config directory.
pub const USER_FILE_NAME: &str = "kotlin-server-manifest.toml";

/// The host bongos never downloads from, and so refuses in a user manifest (with subdomains).
const REFUSED_HOST: &str = "bongos.rocks";

/// The manifest to use after looking for the user file, and the problem with the file if any.
#[derive(Clone, Debug)]
pub struct UserManifest {
    pub manifest: Manifest,
    /// Names the file and its first error. `None` when the file was missing or merged.
    pub error: Option<String>,
}

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
    /// `None` when the table omits it; a user file's omission takes the built-in value.
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
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

    /// What the app runs with: in a debug build `BONGOS_KOTLIN_SERVERS` when set (it wins over the
    /// user file), else the embedded manifest merged with the user file in `config_dir`.
    pub fn load_for_app(allow_override: bool, config_dir: &Path) -> UserManifest {
        if allow_override && std::env::var_os(OVERRIDE_VAR).is_some() {
            return match Self::load(allow_override) {
                Ok(manifest) => UserManifest {
                    manifest,
                    error: None,
                },
                Err(error) => UserManifest {
                    manifest: Self::embedded(),
                    error: Some(format!("{error:#}")),
                },
            };
        }
        Self::load_user_file(config_dir)
    }

    /// The embedded manifest with the servers of `<config_dir>/kotlin-server-manifest.toml`
    /// merged over it. A server table in the file replaces the built-in one whole, except that an
    /// omitted `args` or `env` keeps the built-in value. A missing file is exactly
    /// `Manifest::embedded()` without an error; an unreadable or malformed one falls back to it
    /// and says why.
    pub fn load_user_file(config_dir: &Path) -> UserManifest {
        let path = config_dir.join(USER_FILE_NAME);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                log::debug!("no user Kotlin server manifest at {}", path.display());
                return UserManifest {
                    manifest: Self::embedded(),
                    error: None,
                };
            }
            Err(error) => return Self::user_file_failed(&path, anyhow!(error)),
        };
        match Self::merge_user_text(&text) {
            Ok(manifest) => {
                log::info!("using the Kotlin server manifest {}", path.display());
                UserManifest {
                    manifest,
                    error: None,
                }
            }
            Err(error) => Self::user_file_failed(&path, error),
        }
    }

    fn user_file_failed(path: &Path, error: anyhow::Error) -> UserManifest {
        let message = format!(
            "{}: {}",
            path.display(),
            format!("{error:#}")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        );
        UserManifest {
            manifest: Self::embedded(),
            error: Some(message),
        }
    }

    fn merge_user_text(text: &str) -> Result<Self> {
        let user = Self::parse(text)?;
        let mut merged = Self::embedded();
        for (name, mut pin) in user.servers {
            ensure!(
                [KotlinServerKind::KotlinLsp, KotlinServerKind::Intellij]
                    .into_iter()
                    .any(|server| server_name(server) == name),
                "{name} is not a Kotlin server bongos knows"
            );
            for (platform, platform_pin) in &pin.platform {
                if let Some(problem) = user_url_problem(&platform_pin.url) {
                    bail!(
                        "{name} on {platform}: url {:?}: {problem}",
                        platform_pin.url
                    );
                }
            }
            if let Some(builtin) = merged.servers.get(&name) {
                pin.args = pin.args.or_else(|| builtin.args.clone());
                pin.env = pin.env.or_else(|| builtin.env.clone());
            }
            merged.servers.insert(name, pin);
        }
        Ok(merged)
    }

    /// The arguments and environment `server` starts with, whatever build runs.
    pub(crate) fn args_and_env(
        &self,
        server: KotlinServerKind,
    ) -> Option<(Vec<String>, BTreeMap<String, String>)> {
        let pin = self.servers.get(server_name(server))?;
        Some((
            pin.args.clone().unwrap_or_default(),
            pin.env.clone().unwrap_or_default(),
        ))
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
            args: pin.args.clone().unwrap_or_default(),
            env: pin.env.clone().unwrap_or_default(),
            built: pin.built,
            expires: pin.expires,
        })
    }
}

/// Why `url` from the user manifest may not be downloaded, or `None`: only `https` (loopback
/// `http` in a debug build), no user name or password, and never `bongos.rocks` or a subdomain.
pub(crate) fn user_url_problem(url: &str) -> Option<String> {
    let url = match http_client::Url::parse(url) {
        Ok(url) => url,
        Err(error) => return Some(format!("it is not a URL ({error})")),
    };
    if let Some(problem) = crate::remote_manifest::manifest_url_problem(&url) {
        return Some(problem.to_string());
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    if host == REFUSED_HOST || host.ends_with(&format!(".{REFUSED_HOST}")) {
        return Some(format!("{REFUSED_HOST} is never a download host"));
    }
    None
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

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn user_table(server: &str, url: &str, extra: &str) -> String {
        format!(
            r#"
[{server}]
version = "9.9.9"
{extra}

[{server}.platform.linux-x64]
url = "{url}"
sha256 = "{SHA}"
archive = "tar.gz"
launcher = "bin/launcher"
"#
        )
    }

    fn load(text: Option<&str>) -> (UserManifest, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        if let Some(text) = text {
            fs::write(temp.path().join(USER_FILE_NAME), text).unwrap();
        }
        (Manifest::load_user_file(temp.path()), temp)
    }

    fn entry(manifest: &Manifest, server: KotlinServerKind) -> ManifestEntry {
        manifest.entry(server, "linux-x64").unwrap()
    }

    #[test]
    fn test_missing_user_file_is_exactly_the_embedded_manifest() {
        let (loaded, _temp) = load(None);
        assert_eq!(loaded.error, None);
        let embedded = Manifest::embedded();
        for server in [KotlinServerKind::KotlinLsp, KotlinServerKind::Intellij] {
            for platform in PLATFORMS {
                assert_eq!(
                    loaded.manifest.entry(server, platform).unwrap(),
                    embedded.entry(server, platform).unwrap()
                );
            }
        }
    }

    #[test]
    fn test_valid_user_file_parses_and_keeps_other_builtin_servers() {
        let text = user_table(
            "kotlin-lsp",
            "https://example.com/server.tar.gz",
            "args = [\"--stdio\", \"--x\"]\n[kotlin-lsp.env]\nA = \"b\"\n",
        );
        // The env table must come after the platform keys of its own table; build it apart.
        let text = text.replace("[kotlin-lsp.env]\nA = \"b\"\n", "");
        let text = format!("{text}\n[kotlin-lsp.env]\nA = \"b\"\n");
        let (loaded, _temp) = load(Some(&text));
        assert_eq!(loaded.error, None, "{:?}", loaded.error);
        let lsp = entry(&loaded.manifest, KotlinServerKind::KotlinLsp);
        assert_eq!(lsp.version, "9.9.9");
        assert_eq!(lsp.url, "https://example.com/server.tar.gz");
        assert_eq!(lsp.sha256, SHA);
        assert_eq!(lsp.args, ["--stdio", "--x"]);
        assert_eq!(lsp.env.get("A").map(String::as_str), Some("b"));
        // A file with only kotlin-lsp leaves the built-in intellij.
        assert_eq!(
            entry(&loaded.manifest, KotlinServerKind::Intellij),
            entry(&Manifest::embedded(), KotlinServerKind::Intellij)
        );
    }

    #[test]
    fn test_a_user_table_replaces_the_builtin_table_whole() {
        let (loaded, _temp) = load(Some(&user_table(
            "kotlin-lsp",
            "https://example.com/server.tar.gz",
            "",
        )));
        assert_eq!(loaded.error, None);
        // The file pins linux-x64 only, with no expiry: nothing of the built-in table is left.
        assert!(
            loaded
                .manifest
                .entry(KotlinServerKind::KotlinLsp, "macos-arm64")
                .is_err()
        );
        let lsp = entry(&loaded.manifest, KotlinServerKind::KotlinLsp);
        assert_eq!((lsp.built, lsp.expires), (None, None));
        assert_eq!(lsp.strip_components, 0);
    }

    #[test]
    fn test_omitted_args_and_env_come_from_the_builtin_table() {
        let (loaded, _temp) = load(Some(&user_table(
            "kotlin-lsp",
            "https://example.com/server.tar.gz",
            "",
        )));
        let builtin = entry(&Manifest::embedded(), KotlinServerKind::KotlinLsp);
        let lsp = entry(&loaded.manifest, KotlinServerKind::KotlinLsp);
        assert!(lsp.args.contains(&"--stdio".to_string()));
        assert_eq!(lsp.args, builtin.args);
        assert_eq!(lsp.env, builtin.env);
        // An explicit empty list is not an omission.
        let (loaded, _temp) = load(Some(&user_table(
            "kotlin-lsp",
            "https://example.com/server.tar.gz",
            "args = []",
        )));
        assert!(
            entry(&loaded.manifest, KotlinServerKind::KotlinLsp)
                .args
                .is_empty()
        );
    }

    #[test]
    fn test_a_bad_user_file_falls_back_and_names_the_file_and_error() {
        let good = user_table("kotlin-lsp", "https://example.com/s.tar.gz", "");
        let cases = [
            (
                "missing sha256",
                good.replace(&format!("sha256 = \"{SHA}\"\n"), ""),
            ),
            ("bad sha256", good.replace(SHA, "abc")),
            (
                "unknown field",
                good.replace("version", "colour = 1\nversion"),
            ),
            (
                "unknown server",
                good.replace("[kotlin-lsp", "[other")
                    .replace("kotlin-lsp.", "other."),
            ),
            ("not toml", "[[[".to_string()),
        ];
        for (label, text) in cases {
            let (loaded, temp) = load(Some(&text));
            let error = loaded.error.unwrap_or_else(|| panic!("{label}: no error"));
            assert!(error.contains(USER_FILE_NAME), "{label}: {error}");
            assert!(
                error.contains(&temp.path().display().to_string()),
                "{label}: {error}"
            );
            assert_eq!(
                entry(&loaded.manifest, KotlinServerKind::KotlinLsp),
                entry(&Manifest::embedded(), KotlinServerKind::KotlinLsp),
                "{label}"
            );
        }
    }

    #[test]
    fn test_unreadable_user_file_falls_back_with_an_error() {
        let temp = tempfile::tempdir().unwrap();
        // A directory where the file should be cannot be read as text.
        fs::create_dir(temp.path().join(USER_FILE_NAME)).unwrap();
        let loaded = Manifest::load_user_file(temp.path());
        assert!(loaded.error.unwrap().contains(USER_FILE_NAME));
    }

    #[test]
    fn test_user_file_urls_are_restricted() {
        let refused = [
            "https://bongos.rocks/s.tar.gz",
            "https://www.bongos.rocks/s.tar.gz",
            "https://BONGOS.ROCKS/s.tar.gz",
            "https://cdn.Bongos.Rocks/s.tar.gz",
            "http://example.com/s.tar.gz",
            "ftp://example.com/s.tar.gz",
            "https://user:pass@example.com/s.tar.gz",
            "https://user@example.com/s.tar.gz",
            "not a url",
        ];
        for url in refused {
            let (loaded, _temp) = load(Some(&user_table("kotlin-lsp", url, "")));
            assert!(loaded.error.is_some(), "{url} was accepted");
        }
        for url in [
            "https://example.com/s.tar.gz",
            "https://notbongos.rocks/s.tar.gz",
            "http://127.0.0.1:8123/s.tar.gz",
        ] {
            let (loaded, _temp) = load(Some(&user_table("kotlin-lsp", url, "")));
            assert_eq!(loaded.error, None, "{url}");
        }
    }

    #[test]
    fn test_the_environment_override_wins_over_the_user_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(USER_FILE_NAME),
            user_table("kotlin-lsp", "https://example.com/s.tar.gz", ""),
        )
        .unwrap();
        // Without the variable the user file applies; `load_with` is the override path.
        let loaded = Manifest::load_for_app(true, temp.path());
        let version = |manifest: &Manifest| entry(manifest, KotlinServerKind::KotlinLsp).version;
        if std::env::var_os(OVERRIDE_VAR).is_none() {
            assert_eq!(version(&loaded.manifest), "9.9.9");
        }
        // A release build never reads the variable, but still reads the user file.
        assert_eq!(
            version(&Manifest::load_for_app(false, temp.path()).manifest),
            "9.9.9"
        );
    }
}
