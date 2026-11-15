//! Downloads, verifies and unpacks a pinned Kotlin server (decision D6). `install` is the only way
//! an archive reaches the disk as a runnable server, and it only extracts a download whose SHA-256
//! equals the manifest's: a tampered archive is never extracted, let alone run.
//!
//! Layout under `dir` (`<install root>/<server>`): the body streams to `.<version>.download` while
//! it is hashed, a verified archive unpacks into `.<version>.staging`, and the unpacked root is
//! renamed to `<version>`. Failure, cancellation and dropping the future all remove both partial
//! paths. kotlin-lsp keeps every other version (a running server or another process may use it;
//! `prune` removes the unused ones at startup); IntelliJ keeps one version.
//!
//! An install holds the version's lock (`locks.rs`, `<dir>/.<version>.lock`), so two installs of
//! one version (two projects or two processes starting Kotlin at once) take turns, while installs
//! of different versions run side by side. `ensure_installed` checks for the build again once it
//! holds the lock, so the second caller finds the first one's result instead of downloading again.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result, bail, ensure};
use async_compression::futures::bufread::GzipDecoder;
use futures::{AsyncWriteExt as _, StreamExt as _, io::BufReader};
use gpui::{App, AppContext as _, Task};
use http_client::{
    HttpClient,
    github_download::{DownloadCancelled, download_verified_file},
};
use settings::KotlinServerKind;

use crate::{
    installed::{MARKER, Marker, check_dir, server_name},
    locks::{LockRegistry, LockWaitCancelled},
    manifest::{ArchiveKind, ManifestEntry},
    manifest_cache::ManifestCache,
};

/// Progress is reported each time this many more bytes have arrived.
pub const PROGRESS_STEP: u64 = http_client::github_download::VERIFIED_DOWNLOAD_PROGRESS_STEP;

/// `prune` keeps a version whose install marker is younger than this: it may have just been
/// installed by a process that has not recorded it yet.
pub const PRUNE_GRACE: Duration = Duration::from_secs(10 * 60);

fn download_file(dir: &Path, version: &str) -> PathBuf {
    dir.join(format!(".{version}.download"))
}

fn staging_dir(dir: &Path, version: &str) -> PathBuf {
    dir.join(format!(".{version}.staging"))
}

/// `<dir>/.<version>.pending`: a background download installed this build and has not recorded it
/// as the trial yet. `prune` keeps a version with this marker.
pub fn pending_marker_path(dir: &Path, version: &str) -> PathBuf {
    dir.join(format!(".{version}.pending"))
}

/// How an install takes its version lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallMode {
    /// A start: waits for the version lock until it is free or the install is cancelled.
    Start,
    /// A background download: one try for the version lock (busy gives `VersionBusy`), and a fresh
    /// install leaves a pending-trial marker, written before the lock is released.
    Background,
}

/// A background install found its version locked by another install.
#[derive(Debug)]
pub struct VersionBusy {
    pub version: String,
}

impl std::fmt::Display for VersionBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} is being installed elsewhere", self.version)
    }
}

impl std::error::Error for VersionBusy {}

/// Called with the bytes downloaded and the total from `Content-Length`, when the server sent one.
pub type ProgressFn = dyn Fn(u64, Option<u64>) + Send + Sync;

/// The error of an install stopped by its cancel flag. Its message is the status text.
#[derive(Debug)]
pub struct InstallCancelled;

impl std::fmt::Display for InstallCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("install cancelled")
    }
}

impl std::error::Error for InstallCancelled {}

#[derive(Debug)]
pub struct InstalledServer {
    pub server: KotlinServerKind,
    pub version: String,
    /// `<dir>/<version>`.
    pub dir: PathBuf,
    pub launcher: PathBuf,
}

/// An archive whose digest matched the manifest. Only `install_locked` makes one, right after
/// `download_verified_file` checked it, and the extractors take nothing else.
struct VerifiedArchive(PathBuf);

/// Removes the install's partial files when dropped: on success there are none left, on an error,
/// a cancel or a dropped future they are the partial install.
struct PartialFiles {
    paths: Vec<PathBuf>,
}

impl PartialFiles {
    fn new(dir: &Path, version: &str) -> Self {
        let this = Self {
            paths: vec![download_file(dir, version), staging_dir(dir, version)],
        };
        this.remove();
        this
    }

    fn remove(&self) {
        for path in &self.paths {
            if let Err(error) = remove_path(path) {
                log::warn!("removing {}: {error}", path.display());
            }
        }
    }
}

impl Drop for PartialFiles {
    fn drop(&mut self) {
        self.remove();
    }
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(InstallCancelled.into());
    }
    Ok(())
}

/// Starts `ensure_installed` on the background executor, with the process's lock registry.
/// Dropping the task cancels the install and removes its partial files; setting `cancel` stops it
/// at the next chunk, or ends its wait for the version lock.
pub fn spawn_ensure_installed(
    server: KotlinServerKind,
    entry: ManifestEntry,
    dir: PathBuf,
    http: Arc<dyn HttpClient>,
    progress: Arc<ProgressFn>,
    cancel: Arc<AtomicBool>,
    cx: &mut App,
) -> Task<Result<InstalledServer>> {
    let locks = LockRegistry::global(cx);
    spawn_ensure_installed_with(
        InstallMode::Start,
        locks,
        server,
        entry,
        dir,
        http,
        progress,
        cancel,
        cx,
    )
}

/// `spawn_ensure_installed` with an explicit mode and lock registry.
#[allow(clippy::too_many_arguments)]
pub fn spawn_ensure_installed_with(
    mode: InstallMode,
    locks: Arc<LockRegistry>,
    server: KotlinServerKind,
    entry: ManifestEntry,
    dir: PathBuf,
    http: Arc<dyn HttpClient>,
    progress: Arc<ProgressFn>,
    cancel: Arc<AtomicBool>,
    cx: &App,
) -> Task<Result<InstalledServer>> {
    cx.background_spawn(async move {
        ensure_installed_with(
            mode,
            &locks,
            server,
            &entry,
            &dir,
            http.as_ref(),
            progress.as_ref(),
            &cancel,
        )
        .await
    })
}

