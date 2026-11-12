//! The remote server manifest (`servers.json`) and its detached signature (`servers.json.sig`).
//!
//! A remote manifest lists newer kotlin-lsp builds than the pin embedded in `servers.toml`. It is
//! used only after one of its Ed25519 signatures verifies the exact manifest bytes with a key
//! compiled into the binary (`RELEASE_KEYS`). A debug build also trusts one key from
//! `BONGOS_KOTLIN_MANIFEST_KEY` (`<id>:<base64>`); a release build never reads the variable.
//!
//! Parsing is lenient so that older builds can read newer manifests: unknown keys and fields are
//! ignored, and an entry the build does not understand or that fails a check is skipped. A
//! manifest's `args` and `env` are never used: they always come from the embedded kotlin-lsp pin.

use std::{cmp::Ordering, collections::BTreeMap};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::NaiveDate;
use http_client::Url;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::Deserialize;
use settings::KotlinServerKind;

use crate::manifest::{
    ArchiveKind, Manifest, ManifestEntry, is_plain_name, is_relative_path, is_sha256_hex,
};

/// The manifest schema this build understands. A manifest with a higher `schema` is still read.
pub const SCHEMA: u64 = 1;

/// The `kind` every remote server manifest carries, so a signature over another signed document
/// never verifies as a server manifest.
pub const KIND: &str = "bongos-kotlin-servers";

/// A larger manifest is refused before it is parsed or verified.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// A larger signature file is refused before it is parsed.
pub const MAX_SIGNATURE_BYTES: usize = 16 * 1024;

/// Entries after this many are not read.
pub const MAX_ENTRIES: usize = 64;

/// The public keys that sign release manifests: (key id, base64 of the 32-byte Ed25519 key).
pub const RELEASE_KEYS: &[(&str, &str)] = &[];

/// Names one extra trusted key (`<id>:<base64>`), in debug builds only.
pub const KEY_VAR: &str = "BONGOS_KOTLIN_MANIFEST_KEY";

/// The JetBrains download host. Archive URLs may name it or the manifest's own host.
pub const JETBRAINS_DOWNLOAD_HOST: &str = "download.jetbrains.com";

/// A public key a manifest signature may verify against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedKey {
    pub id: String,
    pub public_key: [u8; 32],
}

impl TrustedKey {
    /// A key from its id and the base64 of its 32 bytes.
    pub fn parse(id: &str, base64: &str) -> Result<Self> {
        ensure!(!id.is_empty(), "the key id is empty");
        let bytes = BASE64
            .decode(base64.trim())
            .with_context(|| format!("key {id}: not base64"))?;
        let public_key = <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| anyhow!("key {id}: {} bytes, expected 32", bytes.len()))?;
        Ok(Self {
            id: id.to_string(),
            public_key,
        })
    }
}

/// The keys this build trusts: `RELEASE_KEYS`, plus in a debug build (`allow_override`, from
/// `cfg!(debug_assertions)`) the key `BONGOS_KOTLIN_MANIFEST_KEY` names.
pub fn trusted_keys(allow_override: bool) -> Vec<TrustedKey> {
    trusted_keys_with(allow_override, std::env::var(KEY_VAR).ok())
}

fn trusted_keys_with(allow_override: bool, override_key: Option<String>) -> Vec<TrustedKey> {
    let mut keys: Vec<TrustedKey> = RELEASE_KEYS
        .iter()
        .filter_map(|(id, base64)| match TrustedKey::parse(id, base64) {
            Ok(key) => Some(key),
            Err(error) => {
                log::error!("ignoring a built-in manifest key: {error:#}");
                None
            }
        })
        .collect();
    if allow_override && let Some(value) = override_key {
        let parsed = value
            .split_once(':')
            .ok_or_else(|| anyhow!("expected <id>:<base64>"))
            .and_then(|(id, base64)| TrustedKey::parse(id, base64));
        match parsed {
            Ok(key) => {
                log::warn!(
                    "trusting the Kotlin server manifest key {} from {KEY_VAR}",
                    key.id
                );
                keys.push(key);
            }
            Err(error) => log::error!("ignoring {KEY_VAR}: {error:#}"),
        }
    }
    keys
}

