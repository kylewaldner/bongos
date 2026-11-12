//! The cached remote manifest and the update state, under `<install_root>/manifest/`.
//!
//! - `cache.json` holds the exact `servers.json` bytes and the exact `.sig` bytes in one file, so a
//!   single rename replaces both and no crash can pair one manifest with another's signature. It is
//!   written only after the pair verifies, and only when its serial is higher than the cached one.
//!   Every load verifies it again, so a corrupt, foreign or unknown-key cache counts as absent.
//! - `state.json` holds `last_check`, `fetch_lease`, the last outcome, and the kotlin-lsp builds
//!   (active, previous, trial, rejected).
//!
//! Readers read both files without a lock: every write stages the new contents, syncs them and
//! renames them into place. Writers hold the state lock (`locks.rs`) only for a short local read,
//! modify and write, never across a network request: `with_state` takes a synchronous closure.
//!
//! A fetch is claimed across processes in three phases: `claim_fetch` (phase 1) takes a lease
//! under the state lock, the caller fetches and verifies without any lock (phase 2), and
//! `finish_fetch` (phase 3) stores a higher serial, records the outcome and clears the lease.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use http_client::Url;
use serde::{Deserialize, Serialize};

use crate::{
    locks::{LockRegistry, MANIFEST_DIR},
    remote_manifest::{MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES, RemoteManifest, TrustedKey},
    resolve::{commit_staged, stage},
};

pub const CACHE_FILE: &str = "cache.json";
pub const STATE_FILE: &str = "state.json";
const CACHE_VERSION: u32 = 1;
const STATE_VERSION: u32 = 1;

/// How long a claimed fetch keeps other processes from fetching: longer than the 30 s fetch cap,
/// so a live fetch is never doubled, and short enough that a fetch whose process died is taken
/// over at the next check.
pub const FETCH_LEASE: Duration = Duration::from_secs(60);

/// A larger `cache.json` is refused before it is read: base64 of the largest pair, plus room.
const MAX_CACHE_BYTES: u64 = ((MAX_MANIFEST_BYTES + MAX_SIGNATURE_BYTES) as u64 / 3 + 1) * 4 + 4096;

/// One downloaded build, named by version and archive digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildId {
    pub version: String,
    pub sha256: String,
}

/// The kotlin-lsp builds bongos switches between (trial, then active; previous for rollback).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KotlinLspBuilds {
    pub active: Option<BuildId>,
    pub previous: Option<BuildId>,
    pub trial: Option<BuildId>,
    pub rejected: Vec<BuildId>,
}

/// `state.json`. A missing file, a corrupt one, or one with another `version` reads as the default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerState {
    /// When the last fetch finished, successful or not.
    pub last_check: Option<DateTime<Utc>>,
    /// Until when a claimed fetch keeps other processes from fetching.
    pub fetch_lease: Option<DateTime<Utc>>,
    /// The last fetch's outcome, one line for the log and the status menu.
    pub last_outcome: Option<String>,
    #[serde(rename = "kotlin-lsp")]
    pub kotlin_lsp: KotlinLspBuilds,
}

#[derive(Serialize)]
struct StateFileOut<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a ServerState,
}

#[derive(Deserialize)]
struct StateFileIn {
    version: u32,
    #[serde(flatten)]
    state: ServerState,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    /// Base64 of the exact `servers.json` bytes.
    servers: String,
    /// Base64 of the exact `servers.json.sig` bytes.
    sig: String,
}

/// A cached manifest that verified on load, with the bytes it verified from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedManifest {
    pub manifest: RemoteManifest,
    pub servers: Vec<u8>,
    pub sig: Vec<u8>,
}

/// The bodies of a fetch (phase 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedManifest {
    pub servers: Vec<u8>,
    pub sig: Vec<u8>,
}

/// The lease a claimed fetch holds. `finish_fetch` clears the state's lease only if it is still
/// this one, so a fetch that outlived its lease never clears the lease of the process that took
/// over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    pub until: DateTime<Utc>,
}

/// The result of phase 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// This caller fetches, then calls `finish_fetch` with the lease.
    Claimed(Lease),
    /// Another check finished recently enough; nothing to do.
    Fresh,
    /// Another fetch holds a live lease ("a check is already running").
    Busy,
}