/// The pinned build of `server` under `dir`, installing it first when it is missing. An installed
/// build costs no network and no lock: this is what a start calls.
pub async fn ensure_installed(
    locks: &LockRegistry,
    server: KotlinServerKind,
    entry: &ManifestEntry,
    dir: &Path,
    http: &dyn HttpClient,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<InstalledServer> {
    ensure_installed_with(
        InstallMode::Start,
        locks,
        server,
        entry,
        dir,
        http,
        progress,
        cancel,
    )
    .await
}

/// `ensure_installed` taking the version lock as `mode` says.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_installed_with(
    mode: InstallMode,
    locks: &LockRegistry,
    server: KotlinServerKind,
    entry: &ManifestEntry,
    dir: &Path,
    http: &dyn HttpClient,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<InstalledServer> {
    check_cancel(cancel)?;
    let final_dir = dir.join(&entry.version);
    if check_dir(&final_dir, entry).is_some() {
        return Ok(installed_server(server, entry, final_dir));
    }
    let _lock = lock_version(mode, locks, dir, &entry.version, cancel).await?;
    // Another install of this version, in this process or another, may have finished meanwhile.
    check_cancel(cancel)?;
    if check_dir(&final_dir, entry).is_some() {
        return Ok(installed_server(server, entry, final_dir));
    }
    install_locked(mode, server, entry, dir, http, progress, cancel).await
}

/// Installs the pinned build `entry` of `server` under `dir`, replacing any copy already there.
pub async fn install(
    locks: &LockRegistry,
    server: KotlinServerKind,
    entry: &ManifestEntry,
    dir: &Path,
    http: &dyn HttpClient,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<InstalledServer> {
    check_cancel(cancel)?;
    let _lock = lock_version(InstallMode::Start, locks, dir, &entry.version, cancel).await?;
    install_locked(
        InstallMode::Start,
        server,
        entry,
        dir,
        http,
        progress,
        cancel,
    )
    .await
}

/// The version lock of `version` under `dir`: waited for (until `cancel`) by a start, tried once
/// by a background download.
async fn lock_version(
    mode: InstallMode,
    locks: &LockRegistry,
    dir: &Path,
    version: &str,
    cancel: &AtomicBool,
) -> Result<crate::locks::LockGuard> {
    match mode {
        InstallMode::Start => locks
            .version_lock(dir, version, cancel)
            .await
            .map_err(|error| {
                if error.downcast_ref::<LockWaitCancelled>().is_some() {
                    InstallCancelled.into()
                } else {
                    error
                }
            }),
        InstallMode::Background => locks.try_version_lock(dir, version)?.ok_or_else(|| {
            VersionBusy {
                version: version.to_string(),
            }
            .into()
        }),
    }
}

fn installed_server(
    server: KotlinServerKind,
    entry: &ManifestEntry,
    final_dir: PathBuf,
) -> InstalledServer {
    InstalledServer {
        server,
        version: entry.version.clone(),
        launcher: final_dir.join(&entry.launcher),
        dir: final_dir,
    }
}

/// `install` for a caller that holds the version lock.
async fn install_locked(
    mode: InstallMode,
    server: KotlinServerKind,
    entry: &ManifestEntry,
    dir: &Path,
    http: &dyn HttpClient,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<InstalledServer> {
    check_cancel(cancel)?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let _partial = PartialFiles::new(dir, &entry.version);

    let download = download_file(dir, &entry.version);
    download_verified_file(http, &entry.url, &entry.sha256, &download, progress, cancel)
        .await
        .map_err(|error| {
            if error.downcast_ref::<DownloadCancelled>().is_some() {
                InstallCancelled.into()
            } else {
                error
            }
        })?;
    let archive = VerifiedArchive(download);
    check_cancel(cancel)?;

    let staging = staging_dir(dir, &entry.version);
    fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    extract(entry, &archive, &staging, cancel)
        .await
        .with_context(|| format!("unpacking {}", entry.url))?;
    check_cancel(cancel)?;

    let root = strip_components(&staging, entry.strip_components)
        .with_context(|| format!("unpacking {}", entry.url))?;
    let launcher = root.join(&entry.launcher);
    ensure!(
        launcher.is_file(),
        "{} has no launcher at {}",
        entry.url,
        entry.launcher
    );
    util::fs::make_file_executable(&launcher)
        .await
        .with_context(|| format!("making {} executable", launcher.display()))?;
    let marker = Marker {
        version: entry.version.clone(),
        sha256: entry.sha256.clone(),
    };
    fs::write(root.join(MARKER), serde_json::to_string(&marker)?)
        .with_context(|| format!("writing {MARKER}"))?;
    check_cancel(cancel)?;

    let final_dir = dir.join(&entry.version);
    remove_path(&final_dir).with_context(|| format!("replacing {}", final_dir.display()))?;
    fs::rename(&root, &final_dir)
        .with_context(|| format!("moving the unpacked server to {}", final_dir.display()))?;
    if server == KotlinServerKind::Intellij {
        remove_other_versions(dir, &entry.version);
    }

    ensure!(
        check_dir(&final_dir, entry).is_some(),
        "{} did not install correctly",
        server_name(server)
    );
    if mode == InstallMode::Background {
        // Written while the version lock is held. A crash between the rename and this write leaves
        // a fresh install marker, which `prune` respects for `PRUNE_GRACE`.
        let pending = pending_marker_path(dir, &entry.version);
        fs::write(&pending, &entry.sha256)
            .with_context(|| format!("writing {}", pending.display()))?;
    }
    Ok(installed_server(server, entry, final_dir))
}

async fn extract(
    entry: &ManifestEntry,
    archive: &VerifiedArchive,
    staging: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    let file = async_fs::File::open(&archive.0)
        .await
        .with_context(|| format!("opening {}", archive.0.display()))?;
    match entry.archive {
        ArchiveKind::TarGz => {
            let decoder = GzipDecoder::new(BufReader::new(file));
            let mut entries = async_tar::Archive::new(decoder).entries()?;
            while let Some(item) = entries.next().await {
                check_cancel(cancel)?;
                let mut item = item?;
                if !item.unpack_in(staging).await? {
                    log::warn!(
                        "skipped an archive entry with an unsafe path in {}",
                        entry.url
                    );
                }
            }
            Ok(())
        }
        ArchiveKind::Zip => extract_zip(staging, file, cancel).await,
    }
}

/// Extracts a zip like `util::archive::extract_seekable_zip`, but checks `cancel` before each
/// entry, so cancelling a 390 MB bundle does not wait for the whole extraction.
async fn extract_zip(staging: &Path, file: async_fs::File, cancel: &AtomicBool) -> Result<()> {
    let mut reader = async_zip::base::read::seek::ZipFileReader::new(BufReader::new(file))
        .await
        .context("reading the zip archive")?;
    let staging = staging
        .canonicalize()
        .unwrap_or_else(|_| staging.to_path_buf());
    for (index, entry) in reader.file().entries().to_vec().into_iter().enumerate() {
        check_cancel(cancel)?;
        let name = entry
            .filename()
            .as_str()
            .context("reading zip entry file name")?;
        let normal = Path::new(name).components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
        if !normal {
            log::warn!("skipped a zip entry with an unsafe path: {name}");
            continue;
        }
        let path = staging.join(name);
        if entry
            .dir()
            .with_context(|| format!("reading zip entry metadata for {}", path.display()))?
        {
            fs::create_dir_all(&path)
                .with_context(|| format!("creating directory {}", path.display()))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let mut output = async_fs::File::create(&path)
            .await
            .with_context(|| format!("creating {}", path.display()))?;
        let mut entry_reader = reader
            .reader_with_entry(index)
            .await
            .with_context(|| format!("reading zip entry {name}"))?;
        futures::io::copy(&mut entry_reader, &mut output)
            .await
            .with_context(|| format!("extracting {}", path.display()))?;
        output.flush().await?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_permissions().filter(|mode| *mode != 0) {
            use std::os::unix::fs::PermissionsExt as _;
            output
                .set_permissions(fs::Permissions::from_mode(u32::from(mode)))
                .await
                .with_context(|| format!("setting permissions of {}", path.display()))?;
        }
    }
    Ok(())
}

/// Descends `count` single-directory levels below `staging` and returns the directory reached.
fn strip_components(staging: &Path, count: usize) -> Result<PathBuf> {
    let mut root = staging.to_path_buf();
    for _ in 0..count {
        let mut children = fs::read_dir(&root)?
            .map(|child| child.map(|child| child.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        let (Some(child), true) = (children.pop(), children.is_empty()) else {
            bail!("expected one top-level directory in the archive");
        };
        ensure!(
            child.is_dir(),
            "expected the archive's top-level entry to be a directory"
        );
        root = child;
    }
    Ok(root)
}

/// Deletes every entry of `dir` except `keep` and the dot-files (lock files, other installs'
/// partial files). Best effort: a version that cannot be removed is logged and left for the next
/// install.
fn remove_other_versions(dir: &Path, keep: &str) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("listing {}: {error}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        if entry.file_name() != keep
            && !entry.file_name().to_string_lossy().starts_with('.')
            && let Err(error) = remove_path(&entry.path())
        {
            log::warn!(
                "removing the old version {}: {error}",
                entry.path().display()
            );
        }
    }
}

/// What `prune` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// The versions deleted.
    pub removed: Vec<String>,
    /// The versions skipped because another install held their lock.
    pub busy: Vec<String>,
}

/// Deletes every kotlin-lsp version directory of `dir` except the active, previous and trial builds
/// of `state.json` and `pin_version`. A version with a pending-trial marker, or an install marker
/// younger than `PRUNE_GRACE` at `now`, stays too. Under the state lock it reads the keep set and
/// takes each other version's lock with one `try_lock`: a busy version is skipped, never waited
/// for. The deletions run after the state lock is released, each still under its version lock, so
/// a large delete never holds up a `state.json` write. Dot-files (lock files, markers, partial
/// files) are never touched, except the `.download` file and `.staging` directory older installs
/// left behind, deleted once untouched for `PRUNE_GRACE`.
pub async fn prune(
    cache: &ManifestCache,
    locks: &LockRegistry,
    dir: &Path,
    pin_version: Option<&str>,
    now: SystemTime,
) -> Result<Pruned> {
    let (victims, busy) = cache
        .with_state(|state| {
            let builds = &state.kotlin_lsp;
            let keep: Vec<&str> = [&builds.active, &builds.previous, &builds.trial]
                .into_iter()
                .filter_map(|build| build.as_ref().map(|build| build.version.as_str()))
                .chain(pin_version)
                .collect();
            let mut victims = Vec::new();
            let mut busy = Vec::new();
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        log::warn!("listing {}: {error}", dir.display());
                    }
                    return (victims, busy);
                }
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                if name.starts_with('.') || !is_dir || keep.contains(&name.as_str()) {
                    continue;
                }
                let path = entry.path();
                if pending_marker_path(dir, &name).exists() || recently_installed(&path, now) {
                    continue;
                }
                match locks.try_version_lock(dir, &name) {
                    Ok(Some(lock)) => victims.push((name, path, lock)),
                    Ok(None) => busy.push(name),
                    Err(error) => log::warn!("not pruning {}: {error:#}", path.display()),
                }
            }
            (victims, busy)
        })
        .await?;

    for legacy in LEGACY_PARTIALS {
        let path = dir.join(legacy);
        let stale = fs::symlink_metadata(&path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age >= PRUNE_GRACE)
            });
        if stale {
            match remove_path(&path) {
                Ok(()) => log::info!("prune: removed {}", path.display()),
                Err(error) => log::warn!("removing {}: {error}", path.display()),
            }
        }
    }
    let mut pruned = Pruned {
        removed: Vec::new(),
        busy,
    };
    // Each version lock is held until its directory is gone, then released.
    for (name, path, _lock) in victims {
        match remove_path(&path) {
            Ok(()) => pruned.removed.push(name),
            Err(error) => log::warn!("removing {}: {error}", path.display()),
        }
    }
    Ok(pruned)
}

