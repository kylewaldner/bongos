//! Preflight verdicts for installing an extension. Everything here is a pure function of probe
//! results and facts handed in by the caller: nothing spawns a process, reads a file, touches
//! the network or reads the clock.

use std::fmt;
use std::ops::RangeInclusive;

use crate::source::SourceUrl;

/// Message when Rust is needed and cargo or rustc cannot be found.
pub const NO_RUST_MESSAGE: &str = "Building this extension needs Rust: install rustup from https://rustup.rs, then run `rustup target add wasm32-wasip2`. If Rust is installed, start bongos from a shell or add `~/.cargo/bin` to the desktop session's PATH.";
/// Message when the wasm target is missing and rustup is available.
pub const TARGET_MISSING_WITH_RUSTUP_MESSAGE: &str =
    "Run `rustup target add wasm32-wasip2`, then install again.";
/// Message when the wasm target is missing and rustup is not available.
pub const TARGET_MISSING_WITHOUT_RUSTUP_MESSAGE: &str =
    "Add the wasm32-wasip2 target to your Rust toolchain.";
/// Message when git cannot be found.
pub const GIT_MISSING_MESSAGE: &str =
    "git was not found: install git and make sure it is on PATH, then install again.";
/// Message when the extension has no `extension.toml` and no old manifest either.
pub const MANIFEST_MISSING_MESSAGE: &str =
    "This extension has no extension.toml; add one to the extension's root, then install again.";
/// Message when only the retired `extension.json` manifest is present.
pub const OLD_MANIFEST_MESSAGE: &str = "This extension uses the retired extension.json format and needs an extension.toml; ask the author to convert it.";

const TIMED_OUT_TEMPLATE: &str =
    "{tool} did not respond. Check that it works from a shell, then install again.";
const BLOCKED_HOST_TEMPLATE: &str =
    "The repository host {host} is not allowed; use a repository on a different host.";
const BAD_GRAMMAR_URL_TEMPLATE: &str =
    "The grammar repository URL {url} is not valid; fix it in extension.toml, then install again.";
const SCHEMA_TEMPLATE: &str = "This extension uses manifest schema version {found}; this bongos supports versions {min} to {max}.";
const SUPPRESSED_TEMPLATE: &str =
    "The extension {id} is not allowed to be installed; remove it from your install list.";
const INSTALLED_ELSEWHERE_TEMPLATE: &str =
    "{id} is already installed from another source; remove {id} first.";
const API_TEMPLATE: &str =
    "This extension needs extension API {needed}; this bongos supports up to {max}.";

/// A tool whose presence is probed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Git,
    Cargo,
    Rustc,
    Rustup,
    WasmTarget,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Git => "git",
            Tool::Cargo => "cargo",
            Tool::Rustc => "rustc",
            Tool::Rustup => "rustup",
            Tool::WasmTarget => "the wasm32-wasip2 target check",
        }
    }
}

/// Result of probing one tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Found,
    Missing,
    TimedOut,
}

impl Probe {
    fn is_found(self) -> bool {
        self == Probe::Found
    }
}

/// Probe results for every tool preflight cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolProbe {
    pub git: Probe,
    pub cargo: Probe,
    /// Whether `rustc` is found and reports a sysroot.
    pub rustc_sysroot: Probe,
    /// Whether the wasm32-wasip2 target is installed.
    pub wasm_target: Probe,
    pub rustup: Probe,
}

impl ToolProbe {
    /// What an empty PATH looks like: nothing found.
    pub const ALL_MISSING: ToolProbe = ToolProbe {
        git: Probe::Missing,
        cargo: Probe::Missing,
        rustc_sysroot: Probe::Missing,
        wasm_target: Probe::Missing,
        rustup: Probe::Missing,
    };
}