#[derive(Deserialize)]
struct SignatureFile {
    signatures: Vec<Signature>,
}

#[derive(Deserialize)]
struct Signature {
    key: String,
    ed25519: String,
}

/// Succeeds when at least one signature in `signature` verifies the exact bytes of `manifest` with
/// the key in `keys` that has the same id. Signatures with unknown ids are skipped.
pub fn verify_signature(
    manifest: &[u8],
    signature: Option<&[u8]>,
    keys: &[TrustedKey],
) -> Result<()> {
    check_manifest_size(manifest)?;
    let signature = signature.ok_or_else(|| anyhow!("the manifest has no signature file"))?;
    ensure!(
        signature.len() <= MAX_SIGNATURE_BYTES,
        "the manifest signature file is {} bytes, more than {MAX_SIGNATURE_BYTES}",
        signature.len()
    );
    let file: SignatureFile =
        serde_json::from_slice(signature).context("parsing the manifest signature file")?;
    for listed in &file.signatures {
        let Ok(bytes) = BASE64.decode(listed.ed25519.trim()) else {
            log::warn!("manifest signature for key {} is not base64", listed.key);
            continue;
        };
        if bytes.len() != 64 {
            log::warn!(
                "manifest signature for key {} is {} bytes, expected 64",
                listed.key,
                bytes.len()
            );
            continue;
        }
        for key in keys.iter().filter(|key| key.id == listed.key) {
            if UnparsedPublicKey::new(&ED25519, &key.public_key)
                .verify(manifest, &bytes)
                .is_ok()
            {
                return Ok(());
            }
        }
    }
    bail!("no signature on the manifest verifies with a trusted key")
}

fn check_manifest_size(manifest: &[u8]) -> Result<()> {
    ensure!(
        manifest.len() <= MAX_MANIFEST_BYTES,
        "the manifest is {} bytes, more than {MAX_MANIFEST_BYTES}",
        manifest.len()
    );
    Ok(())
}

/// One kotlin-lsp build from a remote manifest, with the platforms it passed the checks on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBuild {
    pub version: String,
    /// Platform key (`current_platform()`) to the entry the installer takes.
    pub platforms: BTreeMap<String, ManifestEntry>,
}

impl RemoteBuild {
    pub fn entry(&self, platform: &str) -> Option<&ManifestEntry> {
        self.platforms.get(platform)
    }
}

/// A verified remote manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteManifest {
    pub schema: u64,
    /// Only grows between publications; a cached manifest is never replaced by a lower or equal one.
    pub serial: u64,
    pub kotlin_lsp: Vec<RemoteBuild>,
}

impl RemoteManifest {
    /// Verifies `manifest` against `signature` with `keys`, then parses it. `manifest_url` is where
    /// the manifest came from; archive URLs may name its host (see `is_allowed_archive_url`).
    pub fn verify_and_parse(
        manifest: &[u8],
        signature: Option<&[u8]>,
        manifest_url: &Url,
        keys: &[TrustedKey],
    ) -> Result<Self> {
        verify_signature(manifest, signature, keys)?;
        Self::parse_unverified(manifest, manifest_url)
    }

    /// `verify_and_parse` with the keys this build trusts (`trusted_keys`).
    pub fn load(
        manifest: &[u8],
        signature: Option<&[u8]>,
        manifest_url: &Url,
        allow_override: bool,
    ) -> Result<Self> {
        Self::verify_and_parse(
            manifest,
            signature,
            manifest_url,
            &trusted_keys(allow_override),
        )
    }

    /// The newest build that has an entry for `platform`.
    pub fn newest(&self, platform: &str) -> Option<&ManifestEntry> {
        self.kotlin_lsp
            .iter()
            .filter_map(|build| build.entry(platform))
            .max_by(|a, b| compare_versions(&a.version, &b.version).unwrap_or(Ordering::Equal))
    }