/// The result of phase 3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The fetched manifest verified and replaced the cache.
    Stored { serial: u64 },
    /// The fetched manifest verified, but the cache on disk already has this serial or a higher
    /// one; the cache is unchanged.
    NotNewer { serial: u64, cached: u64 },
    /// The fetched manifest did not verify or parse; the cache is unchanged.
    Rejected { reason: String },
    /// The fetch itself failed; the cache is unchanged.
    Failed { reason: String },
}

impl FetchOutcome {
    /// The `kotlin.log` line, also kept as the state's `last_outcome`.
    pub fn describe(&self) -> String {
        match self {
            Self::Stored { serial } => format!("manifest: fetched serial {serial}"),
            Self::NotNewer { serial, cached } => {
                format!("manifest: rejected: serial {serial} is not newer than {cached}")
            }
            Self::Rejected { reason } | Self::Failed { reason } => {
                format!("manifest: rejected: {reason}")
            }
        }
    }
}

/// The manifest cache and update state of one install root.
pub struct ManifestCache {
    install_root: PathBuf,
    dir: PathBuf,
    locks: Arc<LockRegistry>,
}

impl ManifestCache {
    pub fn new(install_root: &Path, locks: Arc<LockRegistry>) -> Self {
        Self {
            install_root: install_root.to_path_buf(),
            dir: install_root.join(MANIFEST_DIR),
            locks,
        }
    }

    pub fn cache_path(&self) -> PathBuf {
        self.dir.join(CACHE_FILE)
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    /// The cached manifest, verified again with `keys`. A missing, corrupt, foreign or unknown-key
    /// cache is `None` (and logged). Takes no lock.
    pub fn load(&self, manifest_url: &Url, keys: &[TrustedKey]) -> Option<CachedManifest> {
        match load_cache(&self.cache_path(), manifest_url, keys) {
            Ok(cached) => cached,
            Err(error) => {
                log::info!("manifest: ignoring the cached manifest: {error:#}");
                None
            }
        }
    }

    /// `state.json` as it is now. Takes no lock; for a read, modify and write use `with_state`.
    pub fn read_state(&self) -> ServerState {
        read_state(&self.state_path()).unwrap_or_else(|error| {
            log::info!("manifest: {error:#}");
            ServerState::default()
        })
    }

    /// Runs `f` on `state.json` under the state lock and writes the result if it changed. When
    /// the lock stays busy past its limit, or the file cannot be read or written, this returns an
    /// error and `state.json` is unchanged.
    pub async fn with_state<R>(&self, f: impl FnOnce(&mut ServerState) -> R) -> Result<R> {
        self.with_locked_state(|state| Ok(f(state))).await
    }

    /// Phase 1 of a fetch. Under the state lock: a `last_check` younger than `max_age` (unless
    /// `force`) gives `Fresh`, a live lease gives `Busy`, and otherwise this takes the lease.
    pub async fn claim_fetch(
        &self,
        now: DateTime<Utc>,
        max_age: Duration,
        force: bool,
    ) -> Result<Claim> {
        let max_age = chrono::Duration::from_std(max_age)?;
        let lease_length = chrono::Duration::from_std(FETCH_LEASE)?;
        self.with_state(|state| {
            // A time in the future counts as stale (the clock moved back), but never further
            // than one lease.
            let fresh = state
                .last_check
                .is_some_and(|checked| checked <= now && now - checked < max_age);
            if fresh && !force {
                return Claim::Fresh;
            }
            let leased = state
                .fetch_lease
                .is_some_and(|until| now < until && until <= now + lease_length);
            if leased {
                return Claim::Busy;
            }
            let until = now + lease_length;
            state.fetch_lease = Some(until);
            Claim::Claimed(Lease { until })
        })
        .await
    }

    /// Phase 3 of a fetch. Under the state lock: verifies `fetched` again, stores it if its serial
    /// is higher than the cache on disk (which another process may have advanced since phase 1),
    /// sets `last_check` and the outcome, and clears `lease` if it is still the state's lease.
    pub async fn finish_fetch(
        &self,
        lease: Lease,
        now: DateTime<Utc>,
        fetched: Result<FetchedManifest>,
        manifest_url: &Url,
        keys: &[TrustedKey],
    ) -> Result<FetchOutcome> {
        let cache_path = self.cache_path();
        self.with_locked_state(|state| {
            let outcome = match fetched {
                Err(error) => FetchOutcome::Failed {
                    reason: format!("{error:#}"),
                },
                Ok(fetched) => store_if_newer(&cache_path, &fetched, manifest_url, keys)?,
            };
            state.last_check = Some(now);
            state.last_outcome = Some(outcome.describe());
            if state.fetch_lease == Some(lease.until) {
                state.fetch_lease = None;
            }
            Ok(outcome)
        })
        .await
    }

    async fn with_locked_state<R>(
        &self,
        f: impl FnOnce(&mut ServerState) -> Result<R>,
    ) -> Result<R> {
        let _lock = match self.locks.state_lock(&self.install_root).await {
            Ok(lock) => lock,
            Err(error) => {
                log::error!("manifest: skipping a state update: {error:#}");
                return Err(error);
            }
        };
        let path = self.state_path();
        let before = read_state(&path)?;
        let mut state = before.clone();
        let result = f(&mut state)?;
        if state != before {
            write_state(&path, &state)?;
        }
        Ok(result)
    }
}

fn load_cache(
    path: &Path,
    manifest_url: &Url,
    keys: &[TrustedKey],
) -> Result<Option<CachedManifest>> {
    let Some(bytes) = read_capped(path, MAX_CACHE_BYTES)? else {
        return Ok(None);
    };
    let file: CacheFile = serde_json::from_slice(&bytes).context("parsing the cache file")?;
    if file.version != CACHE_VERSION {
        bail!("the cache file has unsupported version {}", file.version);
    }
    let servers = BASE64
        .decode(&file.servers)
        .context("decoding the cached manifest")?;
    let sig = BASE64
        .decode(&file.sig)
        .context("decoding the cached signature")?;
    let manifest = RemoteManifest::verify_and_parse(&servers, Some(&sig), manifest_url, keys)
        .context("verifying the cached manifest")?;
    Ok(Some(CachedManifest {
        manifest,
        servers,
        sig,
    }))
}

fn encode_cache(fetched: &FetchedManifest) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&CacheFile {
        version: CACHE_VERSION,
        servers: BASE64.encode(&fetched.servers),
        sig: BASE64.encode(&fetched.sig),
    })?)
}