/// An extension API version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ApiVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `major.minor.patch`; missing components are 0.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.split(['-', '+']).next()?;
        let mut parts = [0u32; 3];
        for (count, piece) in text.split('.').enumerate() {
            if count == 3 {
                return None;
            }
            parts[count] = piece.trim().parse().ok()?;
        }
        Some(Self::new(parts[0], parts[1], parts[2]))
    }

    /// The lowest version a cargo requirement string allows, such as `^0.7`, `~0.7.1` or
    /// `>=0.7, <0.9`.
    ///
    /// Takes the highest lower bound among the comma-separated parts (`>=`, `>`, `=`, `^`, `~`
    /// and bare versions). `<` and `<=` parts give no lower bound; with no lower bound at all
    /// the answer is `0.0.0`. `>X.Y.Z` bumps the last component given, so `>0.7.0` is `0.7.1`.
    /// Returns `None` when a part cannot be read.
    pub fn lowest_allowed(requirement: &str) -> Option<Self> {
        let mut lowest = Self::new(0, 0, 0);
        for part in requirement.split(',') {
            let part = part.trim();
            if part.starts_with('<') {
                continue;
            }
            let exclusive = part.starts_with('>') && !part.starts_with(">=");
            let text = part.trim_start_matches(|c: char| c.is_whitespace() || "^~=<>".contains(c));
            let mut bound = Self::parse(text)?;
            if exclusive {
                let given = text.split(['-', '+']).next()?.split('.').count();
                match given {
                    1 => bound.major += 1,
                    2 => bound.minor += 1,
                    _ => bound.patch += 1,
                }
            }
            lowest = lowest.max(bound);
        }
        Some(lowest)
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Facts about a cloned extension, gathered by the caller.
#[derive(Clone, Debug, Default)]
pub struct ManifestFacts {
    pub has_extension_toml: bool,
    pub has_extension_json: bool,
    pub schema_version: u32,
    pub id: String,
    /// Text of the extension's `Cargo.toml`; `None` when it has none. The toolchain is checked
    /// only when this is `Some`: grammar-only extensions need just git. The API requirement is
    /// read from the `zed_extension_api` dependency here, so a renamed or target-specific
    /// dependency is not seen; the post-build API check is the backstop for those.
    pub cargo_toml: Option<String>,
    /// Text of the workspace manifest, for `{ workspace = true }` dependencies.
    pub workspace_toml: Option<String>,
    pub grammar_repositories: Vec<String>,
}

/// Why an install is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    GitMissing,
    /// A probed tool did not answer in time; treated as missing.
    ToolTimedOut(Tool),
    BlockedHost(String),
    /// A grammar repository URL that cannot be parsed.
    BadGrammarUrl(String),
    ManifestMissing,
    OldManifest,
    SchemaVersion {
        found: u32,
        supported: RangeInclusive<u32>,
    },
    Suppressed(String),
    InstalledElsewhere(String),
    NoRust,
    TargetMissingWithRustup,
    TargetMissingWithoutRustup,
    ApiTooNew {
        needed: ApiVersion,
        max: ApiVersion,
    },
}