    fn parse_unverified(manifest: &[u8], manifest_url: &Url) -> Result<Self> {
        check_manifest_size(manifest)?;
        let raw: RawManifest =
            serde_json::from_slice(manifest).context("parsing the remote server manifest")?;
        ensure!(
            raw.kind == KIND,
            "the manifest kind is {:?}, expected {KIND:?}",
            raw.kind
        );
        let (args, env) = Manifest::embedded()
            .args_and_env(KotlinServerKind::KotlinLsp)
            .ok_or_else(|| anyhow!("the embedded manifest has no kotlin-lsp pin"))?;
        let kotlin_lsp = raw
            .kotlin_lsp
            .into_iter()
            .take(MAX_ENTRIES)
            .filter_map(
                |value| match build_from_value(value, manifest_url, &args, &env) {
                    Ok(build) => Some(build),
                    Err(error) => {
                        log::info!("skipping a remote kotlin-lsp entry: {error:#}");
                        None
                    }
                },
            )
            .collect();
        Ok(Self {
            schema: raw.schema,
            serial: raw.serial,
            kotlin_lsp,
        })
    }
}

#[derive(Deserialize)]
struct RawManifest {
    schema: u64,
    kind: String,
    serial: u64,
    #[serde(rename = "kotlin-lsp", default)]
    kotlin_lsp: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawBuild {
    version: String,
    #[serde(default)]
    built: Option<NaiveDate>,
    #[serde(default)]
    expires: Option<NaiveDate>,
    platform: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawPlatform {
    url: String,
    sha256: String,
    archive: String,
    #[serde(default)]
    strip_components: usize,
    launcher: String,
    #[serde(default)]
    runtime: Option<String>,
}

fn build_from_value(
    value: serde_json::Value,
    manifest_url: &Url,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<RemoteBuild> {
    // Checked before the rest of the entry, whose shape a later schema may change.
    let requires = match value.get("requires") {
        None => 1,
        Some(requires) => requires
            .as_u64()
            .ok_or_else(|| anyhow!("requires {requires} is not a whole number"))?,
    };
    ensure!(
        requires <= SCHEMA,
        "it requires schema {requires}, this build reads {SCHEMA}"
    );
    let raw: RawBuild = serde_json::from_value(value)?;
    ensure!(
        parse_version(&raw.version).is_some() && is_plain_name(&raw.version),
        "version {:?} is not dot-separated whole numbers",
        raw.version
    );
    let expires = raw
        .expires
        .ok_or_else(|| anyhow!("{}: no expires date", raw.version))?;
    let mut platforms = BTreeMap::new();
    for (platform, value) in raw.platform {
        let entry = serde_json::from_value::<RawPlatform>(value)
            .map_err(anyhow::Error::from)
            .and_then(|pin| {
                platform_entry(
                    pin,
                    manifest_url,
                    &raw.version,
                    raw.built,
                    expires,
                    args,
                    env,
                )
            });
        match entry {
            Ok(entry) => {
                platforms.insert(platform, entry);
            }
            Err(error) => log::info!("skipping {} on {platform}: {error:#}", raw.version),
        }
    }
    ensure!(
        !platforms.is_empty(),
        "{}: no platform passed the checks",
        raw.version
    );
    Ok(RemoteBuild {
        version: raw.version,
        platforms,
    })
}

fn platform_entry(
    pin: RawPlatform,
    manifest_url: &Url,
    version: &str,
    built: Option<NaiveDate>,
    expires: NaiveDate,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<ManifestEntry> {
    let archive = match pin.archive.as_str() {
        "tar.gz" => ArchiveKind::TarGz,
        "zip" => ArchiveKind::Zip,
        other => bail!("unknown archive kind {other:?}"),
    };
    let sha256 = pin.sha256.to_ascii_lowercase();
    ensure!(
        is_sha256_hex(&sha256),
        "sha256 must be 64 hexadecimal characters, got {:?}",
        pin.sha256
    );
    ensure!(
        is_relative_path(&pin.launcher),
        "launcher {:?} is not a relative path inside the archive",
        pin.launcher
    );
    if let Some(runtime) = &pin.runtime {
        ensure!(
            is_relative_path(runtime),
            "runtime {runtime:?} is not a relative path inside the archive"
        );
    }
    ensure!(
        is_allowed_archive_url(&pin.url, manifest_url),
        "archive URL {:?} is not on an allowed host",
        pin.url
    );
    Ok(ManifestEntry {
        version: version.to_string(),
        url: pin.url,
        sha256,
        archive,
        strip_components: pin.strip_components,
        launcher: pin.launcher,
        runtime: pin.runtime,
        args: args.to_vec(),
        env: env.clone(),
        built,
        expires: Some(expires),
    })
}

/// Whether a manifest fetched from `manifest_url` may name `url` as an archive: `https://` on
/// `download.jetbrains.com` or on the manifest's own host and port, or `http://` on `127.0.0.1` or
/// `localhost` (tests). A URL with a user name or password is refused.
pub fn is_allowed_archive_url(url: &str, manifest_url: &Url) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    match url.scheme() {
        "https" => {
            (host == JETBRAINS_DOWNLOAD_HOST && url.port().is_none())
                || (manifest_url.host_str() == Some(host)
                    && manifest_url.port_or_known_default() == url.port_or_known_default())
        }
        "http" => host == "127.0.0.1" || host == "localhost",
        _ => false,
    }
}

/// The integers of a version made only of dot-separated whole numbers, or `None`.
pub fn parse_version(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(|part| {
            if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
                part.parse().ok()
            } else {
                None
            }
        })
        .collect()
}

/// Compares two versions as integer sequences ("263.10.0" > "263.9.1"); `None` when either is not
/// dot-separated whole numbers.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    Some(parse_version(a)?.cmp(&parse_version(b)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use serde_json::json;

    const DIGEST: &str = "ae0c4b01e17ab73a4a5227a78ea99e6dcbe3e499600315b22759a4e4cddd11df";

    fn key_pair(seed: u8) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap()
    }