/// The partial download and staging names of older installs, which had no version in them.
const LEGACY_PARTIALS: [&str; 2] = [".download", ".staging"];

/// Whether the install marker in `version_dir` was written less than `PRUNE_GRACE` before `now`
/// (or after it: the clock moved back).
fn recently_installed(version_dir: &Path, now: SystemTime) -> bool {
    let Ok(modified) = fs::metadata(version_dir.join(MARKER)).and_then(|meta| meta.modified())
    else {
        return false;
    };
    match now.duration_since(modified) {
        Ok(age) => age < PRUNE_GRACE,
        Err(_) => true,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        collections::BTreeMap,
        pin::Pin,
        sync::Mutex,
        task::{Context, Poll},
    };

    use futures::{AsyncRead, AsyncReadExt as _, FutureExt as _, executor::block_on, io::Cursor};
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::{
        installed::check_installed,
        locks::tests::thread_registry,
        manifest_cache::{BuildId, ManifestCache},
    };

    const SERVER: KotlinServerKind = KotlinServerKind::KotlinLsp;
    const VERSION: &str = "1.2.3";
    const LAUNCHER_BODY: &[u8] = b"#!/bin/sh\nexit 0\n";

    pub(crate) fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Bytes that do not compress, so the archive is about as large as the payload.
    pub(crate) fn noise(length: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    pub(crate) fn files(noise_length: usize, with_launcher: bool) -> Vec<(String, u32, Vec<u8>)> {
        let mut files = vec![("server-1.2.3/README".to_string(), 0o644, b"hello".to_vec())];
        if with_launcher {
            // Stored without the execute bit: the installer must set it.
            files.push((
                "server-1.2.3/bin/launcher".into(),
                0o644,
                LAUNCHER_BODY.to_vec(),
            ));
        }
        if noise_length > 0 {
            files.push(("server-1.2.3/lib/blob".into(), 0o644, noise(noise_length)));
        }
        files
    }

    pub(crate) fn tar_gz(files: &[(String, u32, Vec<u8>)]) -> Vec<u8> {
        block_on(async {
            let mut builder = async_tar::Builder::new(Vec::new());
            for (path, mode, data) in files {
                let mut header = async_tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(data.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                builder.append(&header, data.as_slice()).await.unwrap();
            }
            let tar = builder.into_inner().await.unwrap();
            let mut encoder = async_compression::futures::bufread::GzipEncoder::new(
                BufReader::new(Cursor::new(tar)),
            );
            let mut gzipped = Vec::new();
            encoder.read_to_end(&mut gzipped).await.unwrap();
            gzipped
        })
    }

    pub(crate) fn zip(files: &[(String, u32, Vec<u8>)]) -> Vec<u8> {
        use async_zip::{Compression, ZipEntryBuilder, base::write::ZipFileWriter};
        block_on(async {
            let mut writer = ZipFileWriter::new(Vec::new());
            for (path, mode, data) in files {
                let entry = ZipEntryBuilder::new(path.clone().into(), Compression::Deflate)
                    .unix_permissions(*mode as u16);
                writer.write_entry_whole(entry, data).await.unwrap();
            }
            writer.close().await.unwrap()
        })
    }

    pub(crate) fn entry_for(archive: &[u8], kind: ArchiveKind) -> ManifestEntry {
        ManifestEntry {
            version: VERSION.into(),
            url: "http://127.0.0.1:1/server".into(),
            sha256: sha256(archive),
            archive: kind,
            strip_components: 1,
            launcher: "bin/launcher".into(),
            runtime: None,
            args: vec!["--stdio".into()],
            env: BTreeMap::from([("A".to_string(), "b".to_string())]),
            built: None,
            expires: None,
        }
    }

    /// Serves its bytes in small reads and raises `cancel` once `cancel_at` bytes went out.
    struct Trickle {
        data: Cursor<Vec<u8>>,
        sent: usize,
        cancel: Option<(usize, Arc<AtomicBool>)>,
    }

    impl AsyncRead for Trickle {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            let limit = buffer.len().min(16 * 1024);
            let result = Pin::new(&mut self.data).poll_read(cx, &mut buffer[..limit]);
            if let Poll::Ready(Ok(read)) = &result {
                self.sent += read;
                if let Some((threshold, flag)) = &self.cancel
                    && self.sent >= *threshold
                {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            result
        }
    }

    type Client = Arc<http_client::HttpClientWithUrl>;

    fn serving(bytes: Vec<u8>, cancel: Option<(usize, Arc<AtomicBool>)>) -> Client {
        let bytes = Arc::new(bytes);
        let cancel = Mutex::new(cancel);
        FakeHttpClient::create(move |_| {
            let bytes = bytes.clone();
            let cancel = cancel.lock().unwrap().clone();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header("Content-Length", bytes.len())
                    .body(AsyncBody::from_reader(Trickle {
                        data: Cursor::new(bytes.to_vec()),
                        sent: 0,
                        cancel,
                    }))
                    .unwrap())
            }
        })
    }

    struct Run {
        result: Result<InstalledServer>,
        events: Vec<(u64, Option<u64>)>,
    }

    fn run_install(client: &Client, entry: &ManifestEntry, dir: &Path, cancel: &AtomicBool) -> Run {
        let events = Mutex::new(Vec::new());
        let record = |done, total| events.lock().unwrap().push((done, total));
        let result = block_on(install(
            &thread_registry(),
            SERVER,
            entry,
            dir,
            client.as_ref(),
            &record,
            cancel,
        ));
        Run {
            result,
            events: events.into_inner().unwrap(),
        }
    }

    /// The entries of `dir`, lock files left out (they are never deleted).
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| !name.ends_with(".lock"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn assert_installed(dir: &Path, installed: &InstalledServer, entry: &ManifestEntry) {
        assert_eq!(installed.dir, dir.join(VERSION));
        assert_eq!(installed.launcher, dir.join(VERSION).join("bin/launcher"));
        assert_eq!(fs::read(&installed.launcher).unwrap(), LAUNCHER_BODY);
        assert_eq!(
            fs::read_to_string(dir.join(VERSION).join("README")).unwrap(),
            "hello"
        );
        let install_root = dir.parent().unwrap();
        // The directory is named after the server, so check_installed finds what install wrote.
        let binary = check_installed(install_root, SERVER, entry).expect("installed");
        assert_eq!(binary.path, installed.launcher);
        assert_eq!(names(dir), vec![VERSION.to_string()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&installed.launcher)
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "launcher is not executable");
        }
    }

    fn server_dir(root: &Path) -> PathBuf {
        root.join(server_name(SERVER))
    }

    #[test]
    fn test_correct_digest_installs_a_tar_gz() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let run = run_install(
            &serving(archive.clone(), None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        let installed = run.result.unwrap();
        assert_installed(&dir, &installed, &entry);
        assert_eq!(
            run.events.last().copied(),
            Some((archive.len() as u64, Some(archive.len() as u64)))
        );
    }

    #[test]
    fn test_correct_digest_installs_a_zip() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = zip(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::Zip);
        let installed = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        )
        .result
        .unwrap();
        assert_installed(&dir, &installed, &entry);
    }

    #[test]
    fn test_zip_extraction_checks_cancel_at_entry_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let archive_path = root.path().join("bundle.zip");
        fs::write(&archive_path, zip(&files(1024, true))).unwrap();
        let extract_into = |name: &str, cancel: &AtomicBool| {
            let staging = root.path().join(name);
            fs::create_dir_all(&staging).unwrap();
            let result = block_on(async {
                let file = async_fs::File::open(&archive_path).await.unwrap();
                extract_zip(&staging, file, cancel).await
            });
            (staging, result)
        };

        let (staging, result) = extract_into("cancelled", &AtomicBool::new(true));
        let error = result.unwrap_err();
        assert!(
            error.downcast_ref::<InstallCancelled>().is_some(),
            "{error:#}"
        );
        assert!(
            fs::read_dir(&staging).unwrap().next().is_none(),
            "an entry was extracted after the cancel"
        );

        let (staging, result) = extract_into("whole", &AtomicBool::new(false));
        result.unwrap();
        assert_eq!(
            fs::read(staging.join("server-1.2.3/README")).unwrap(),
            b"hello"
        );
        assert!(staging.join("server-1.2.3/lib/blob").is_file());
    }

    #[test]
    fn test_wrong_digest_is_rejected_and_nothing_is_extracted() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);

        // The server sends one flipped byte; the pin is the genuine digest.
        let mut tampered = archive.clone();
        let last = tampered.len() / 2;
        tampered[last] ^= 0xff;
        let run = run_install(
            &serving(tampered.clone(), None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        let message = run.result.unwrap_err().to_string();
        assert!(message.contains(&entry.url), "{message}");
        assert!(message.contains(&entry.sha256), "{message}");
        assert!(message.contains(&sha256(&tampered)), "{message}");
        assert!(names(&dir).is_empty(), "left behind: {:?}", names(&dir));
        assert!(check_installed(root.path(), SERVER, &entry).is_none());

        // The same bytes with the right pin pass, so the failure above was the digest.
        let genuine = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        genuine.result.unwrap();
    }

    #[test]
    fn test_a_user_file_pin_for_other_bytes_is_rejected_and_nothing_is_installed() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join(crate::manifest::USER_FILE_NAME),
            format!(
                r#"
[kotlin-lsp]
version = "1.2.3"

[kotlin-lsp.platform.linux-x64]
url = "http://127.0.0.1:1/server"
sha256 = "{}"
archive = "tar.gz"
strip_components = 1
launcher = "bin/launcher"
"#,
                sha256(b"something else")
            ),
        )
        .unwrap();
        let loaded = crate::manifest::Manifest::load_user_file(config.path());
        assert_eq!(loaded.error, None);
        let entry = loaded.manifest.entry(SERVER, "linux-x64").unwrap();
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        assert!(run.result.is_err());
        assert!(names(&dir).is_empty(), "left behind: {:?}", names(&dir));
        assert!(check_installed(root.path(), SERVER, &entry).is_none());
    }

    #[test]
    fn test_a_pin_for_other_bytes_rejects_a_zip_too() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = zip(&files(0, true));
        let mut entry = entry_for(&archive, ArchiveKind::Zip);
        entry.sha256 = sha256(b"something else");
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        assert!(run.result.unwrap_err().to_string().contains("SHA-256"));
        assert!(names(&dir).is_empty());
    }

    #[test]
    fn test_progress_ends_at_the_size_in_steps_of_one_megabyte() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(3 * 1024 * 1024 + 123, true));
        let size = archive.len() as u64;
        assert!(size > 3 * PROGRESS_STEP);
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        run.result.unwrap();

        assert_eq!(run.events.first(), Some(&(0, Some(size))));
        assert_eq!(run.events.last(), Some(&(size, Some(size))));
        assert!(run.events.iter().all(|(_, total)| *total == Some(size)));
        let done: Vec<u64> = run.events.iter().map(|(done, _)| *done).collect();
        assert!(done.windows(2).all(|pair| pair[1] > pair[0]), "{done:?}");
        // Every event but the first and the last is at least a step after the one before it.
        let middle = &done[..done.len() - 1];
        assert!(
            middle
                .windows(2)
                .all(|pair| pair[1] - pair[0] >= PROGRESS_STEP),
            "{done:?}"
        );
        assert!(done.len() >= 4, "{done:?}");
    }

    #[test]
    fn test_cancel_mid_stream_removes_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(3 * 1024 * 1024, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let cancel = Arc::new(AtomicBool::new(false));
        let client = serving(
            archive.clone(),
            Some((PROGRESS_STEP as usize, cancel.clone())),
        );

        let run = run_install(&client, &entry, &dir, &cancel);
        let error = run.result.unwrap_err();
        assert_eq!(error.to_string(), "install cancelled");
        assert!(error.downcast_ref::<InstallCancelled>().is_some());
        let last = run.events.last().unwrap().0;
        assert!(last < archive.len() as u64, "the whole archive arrived");
        assert!(names(&dir).is_empty(), "left behind: {:?}", names(&dir));
        assert!(check_installed(root.path(), SERVER, &entry).is_none());

        // The next start retries from zero.
        let retry = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        assert_installed(&dir, &retry.result.unwrap(), &entry);
    }

    #[test]
    fn test_cancel_before_start_touches_nothing() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(true),
        );
        assert_eq!(run.result.unwrap_err().to_string(), "install cancelled");
        assert!(!dir.exists());
    }

    #[test]
    fn test_failure_then_retry_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);

        let failing = FakeHttpClient::create(|_| async move {
            Ok(Response::builder()
                .status(503)
                .body(AsyncBody::default())
                .unwrap())
        });
        let run = run_install(&failing, &entry, &dir, &AtomicBool::new(false));
        let message = run.result.unwrap_err().to_string();
        assert!(
            message.contains(&entry.url) && message.contains("503"),
            "{message}"
        );
        assert!(names(&dir).is_empty());

        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        assert_installed(&dir, &run.result.unwrap(), &entry);
    }

    #[test]
    fn test_an_existing_version_survives_and_stale_partials_are_deleted() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        fs::create_dir_all(dir.join("0.9/bin")).unwrap();
        fs::write(dir.join("0.9/bin/launcher"), "old").unwrap();
        fs::create_dir_all(staging_dir(&dir, VERSION).join("leftover")).unwrap();
        fs::write(download_file(&dir, VERSION), "half").unwrap();

        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        run.result.unwrap();
        assert_eq!(names(&dir), ["0.9", VERSION]);
        assert_eq!(fs::read(dir.join("0.9/bin/launcher")).unwrap(), b"old");
        assert!(dir.join(".1.2.3.lock").is_file());
    }

    #[test]
    fn test_intellij_keeps_one_version_but_never_a_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(server_name(KotlinServerKind::Intellij));
        fs::create_dir_all(dir.join("0.9/bin")).unwrap();
        fs::write(dir.join(".0.9.lock"), "").unwrap();
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let client = serving(archive, None);
        block_on(install(
            &thread_registry(),
            KotlinServerKind::Intellij,
            &entry,
            &dir,
            client.as_ref(),
            &|_, _| {},
            &AtomicBool::new(false),
        ))
        .unwrap();
        assert_eq!(names(&dir), [VERSION]);
        assert!(dir.join(".0.9.lock").is_file());
    }

    #[test]
    fn test_an_archive_without_the_launcher_leaves_no_version() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, false));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        let message = run.result.unwrap_err().to_string();
        assert!(message.contains("no launcher"), "{message}");
        assert!(names(&dir).is_empty());
    }

    #[test]
    fn test_strip_components_wants_one_top_level_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("top/inner")).unwrap();
        assert_eq!(
            strip_components(root.path(), 2).unwrap(),
            root.path().join("top/inner")
        );
        assert_eq!(strip_components(root.path(), 0).unwrap(), root.path());
        fs::create_dir_all(root.path().join("second")).unwrap();
        assert!(strip_components(root.path(), 1).is_err());
    }

    #[test]
    fn test_dropping_the_install_removes_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(3 * 1024 * 1024, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let client = serving(archive, None);
        let cancel = AtomicBool::new(false);
        let polls = Mutex::new(0);
        block_on(async {
            let progress = |_done, _total| *polls.lock().unwrap() += 1;
            let registry = thread_registry();
            let mut future = Box::pin(install(
                &registry,
                SERVER,
                &entry,
                &dir,
                client.as_ref(),
                &progress,
                &cancel,
            ));
            // Poll until the download has produced a file, then drop the future.
            futures::future::poll_fn(|cx| {
                let _ = future.as_mut().poll(cx);
                if download_file(&dir, VERSION).exists() && *polls.lock().unwrap() > 0 {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            assert!(download_file(&dir, VERSION).exists());
            drop(future);
        });
        assert!(names(&dir).is_empty(), "left behind: {:?}", names(&dir));
    }

    #[gpui::test]
    async fn test_spawn_ensure_installed_runs_in_the_background(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let client: Arc<dyn HttpClient> = serving(archive, None);
        let installed = cx
            .update(|cx| {
                spawn_ensure_installed(
                    SERVER,
                    entry.clone(),
                    dir.clone(),
                    client,
                    Arc::new(|_, _| {}),
                    Arc::new(AtomicBool::new(false)),
                    cx,
                )
            })
            .await
            .unwrap();
        assert_installed(&dir, &installed, &entry);
    }

    /// A response whose body counts as in flight until it is dropped.
    struct Tracked {
        inner: Trickle,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl AsyncRead for Tracked {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_read(cx, buffer)
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Serves `bytes`, counting the requests and how many bodies are open at once.
    struct Counting {
        client: Client,
        requests: Arc<std::sync::atomic::AtomicUsize>,
        most_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn counting(bytes: Vec<u8>) -> Counting {
        use std::sync::atomic::AtomicUsize;
        let requests = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let most_in_flight = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(bytes);
        let client = FakeHttpClient::create({
            let (requests, most_in_flight) = (requests.clone(), most_in_flight.clone());
            move |_| {
                requests.fetch_add(1, Ordering::SeqCst);
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                most_in_flight.fetch_max(now, Ordering::SeqCst);
                let body = Tracked {
                    inner: Trickle {
                        data: Cursor::new(bytes.to_vec()),
                        sent: 0,
                        cancel: None,
                    },
                    in_flight: in_flight.clone(),
                };
                let length = bytes.len();
                async move {
                    Ok(Response::builder()
                        .status(200)
                        .header("Content-Length", length)
                        .body(AsyncBody::from_reader(body))
                        .unwrap())
                }
            }
        });
        Counting {
            client,
            requests,
            most_in_flight,
        }
    }

    #[test]
    fn test_installs_of_one_directory_take_turns() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(512 * 1024, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let served = counting(archive);
        let cancel = AtomicBool::new(false);
        let registry = thread_registry();
        let (first, second) = block_on(async {
            futures::join!(
                install(
                    &registry,
                    SERVER,
                    &entry,
                    &dir,
                    served.client.as_ref(),
                    &|_, _| {},
                    &cancel
                ),
                install(
                    &registry,
                    SERVER,
                    &entry,
                    &dir,
                    served.client.as_ref(),
                    &|_, _| {},
                    &cancel
                ),
            )
        });
        first.unwrap();
        second.unwrap();
        assert_eq!(served.requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            served.most_in_flight.load(Ordering::SeqCst),
            1,
            "two installs shared .download and .staging"
        );
        assert_eq!(names(&dir), [VERSION]);
    }

    #[test]
    fn test_concurrent_ensure_in_two_processes_downloads_once_and_a_later_one_not_at_all() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(512 * 1024, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let served = counting(archive);
        let cancel = AtomicBool::new(false);
        // One registry per simulated process.
        let (first_process, second_process) = (thread_registry(), thread_registry());
        let ensure = |registry: &Arc<LockRegistry>| {
            let registry = registry.clone();
            let (entry, dir, served, cancel) = (&entry, &dir, &served, &cancel);
            async move {
                ensure_installed(
                    &registry,
                    SERVER,
                    entry,
                    dir,
                    served.client.as_ref(),
                    &|_, _| {},
                    cancel,
                )
                .await
            }
        };
        let (first, second) =
            block_on(async { futures::join!(ensure(&first_process), ensure(&second_process)) });
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first.dir, second.dir);
        assert_installed(&dir, &first, &entry);
        assert_eq!(served.requests.load(Ordering::SeqCst), 1);
        assert_eq!(served.most_in_flight.load(Ordering::SeqCst), 1);

        block_on(ensure(&first_process)).unwrap();
        assert_eq!(
            served.requests.load(Ordering::SeqCst),
            1,
            "installed: no network"
        );
    }

    #[test]
    fn test_concurrent_ensure_in_one_process_downloads_once() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(512 * 1024, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let served = counting(archive);
        let cancel = AtomicBool::new(false);
        let registry = thread_registry();
        let ensure = || {
            ensure_installed(
                &registry,
                SERVER,
                &entry,
                &dir,
                served.client.as_ref(),
                &|_, _| {},
                &cancel,
            )
        };
        let (first, second) = block_on(async { futures::join!(ensure(), ensure()) });
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first.dir, second.dir);
        assert_installed(&dir, &first, &entry);
        assert_eq!(served.requests.load(Ordering::SeqCst), 1);

        block_on(ensure()).unwrap();
        assert_eq!(
            served.requests.load(Ordering::SeqCst),
            1,
            "installed: no network"
        );
    }

    #[test]
    fn test_ensure_installs_over_a_marker_for_another_digest() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let served = counting(archive);
        // A version directory whose marker names another digest does not count as installed.
        let version_dir = dir.join(VERSION);
        fs::create_dir_all(version_dir.join("bin")).unwrap();
        fs::write(version_dir.join("bin/launcher"), LAUNCHER_BODY).unwrap();
        fs::write(
            version_dir.join(MARKER),
            serde_json::to_string(&Marker {
                version: VERSION.into(),
                sha256: "0".repeat(64),
            })
            .unwrap(),
        )
        .unwrap();
        let installed = block_on(ensure_installed(
            &thread_registry(),
            SERVER,
            &entry,
            &dir,
            served.client.as_ref(),
            &|_, _| {},
            &AtomicBool::new(false),
        ))
        .unwrap();
        assert_installed(&dir, &installed, &entry);
        assert_eq!(served.requests.load(Ordering::SeqCst), 1);
    }

    /// An archive entry named `../escape`, which a careless extractor writes next to the install.
    fn hostile_tar_gz() -> Vec<u8> {
        block_on(async {
            let mut builder = async_tar::Builder::new(Vec::new());
            let mut header = async_tar::Header::new_gnu();
            let data = b"pwned";
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            let name = b"server-1.2.3/../../escape";
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_cksum();
            builder.append(&header, &data[..]).await.unwrap();
            let mut header = async_tar::Header::new_gnu();
            header.set_path("server-1.2.3/bin/launcher").unwrap();
            header.set_size(LAUNCHER_BODY.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, LAUNCHER_BODY).await.unwrap();
            let tar = builder.into_inner().await.unwrap();
            let mut encoder = async_compression::futures::bufread::GzipEncoder::new(
                BufReader::new(Cursor::new(tar)),
            );
            let mut gzipped = Vec::new();
            encoder.read_to_end(&mut gzipped).await.unwrap();
            gzipped
        })
    }

    #[test]
    fn test_a_tar_with_a_path_traversal_entry_writes_nothing_outside() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = hostile_tar_gz();
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        run.result.unwrap();
        for escaped in [
            dir.join("escape"),
            root.path().join("escape"),
            dir.join(VERSION).join("escape"),
        ] {
            assert!(!escaped.exists(), "{} was written", escaped.display());
        }
        assert!(
            fs::read_dir(root.path()).unwrap().count() == 1,
            "only the server directory"
        );
    }

    #[test]
    fn test_a_zip_with_a_path_traversal_entry_writes_nothing_outside() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = zip(&[
            ("server-1.2.3/../../escape".into(), 0o644, b"pwned".to_vec()),
            (
                "server-1.2.3/bin/launcher".into(),
                0o644,
                LAUNCHER_BODY.to_vec(),
            ),
        ]);
        let entry = entry_for(&archive, ArchiveKind::Zip);
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        run.result.unwrap();
        for escaped in [
            dir.join("escape"),
            root.path().join("escape"),
            dir.join(VERSION).join("escape"),
        ] {
            assert!(!escaped.exists(), "{} was written", escaped.display());
        }
        assert!(
            fs::read_dir(root.path()).unwrap().count() == 1,
            "only the server directory"
        );
    }

    /// The real client against a loopback server: a redirect is followed, `Content-Length` gives
    /// the total, and a body labeled `Content-Encoding: gzip` arrives as sent (the digest is of the
    /// archive on the wire, which is itself gzip).
    #[test]
    fn test_real_client_follows_a_redirect_and_keeps_the_bytes() {
        let archive = tar_gz(&files(2 * 1024 * 1024, true));
        let entry_template = entry_for(&archive, ArchiveKind::TarGz);
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let served = archive.clone();
        let paths = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for _ in 0..2 {
                let request = server.recv().unwrap();
                paths.push(request.url().to_string());
                let header = |name: &str, value: &str| {
                    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
                };
                if request.url() == "/download" {
                    let location = format!("http://127.0.0.1:{port}/kotlin-server.tar.gz");
                    request
                        .respond(
                            tiny_http::Response::empty(302)
                                .with_header(header("Location", &location)),
                        )
                        .unwrap();
                } else {
                    request
                        .respond(
                            tiny_http::Response::from_data(served.clone())
                                // A body over 32 KB is chunked by default, which has no length.
                                .with_chunked_threshold(usize::MAX)
                                .with_header(header("Content-Encoding", "gzip")),
                        )
                        .unwrap();
                }
            }
            paths
        });

        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let mut entry = entry_template;
        entry.url = format!("http://127.0.0.1:{port}/download");
        let client = reqwest_client::ReqwestClient::new();
        let events = Mutex::new(Vec::new());
        let installed = block_on(install(
            &thread_registry(),
            SERVER,
            &entry,
            &dir,
            &client,
            &|done, total| events.lock().unwrap().push((done, total)),
            &AtomicBool::new(false),
        ))
        .unwrap();
        assert_installed(&dir, &installed, &entry);
        assert_eq!(
            paths.join().unwrap(),
            ["/download", "/kotlin-server.tar.gz"]
        );
        let events = events.into_inner().unwrap();
        let length = archive.len() as u64;
        assert_eq!(events.first(), Some(&(0, Some(length))));
        assert_eq!(events.last(), Some(&(length, Some(length))));
    }

    #[gpui::test]
    fn test_a_start_waiting_on_a_busy_version_lock_can_be_cancelled(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let served = counting(archive);
        let other_process = thread_registry();
        let this_process = Arc::new(LockRegistry::new(Arc::new(cx.executor())));
        let held = other_process
            .try_version_lock(&dir, VERSION)
            .unwrap()
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));

        let task = cx.executor().spawn({
            let (this_process, dir, cancel) = (this_process.clone(), dir.clone(), cancel.clone());
            let client = served.client.clone();
            async move {
                ensure_installed(
                    &this_process,
                    SERVER,
                    &entry,
                    &dir,
                    client.as_ref(),
                    &|_, _| {},
                    &cancel,
                )
                .await
                .map(drop)
            }
        });
        // The wait has no time limit.
        cx.executor().advance_clock(Duration::from_secs(60 * 60));
        cx.run_until_parked();
        let mut task = Some(task);
        assert!(task.as_mut().unwrap().now_or_never().is_none());

        cancel.store(true, Ordering::SeqCst);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        let error = task.take().unwrap().now_or_never().unwrap().unwrap_err();
        assert_eq!(error.to_string(), "install cancelled");
        assert!(error.downcast_ref::<InstallCancelled>().is_some());
        assert_eq!(served.requests.load(Ordering::SeqCst), 0);

        // The cancelled wait freed the registry's mutex: once the other process lets go, this one
        // takes the same lock at once.
        assert!(
            this_process
                .try_version_lock(&dir, VERSION)
                .unwrap()
                .is_none()
        );
        drop(held);
        assert!(
            this_process
                .try_version_lock(&dir, VERSION)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_a_background_install_does_not_wait_for_a_busy_version() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let served = counting(archive);
        let other_process = thread_registry();
        let held = other_process
            .try_version_lock(&dir, VERSION)
            .unwrap()
            .unwrap();
        let background = || {
            block_on(ensure_installed_with(
                InstallMode::Background,
                &thread_registry(),
                SERVER,
                &entry,
                &dir,
                served.client.as_ref(),
                &|_, _| {},
                &AtomicBool::new(false),
            ))
        };
        let error = background().unwrap_err();
        assert!(error.downcast_ref::<VersionBusy>().is_some(), "{error:#}");
        assert_eq!(served.requests.load(Ordering::SeqCst), 0);

        // Free, it installs and leaves the pending-trial marker; a start leaves none.
        drop(held);
        background().unwrap();
        assert_eq!(
            fs::read_to_string(pending_marker_path(&dir, VERSION)).unwrap(),
            entry.sha256
        );
        assert_eq!(names(&dir), [".1.2.3.pending", VERSION]);
    }

    /// A version directory with an install marker last written at `modified`.
    fn version_at(dir: &Path, version: &str, modified: SystemTime) {
        let version_dir = dir.join(version);
        fs::create_dir_all(&version_dir).unwrap();
        let marker = fs::File::create(version_dir.join(MARKER)).unwrap();
        marker.set_modified(modified).unwrap();
    }

    fn build(version: &str) -> Option<BuildId> {
        Some(BuildId {
            version: version.into(),
            sha256: "0".repeat(64),
        })
    }

    #[test]
    fn test_startup_prune_keeps_the_recorded_builds_and_skips_a_busy_version() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let now = SystemTime::now() + Duration::from_secs(60 * 60);
        let old = now - Duration::from_secs(24 * 60 * 60);
        for version in ["1.0", "2.0", "3.0", "4.0", "5.0", "6.0"] {
            version_at(&dir, version, old);
        }
        fs::write(dir.join("stray-file"), "").unwrap();
        let registry = thread_registry();
        let cache = ManifestCache::new(root.path(), registry.clone());
        block_on(cache.with_state(|state| {
            state.kotlin_lsp.active = build("1.0");
            state.kotlin_lsp.previous = build("2.0");
            state.kotlin_lsp.trial = build("3.0");
        }))
        .unwrap();
        let other_process = thread_registry();
        let _busy = other_process
            .try_version_lock(&dir, "6.0")
            .unwrap()
            .unwrap();

        let pruned = block_on(prune(&cache, &registry, &dir, Some("4.0"), now)).unwrap();
        assert_eq!(
            pruned,
            Pruned {
                removed: vec!["5.0".into()],
                busy: vec!["6.0".into()],
            }
        );
        assert_eq!(
            names(&dir),
            ["1.0", "2.0", "3.0", "4.0", "6.0", "stray-file"]
        );
        // Lock files stay, for the pruned version too.
        assert!(dir.join(".5.0.lock").is_file());
        assert!(dir.join(".6.0.lock").is_file());
    }

    #[test]
    fn test_prune_keeps_a_pending_trial_and_a_fresh_install() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let now = SystemTime::now() + Duration::from_secs(60 * 60);
        let old = now - Duration::from_secs(11 * 60);
        version_at(&dir, "7.0", old);
        fs::write(pending_marker_path(&dir, "7.0"), "digest").unwrap();
        version_at(&dir, "8.0", now - Duration::from_secs(9 * 60));
        version_at(&dir, "9.0", old);
        let registry = thread_registry();
        let cache = ManifestCache::new(root.path(), registry.clone());

        let pruned = block_on(prune(&cache, &registry, &dir, None, now)).unwrap();
        assert_eq!(pruned.removed, ["9.0"]);
        assert!(pruned.busy.is_empty());
        assert_eq!(names(&dir), [".7.0.pending", "7.0", "8.0"]);
        // No lock was taken for the kept versions; their locks are free.
        assert!(registry.try_version_lock(&dir, "7.0").unwrap().is_some());
    }

    #[test]
    fn test_prune_removes_the_partial_files_of_older_installs() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        fs::create_dir_all(dir.join(".staging/leftover")).unwrap();
        fs::write(dir.join(".download"), "half").unwrap();
        fs::write(pending_marker_path(&dir, "7.0"), "digest").unwrap();
        fs::write(dir.join(".7.0.download"), "half").unwrap();
        let registry = thread_registry();
        let _lock = registry.try_version_lock(&dir, "7.0").unwrap().unwrap();
        let cache = ManifestCache::new(root.path(), registry.clone());

        // Fresh leftovers may belong to an older bongos still installing: they stay.
        block_on(prune(&cache, &registry, &dir, None, SystemTime::now())).unwrap();
        assert!(dir.join(".download").is_file());
        assert!(dir.join(".staging/leftover").is_dir());

        let later = SystemTime::now() + PRUNE_GRACE + Duration::from_secs(60);
        block_on(prune(&cache, &registry, &dir, None, later)).unwrap();
        assert_eq!(names(&dir), [".7.0.download", ".7.0.pending"]);
        assert!(dir.join(".7.0.lock").is_file());
    }

    #[test]
    fn test_prune_without_a_server_dir_does_nothing() {
        let root = tempfile::tempdir().unwrap();
        let registry = thread_registry();
        let cache = ManifestCache::new(root.path(), registry.clone());
        let pruned = block_on(prune(
            &cache,
            &registry,
            &server_dir(root.path()),
            None,
            SystemTime::now(),
        ))
        .unwrap();
        assert_eq!(pruned, Pruned::default());
    }
}
