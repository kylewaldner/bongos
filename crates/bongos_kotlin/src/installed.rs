//! Where an installed Kotlin server lives and how to start it. The installer writes
//! `<install root>/<server>/<version>/.bongos-installed` (version and archive digest) once a
//! verified archive is extracted. A server counts as installed only when that file names the pinned
//! version and digest and the pinned launcher is executable. Nothing here touches the network.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use lsp::LanguageServerBinary;
use serde::{Deserialize, Serialize};
use settings::KotlinServerKind;

use crate::manifest::ManifestEntry;

pub const MARKER: &str = ".bongos-installed";

/// The short name used in `kotlin.log`, directory names and messages.
pub fn server_name(server: KotlinServerKind) -> &'static str {
    match server {
        KotlinServerKind::KotlinLsp => "kotlin-lsp",
        KotlinServerKind::Intellij => "intellij",
    }
}

/// The contents of the marker file.
#[derive(Serialize, Deserialize)]
pub(crate) struct Marker {
    pub version: String,
    pub sha256: String,
}

/// `<install root>/<server>/<version>`.
pub fn version_dir(install_root: &Path, server: KotlinServerKind, version: &str) -> PathBuf {
    install_root.join(server_name(server)).join(version)
}

/// The command for the pinned build of `server`, or `None` when it is not installed: no marker,
/// a marker for another version or digest, or a launcher that is missing or not executable.
/// Arguments and environment come from the pin, not from the disk.
pub fn check_installed(
    install_root: &Path,
    server: KotlinServerKind,
    entry: &ManifestEntry,
) -> Option<LanguageServerBinary> {
    check_dir(&version_dir(install_root, server, &entry.version), entry)
}

/// `check_installed` for a version directory.
pub(crate) fn check_dir(dir: &Path, entry: &ManifestEntry) -> Option<LanguageServerBinary> {
    let marker: Marker = serde_json::from_str(&fs::read_to_string(dir.join(MARKER)).ok()?).ok()?;
    if marker.version != entry.version || !marker.sha256.eq_ignore_ascii_case(&entry.sha256) {
        return None;
    }
    let launcher = dir.join(&entry.launcher);
    if !is_executable_file(&launcher) {
        return None;
    }
    Some(LanguageServerBinary {
        path: launcher,
        arguments: entry.args.iter().map(OsString::from).collect(),
        env: Some(entry.env.clone().into_iter().collect()),
    })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// "<server> is not installed", the error a start reports when the pinned build is missing.
pub fn not_installed(server: KotlinServerKind) -> anyhow::Error {
    anyhow::anyhow!("{} is not installed", server_name(server))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::manifest::ArchiveKind;

    pub(crate) fn entry(version: &str, sha256: &str) -> ManifestEntry {
        ManifestEntry {
            version: version.into(),
            url: "http://127.0.0.1/unused".into(),
            sha256: sha256.into(),
            archive: ArchiveKind::TarGz,
            strip_components: 0,
            launcher: "bin/kotlin-lsp".into(),
            runtime: None,
            args: vec!["--stdio".into()],
            env: BTreeMap::from([("A".to_string(), "b".to_string())]),
            built: None,
            expires: None,
        }
    }

    const DIGEST: &str = "ae0c4b01e17ab73a4a5227a78ea99e6dcbe3e499600315b22759a4e4cddd11df";

    /// Lays out an installed server the way the installer leaves it.
    pub(crate) fn install(root: &Path, entry: &ManifestEntry) -> PathBuf {
        let dir = version_dir(root, KotlinServerKind::KotlinLsp, &entry.version);
        let launcher = dir.join(&entry.launcher);
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(&launcher, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let marker = Marker {
            version: entry.version.clone(),
            sha256: entry.sha256.clone(),
        };
        fs::write(dir.join(MARKER), serde_json::to_string(&marker).unwrap()).unwrap();
        dir
    }

    #[test]
    fn test_missing_server_is_not_installed() {
        let root = tempfile::tempdir().unwrap();
        let entry = entry("1.0", DIGEST);
        assert!(check_installed(root.path(), KotlinServerKind::KotlinLsp, &entry).is_none());
        assert_eq!(
            not_installed(KotlinServerKind::Intellij).to_string(),
            "intellij is not installed"
        );
    }

    #[test]
    fn test_marker_and_launcher_make_a_binary_from_the_pin() {
        let root = tempfile::tempdir().unwrap();
        let entry = entry("1.0", DIGEST);
        let dir = install(root.path(), &entry);
        let binary = check_installed(root.path(), KotlinServerKind::KotlinLsp, &entry).unwrap();
        assert_eq!(binary.path, dir.join("bin/kotlin-lsp"));
        assert_eq!(binary.arguments, vec![OsString::from("--stdio")]);
        assert_eq!(binary.env.unwrap().get("A").map(String::as_str), Some("b"));
        // Another server's directory does not count.
        assert!(check_installed(root.path(), KotlinServerKind::Intellij, &entry).is_none());
    }

    #[test]
    fn test_marker_must_name_the_pinned_version_and_digest() {
        let root = tempfile::tempdir().unwrap();
        let installed = entry("1.0", DIGEST);
        let dir = install(root.path(), &installed);

        // A new pin with another digest for the same version is not satisfied by the old files.
        let other_digest = entry("1.0", &"0".repeat(64));
        assert!(check_installed(root.path(), KotlinServerKind::KotlinLsp, &other_digest).is_none());
        // Another version has no directory.
        let other_version = entry("2.0", DIGEST);
        assert!(
            check_installed(root.path(), KotlinServerKind::KotlinLsp, &other_version).is_none()
        );
        // A marker naming another version than its directory does not count.
        fs::write(
            dir.join(MARKER),
            format!(r#"{{ "version": "0.9", "sha256": "{DIGEST}" }}"#),
        )
        .unwrap();
        assert!(check_installed(root.path(), KotlinServerKind::KotlinLsp, &installed).is_none());
        // The old placeholder marker (launcher, args) is not a marker.
        fs::write(dir.join(MARKER), r#"{ "launcher": "bin/kotlin-lsp" }"#).unwrap();
        assert!(check_installed(root.path(), KotlinServerKind::KotlinLsp, &installed).is_none());
    }

    #[test]
    fn test_marker_without_launcher_is_not_installed() {
        let root = tempfile::tempdir().unwrap();
        let entry = entry("1.0", DIGEST);
        let dir = install(root.path(), &entry);
        fs::remove_file(dir.join("bin/kotlin-lsp")).unwrap();
        assert!(check_installed(root.path(), KotlinServerKind::KotlinLsp, &entry).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_launcher_must_be_executable() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let entry = entry("1.0", DIGEST);
        let dir = install(root.path(), &entry);
        fs::set_permissions(
            dir.join("bin/kotlin-lsp"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(check_installed(root.path(), KotlinServerKind::KotlinLsp, &entry).is_none());
    }
}
