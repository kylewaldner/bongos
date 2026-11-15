//! Installs the fake jdtls and terraform-ls archives that `script/bongos/fixtures/kotlin-servers/
//! make-archives` builds, through the pinned-server API, from a fake loopback host: the genuine
//! archives install and run the fake server, and the tampered copies are refused before anything
//! is extracted.
#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use bongos_kotlin::{
    LockClock, LockRegistry, Manifest,
    pinned::{self, JDTLS, PinnedServer, TERRAFORM_LS},
};
use futures::{executor::block_on, future::BoxFuture};
use http_client::{AsyncBody, FakeHttpClient, HttpClientWithUrl, Response};

const FAKE_SERVER: &str = env!("CARGO_BIN_EXE_bongos-fake-kotlin-server");

struct NoWait;

impl LockClock for NoWait {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        Box::pin(futures::future::ready(()))
    }
}

/// Runs make-archives into `out` and returns the servers.toml it wrote.
fn make_archives(out: &Path) -> PathBuf {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../script/bongos/fixtures/kotlin-servers/make-archives");
    let output = Command::new("bash")
        .arg(script)
        .arg(out)
        .arg(FAKE_SERVER)
        .arg("9")
        .env("TMPDIR", out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "make-archives failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    out.join("servers.toml")
}

/// Serves `<dir>/<file>` for `http://127.0.0.1:9/<file>`, or `<dir>/<renamed>` where `renames`
/// maps the file to another one.
fn serving(dir: &Path, renames: BTreeMap<&'static str, &'static str>) -> Arc<HttpClientWithUrl> {
    let dir = dir.to_path_buf();
    FakeHttpClient::create(move |request| {
        let name = request.uri().path().trim_start_matches('/').to_string();
        let name = renames
            .get(name.as_str())
            .map(|renamed| renamed.to_string())
            .unwrap_or(name);
        let bytes = fs::read(dir.join(name)).unwrap();
        async move {
            Ok(Response::builder()
                .status(200)
                .body(AsyncBody::from(bytes))
                .unwrap())
        }
    })
}

fn install(
    client: &HttpClientWithUrl,
    manifest: &Manifest,
    server: PinnedServer,
    languages: &Path,
) -> anyhow::Result<pinned::PinnedInstall> {
    let entry = pinned::entry(manifest, server.name, "linux-x64")?;
    let locks = LockRegistry::new(Arc::new(NoWait));
    block_on(pinned::ensure_installed(
        &locks,
        server,
        &entry,
        languages,
        client,
        &|_, _| {},
        &AtomicBool::new(false),
    ))
}

/// The files under `dir`, lock files left out.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if !path.to_string_lossy().ends_with(".lock") {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn test_fixture_archives_install_and_tampered_copies_are_refused() {
    let fixtures = tempfile::tempdir().unwrap();
    let manifest_path = make_archives(fixtures.path());
    let manifest = Manifest::parse(&fs::read_to_string(manifest_path).unwrap()).unwrap();

    for (server, archive, tampered) in [
        (JDTLS, "jdtls.tar.gz", "jdtls.tampered.tar.gz"),
        (TERRAFORM_LS, "terraform-ls.zip", "terraform-ls.tampered.zip"),
    ] {
        let entry = pinned::entry(&manifest, server.name, "linux-x64").unwrap();
        assert_eq!(entry.version, "0.0.2");
        assert_eq!(entry.expires, None);

        let languages = tempfile::tempdir().unwrap();
        let bad = serving(fixtures.path(), BTreeMap::from([(archive, tampered)]));
        let error = install(&bad, &manifest, server, languages.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains(&entry.sha256),
            "{}: {error:#}",
            server.name
        );
        assert!(
            files_under(languages.path()).is_empty(),
            "{}: left behind {:?}",
            server.name,
            files_under(languages.path())
        );
        assert!(pinned::check_installed(server, &entry, languages.path()).is_none());

        let good = serving(fixtures.path(), BTreeMap::new());
        let installed = install(&good, &manifest, server, languages.path()).unwrap();
        assert_eq!(
            installed.dir,
            languages
                .path()
                .join(server.install_subdir)
                .join(&entry.version)
        );
        let binary = pinned::check_installed(server, &entry, languages.path()).unwrap();
        assert_eq!(binary.path, installed.binary.path);
        // The launcher runs the fake server.
        let launcher = fs::read_to_string(&binary.path).unwrap();
        assert!(launcher.contains(FAKE_SERVER), "{launcher}");
    }

    let terraform = pinned::entry(&manifest, "terraform-ls", "linux-x64").unwrap();
    assert_eq!(terraform.args, ["serve"]);
    assert_eq!(
        terraform.env.get("CHECKPOINT_DISABLE").map(String::as_str),
        Some("1")
    );
}