    fn trusted(id: &str, pair: &Ed25519KeyPair) -> TrustedKey {
        TrustedKey::parse(id, &BASE64.encode(pair.public_key().as_ref())).unwrap()
    }

    fn sign(manifest: &[u8], signers: &[(&str, &Ed25519KeyPair)]) -> Vec<u8> {
        let signatures: Vec<_> = signers
            .iter()
            .map(|(id, pair)| json!({"key": id, "ed25519": BASE64.encode(pair.sign(manifest))}))
            .collect();
        serde_json::to_vec(&json!({ "signatures": signatures })).unwrap()
    }

    fn manifest_url() -> Url {
        Url::parse("https://manifests.example.com/kotlin/servers.json").unwrap()
    }

    fn platform(url: &str) -> serde_json::Value {
        json!({
            "url": url, "sha256": DIGEST, "archive": "tar.gz", "strip_components": 1,
            "launcher": "bin/intellij-server", "runtime": "jbr/bin/java"
        })
    }

    fn build(version: &str, url: &str) -> serde_json::Value {
        json!({
            "version": version, "built": "2026-11-10", "expires": "2026-12-09", "requires": 1,
            "platform": { "linux-x64": platform(url) }
        })
    }

    fn jetbrains(version: &str) -> String {
        format!(
            "https://download.jetbrains.com/language-server/kotlin-server/{version}/kotlin-server-{version}.tar.gz"
        )
    }

    fn manifest(entries: Vec<serde_json::Value>) -> Vec<u8> {
        serde_json::to_vec_pretty(&json!({
            "schema": 1, "kind": KIND, "serial": 2026111101, "published": "2026-11-11",
            "kotlin-lsp": entries
        }))
        .unwrap()
    }

    fn parse(bytes: &[u8]) -> RemoteManifest {
        RemoteManifest::parse_unverified(bytes, &manifest_url()).unwrap()
    }

    fn versions(manifest: &RemoteManifest) -> Vec<&str> {
        manifest
            .kotlin_lsp
            .iter()
            .map(|build| build.version.as_str())
            .collect()
    }

    #[test]
    fn test_valid_signature_verifies_and_parses() {
        let pair = key_pair(7);
        let bytes = manifest(vec![build("263.8000.0", &jetbrains("263.8000.0"))]);
        let sig = sign(&bytes, &[("release-1", &pair)]);
        let parsed = RemoteManifest::verify_and_parse(
            &bytes,
            Some(&sig),
            &manifest_url(),
            &[trusted("release-1", &pair)],
        )
        .unwrap();
        assert_eq!(parsed.serial, 2026111101);
        assert_eq!(parsed.schema, 1);
        let entry = parsed.newest("linux-x64").unwrap();
        assert_eq!(entry.version, "263.8000.0");
        assert_eq!(entry.sha256, DIGEST);
        assert_eq!(entry.archive, ArchiveKind::TarGz);
        assert_eq!(entry.expires, NaiveDate::from_ymd_opt(2026, 12, 9));
        assert_eq!(entry.built, NaiveDate::from_ymd_opt(2026, 11, 10));
        assert_eq!(entry.runtime.as_deref(), Some("jbr/bin/java"));
    }