/// Verifies `fetched` and writes it over the cache if its serial is higher. Called under the
/// state lock, so no other writer runs between the serial check and the rename.
fn store_if_newer(
    cache_path: &Path,
    fetched: &FetchedManifest,
    manifest_url: &Url,
    keys: &[TrustedKey],
) -> Result<FetchOutcome> {
    let manifest = match RemoteManifest::verify_and_parse(
        &fetched.servers,
        Some(&fetched.sig),
        manifest_url,
        keys,
    ) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Ok(FetchOutcome::Rejected {
                reason: format!("{error:#}"),
            });
        }
    };
    let cached = match load_cache(cache_path, manifest_url, keys) {
        Ok(cached) => cached,
        Err(error) => {
            log::info!("manifest: replacing an unusable cache: {error:#}");
            None
        }
    };
    if let Some(cached) = cached
        && cached.manifest.serial >= manifest.serial
    {
        return Ok(FetchOutcome::NotNewer {
            serial: manifest.serial,
            cached: cached.manifest.serial,
        });
    }
    write_atomically(cache_path, &encode_cache(fetched)?)?;
    Ok(FetchOutcome::Stored {
        serial: manifest.serial,
    })
}

/// Reads `state.json`. Missing, corrupt or another version reads as the default; only an I/O
/// error is an error, so a write never replaces a file this process could not read.
fn read_state(path: &Path) -> Result<ServerState> {
    let Some(bytes) = read_capped(path, MAX_CACHE_BYTES)? else {
        return Ok(ServerState::default());
    };
    Ok(parse_state(&bytes).unwrap_or_else(|error| {
        log::info!("manifest: treating {} as empty: {error:#}", path.display());
        ServerState::default()
    }))
}

