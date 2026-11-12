use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

use anyhow::{Context, Result};
use async_compression::futures::bufread::{BzDecoder, GzipDecoder};
use futures::{
    AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt, io::BufReader,
};
use sha2::{Digest, Sha256};

use crate::{HttpClient, github::AssetKind};

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct GithubBinaryMetadata {
    pub metadata_version: u64,
    pub digest: Option<String>,
}

impl GithubBinaryMetadata {
    pub async fn read_from_file(metadata_path: &Path) -> Result<GithubBinaryMetadata> {
        let metadata_content = async_fs::read_to_string(metadata_path)
            .await
            .with_context(|| format!("reading metadata file at {metadata_path:?}"))?;
        serde_json::from_str(&metadata_content)
            .with_context(|| format!("parsing metadata file at {metadata_path:?}"))
    }

    pub async fn write_to_file(&self, metadata_path: &Path) -> Result<()> {
        let metadata_content = serde_json::to_string(self)
            .with_context(|| format!("serializing metadata for {metadata_path:?}"))?;
        async_fs::write(metadata_path, metadata_content.as_bytes())
            .await
            .with_context(|| format!("writing metadata file at {metadata_path:?}"))?;
        Ok(())
    }
}

pub async fn download_server_binary(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<(), anyhow::Error> {
    log::info!("downloading github artifact from {url}");
    let Some(destination_parent) = destination_path.parent() else {
        anyhow::bail!("destination path has no parent: {destination_path:?}");
    };

    let staging_path = staging_path(destination_parent, asset_kind)?;
    let mut response = http_client
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("downloading release from {url}"))?;
    let body = response.body_mut();

    if let Err(err) = extract_to_staging(body, digest, url, &staging_path, asset_kind).await {
        cleanup_staging_path(&staging_path, asset_kind).await;
        return Err(err);
    }

    if let Err(err) = finalize_download(&staging_path, destination_path).await {
        cleanup_staging_path(&staging_path, asset_kind).await;
        return Err(err);
    }

    Ok(())
}

pub async fn download_server_raw_binary(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    binary_file_name: &str,
) -> Result<(), anyhow::Error> {
    log::info!("downloading raw binary from {url}");
    let Some(destination_parent) = destination_path.parent() else {
        anyhow::bail!("destination path has no parent: {destination_path:?}");
    };

    let staging_path = staging_dir_path(destination_parent)?;
    let result = async {
        let mut response = http_client
            .get(url, Default::default(), true)
            .await
            .with_context(|| format!("downloading release from {url}"))?;

        let binary_path = staging_path.join(binary_file_name);
        let mut writer = HashingWriter {
            writer: async_fs::File::create(&binary_path)
                .await
                .with_context(|| format!("creating a file {binary_path:?} for {url}"))?,
            hasher: Sha256::new(),
        };
        futures::io::copy(&mut BufReader::new(response.body_mut()), &mut writer)
            .await
            .with_context(|| format!("saving binary contents from {url}"))?;
        let asset_sha_256 = writer
            .finish()
            .await
            .with_context(|| format!("flushing binary contents for {url}"))?;

        if let Some(expected_sha_256) = digest {
            anyhow::ensure!(
                asset_sha_256 == expected_sha_256,
                "{url} asset got SHA-256 mismatch. Expected: {expected_sha_256}, Got: {asset_sha_256}",
            );
        }

        util::fs::make_file_executable(&binary_path)
            .await
            .with_context(|| format!("marking {binary_path:?} as executable"))?;
        finalize_download(&staging_path, destination_path).await
    }
    .await;

    if let Err(err) = result {
        if let Err(err) = async_fs::remove_dir_all(&staging_path).await {
            log::warn!("failed to remove staging directory {staging_path:?}: {err:?}");
        }
        return Err(err);
    }

    Ok(())
}

/// `download_verified_file` reports progress each time this many more bytes have arrived.
pub const VERIFIED_DOWNLOAD_PROGRESS_STEP: u64 = 1024 * 1024;

/// `download_verified_file` checks its cancel flag before reading each chunk of this size.
const VERIFIED_DOWNLOAD_CHUNK_SIZE: usize = 64 * 1024;