    #[test]
    fn test_verification_failures() {
        let pair = key_pair(7);
        let keys = [trusted("release-1", &pair)];
        let bytes = manifest(vec![build("263.8000.0", &jetbrains("263.8000.0"))]);
        let sig = sign(&bytes, &[("release-1", &pair)]);
        let url = manifest_url();

        // One changed byte.
        let mut changed = bytes.clone();
        let index = changed.iter().position(|byte| *byte == b'8').unwrap();
        changed[index] = b'9';
        assert!(verify_signature(&changed, Some(&sig), &keys).is_err());

        // The wrong key under the right id.
        let wrong = [trusted("release-1", &key_pair(8))];
        assert!(verify_signature(&bytes, Some(&sig), &wrong).is_err());

        // An unknown id only.
        let other = [trusted("release-2", &pair)];
        assert!(verify_signature(&bytes, Some(&sig), &other).is_err());

        // No keys at all (a release build with empty RELEASE_KEYS).
        assert!(verify_signature(&bytes, Some(&sig), &[]).is_err());

        // An empty `.sig`, an empty signature list and a missing `.sig`.
        assert!(verify_signature(&bytes, Some(b""), &keys).is_err());
        assert!(verify_signature(&bytes, Some(br#"{"signatures":[]}"#), &keys).is_err());
        assert!(verify_signature(&bytes, None, &keys).is_err());
        assert!(RemoteManifest::verify_and_parse(&bytes, None, &url, &keys).is_err());

        // A correctly signed manifest of the wrong kind.
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["kind"] = json!("bongos-something-else");
        let other_kind = serde_json::to_vec(&value).unwrap();
        let other_sig = sign(&other_kind, &[("release-1", &pair)]);
        verify_signature(&other_kind, Some(&other_sig), &keys).unwrap();
        let error = RemoteManifest::verify_and_parse(&other_kind, Some(&other_sig), &url, &keys)
            .unwrap_err();
        assert!(format!("{error:#}").contains("kind"), "{error:#}");
    }

    #[test]
    fn test_two_signatures_verify_with_either_key() {
        let (old, new) = (key_pair(1), key_pair(2));
        let bytes = manifest(vec![build("263.8000.0", &jetbrains("263.8000.0"))]);
        let sig = sign(&bytes, &[("old", &old), ("new", &new)]);
        verify_signature(&bytes, Some(&sig), &[trusted("old", &old)]).unwrap();
        verify_signature(&bytes, Some(&sig), &[trusted("new", &new)]).unwrap();
        verify_signature(
            &bytes,
            Some(&sig),
            &[trusted("unrelated", &key_pair(3)), trusted("new", &new)],
        )
        .unwrap();
    }

    #[test]
    fn test_oversize_bodies_are_refused() {
        let pair = key_pair(7);
        let keys = [trusted("release-1", &pair)];
        let mut big = manifest(vec![build("263.8000.0", &jetbrains("263.8000.0"))]);
        big.resize(MAX_MANIFEST_BYTES + 1, b' ');
        let sig = sign(&big, &[("release-1", &pair)]);
        let error = verify_signature(&big, Some(&sig), &keys).unwrap_err();
        assert!(error.to_string().contains("more than"), "{error}");
        assert!(RemoteManifest::parse_unverified(&big, &manifest_url()).is_err());

        let bytes = manifest(vec![build("263.8000.0", &jetbrains("263.8000.0"))]);
        let mut big_sig = sign(&bytes, &[("release-1", &pair)]);
        let close = big_sig.pop().unwrap();
        big_sig.resize(MAX_SIGNATURE_BYTES, b' ');
        big_sig.push(close);
        let error = verify_signature(&bytes, Some(&big_sig), &keys).unwrap_err();
        assert!(error.to_string().contains("more than"), "{error}");
    }

    #[test]
    fn test_at_most_64_entries_are_read() {
        let entries = (0..100)
            .map(|minor| build(&format!("263.{minor}.0"), &jetbrains("263.0.0")))
            .collect();
        let parsed = parse(&manifest(entries));
        assert_eq!(parsed.kotlin_lsp.len(), MAX_ENTRIES);
        assert_eq!(parsed.newest("linux-x64").unwrap().version, "263.63.0");
    }

    #[test]
    fn test_newer_schema_keeps_understood_entries() {
        let mut future = build("264.1.0", &jetbrains("264.1.0"));
        future["requires"] = json!(2);
        future["platform"] = json!("a new shape");
        let mut known = build("263.8000.0", &jetbrains("263.8000.0"));
        known["channel"] = json!("stable");
        known["platform"]["linux-x64"]["signature_v2"] = json!({"x": 1});
        let bytes = serde_json::to_vec(&json!({
            "schema": 2, "kind": KIND, "serial": 7, "mirrors": ["a", "b"],
            "intellij": [{ "version": "272.1.0", "something": true }],
            "kotlin-lsp": [future, known]
        }))
        .unwrap();
        let parsed = parse(&bytes);
        assert_eq!(parsed.schema, 2);
        assert_eq!(parsed.serial, 7);
        assert_eq!(versions(&parsed), ["263.8000.0"]);
    }

    #[test]
    fn test_bad_entries_are_skipped() {
        let good = build("263.8000.0", &jetbrains("263.8000.0"));
        let mut requires_2 = build("264.1.0", &jetbrains("264.1.0"));
        requires_2["requires"] = json!(2);
        let mut tar_zst = build("264.2.0", &jetbrains("264.2.0"));
        tar_zst["platform"]["linux-x64"]["archive"] = json!("tar.zst");
        let non_numeric = build("264.3.0-eap", &jetbrains("264.3.0"));
        let empty_part = build("264..4", &jetbrains("264.4.0"));
        let http_remote = build(
            "264.5.0",
            "http://download.jetbrains.com/kotlin-server.tar.gz",
        );
        let third_host = build(
            "264.6.0",
            "https://downloads.example.org/kotlin-server.tar.gz",
        );
        let user_info = build(
            "264.7.0",
            "https://download.jetbrains.com@downloads.example.org/kotlin-server.tar.gz",
        );
        let mut no_expires = build("264.8.0", &jetbrains("264.8.0"));
        no_expires.as_object_mut().unwrap().remove("expires");
        let mut bad_digest = build("264.9.0", &jetbrains("264.9.0"));
        bad_digest["platform"]["linux-x64"]["sha256"] = json!(&DIGEST[1..]);
        let mut escaping_launcher = build("264.10.0", &jetbrains("264.10.0"));
        escaping_launcher["platform"]["linux-x64"]["launcher"] = json!("../../bin/sh");
        let mut uppercase_digest = build("263.9000.0", &jetbrains("263.9000.0"));
        uppercase_digest["platform"]["linux-x64"]["sha256"] = json!(DIGEST.to_ascii_uppercase());

        let parsed = parse(&manifest(vec![
            requires_2,
            tar_zst,
            non_numeric,
            empty_part,
            http_remote,
            third_host,
            user_info,
            no_expires,
            bad_digest,
            escaping_launcher,
            good,
            uppercase_digest,
        ]));
        assert_eq!(versions(&parsed), ["263.8000.0", "263.9000.0"]);
        assert_eq!(parsed.newest("linux-x64").unwrap().sha256, DIGEST);
    }

    #[test]
    fn test_a_bad_platform_drops_only_that_platform() {
        let mut entry = build("263.8000.0", &jetbrains("263.8000.0"));
        let mut mac = platform(&jetbrains("263.8000.0"));
        mac["archive"] = json!("dmg");
        entry["platform"]["macos-arm64"] = mac;
        let parsed = parse(&manifest(vec![entry]));
        assert!(parsed.newest("linux-x64").is_some());
        assert!(parsed.newest("macos-arm64").is_none());
    }

    #[test]
    fn test_manifest_args_and_env_are_ignored() {
        let mut entry = build("263.8000.0", &jetbrains("263.8000.0"));
        entry["args"] = json!(["--stdio", "--data-sharing=all"]);
        entry["env"] = json!({"INTELLIJ_UPDATES_ENABLED": "true", "LD_PRELOAD": "/tmp/x.so"});
        entry["platform"]["linux-x64"]["args"] = json!(["--evil"]);
        entry["platform"]["linux-x64"]["env"] = json!({"LD_PRELOAD": "/tmp/x.so"});
        let parsed = parse(&manifest(vec![entry]));
        let remote = parsed.newest("linux-x64").unwrap();
        let pin = Manifest::embedded()
            .entry(KotlinServerKind::KotlinLsp, "linux-x64")
            .unwrap();
        assert_eq!(remote.args, pin.args);
        assert_eq!(remote.env, pin.env);
        assert!(remote.args.contains(&"--data-sharing=none".to_string()));
        assert_eq!(
            remote
                .env
                .get("INTELLIJ_UPDATES_ENABLED")
                .map(String::as_str),
            Some("false")
        );
        assert!(!remote.env.contains_key("LD_PRELOAD"));
    }

    #[test]
    fn test_env_key_is_read_only_when_allowed() {
        let pair = key_pair(5);
        let value = format!("dev:{}", BASE64.encode(pair.public_key().as_ref()));
        assert!(RELEASE_KEYS.is_empty());
        // Not allowed (a release build): the variable is ignored.
        assert!(trusted_keys_with(false, Some(value.clone())).is_empty());
        // Allowed (a debug build): the key is trusted.
        assert_eq!(
            trusted_keys_with(true, Some(value)),
            [trusted("dev", &pair)]
        );
        // Allowed but malformed: ignored.
        assert!(trusted_keys_with(true, Some("no-colon".into())).is_empty());
        assert!(trusted_keys_with(true, Some("dev:AAAA".into())).is_empty());
        assert!(trusted_keys_with(true, None).is_empty());
    }

    #[test]
    fn test_release_keys_are_well_formed() {
        for (id, base64) in RELEASE_KEYS {
            TrustedKey::parse(id, base64).unwrap();
        }
    }

    #[test]
    fn test_version_ordering() {
        assert_eq!(
            compare_versions("263.10.0", "263.9.1"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_versions("263.7084.0", "263.8000.0"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_versions("263.1.0", "263.1.0"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_versions("263.1", "263.1.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("263.1.0-eap", "263.1.0"), None);
        assert_eq!(parse_version("263.+1.0"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1.99999999999999999999999"), None);
        assert_eq!(parse_version("263.10.0"), Some(vec![263, 10, 0]));
    }

    #[test]
    fn test_archive_url_rules() {
        let url = manifest_url();
        let allowed = |archive: &str| is_allowed_archive_url(archive, &url);
        assert!(allowed(&jetbrains("263.8000.0")));
        assert!(allowed(
            "https://manifests.example.com/kotlin/kotlin-server.tar.gz"
        ));
        assert!(allowed("http://127.0.0.1:8123/kotlin-server.tar.gz"));
        assert!(allowed("http://localhost:8123/kotlin-server.tar.gz"));
        assert!(!allowed(
            "http://manifests.example.com/kotlin-server.tar.gz"
        ));
        assert!(!allowed(
            "http://download.jetbrains.com/kotlin-server.tar.gz"
        ));
        assert!(!allowed("https://example.org/kotlin-server.tar.gz"));
        assert!(!allowed(
            "https://download.jetbrains.com.example.org/x.tar.gz"
        ));
        assert!(!allowed("https://download.jetbrains.com:8443/x.tar.gz"));
        assert!(!allowed("https://manifests.example.com:8443/x.tar.gz"));
        assert!(!allowed("ftp://download.jetbrains.com/x.tar.gz"));
        assert!(!allowed("file:///tmp/x.tar.gz"));
        assert!(!allowed("not a url"));

        // A manifest served from loopback over http (tests and the scenario).
        let loopback = Url::parse("http://127.0.0.1:8123/servers.json").unwrap();
        let bytes = manifest(vec![build(
            "263.8000.0",
            "http://127.0.0.1:8123/kotlin-server.tar.gz",
        )]);
        let parsed = RemoteManifest::parse_unverified(&bytes, &loopback).unwrap();
        assert_eq!(versions(&parsed), ["263.8000.0"]);
    }
}
