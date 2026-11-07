//! Bongos application behavior that sits on top of the inherited editor.

use gpui::actions;
use std::{fs, io, io::Write as _, path::Path};

actions!(
    bongos,
    [
        /// Installs the bongos command line tool so it can be run from a terminal as `bongos`.
        InstallCli,
    ]
);

/// Writes `settings.json` and `keymap.json` into the config directory when they do not exist yet.
///
/// Files that already exist are never read or modified. A failure is printed to stderr (logging starts later) and does not stop
/// the launch, because the editor creates these files on demand when the user opens them.
pub fn ensure_config_files() {
    let files = [
        (
            paths::settings_file().as_path(),
            settings::initial_user_settings_content(),
        ),
        (
            paths::keymap_file().as_path(),
            settings::initial_keymap_content(),
        ),
    ];
    for (path, content) in files {
        if let Err(error) = write_if_absent(path, &content) {
            eprintln!("bongos: could not create {}: {error}", path.display());
        }
    }
}

/// Creates `path` with `content`, and its parent directory, unless `path` already exists.
///
/// Returns whether the file was written.
pub fn write_if_absent(path: &Path, content: &str) -> io::Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(content.as_bytes())?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn install_cli_action_dispatches_by_name(cx: &mut gpui::TestAppContext) {
        let action = cx.update(|cx| cx.build_action("bongos::InstallCli", None).unwrap());

        assert!(action.as_any().is::<InstallCli>());
        assert_eq!(action.name(), "bongos::InstallCli");
    }

    #[test]
    fn creates_absent_file_and_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("config")
            .join("nested")
            .join("settings.json");

        assert!(write_if_absent(&path, "{}\n").unwrap());

        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");
    }

    #[test]
    fn leaves_existing_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.json");
        let original = b"// mine\r\n[\xff]";
        fs::write(&path, original).unwrap();

        assert!(!write_if_absent(&path, "[]\n").unwrap());

        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn leaves_existing_empty_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "").unwrap();

        assert!(!write_if_absent(&path, "{}\n").unwrap());

        assert_eq!(fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn initial_contents_are_not_empty() {
        assert!(!settings::initial_user_settings_content().trim().is_empty());
        assert!(!settings::initial_keymap_content().trim().is_empty());
    }
}
