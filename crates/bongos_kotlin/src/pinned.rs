//! Language servers other than Kotlin's that bongos pins in `servers.toml`, looked up by name:
//! jdtls (Java) and terraform-ls (Terraform). They use the Kotlin servers' manifest schema, user
//! file and installer: an archive is checked against the pinned SHA-256 before anything is
//! extracted (zip and tar.gz alike), the version directory appears by one rename, and a failure or
//! a cancel leaves no partial files. These servers never expire (their pins have no `expires`, and
//! nothing here reads one) and the remote manifest never names them.
//!
//! A server installs to `<languages dir>/<install_subdir>/<version>`; the app passes
//! `paths::languages_dir()`.

use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use anyhow::{Result, anyhow};
use http_client::HttpClient;
use lsp::LanguageServerBinary;

use crate::{
    install::{InstallMode, ensure_in},
    installed::check_dir,
    locks::LockRegistry,
    manifest::{Manifest, ManifestEntry},
};

/// A pinned server: its table name in `servers.toml` and where it installs under the languages
/// directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedServer {
    pub name: &'static str,
    pub install_subdir: &'static str,
}

/// The Eclipse JDT Language Server. One archive serves every platform; it needs a JDK to run.
pub const JDTLS: PinnedServer = PinnedServer {
    name: "jdtls",
    install_subdir: "java/jdtls",
};

/// HashiCorp's Terraform language server, one binary per platform.
pub const TERRAFORM_LS: PinnedServer = PinnedServer {
    name: "terraform-ls",
    install_subdir: "terraform/terraform-ls",
};

/// Every server this module installs.
pub const ALL: [PinnedServer; 2] = [JDTLS, TERRAFORM_LS];

impl PinnedServer {
    pub fn by_name(name: &str) -> Option<Self> {
        ALL.into_iter().find(|server| server.name == name)
    }

    /// `<languages_dir>/<install_subdir>`: holds one directory per installed version.
    pub fn install_dir(&self, languages_dir: &Path) -> PathBuf {
        languages_dir.join(self.install_subdir)
    }

    /// `<languages_dir>/<install_subdir>/<version>`.
    pub fn version_dir(&self, languages_dir: &Path, version: &str) -> PathBuf {
        self.install_dir(languages_dir).join(version)
    }
}

/// The pin of the server `name` for `platform` (`current_platform()` in the app) in `manifest`,
/// which is the embedded one merged with the user file in the app. An error for a name this module
/// does not know, a server the manifest does not pin, or a platform it has no build for.
pub fn entry(manifest: &Manifest, name: &str, platform: &str) -> Result<ManifestEntry> {
    let server =
        PinnedServer::by_name(name).ok_or_else(|| anyhow!("{name} is not a pinned server"))?;
    manifest.named_entry(server.name, platform)
}

/// The command for the pinned build of `server`, or `None` when it is not installed: no install
/// marker naming the pinned version and digest, or a launcher that is missing or not executable.
/// It reads only the disk, never the network.
pub fn check_installed(
    server: PinnedServer,
    entry: &ManifestEntry,
    languages_dir: &Path,
) -> Option<LanguageServerBinary> {
    check_dir(&server.version_dir(languages_dir, &entry.version), entry)
}

/// An installed pinned server.
#[derive(Debug)]
pub struct PinnedInstall {
    pub server: PinnedServer,
    pub version: String,
    /// `<languages_dir>/<install_subdir>/<version>`.
    pub dir: PathBuf,
    /// The launcher with the pin's arguments and environment.
    pub binary: LanguageServerBinary,
}

