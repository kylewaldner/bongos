//! Where an installed Kotlin server lives and how to start it. The installers (a later step) write
//! `<install root>/<server>/<version>/.bongos-installed`; a server without one is "not installed".

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use lsp::LanguageServerBinary;
use serde::Deserialize;
use settings::KotlinServerKind;

pub const MARKER: &str = ".bongos-installed";

/// The short name used in `kotlin.log`, directory names and messages.
pub fn server_name(server: KotlinServerKind) -> &'static str {
    match server {
        KotlinServerKind::KotlinLsp => "kotlin-lsp",
        KotlinServerKind::Intellij => "intellij",
    }
}

#[derive(Deserialize)]
struct Marker {
    /// Launcher path relative to the version directory.
    launcher: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// The newest installed version of `server` under `install_root`, or `None` when there is no
/// marker, it cannot be read, or its launcher is missing. Reads the disk, never the network.
pub fn find_installed(
    install_root: &Path,
    server: KotlinServerKind,
) -> Option<LanguageServerBinary> {
    let server_dir = install_root.join(server_name(server));
    let mut versions: Vec<PathBuf> = fs::read_dir(&server_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.join(MARKER).is_file())
        .collect();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .find_map(|dir| binary_in(&dir).ok())
}

fn binary_in(dir: &Path) -> anyhow::Result<LanguageServerBinary> {
    let marker: Marker = serde_json::from_str(&fs::read_to_string(dir.join(MARKER))?)?;
    let launcher = dir.join(&marker.launcher);
    anyhow::ensure!(launcher.is_file(), "{} is missing", launcher.display());
    Ok(LanguageServerBinary {
        path: launcher,
        arguments: marker.args.into_iter().map(OsString::from).collect(),
        env: Some(marker.env.into_iter().collect()),
    })
}

/// "<server> is not installed", the error a start reports until the installers exist.
pub fn not_installed(server: KotlinServerKind) -> anyhow::Error {
    anyhow::anyhow!("{} is not installed", server_name(server))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(root: &Path, server: &str, version: &str, launcher: &str) -> PathBuf {
        let dir = root.join(server).join(version);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join(launcher), "#!/bin/sh\n").unwrap();
        fs::write(
            dir.join(MARKER),
            format!(
                r#"{{ "launcher": "{launcher}", "args": ["--stdio"], "env": {{ "A": "b" }} }}"#
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn test_missing_server_is_not_installed() {
        let root = tempfile::tempdir().unwrap();
        assert!(find_installed(root.path(), KotlinServerKind::KotlinLsp).is_none());
        assert_eq!(
            not_installed(KotlinServerKind::Intellij).to_string(),
            "intellij is not installed"
        );
    }

    #[test]
    fn test_marker_and_launcher_make_a_binary() {
        let root = tempfile::tempdir().unwrap();
        let dir = install(root.path(), "kotlin-lsp", "1.0", "bin/kotlin-lsp");
        let binary = find_installed(root.path(), KotlinServerKind::KotlinLsp).unwrap();
        assert_eq!(binary.path, dir.join("bin/kotlin-lsp"));
        assert_eq!(binary.arguments, vec![OsString::from("--stdio")]);
        assert_eq!(binary.env.unwrap().get("A").map(String::as_str), Some("b"));
        // Another server's directory does not count.
        assert!(find_installed(root.path(), KotlinServerKind::Intellij).is_none());
    }

    #[test]
    fn test_marker_without_launcher_is_not_installed() {
        let root = tempfile::tempdir().unwrap();
        let dir = install(root.path(), "kotlin-lsp", "1.0", "bin/kotlin-lsp");
        fs::remove_file(dir.join("bin/kotlin-lsp")).unwrap();
        assert!(find_installed(root.path(), KotlinServerKind::KotlinLsp).is_none());
    }

    #[test]
    fn test_newest_version_wins() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), "kotlin-lsp", "1.0", "bin/k");
        let newer = install(root.path(), "kotlin-lsp", "2.0", "bin/k");
        let binary = find_installed(root.path(), KotlinServerKind::KotlinLsp).unwrap();
        assert_eq!(binary.path, newer.join("bin/k"));
    }
}
