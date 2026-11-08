//! Downloads, verifies and unpacks a pinned Kotlin server (decision D6). `install` is the only way
//! an archive reaches the disk as a runnable server, and it only extracts a download whose SHA-256
//! equals the manifest's: a tampered archive is never extracted, let alone run.
//!
//! Layout under `dir` (`<install root>/<server>`): the body streams to `.download` while it is
//! hashed, a verified archive unpacks into `.staging`, and the unpacked root is renamed to
//! `<version>`. Every other entry of `dir` is deleted afterwards, so one version stays. Failure,
//! cancellation and dropping the future all remove `.download` and `.staging`.
//!
//! Callers must not run two installs of one server at the same time; they share `dir`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result, bail, ensure};
use async_compression::futures::bufread::GzipDecoder;
use futures::{AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _, io::BufReader};
use gpui::{App, AppContext as _, Task};
use http_client::HttpClient;
use settings::KotlinServerKind;
use sha2::{Digest as _, Sha256};

use crate::{
    installed::{MARKER, Marker, check_dir, server_name},
    manifest::{ArchiveKind, ManifestEntry},
};

/// Progress is reported each time this many more bytes have arrived.
pub const PROGRESS_STEP: u64 = 1024 * 1024;

const DOWNLOAD_FILE: &str = ".download";
const STAGING_DIR: &str = ".staging";
const CHUNK_SIZE: usize = 64 * 1024;

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

/// An archive whose digest matched the manifest. Only `download_verified` makes one, and the
/// extractors take nothing else.
struct VerifiedArchive(PathBuf);

/// Removes the install's partial files when dropped: on success there are none left, on an error,
/// a cancel or a dropped future they are the partial install.
struct PartialFiles {
    paths: Vec<PathBuf>,
}

impl PartialFiles {
    fn new(dir: &Path) -> Self {
        let this = Self {
            paths: vec![dir.join(DOWNLOAD_FILE), dir.join(STAGING_DIR)],
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

/// Starts `install` on the background executor. Dropping the task cancels the install and removes
/// its partial files; setting `cancel` stops it at the next chunk.
pub fn spawn_install(
    server: KotlinServerKind,
    entry: ManifestEntry,
    dir: PathBuf,
    http: Arc<dyn HttpClient>,
    progress: Arc<ProgressFn>,
    cancel: Arc<AtomicBool>,
    cx: &App,
) -> Task<Result<InstalledServer>> {
    cx.background_spawn(async move {
        install(
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

/// Installs the pinned build `entry` of `server` under `dir`.
pub async fn install(
    server: KotlinServerKind,
    entry: &ManifestEntry,
    dir: &Path,
    http: &dyn HttpClient,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<InstalledServer> {
    check_cancel(cancel)?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let _partial = PartialFiles::new(dir);

    let archive =
        download_verified(entry, &dir.join(DOWNLOAD_FILE), http, progress, cancel).await?;
    check_cancel(cancel)?;

    let staging = dir.join(STAGING_DIR);
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
    remove_other_versions(dir, &entry.version);

    ensure!(
        check_dir(&final_dir, entry).is_some(),
        "{} did not install correctly",
        server_name(server)
    );
    Ok(InstalledServer {
        server,
        version: entry.version.clone(),
        launcher: final_dir.join(&entry.launcher),
        dir: final_dir,
    })
}

/// Streams the archive to `path`, hashing as it goes, and returns it only when the digest matches.
async fn download_verified(
    entry: &ManifestEntry,
    path: &Path,
    http: &dyn HttpClient,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<VerifiedArchive> {
    let url = &entry.url;
    let mut response = http
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("downloading {url}"))?;
    ensure!(
        response.status().is_success(),
        "downloading {url}: the server answered {}",
        response.status()
    );
    let total = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    let mut file = async_fs::File::create(path)
        .await
        .with_context(|| format!("creating {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let (mut downloaded, mut reported) = (0u64, 0u64);
    progress(0, total);
    loop {
        check_cancel(cancel)?;
        let read = response
            .body_mut()
            .read(&mut buffer)
            .await
            .with_context(|| format!("downloading {url}"))?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        hasher.update(chunk);
        file.write_all(chunk)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        downloaded += read as u64;
        if downloaded - reported >= PROGRESS_STEP {
            reported = downloaded;
            progress(downloaded, total);
        }
    }
    file.flush()
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    drop(file);
    if reported != downloaded {
        progress(downloaded, total);
    }

    let actual = format!("{:x}", hasher.finalize());
    ensure!(
        actual == entry.sha256,
        "{url} failed its SHA-256 check: expected {}, got {actual}",
        entry.sha256
    );
    Ok(VerifiedArchive(path.to_path_buf()))
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
        ArchiveKind::Zip => util::archive::extract_seekable_zip(staging, file).await,
    }
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

/// Deletes every entry of `dir` except `keep`. Best effort: a version that cannot be removed is
/// logged and left for the next install.
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
            && let Err(error) = remove_path(&entry.path())
        {
            log::warn!(
                "removing the old version {}: {error}",
                entry.path().display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        pin::Pin,
        sync::Mutex,
        task::{Context, Poll},
    };

    use futures::{AsyncRead, executor::block_on, io::Cursor};
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};

    use super::*;
    use crate::installed::check_installed;

    const SERVER: KotlinServerKind = KotlinServerKind::KotlinLsp;
    const VERSION: &str = "1.2.3";
    const LAUNCHER_BODY: &[u8] = b"#!/bin/sh\nexit 0\n";

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Bytes that do not compress, so the archive is about as large as the payload.
    fn noise(length: usize) -> Vec<u8> {
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

    fn files(noise_length: usize, with_launcher: bool) -> Vec<(String, u32, Vec<u8>)> {
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

    fn tar_gz(files: &[(String, u32, Vec<u8>)]) -> Vec<u8> {
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

    fn zip(files: &[(String, u32, Vec<u8>)]) -> Vec<u8> {
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

    fn entry_for(archive: &[u8], kind: ArchiveKind) -> ManifestEntry {
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

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
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
    fn test_other_versions_and_stale_partials_are_deleted() {
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        fs::create_dir_all(dir.join("0.9/bin")).unwrap();
        fs::write(dir.join("0.9/bin/launcher"), "old").unwrap();
        fs::create_dir_all(dir.join(".staging/leftover")).unwrap();
        fs::write(dir.join(".download"), "half").unwrap();

        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let run = run_install(
            &serving(archive, None),
            &entry,
            &dir,
            &AtomicBool::new(false),
        );
        assert_installed(&dir, &run.result.unwrap(), &entry);
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
            let mut future = Box::pin(install(
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
                if dir.join(DOWNLOAD_FILE).exists() && *polls.lock().unwrap() > 0 {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            assert!(dir.join(DOWNLOAD_FILE).exists());
            drop(future);
        });
        assert!(names(&dir).is_empty(), "left behind: {:?}", names(&dir));
    }

    #[gpui::test]
    async fn test_spawn_install_runs_in_the_background(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let dir = server_dir(root.path());
        let archive = tar_gz(&files(0, true));
        let entry = entry_for(&archive, ArchiveKind::TarGz);
        let client: Arc<dyn HttpClient> = serving(archive, None);
        let installed = cx
            .update(|cx| {
                spawn_install(
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
}