impl Refusal {
    /// One sentence saying what is wrong, followed by what to do.
    pub fn message(&self) -> String {
        match self {
            Refusal::GitMissing => GIT_MISSING_MESSAGE.to_string(),
            Refusal::ToolTimedOut(tool) => TIMED_OUT_TEMPLATE.replace("{tool}", tool.name()),
            Refusal::BlockedHost(host) => BLOCKED_HOST_TEMPLATE.replace("{host}", host),
            Refusal::BadGrammarUrl(url) => BAD_GRAMMAR_URL_TEMPLATE.replace("{url}", url),
            Refusal::ManifestMissing => MANIFEST_MISSING_MESSAGE.to_string(),
            Refusal::OldManifest => OLD_MANIFEST_MESSAGE.to_string(),
            Refusal::SchemaVersion { found, supported } => SCHEMA_TEMPLATE
                .replace("{found}", &found.to_string())
                .replace("{min}", &supported.start().to_string())
                .replace("{max}", &supported.end().to_string()),
            Refusal::Suppressed(id) => SUPPRESSED_TEMPLATE.replace("{id}", id),
            Refusal::InstalledElsewhere(id) => INSTALLED_ELSEWHERE_TEMPLATE.replace("{id}", id),
            Refusal::NoRust => NO_RUST_MESSAGE.to_string(),
            Refusal::TargetMissingWithRustup => TARGET_MISSING_WITH_RUSTUP_MESSAGE.to_string(),
            Refusal::TargetMissingWithoutRustup => {
                TARGET_MISSING_WITHOUT_RUSTUP_MESSAGE.to_string()
            }
            Refusal::ApiTooNew { needed, max } => API_TEMPLATE
                .replace("{needed}", &needed.to_string())
                .replace("{max}", &max.to_string()),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for Refusal {}

fn check_git(git: Probe) -> Result<(), Refusal> {
    match git {
        Probe::Found => Ok(()),
        Probe::Missing => Err(Refusal::GitMissing),
        Probe::TimedOut => Err(Refusal::ToolTimedOut(Tool::Git)),
    }
}

/// Checks run before cloning: git first, then the repository host.
pub fn check_before_clone(
    probe: &ToolProbe,
    url: &SourceUrl,
    is_blocked_host: impl Fn(&str) -> bool,
) -> Result<(), Refusal> {
    check_git(probe.git)?;
    check_host(url, &is_blocked_host)
}

fn check_host(url: &SourceUrl, is_blocked_host: &impl Fn(&str) -> bool) -> Result<(), Refusal> {
    if let Some(host) = url.host() {
        let host = host.trim_end_matches('.');
        if is_blocked_host(host) {
            return Err(Refusal::BlockedHost(host.to_string()));
        }
    }
    Ok(())
}

/// Parses a repository URL, refusing one that does not parse. The host is lowercase.
pub fn repository_host(url: &str) -> Result<SourceUrl, Refusal> {
    SourceUrl::parse(url).map_err(|_| Refusal::BadGrammarUrl(url.to_string()))
}

/// Checks run after cloning, in order: git, manifest, schema version, suppression, other
/// installs, grammar hosts, toolchain, API requirement.
pub fn check_after_clone(
    probe: &ToolProbe,
    facts: &ManifestFacts,
    is_blocked_host: impl Fn(&str) -> bool,
    is_suppressed: impl Fn(&str) -> bool,
    is_installed_elsewhere: impl Fn(&str) -> bool,
    schema_version_range: RangeInclusive<u32>,
    api_max: ApiVersion,
) -> Result<(), Refusal> {
    check_git(probe.git)?;
    if !facts.has_extension_toml {
        return Err(if facts.has_extension_json {
            Refusal::OldManifest
        } else {
            Refusal::ManifestMissing
        });
    }
    if !schema_version_range.contains(&facts.schema_version) {
        return Err(Refusal::SchemaVersion {
            found: facts.schema_version,
            supported: schema_version_range,
        });
    }
    if is_suppressed(&facts.id) {
        return Err(Refusal::Suppressed(facts.id.clone()));
    }
    if is_installed_elsewhere(&facts.id) {
        return Err(Refusal::InstalledElsewhere(facts.id.clone()));
    }
    for url in &facts.grammar_repositories {
        check_host(&repository_host(url)?, &is_blocked_host)?;
    }
    // Only a Cargo.toml needs the Rust toolchain; grammars need only git, checked first.
    if let Some(cargo_toml) = &facts.cargo_toml {
        check_toolchain(probe)?;
        if let Some(needed) = api_requirement(cargo_toml, facts.workspace_toml.as_deref()) {
            check_built_api(needed, api_max)?;
        }
    }
    Ok(())
}

fn check_toolchain(probe: &ToolProbe) -> Result<(), Refusal> {
    for (result, tool) in [
        (probe.cargo, Tool::Cargo),
        (probe.rustc_sysroot, Tool::Rustc),
    ] {
        match result {
            Probe::Found => {}
            Probe::TimedOut => return Err(Refusal::ToolTimedOut(tool)),
            Probe::Missing => return Err(Refusal::NoRust),
        }
    }
    match probe.wasm_target {
        Probe::Found => Ok(()),
        Probe::TimedOut => Err(Refusal::ToolTimedOut(Tool::WasmTarget)),
        Probe::Missing if probe.rustup == Probe::TimedOut => {
            Err(Refusal::ToolTimedOut(Tool::Rustup))
        }
        Probe::Missing if probe.rustup.is_found() => Err(Refusal::TargetMissingWithRustup),
        Probe::Missing => Err(Refusal::TargetMissingWithoutRustup),
    }
}

/// The lowest `zed_extension_api` version the extension's Cargo.toml allows, or `None` when
/// it has no such dependency (or the requirement cannot be read).
pub fn api_requirement(cargo_toml: &str, workspace_toml: Option<&str>) -> Option<ApiVersion> {
    let manifest: toml::Table = cargo_toml.parse().ok()?;
    let dependency = manifest
        .get("dependencies")?
        .as_table()?
        .get("zed_extension_api")?;
    let requirement = match dependency {
        toml::Value::String(version) => version.clone(),
        toml::Value::Table(table) => {
            if let Some(version) = table.get("version").and_then(|v| v.as_str()) {
                version.to_string()
            } else if table.get("workspace").and_then(|v| v.as_bool()) == Some(true) {
                let workspace: toml::Table = workspace_toml?.parse().ok()?;
                match workspace
                    .get("workspace")?
                    .as_table()?
                    .get("dependencies")?
                    .as_table()?
                    .get("zed_extension_api")?
                {
                    toml::Value::String(version) => version.clone(),
                    toml::Value::Table(table) => table.get("version")?.as_str()?.to_string(),
                    _ => return None,
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };
    ApiVersion::lowest_allowed(&requirement)
}

/// Compares an API version (required or built) with the newest this bongos supports.
pub fn check_built_api(built: ApiVersion, api_max: ApiVersion) -> Result<(), Refusal> {
    if built > api_max {
        Err(Refusal::ApiTooNew {
            needed: built,
            max: api_max,
        })
    } else {
        Ok(())
    }
}