fn parse_state(bytes: &[u8]) -> Result<ServerState> {
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let Version { version } = serde_json::from_slice(bytes).context("parsing the state file")?;
    if version != STATE_VERSION {
        bail!("unsupported state file version {version}");
    }
    let file: StateFileIn = serde_json::from_slice(bytes).context("parsing the state file")?;
    debug_assert_eq!(file.version, STATE_VERSION);
    Ok(file.state)
}

fn write_state(path: &Path, state: &ServerState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&StateFileOut {
        version: STATE_VERSION,
        state,
    })?;
    write_atomically(path, &bytes)
}

/// Stages `contents` next to `path`, syncs it, and renames it into place: a reader sees the old
/// file or the new one, never a mix. The directory is synced too, so the rename survives a power
/// loss.
fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no directory", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let staged = stage(path, contents)?;
    commit_staged(&staged, path)?;
    // The new file is in place; a failed directory sync only weakens durability, so it is logged.
    if let Err(error) = fs::File::open(dir).and_then(|dir| dir.sync_all()) {
        log::warn!("syncing {}: {error}", dir.display());
    }
    Ok(())
}

/// The file's bytes, `None` when it does not exist, or an error when it is larger than `cap`.
fn read_capped(path: &Path, cap: u64) -> Result<Option<Vec<u8>>> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > cap => {
            bail!("{} is {} bytes, over {cap}", path.display(), metadata.len())
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::locks::{LockTimedOut, tests::thread_registry};
    use futures::{FutureExt as _, executor::block_on};
    use gpui::TestAppContext;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use serde_json::json;

    const DIGEST: &str = "ae0c4b01e17ab73a4a5227a78ea99e6dcbe3e499600315b22759a4e4cddd11df";
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    pub(crate) fn key_pair(seed: u8) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap()
    }

    fn trusted(id: &str, pair: &Ed25519KeyPair) -> TrustedKey {
        TrustedKey::parse(id, &BASE64.encode(pair.public_key().as_ref())).unwrap()
    }

    pub(crate) fn release_keys() -> Vec<TrustedKey> {
        vec![trusted("release-1", &key_pair(7))]
    }

    fn manifest_url() -> Url {
        Url::parse("https://manifests.example.com/kotlin/servers.json").unwrap()
    }

    pub(crate) fn servers(serial: u64, version: &str) -> Vec<u8> {
        let url = format!(
            "https://download.jetbrains.com/language-server/kotlin-server/{version}/kotlin-server-{version}.tar.gz"
        );
        serde_json::to_vec_pretty(&json!({
            "schema": 1, "kind": "bongos-kotlin-servers", "serial": serial,
            "published": "2026-11-11",
            "kotlin-lsp": [{
                "version": version, "built": "2026-11-10", "expires": "2026-12-09", "requires": 1,
                "platform": { "linux-x64": {
                    "url": url, "sha256": DIGEST, "archive": "tar.gz", "strip_components": 1,
                    "launcher": "bin/intellij-server", "runtime": "jbr/bin/java"
                } }
            }]
        }))
        .unwrap()
    }

    pub(crate) fn signed_with(
        servers: Vec<u8>,
        id: &str,
        pair: &Ed25519KeyPair,
    ) -> FetchedManifest {
        let sig = serde_json::to_vec(&json!({
            "signatures": [{ "key": id, "ed25519": BASE64.encode(pair.sign(&servers)) }]
        }))
        .unwrap();
        FetchedManifest { servers, sig }
    }

    pub(crate) fn signed(serial: u64, version: &str) -> FetchedManifest {
        signed_with(servers(serial, version), "release-1", &key_pair(7))
    }

    pub(crate) fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-12T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn seconds(n: i64) -> chrono::Duration {
        chrono::Duration::seconds(n)
    }

    fn cache_at(root: &Path) -> ManifestCache {
        ManifestCache::new(root, thread_registry())
    }

    fn claim(cache: &ManifestCache, at: DateTime<Utc>) -> Lease {
        match block_on(cache.claim_fetch(at, DAY, false)).unwrap() {
            Claim::Claimed(lease) => lease,
            other => panic!("expected a claim, got {other:?}"),
        }
    }

    fn fetch(cache: &ManifestCache, at: DateTime<Utc>, fetched: FetchedManifest) -> FetchOutcome {
        let lease = claim(cache, at);
        block_on(cache.finish_fetch(lease, at, Ok(fetched), &manifest_url(), &release_keys()))
            .unwrap()
    }

    fn cached_serial(cache: &ManifestCache) -> Option<u64> {
        cache
            .load(&manifest_url(), &release_keys())
            .map(|cached| cached.manifest.serial)
    }

    #[test]
    fn test_crash_between_stage_and_rename_keeps_the_old_pair() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache_at(root.path());
        let old = signed(1, "263.1.0");
        assert_eq!(
            fetch(&cache, now(), old.clone()),
            FetchOutcome::Stored { serial: 1 }
        );

        // The process dies after staging serial 2 and before the rename.
        let new = signed(2, "263.2.0");
        let staged = stage(&cache.cache_path(), &encode_cache(&new).unwrap()).unwrap();
        assert!(staged.exists());

        let loaded = cache.load(&manifest_url(), &release_keys()).unwrap();
        assert_eq!(loaded.manifest.serial, 1);
        assert_eq!((loaded.servers, loaded.sig), (old.servers, old.sig));

        // The next writer replaces the stale staged file and installs the new pair whole.
        let later = now() + seconds(2 * 24 * 60 * 60);
        assert_eq!(
            fetch(&cache, later, new.clone()),
            FetchOutcome::Stored { serial: 2 }
        );
        let loaded = cache.load(&manifest_url(), &release_keys()).unwrap();
        assert_eq!((loaded.servers, loaded.sig), (new.servers, new.sig));
    }

    #[test]
    fn test_lower_or_equal_serial_is_refused_also_across_processes() {
        let root = tempfile::tempdir().unwrap();
        let this_process = cache_at(root.path());
        let other_process = cache_at(root.path());

        // This process claims the fetch (phase 1) ...
        let lease = claim(&this_process, now());
        // ... and, its lease having expired, another process fetches serial 5 meanwhile.
        let later = now() + seconds(61);
        assert_eq!(
            fetch(&other_process, later, signed(5, "263.5.0")),
            FetchOutcome::Stored { serial: 5 }
        );
        let stored = fs::read(this_process.cache_path()).unwrap();

        for serial in [4, 5] {
            let outcome = block_on(this_process.finish_fetch(
                lease,
                later,
                Ok(signed(serial, "263.4.0")),
                &manifest_url(),
                &release_keys(),
            ))
            .unwrap();
            assert_eq!(outcome, FetchOutcome::NotNewer { serial, cached: 5 });
            assert_eq!(fs::read(this_process.cache_path()).unwrap(), stored);
        }
        assert_eq!(
            block_on(this_process.finish_fetch(
                lease,
                later,
                Ok(signed(6, "263.6.0")),
                &manifest_url(),
                &release_keys(),
            ))
            .unwrap(),
            FetchOutcome::Stored { serial: 6 }
        );
        assert_eq!(cached_serial(&other_process), Some(6));
    }

    #[test]
    fn test_corrupt_foreign_or_unknown_key_cache_counts_as_absent() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache_at(root.path());
        let path = cache.cache_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let write = |fetched: &FetchedManifest| {
            fs::write(&path, encode_cache(fetched).unwrap()).unwrap();
        };

        assert_eq!(cached_serial(&cache), None);
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(cached_serial(&cache), None);
        fs::write(&path, br#"{"version": 2, "servers": "", "sig": ""}"#).unwrap();
        assert_eq!(cached_serial(&cache), None);

        // Signed by a key this build does not know.
        write(&signed_with(
            servers(9, "263.9.0"),
            "stranger",
            &key_pair(8),
        ));
        assert_eq!(cached_serial(&cache), None);
        write(&signed_with(
            servers(9, "263.9.0"),
            "release-1",
            &key_pair(8),
        ));
        assert_eq!(cached_serial(&cache), None);

        // A manifest beside another manifest's signature.
        let foreign = FetchedManifest {
            servers: servers(9, "263.9.0"),
            sig: signed(8, "263.8.0").sig,
        };
        write(&foreign);
        assert_eq!(cached_serial(&cache), None);

        // A signed document of another kind.
        let other_kind = serde_json::to_vec(&json!({
            "schema": 1, "kind": "bongos-something-else", "serial": 9, "kotlin-lsp": []
        }))
        .unwrap();
        write(&signed_with(other_kind, "release-1", &key_pair(7)));
        assert_eq!(cached_serial(&cache), None);

        // An absent cache does not block a lower serial than the unusable one claimed.
        assert_eq!(
            fetch(&cache, now(), signed(3, "263.3.0")),
            FetchOutcome::Stored { serial: 3 }
        );
        assert_eq!(cached_serial(&cache), Some(3));
    }

    #[test]
    fn test_unverified_fetch_and_failed_fetch_leave_the_cache() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache_at(root.path());
        fetch(&cache, now(), signed(1, "263.1.0"));
        let stored = fs::read(cache.cache_path()).unwrap();

        let later = now() + seconds(2 * 24 * 60 * 60);
        let outcome = fetch(
            &cache,
            later,
            signed_with(servers(2, "263.2.0"), "stranger", &key_pair(8)),
        );
        assert!(
            matches!(outcome, FetchOutcome::Rejected { .. }),
            "{outcome:?}"
        );

        let lease = block_on(cache.claim_fetch(later, DAY, true)).unwrap();
        let Claim::Claimed(lease) = lease else {
            panic!("{lease:?}")
        };
        let outcome = block_on(cache.finish_fetch(
            lease,
            later,
            Err(anyhow::anyhow!("offline")),
            &manifest_url(),
            &release_keys(),
        ))
        .unwrap();
        assert_eq!(
            outcome,
            FetchOutcome::Failed {
                reason: "offline".into()
            }
        );
        assert_eq!(fs::read(cache.cache_path()).unwrap(), stored);
        let state = cache.read_state();
        assert_eq!(state.last_check, Some(later));
        assert_eq!(
            state.last_outcome.as_deref(),
            Some("manifest: rejected: offline")
        );
    }

    #[test]
    fn test_fetch_claim() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache_at(root.path());
        let other_process = cache_at(root.path());

        // Nothing checked yet: claimed, and the lease is written.
        let lease = claim(&cache, now());
        assert_eq!(lease.until, now() + seconds(60));
        assert_eq!(cache.read_state().fetch_lease, Some(lease.until));

        // A live lease makes every other caller skip, forced or not.
        for force in [false, true] {
            assert_eq!(
                block_on(other_process.claim_fetch(now() + seconds(30), DAY, force)).unwrap(),
                Claim::Busy
            );
        }

        // An expired lease (its process died in phase 2) is taken over.
        let takeover = claim(&other_process, now() + seconds(61));
        assert_eq!(takeover.until, now() + seconds(121));

        // Phase 3 of the dead fetch's lease leaves the new lease alone; the new owner clears it.
        let at = now() + seconds(70);
        block_on(cache.finish_fetch(
            lease,
            at,
            Err(anyhow::anyhow!("timed out")),
            &manifest_url(),
            &release_keys(),
        ))
        .unwrap();
        assert_eq!(cache.read_state().fetch_lease, Some(takeover.until));
        block_on(other_process.finish_fetch(
            takeover,
            at,
            Ok(signed(1, "263.1.0")),
            &manifest_url(),
            &release_keys(),
        ))
        .unwrap();
        let state = cache.read_state();
        assert_eq!(state.fetch_lease, None);
        assert_eq!(state.last_check, Some(at));

        // A fresh last_check skips, unless forced.
        assert_eq!(
            block_on(cache.claim_fetch(at + seconds(60), DAY, false)).unwrap(),
            Claim::Fresh
        );
        assert!(matches!(
            block_on(cache.claim_fetch(at + seconds(60), DAY, true)).unwrap(),
            Claim::Claimed(_)
        ));

        // A stale last_check is claimed once the forced claim's lease ran out.
        assert!(matches!(
            block_on(cache.claim_fetch(at + seconds(25 * 60 * 60), DAY, false)).unwrap(),
            Claim::Claimed(_)
        ));

        // A last_check or a lease in the future (the clock moved back) does not block forever.
        block_on(cache.with_state(|state| {
            state.last_check = Some(now() + seconds(365 * 24 * 60 * 60));
            state.fetch_lease = Some(now() + seconds(365 * 24 * 60 * 60));
        }))
        .unwrap();
        assert!(matches!(
            block_on(cache.claim_fetch(now(), DAY, false)).unwrap(),
            Claim::Claimed(_)
        ));
    }

    #[gpui::test]
    fn test_with_state_gives_up_when_the_lock_stays_busy(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().unwrap();
        let cache = Arc::new(ManifestCache::new(
            root.path(),
            Arc::new(
                LockRegistry::new(Arc::new(cx.executor())).with_state_limit(Duration::from_secs(5)),
            ),
        ));
        block_on(cache.with_state(|state| state.last_outcome = Some("before".into()))).unwrap();
        let before = fs::read(cache.state_path()).unwrap();

        let other_process = thread_registry();
        let held = block_on(other_process.state_lock(root.path())).unwrap();
        let task = cx.executor().spawn({
            let cache = cache.clone();
            async move {
                cache
                    .with_state(|state| state.last_outcome = Some("after".into()))
                    .await
            }
        });
        cx.executor().advance_clock(Duration::from_secs(6));
        cx.run_until_parked();
        let error = task.now_or_never().unwrap().unwrap_err();
        assert!(error.downcast_ref::<LockTimedOut>().is_some(), "{error:#}");
        assert_eq!(fs::read(cache.state_path()).unwrap(), before);
        drop(held);
    }

    #[test]
    fn test_two_threads_lose_no_update() {
        let root = tempfile::tempdir().unwrap();
        const EACH: usize = 25;
        let threads: Vec<_> = (0..2)
            .map(|thread| {
                // Each thread stands in for a process, with its own registry.
                let registry = Arc::new(
                    LockRegistry::new(Arc::new(crate::locks::tests::ThreadClock))
                        .with_state_limit(Duration::from_secs(600)),
                );
                let cache = ManifestCache::new(root.path(), registry);
                std::thread::spawn(move || {
                    for n in 0..EACH {
                        block_on(cache.with_state(|state| {
                            state.kotlin_lsp.rejected.push(BuildId {
                                version: format!("{thread}.{n}"),
                                sha256: DIGEST.into(),
                            });
                        }))
                        .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let state = cache_at(root.path()).read_state();
        assert_eq!(state.kotlin_lsp.rejected.len(), 2 * EACH);
    }

    #[test]
    fn test_state_round_trips_and_unknown_version_counts_as_empty() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache_at(root.path());
        assert_eq!(cache.read_state(), ServerState::default());

        let build = |version: &str| BuildId {
            version: version.into(),
            sha256: DIGEST.into(),
        };
        let state = ServerState {
            last_check: Some(now()),
            fetch_lease: Some(now() + seconds(60)),
            last_outcome: Some("manifest: fetched serial 3".into()),
            kotlin_lsp: KotlinLspBuilds {
                active: Some(build("263.2.0")),
                previous: Some(build("263.1.0")),
                trial: Some(build("263.3.0")),
                rejected: vec![build("263.0.9")],
            },
        };
        block_on(cache.with_state(|current| *current = state.clone())).unwrap();
        assert_eq!(cache.read_state(), state);
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(cache.state_path()).unwrap()).unwrap();
        assert_eq!(written["version"], 1);
        assert_eq!(written["last_check"], "2026-11-12T10:00:00Z");
        assert_eq!(written["kotlin-lsp"]["active"]["version"], "263.2.0");

        let mut newer = written;
        newer["version"] = json!(2);
        fs::write(cache.state_path(), serde_json::to_vec(&newer).unwrap()).unwrap();
        assert_eq!(cache.read_state(), ServerState::default());
        fs::write(cache.state_path(), b"garbage").unwrap();
        assert_eq!(cache.read_state(), ServerState::default());

        // A with_state that changes nothing leaves the file alone.
        block_on(cache.with_state(|_| ())).unwrap();
        assert_eq!(fs::read(cache.state_path()).unwrap(), b"garbage");
    }
}