/// The pinned build of `server`, downloading and installing it first when it is missing. An
/// installed build costs no network. Other installed versions are kept. Setting `cancel` (or
/// dropping the future) stops the install and removes its partial files; the error is then
/// `InstallCancelled`.
pub async fn ensure_installed(
    locks: &LockRegistry,
    server: PinnedServer,
    entry: &ManifestEntry,
    languages_dir: &Path,
    http: &dyn HttpClient,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<PinnedInstall> {
    let dir = ensure_in(
        InstallMode::Start,
        locks,
        server.name,
        false,
        entry,
        &server.install_dir(languages_dir),
        http,
        progress,
        cancel,
    )
    .await?;
    let binary = check_dir(&dir, entry)
        .ok_or_else(|| anyhow!("{} did not install correctly", server.name))?;
    Ok(PinnedInstall {
        server,
        version: entry.version.clone(),
        dir,
        binary,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::OsString,
        fs,
        sync::Arc,
    };

    use futures::executor::block_on;
    use http_client::FakeHttpClient;

    use super::*;
    use crate::{
        install::{
            InstallCancelled,
            tests::{Client, entry_for, files, serving, sha256, tar_gz, zip},
        },
        locks::tests::thread_registry,
        manifest::{ArchiveKind, EMBEDDED, USER_FILE_NAME},
    };

    const PLATFORMS: [&str; 4] = ["linux-x64", "linux-arm64", "macos-x64", "macos-arm64"];

    #[test]
    fn test_jdtls_and_terraform_ls_pins_parse_and_never_expire() {
        // The raw tables, so a pin that gains `built` or `expires` fails here even if the parsed
        // entry ignored it.
        let raw: toml::Table = toml::from_str(EMBEDDED).unwrap();
        for server in ALL {
            let table = raw[server.name].as_table().unwrap();
            assert!(!table.contains_key("expires"), "{} expires", server.name);
            assert!(!table.contains_key("built"), "{} has a build date", server.name);
        }

        let manifest = Manifest::embedded();
        let jdtls_url = "https://download.eclipse.org/jdtls/milestones/1.63.0/jdt-language-server-1.63.0-202610191322.tar.gz";
        for platform in PLATFORMS {
            let jdtls = entry(&manifest, "jdtls", platform).unwrap();
            assert_eq!(jdtls.version, "1.63.0");
            assert_eq!(jdtls.url, jdtls_url, "one archive for every platform");
            assert_eq!(
                jdtls.sha256,
                "b1f2abe0b3cccb462cf64fb4a5587c7d09df60cf8be7748b152cbd89d018bdc4"
            );
            assert_eq!(jdtls.archive, ArchiveKind::TarGz);
            assert_eq!(jdtls.strip_components, 0);
            assert_eq!(jdtls.launcher, "bin/jdtls");
            assert_eq!((jdtls.built, jdtls.expires), (None, None));

            let terraform = entry(&manifest, "terraform-ls", platform).unwrap();
            assert_eq!(terraform.version, "0.39.1");
            assert!(
                terraform
                    .url
                    .starts_with("https://releases.hashicorp.com/terraform-ls/0.39.1/terraform-ls_0.39.1_"),
                "{}",
                terraform.url
            );
            assert_eq!(terraform.archive, ArchiveKind::Zip);
            assert_eq!(terraform.strip_components, 0);
            assert_eq!(terraform.launcher, "terraform-ls");
            assert_eq!(terraform.args, ["serve"]);
            assert_eq!(
                terraform.env,
                BTreeMap::from([("CHECKPOINT_DISABLE".to_string(), "1".to_string())])
            );
            assert_eq!((terraform.built, terraform.expires), (None, None));
        }
    }

    #[test]
    fn test_entry_by_name_for_every_platform() {
        let manifest = Manifest::embedded();
        let targets = [
            ("linux-x64", "linux_amd64", "fe2a158e4a1a8ec8047dae9e06c439b468412aa579929ba28b2e1258cedef9c0"),
            ("linux-arm64", "linux_arm64", "a72a6d9f72580cfc30cc06d3c973fd5f55790f62e3ddb195998435070d9af36d"),
            ("macos-x64", "darwin_amd64", "25814afdbec9163de4ad8db58960195b9fc65ddc5405a5df7163bbdbd08b5325"),
            ("macos-arm64", "darwin_arm64", "07748aa989fc3b9f6e4538ff83bc2e607ebc20c7587ce6556ae555d631e70b1b"),
        ];
        for (platform, target, digest) in targets {
            let terraform = entry(&manifest, TERRAFORM_LS.name, platform).unwrap();
            assert_eq!(
                terraform.url,
                format!(
                    "https://releases.hashicorp.com/terraform-ls/0.39.1/terraform-ls_0.39.1_{target}.zip"
                )
            );
            assert_eq!(terraform.sha256, digest);
            assert!(entry(&manifest, JDTLS.name, platform).is_ok());
        }
        assert_eq!(
            entry(&manifest, "jdtls", "plan9-mips")
                .unwrap_err()
                .to_string(),
            "jdtls has no build for plan9-mips"
        );
        // The Kotlin servers keep their own, kind-keyed API.
        assert_eq!(
            entry(&manifest, "kotlin-lsp", "linux-x64")
                .unwrap_err()
                .to_string(),
            "kotlin-lsp is not a pinned server"
        );
        assert_eq!(PinnedServer::by_name("terraform-ls"), Some(TERRAFORM_LS));
        assert_eq!(PinnedServer::by_name("intellij"), None);
    }

    #[test]
    fn test_install_roots_are_under_the_languages_dir() {
        let languages = Path::new("/data/languages");
        assert_eq!(
            JDTLS.version_dir(languages, "1.63.0"),
            Path::new("/data/languages/java/jdtls/1.63.0")
        );
        assert_eq!(
            TERRAFORM_LS.version_dir(languages, "0.39.1"),
            Path::new("/data/languages/terraform/terraform-ls/0.39.1")
        );
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn user_jdtls(url: &str) -> String {
        format!(
            r#"
[jdtls]
version = "9.9.9"

[jdtls.platform.linux-x64]
url = "{url}"
sha256 = "{SHA}"
archive = "tar.gz"
launcher = "bin/jdtls"
"#
        )
    }

    fn load_user(text: &str) -> crate::manifest::UserManifest {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(USER_FILE_NAME), text).unwrap();
        Manifest::load_user_file(temp.path())
    }

    #[test]
    fn test_a_user_jdtls_table_replaces_the_builtin_one_and_its_urls_are_checked() {
        let loaded = load_user(&user_jdtls("https://mirror.example.com/jdtls.tar.gz"));
        assert_eq!(loaded.error, None);
        let jdtls = entry(&loaded.manifest, "jdtls", "linux-x64").unwrap();
        assert_eq!(jdtls.version, "9.9.9");
        assert_eq!(jdtls.url, "https://mirror.example.com/jdtls.tar.gz");
        assert_eq!(jdtls.sha256, SHA);
        // Replaced whole: the built-in table's other platforms are gone.
        assert!(entry(&loaded.manifest, "jdtls", "macos-arm64").is_err());
        // The other servers keep their built-in pins.
        let embedded = Manifest::embedded();
        assert_eq!(
            entry(&loaded.manifest, "terraform-ls", "linux-x64").unwrap(),
            entry(&embedded, "terraform-ls", "linux-x64").unwrap()
        );

        for url in [
            "http://mirror.example.com/jdtls.tar.gz",
            "ftp://mirror.example.com/jdtls.tar.gz",
            "https://user:pass@mirror.example.com/jdtls.tar.gz",
            "https://downloads.bongos.rocks/jdtls.tar.gz",
            "not a url",
        ] {
            let loaded = load_user(&user_jdtls(url));
            let error = loaded.error.unwrap_or_else(|| panic!("{url} was accepted"));
            assert!(error.contains("jdtls on linux-x64"), "{error}");
            // A refused file falls back to the built-in pin.
            assert_eq!(
                entry(&loaded.manifest, "jdtls", "linux-x64").unwrap(),
                entry(&embedded, "jdtls", "linux-x64").unwrap()
            );
        }
    }

    fn run(
        client: &dyn HttpClient,
        server: PinnedServer,
        entry: &ManifestEntry,
        languages: &Path,
        cancel: &AtomicBool,
    ) -> Result<PinnedInstall> {
        block_on(ensure_installed(
            &thread_registry(),
            server,
            entry,
            languages,
            client,
            &|_, _| {},
            cancel,
        ))
    }

    /// Every path under `root`, lock files left out (they are never deleted).
    fn tree(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                let name = path.strip_prefix(root).unwrap().display().to_string();
                if !name.ends_with(".lock") {
                    out.push(name);
                }
                if path.is_dir() {
                    walk(&path, root, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.retain(|name| !["java", "java/jdtls", "terraform", "terraform/terraform-ls"].contains(&name.as_str()));
        out.sort();
        out
    }

    fn flipped(archive: &[u8]) -> Vec<u8> {
        let mut tampered = archive.to_vec();
        let middle = tampered.len() / 2;
        tampered[middle] ^= 0xff;
        tampered
    }

    #[test]
    fn test_a_tampered_archive_is_never_extracted_tar_gz_or_zip() {
        for (server, kind) in [(JDTLS, ArchiveKind::TarGz), (TERRAFORM_LS, ArchiveKind::Zip)] {
            let languages = tempfile::tempdir().unwrap();
            let archive = match kind {
                ArchiveKind::TarGz => tar_gz(&files(0, true)),
                ArchiveKind::Zip => zip(&files(0, true)),
            };
            let pin = entry_for(&archive, kind);
            let tampered = flipped(&archive);

            let error = run(
                serving(tampered.clone(), None).as_ref(),
                server,
                &pin,
                languages.path(),
                &AtomicBool::new(false),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(&sha256(&tampered)), "{error}");
            assert!(
                tree(languages.path()).is_empty(),
                "{}: left behind {:?}",
                server.name,
                tree(languages.path())
            );
            assert!(check_installed(server, &pin, languages.path()).is_none());

            // The genuine bytes install, so the failure above was the digest.
            let installed = run(
                serving(archive, None).as_ref(),
                server,
                &pin,
                languages.path(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(installed.dir, server.version_dir(languages.path(), &pin.version));
            assert_eq!(installed.binary.path, installed.dir.join("bin/launcher"));
            assert_eq!(installed.binary.arguments, vec![OsString::from("--stdio")]);
        }
    }

    #[test]
    fn test_a_cancelled_install_leaves_no_partial_files() {
        for (server, kind) in [(JDTLS, ArchiveKind::TarGz), (TERRAFORM_LS, ArchiveKind::Zip)] {
            let languages = tempfile::tempdir().unwrap();
            let archive = match kind {
                ArchiveKind::TarGz => tar_gz(&files(3 * 1024 * 1024, true)),
                ArchiveKind::Zip => zip(&files(3 * 1024 * 1024, true)),
            };
            let pin = entry_for(&archive, kind);
            let cancel = Arc::new(AtomicBool::new(false));
            let client: Client = serving(
                archive.clone(),
                Some((crate::install::PROGRESS_STEP as usize, cancel.clone())),
            );
            let error = run(client.as_ref(), server, &pin, languages.path(), &cancel).unwrap_err();
            assert!(error.downcast_ref::<InstallCancelled>().is_some(), "{error:#}");
            assert!(
                tree(languages.path()).is_empty(),
                "{}: left behind {:?}",
                server.name,
                tree(languages.path())
            );
            assert!(check_installed(server, &pin, languages.path()).is_none());

            // Cancelled before it starts: nothing is created at all.
            let languages = tempfile::tempdir().unwrap();
            let error = run(
                client.as_ref(),
                server,
                &pin,
                languages.path(),
                &AtomicBool::new(true),
            )
            .unwrap_err();
            assert!(error.downcast_ref::<InstallCancelled>().is_some());
            assert_eq!(fs::read_dir(languages.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn test_check_installed_needs_no_http() {
        let languages = tempfile::tempdir().unwrap();
        let archive = zip(&files(0, true));
        let mut pin = entry_for(&archive, ArchiveKind::Zip);
        pin.args = vec!["serve".into()];
        pin.env = BTreeMap::from([("CHECKPOINT_DISABLE".to_string(), "1".to_string())]);
        assert!(check_installed(TERRAFORM_LS, &pin, languages.path()).is_none());
        run(
            serving(archive, None).as_ref(),
            TERRAFORM_LS,
            &pin,
            languages.path(),
            &AtomicBool::new(false),
        )
        .unwrap();

        // `check_installed` takes no client; the binary comes from the disk and the pin.
        let binary = check_installed(TERRAFORM_LS, &pin, languages.path()).unwrap();
        assert_eq!(
            binary.path,
            languages
                .path()
                .join("terraform/terraform-ls")
                .join(&pin.version)
                .join("bin/launcher")
        );
        assert_eq!(binary.arguments, vec![OsString::from("serve")]);
        assert_eq!(
            binary.env.unwrap().get("CHECKPOINT_DISABLE").map(String::as_str),
            Some("1")
        );
        // Another server's directory does not count.
        assert!(check_installed(JDTLS, &pin, languages.path()).is_none());

        // An installed build needs no network: a client that fails every request still works.
        let offline = FakeHttpClient::create(|request| async move {
            panic!("unexpected request to {}", request.uri())
        });
        let installed = run(
            offline.as_ref(),
            TERRAFORM_LS,
            &pin,
            languages.path(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(installed.binary.path, binary.path);
        assert_eq!(installed.version, pin.version);
    }

    #[test]
    fn test_a_zip_entry_that_climbs_out_is_never_written() {
        let languages = tempfile::tempdir().unwrap();
        let mut entries = files(0, true);
        entries.push(("server-1.2.3/../../escape".into(), 0o644, b"pwned".to_vec()));
        entries.push(("server-1.2.3/../../../../escape".into(), 0o644, b"pwned".to_vec()));
        let archive = zip(&entries);
        let pin = entry_for(&archive, ArchiveKind::Zip);
        let installed = run(
            serving(archive, None).as_ref(),
            TERRAFORM_LS,
            &pin,
            languages.path(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let install_dir = TERRAFORM_LS.install_dir(languages.path());
        for escaped in [
            install_dir.join("escape"),
            install_dir.parent().unwrap().join("escape"),
            languages.path().join("escape"),
            installed.dir.join("escape"),
        ] {
            assert!(!escaped.exists(), "{} was written", escaped.display());
        }
        assert!(
            !tree(languages.path()).iter().any(|path| path.ends_with("escape")),
            "{:?}",
            tree(languages.path())
        );
    }
}