/// The error of a `download_verified_file` stopped by its cancel flag.
#[derive(Debug)]
pub struct DownloadCancelled;

impl std::fmt::Display for DownloadCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("download cancelled")
    }
}

impl std::error::Error for DownloadCancelled {}

/// Deletes the file at its path when dropped, unless disarmed.
struct RemoveOnDrop<'a> {
    path: &'a Path,
    armed: bool,
}

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = std::fs::remove_file(self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("removing {}: {error}", self.path.display());
        }
    }
}

/// Streams the body of `url` to `path` while hashing it, and keeps the file only when its SHA-256
/// equals `expected_sha256`. `progress` gets the bytes downloaded and the `Content-Length` total
/// (when the server sent one): once at the start, every `VERIFIED_DOWNLOAD_PROGRESS_STEP` bytes,
/// and at the end of the body. `cancel` is checked between chunks; a cancel returns
/// `DownloadCancelled`. A digest mismatch, any error, a cancel or dropping the future deletes `path`.
pub async fn download_verified_file(
    http: &dyn HttpClient,
    url: &str,
    expected_sha256: &str,
    path: &Path,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<()> {
    let mut guard = RemoveOnDrop { path, armed: true };
    download_and_hash(http, url, expected_sha256, path, progress, cancel).await?;
    guard.armed = false;
    Ok(())
}

async fn download_and_hash(
    http: &dyn HttpClient,
    url: &str,
    expected_sha256: &str,
    path: &Path,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<()> {
    let check_cancel = || {
        if cancel.load(Ordering::Relaxed) {
            Err(anyhow::Error::new(DownloadCancelled))
        } else {
            Ok(())
        }
    };
    check_cancel()?;
    let mut response = http
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("downloading {url}"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "downloading {url}: the server answered {}",
        response.status()
    );
    let total = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    let mut file = async_fs::File::create(path)
        .await
        .with_context(|| format!("creating {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; VERIFIED_DOWNLOAD_CHUNK_SIZE];
    let (mut downloaded, mut reported) = (0u64, 0u64);
    progress(0, total);
    loop {
        check_cancel()?;
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
        if downloaded - reported >= VERIFIED_DOWNLOAD_PROGRESS_STEP {
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
    anyhow::ensure!(
        actual == expected_sha256,
        "{url} failed its SHA-256 check: expected {expected_sha256}, got {actual}",
    );
    Ok(())
}

async fn extract_to_staging(
    body: impl AsyncRead + Unpin,
    digest: Option<&str>,
    url: &str,
    staging_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    match digest {
        Some(expected_sha_256) => {
            let temp_asset_file = tempfile::NamedTempFile::new()
                .with_context(|| format!("creating a temporary file for {url}"))?;
            let (temp_asset_file, _temp_guard) = temp_asset_file.into_parts();
            let mut writer = HashingWriter {
                writer: async_fs::File::from(temp_asset_file),
                hasher: Sha256::new(),
            };
            futures::io::copy(&mut BufReader::new(body), &mut writer)
                .await
                .with_context(|| {
                    format!("saving archive contents into the temporary file for {url}")
                })?;
            let asset_sha_256 = format!("{:x}", writer.hasher.finalize());

            anyhow::ensure!(
                asset_sha_256 == expected_sha_256,
                "{url} asset got SHA-256 mismatch. Expected: {expected_sha_256}, Got: {asset_sha_256}",
            );
            writer
                .writer
                .seek(std::io::SeekFrom::Start(0))
                .await
                .with_context(|| format!("seeking temporary file for {url}"))?;
            stream_file_archive(&mut writer.writer, url, staging_path, asset_kind)
                .await
                .with_context(|| {
                    format!("extracting downloaded asset for {url} into {staging_path:?}")
                })?;
        }
        None => {
            stream_response_archive(body, url, staging_path, asset_kind)
                .await
                .with_context(|| {
                    format!("extracting response for asset {url} into {staging_path:?}")
                })?;
        }
    }
    Ok(())
}

fn staging_dir_path(parent: &Path) -> Result<PathBuf> {
    let dir = tempfile::Builder::new()
        .prefix(".tmp-github-download-")
        .tempdir_in(parent)
        .with_context(|| format!("creating staging directory in {parent:?}"))?;
    Ok(dir.keep())
}

fn staging_path(parent: &Path, asset_kind: AssetKind) -> Result<PathBuf> {
    match asset_kind {
        AssetKind::TarGz | AssetKind::TarBz2 | AssetKind::Zip => staging_dir_path(parent),
        AssetKind::Gz => {
            let path = tempfile::Builder::new()
                .prefix(".tmp-github-download-")
                .tempfile_in(parent)
                .with_context(|| format!("creating staging file in {parent:?}"))?
                .into_temp_path()
                .keep()
                .with_context(|| format!("persisting staging file in {parent:?}"))?;
            Ok(path)
        }
    }
}

async fn cleanup_staging_path(staging_path: &Path, asset_kind: AssetKind) {
    match asset_kind {
        AssetKind::TarGz | AssetKind::TarBz2 | AssetKind::Zip => {
            if let Err(err) = async_fs::remove_dir_all(staging_path).await {
                log::warn!("failed to remove staging directory {staging_path:?}: {err:?}");
            }
        }
        AssetKind::Gz => {
            if let Err(err) = async_fs::remove_file(staging_path).await {
                log::warn!("failed to remove staging file {staging_path:?}: {err:?}");
            }
        }
    }
}

async fn finalize_download(staging_path: &Path, destination_path: &Path) -> Result<()> {
    _ = async_fs::remove_dir_all(destination_path).await;
    async_fs::rename(staging_path, destination_path)
        .await
        .with_context(|| format!("renaming {staging_path:?} to {destination_path:?}"))?;
    Ok(())
}

async fn stream_response_archive(
    response: impl AsyncRead + Unpin,
    url: &str,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    match asset_kind {
        AssetKind::TarGz => extract_tar_gz(destination_path, url, response).await?,
        AssetKind::TarBz2 => extract_tar_bz2(destination_path, url, response).await?,
        AssetKind::Gz => extract_gz(destination_path, url, response).await?,
        AssetKind::Zip => {
            util::archive::extract_zip(destination_path, response).await?;
        }
    };
    Ok(())
}

async fn stream_file_archive(
    file_archive: impl AsyncRead + AsyncSeek + Unpin,
    url: &str,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    match asset_kind {
        AssetKind::TarGz => extract_tar_gz(destination_path, url, file_archive).await?,
        AssetKind::TarBz2 => extract_tar_bz2(destination_path, url, file_archive).await?,
        AssetKind::Gz => extract_gz(destination_path, url, file_archive).await?,
        #[cfg(not(windows))]
        AssetKind::Zip => {
            util::archive::extract_seekable_zip(destination_path, file_archive).await?;
        }
        #[cfg(windows)]
        AssetKind::Zip => {
            util::archive::extract_zip(destination_path, file_archive).await?;
        }
    };
    Ok(())
}

async fn extract_tar_gz(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let decompressed_bytes = GzipDecoder::new(BufReader::new(from));
    unpack_tar_archive(destination_path, url, decompressed_bytes).await?;
    Ok(())
}

async fn extract_tar_bz2(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let decompressed_bytes = BzDecoder::new(BufReader::new(from));
    unpack_tar_archive(destination_path, url, decompressed_bytes).await?;
    Ok(())
}

async fn unpack_tar_archive(
    destination_path: &Path,
    url: &str,
    archive_bytes: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    // We don't need to set the modified time. It's irrelevant to downloaded
    // archive verification, and some filesystems return errors when asked to
    // apply it after extraction.
    let archive = async_tar::ArchiveBuilder::new(archive_bytes)
        .set_preserve_mtime(false)
        .build();
    archive
        .unpack(&destination_path)
        .await
        .with_context(|| format!("extracting {url} to {destination_path:?}"))?;
    Ok(())
}

async fn extract_gz(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let mut decompressed_bytes = GzipDecoder::new(BufReader::new(from));
    let mut file = async_fs::File::create(&destination_path)
        .await
        .with_context(|| {
            format!("creating a file {destination_path:?} for a download from {url}")
        })?;
    futures::io::copy(&mut decompressed_bytes, &mut file)
        .await
        .with_context(|| format!("extracting {url} to {destination_path:?}"))?;
    Ok(())
}

struct HashingWriter<W: AsyncWrite + Unpin> {
    writer: W,
    hasher: Sha256,
}

impl<W: AsyncWrite + Unpin> HashingWriter<W> {
    /// Closes and drops the inner writer, returning the hex SHA-256 digest of
    /// everything written.
    ///
    /// Taking `self` by value guarantees the writer is dropped before this
    /// returns. For file writers this releases the OS handle, which Windows
    /// requires before an ancestor directory can be renamed or deleted; note
    /// that closing alone is not enough, as `async_fs::File` holds its handle
    /// until dropped.
    async fn finish(mut self) -> std::io::Result<String> {
        self.writer.close().await?;
        drop(self.writer);
        Ok(format!("{:x}", self.hasher.finalize()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for HashingWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::result::Result<usize, std::io::Error>> {
        match Pin::new(&mut self.writer).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.hasher.update(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::result::Result<(), std::io::Error>> {
        Pin::new(&mut self.writer).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AsyncBody, Response};
    use futures::future::BoxFuture;
    use http::HeaderValue;
    use url::Url;

    struct StaticResponseClient {
        body: Vec<u8>,
    }

    impl HttpClient for StaticResponseClient {
        fn send(
            &self,
            _req: http::Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let body = self.body.clone();
            Box::pin(async move {
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body))
                    .unwrap())
            })
        }

        fn user_agent(&self) -> Option<&HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }
    }

    /// Serves its bytes in reads of at most 16 KiB and raises `cancel` once `cancel_at` bytes went
    /// out.
    struct Trickle {
        data: futures::io::Cursor<Vec<u8>>,
        sent: usize,
        cancel: Option<(usize, std::sync::Arc<AtomicBool>)>,
    }

    impl AsyncRead for Trickle {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
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

    struct TrickleClient {
        body: Vec<u8>,
        cancel: Option<(usize, std::sync::Arc<AtomicBool>)>,
    }

    impl HttpClient for TrickleClient {
        fn send(
            &self,
            _req: http::Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let body = self.body.clone();
            let cancel = self.cancel.clone();
            Box::pin(async move {
                Ok(Response::builder()
                    .status(200)
                    .header("Content-Length", body.len())
                    .body(AsyncBody::from_reader(Trickle {
                        data: futures::io::Cursor::new(body),
                        sent: 0,
                        cancel,
                    }))
                    .unwrap())
            })
        }

        fn user_agent(&self) -> Option<&HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }
    }

    /// Bytes that look random, so no layer can shrink them.
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

    const URL: &str = "https://example.com/server.tar.gz";

    struct VerifiedRun {
        result: Result<()>,
        events: Vec<(u64, Option<u64>)>,
    }

    fn run_verified(
        client: &TrickleClient,
        expected_sha256: &str,
        path: &Path,
        cancel: &AtomicBool,
    ) -> VerifiedRun {
        let events = std::sync::Mutex::new(Vec::new());
        let record = |done, total| events.lock().unwrap().push((done, total));
        let result = futures::executor::block_on(download_verified_file(
            client,
            URL,
            expected_sha256,
            path,
            &record,
            cancel,
        ));
        VerifiedRun {
            result,
            events: events.into_inner().unwrap(),
        }
    }

    #[test]
    fn verified_download_keeps_a_file_whose_digest_matches() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join(".download");
        let body = noise(100 * 1024);
        let client = TrickleClient {
            body: body.clone(),
            cancel: None,
        };
        let run = run_verified(
            &client,
            &format!("{:x}", Sha256::digest(&body)),
            &path,
            &AtomicBool::new(false),
        );
        run.result.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }

    #[test]
    fn verified_download_deletes_a_file_whose_digest_differs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join(".download");
        let body = noise(100 * 1024);
        let expected = format!("{:x}", Sha256::digest(b"other bytes"));
        let client = TrickleClient {
            body: body.clone(),
            cancel: None,
        };
        let run = run_verified(&client, &expected, &path, &AtomicBool::new(false));
        let message = run.result.unwrap_err().to_string();
        assert!(message.contains(URL), "{message}");
        assert!(message.contains(&expected), "{message}");
        assert!(
            message.contains(&format!("{:x}", Sha256::digest(&body))),
            "{message}"
        );
        assert!(!path.exists(), "the unverified file was kept");
    }

    #[test]
    fn verified_download_cancelled_mid_body_deletes_the_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join(".download");
        let body = noise(3 * 1024 * 1024);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let client = TrickleClient {
            body: body.clone(),
            cancel: Some((VERIFIED_DOWNLOAD_PROGRESS_STEP as usize, cancel.clone())),
        };
        let run = run_verified(
            &client,
            &format!("{:x}", Sha256::digest(&body)),
            &path,
            &cancel,
        );
        let error = run.result.unwrap_err();
        assert!(
            error.downcast_ref::<DownloadCancelled>().is_some(),
            "{error:#}"
        );
        let last = run.events.last().unwrap().0;
        assert!(last < body.len() as u64, "the whole body arrived");
        assert!(!path.exists(), "the partial file was kept");
    }

    #[test]
    fn verified_download_progress_ends_at_the_size_in_steps_of_one_megabyte() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join(".download");
        let body = noise(3 * 1024 * 1024 + 123);
        let size = body.len() as u64;
        let client = TrickleClient {
            body: body.clone(),
            cancel: None,
        };
        let run = run_verified(
            &client,
            &format!("{:x}", Sha256::digest(&body)),
            &path,
            &AtomicBool::new(false),
        );
        run.result.unwrap();

        assert_eq!(run.events.first(), Some(&(0, Some(size))));
        assert_eq!(run.events.last(), Some(&(size, Some(size))));
        assert!(run.events.iter().all(|(_, total)| *total == Some(size)));
        let done: Vec<u64> = run.events.iter().map(|(done, _)| *done).collect();
        assert!(done.windows(2).all(|pair| pair[1] > pair[0]), "{done:?}");
        // Every event but the last is at least a step after the one before it.
        let middle = &done[..done.len() - 1];
        assert!(
            middle
                .windows(2)
                .all(|pair| pair[1] - pair[0] >= VERIFIED_DOWNLOAD_PROGRESS_STEP),
            "{done:?}"
        );
        assert!(done.len() >= 4, "{done:?}");
    }

    #[test]
    fn downloads_raw_binary_into_destination_dir() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let destination_path = temp_dir.path().join("v_1");
            let contents = b"#!/bin/sh\necho hello\n".to_vec();
            let expected_sha_256 = format!("{:x}", Sha256::digest(&contents));
            let client = StaticResponseClient { body: contents };

            download_server_raw_binary(
                &client,
                "https://example.com/agent-binary",
                Some(&expected_sha_256),
                &destination_path,
                "agent-binary",
            )
            .await
            .unwrap();

            let binary_path = destination_path.join("agent-binary");
            assert_eq!(
                std::fs::read(&binary_path).unwrap(),
                b"#!/bin/sh\necho hello\n"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&binary_path)
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o111, 0o111, "binary should be executable");
            }
        });
    }

    #[test]
    fn raw_binary_digest_mismatch_cleans_up_staging() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let destination_path = temp_dir.path().join("v_1");
            let client = StaticResponseClient {
                body: b"some binary".to_vec(),
            };

            let error = download_server_raw_binary(
                &client,
                "https://example.com/agent-binary",
                Some("0000000000000000000000000000000000000000000000000000000000000000"),
                &destination_path,
                "agent-binary",
            )
            .await
            .unwrap_err();

            assert!(error.to_string().contains("SHA-256 mismatch"));
            assert!(!destination_path.exists());
            let leftover_entries = std::fs::read_dir(temp_dir.path()).unwrap().count();
            assert_eq!(leftover_entries, 0, "staging directory should be removed");
        });
    }
}
