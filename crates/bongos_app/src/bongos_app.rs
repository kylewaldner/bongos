//! Bongos application behavior that sits on top of the inherited editor.

use anyhow::{Context as _, Result, bail};
use gpui::actions;
use serde_json::Value;
use std::{
    fs, io,
    io::Write as _,
    path::{Path, PathBuf},
};

actions!(
    bongos,
    [
        /// Installs the bongos command line tool so it can be run from a terminal as `bongos`.
        InstallCli,
        /// Copies your Zed settings and keymap over bongos' own, after asking. The old files are kept as `.bak` copies.
        ImportZedSettings,
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

/// The files `bongos::ImportZedSettings` copies, relative to a config directory.
const IMPORTED_FILES: [&str; 2] = ["settings.json", "keymap.json"];

/// The settings key that points the editor at a server. It is never imported, so Zed's server URL cannot reach bongos.
const SERVER_URL_KEY: &str = "server_url";

/// Where Zed keeps its config on this platform, or `None` when the platform's config directory is unknown.
///
/// This only names a path to read from; nothing is created.
pub fn zed_config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        dirs::config_dir().map(|dir| dir.join("Zed"))
    } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        dirs::config_dir().map(|dir| dir.join("zed"))
    } else {
        Some(paths::home_dir().join(".config").join("zed"))
    }
}

/// What an import did.
#[derive(Debug, PartialEq, Eq)]
pub enum ImportOutcome {
    /// The user declined the confirmation. Nothing was read or written.
    Declined,
    /// Neither Zed file exists. Nothing was written.
    NotFound { zed_dir: PathBuf },
    /// These files were copied.
    Imported(Vec<ImportedFile>),
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImportedFile {
    pub name: &'static str,
    /// Whether an existing bongos file was kept as `<name>.bak`.
    pub backed_up: bool,
    /// Whether `server_url` was removed from the imported content.
    pub removed_server_url: bool,
}

impl ImportOutcome {
    /// The text shown to the user. `None` when they declined, because they already know.
    pub fn message(&self) -> Option<String> {
        match self {
            ImportOutcome::Declined => None,
            ImportOutcome::NotFound { zed_dir } => Some(format!(
                "Zed settings were not found. Looked for settings.json and keymap.json in {}.",
                zed_dir.display()
            )),
            ImportOutcome::Imported(files) => {
                let names: Vec<_> = files.iter().map(|file| file.name).collect();
                let mut message = format!("Imported Zed {}.", names.join(" and "));
                if files.iter().any(|file| file.backed_up) {
                    message.push_str(" Your previous bongos files were kept as .bak copies.");
                }
                if files.iter().any(|file| file.removed_server_url) {
                    message.push_str(" server_url was not imported.");
                }
                Some(message)
            }
        }
    }
}

/// Copies `settings.json` and `keymap.json` from `zed_dir` over the ones in `bongos_dir`.
///
/// `confirm` is awaited before anything is read or written; when it resolves to `false` nothing changes. When neither
/// source exists nothing changes either and `NotFound` is returned. Every source is read and parsed as JSON with
/// comments, and every destination is checked, before the first write, so a failure (an unreadable or invalid file, a
/// destination that is a link to the Zed file) leaves `bongos_dir` as it was. An existing destination file is copied to
/// the first free name of `<name>.bak`, `<name>.bak.1`, `<name>.bak.2` and so on before it is replaced, so an earlier
/// backup is never overwritten. Each file is written to a temporary file and renamed into place. If a later write
/// fails, the error names the files that were already replaced. `server_url` is removed from the settings at any depth
/// inside objects, with comments and formatting kept. This never touches the network.
pub async fn import_zed_settings(
    zed_dir: &Path,
    bongos_dir: &Path,
    confirm: impl std::future::Future<Output = bool>,
) -> Result<ImportOutcome> {
    if !confirm.await {
        return Ok(ImportOutcome::Declined);
    }
    if let (Ok(zed), Ok(ours)) = (fs::canonicalize(zed_dir), fs::canonicalize(bongos_dir))
        && zed == ours
    {
        bail!("the Zed config directory is the bongos config directory");
    }

    let mut pending = Vec::new();
    for name in IMPORTED_FILES {
        let source = zed_dir.join(name);
        let content = match fs::read_to_string(&source) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", source.display()));
            }
        };
        let (content, removed_server_url) = if name == "settings.json" {
            remove_server_url(&content).with_context(|| format!("reading {}", source.display()))?
        } else {
            settings_json::parse_json_with_comments::<Value>(&content)
                .with_context(|| format!("{} is not valid JSON", source.display()))?;
            (content, false)
        };
        let destination = bongos_dir.join(name);
        if let (Ok(source), Ok(destination)) =
            (fs::canonicalize(&source), fs::canonicalize(&destination))
            && source == destination
        {
            bail!(
                "{} is the Zed file; refusing to rewrite it",
                destination.display()
            );
        }
        pending.push((name, content, removed_server_url));
    }
    if pending.is_empty() {
        return Ok(ImportOutcome::NotFound {
            zed_dir: zed_dir.to_path_buf(),
        });
    }

    fs::create_dir_all(bongos_dir).with_context(|| format!("creating {}", bongos_dir.display()))?;
    let mut imported = Vec::new();
    for (name, content, removed_server_url) in pending {
        let written: Vec<_> = imported
            .iter()
            .map(|file: &ImportedFile| file.name)
            .collect();
        let result = replace_file(bongos_dir, name, &content);
        let backed_up = match result {
            Ok(backed_up) => backed_up,
            Err(error) if written.is_empty() => return Err(error),
            Err(error) => {
                return Err(error.context(format!(
                    "{} was already replaced and the rest was not imported",
                    written.join(" and ")
                )));
            }
        };
        imported.push(ImportedFile {
            name,
            backed_up,
            removed_server_url,
        });
    }
    Ok(ImportOutcome::Imported(imported))
}

/// Replaces `dir/name` with `content`, first copying an existing file to the first free backup name. Returns whether a
/// backup was made.
fn replace_file(dir: &Path, name: &str, content: &str) -> Result<bool> {
    let destination = dir.join(name);
    let backed_up = destination.exists();
    if backed_up {
        let backup = (0..)
            .map(|n| match n {
                0 => dir.join(format!("{name}.bak")),
                n => dir.join(format!("{name}.bak.{n}")),
            })
            .find(|path| fs::symlink_metadata(path).is_err())
            .expect("an unbounded range always yields a free name");
        fs::copy(&destination, &backup)
            .with_context(|| format!("backing up {}", destination.display()))?;
    }
    let temporary = dir.join(format!(".{name}.import"));
    fs::write(&temporary, content).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, &destination)
        .with_context(|| format!("writing {}", destination.display()))?;
    Ok(backed_up)
}

/// Removes every `server_url` key from settings text, keeping comments and formatting. Returns the new text and whether
/// anything was removed.
///
/// Fails, rather than returning text that still holds the key, when the text cannot be parsed or a key could not be removed
/// (for example one inside an array).
fn remove_server_url(text: &str) -> Result<(String, bool)> {
    let value: Value = settings_json::parse_json_with_comments(text)
        .context("the settings file is not valid JSON")?;
    let mut key_paths = Vec::new();
    collect_server_url_paths(&value, &mut Vec::new(), &mut key_paths);
    let mut text = text.to_string();
    for key_path in &key_paths {
        let (range, replacement) =
            settings_json::replace_value_in_json_text(&text, key_path, 0, None, None);
        text.replace_range(range, &replacement);
    }
    let value: Value = settings_json::parse_json_with_comments(&text)
        .context("removing server_url left invalid JSON")?;
    if contains_server_url(&value) {
        bail!("could not remove {SERVER_URL_KEY} from the settings");
    }
    Ok((text, !key_paths.is_empty()))
}

fn collect_server_url_paths(value: &Value, path: &mut Vec<String>, found: &mut Vec<Vec<String>>) {
    if let Value::Object(object) = value {
        for (key, child) in object {
            path.push(key.clone());
            if key == SERVER_URL_KEY {
                found.push(path.clone());
            } else {
                collect_server_url_paths(child, path, found);
            }
            path.pop();
        }
    }
}

fn contains_server_url(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(SERVER_URL_KEY) || object.values().any(contains_server_url)
        }
        Value::Array(items) => items.iter().any(contains_server_url),
        _ => false,
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

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        futures::executor::block_on(future)
    }

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let zed = root.path().join("zed");
        let ours = root.path().join("bongos");
        fs::create_dir_all(&zed).unwrap();
        fs::create_dir_all(&ours).unwrap();
        (root, zed, ours)
    }

    fn import(zed: &Path, ours: &Path, answer: bool) -> ImportOutcome {
        block_on(import_zed_settings(zed, ours, async move { answer })).unwrap()
    }

    #[gpui::test]
    fn import_zed_settings_action_dispatches_by_name(cx: &mut gpui::TestAppContext) {
        let action = cx.update(|cx| cx.build_action("bongos::ImportZedSettings", None).unwrap());

        assert!(action.as_any().is::<ImportZedSettings>());
        assert_eq!(action.name(), "bongos::ImportZedSettings");
    }

    #[test]
    fn declined_prompt_changes_nothing() {
        let (_root, zed, ours) = dirs();
        fs::write(zed.join("settings.json"), "{\"theme\": \"One\"}").unwrap();
        fs::write(zed.join("keymap.json"), "[]").unwrap();
        fs::write(ours.join("settings.json"), "{}").unwrap();

        assert_eq!(import(&zed, &ours, false), ImportOutcome::Declined);

        assert_eq!(
            fs::read_to_string(ours.join("settings.json")).unwrap(),
            "{}"
        );
        assert!(!ours.join("settings.json.bak").exists());
        assert!(!ours.join("keymap.json").exists());
    }

    #[test]
    fn confirmation_is_awaited_before_files_are_read() {
        let (_root, zed, ours) = dirs();
        fs::write(zed.join("settings.json"), "{}").unwrap();
        let seen = std::cell::Cell::new(false);
        let confirm = async {
            seen.set(ours.join("settings.json").exists());
            true
        };

        block_on(import_zed_settings(&zed, &ours, confirm)).unwrap();

        assert!(!seen.get());
        assert!(ours.join("settings.json").exists());
    }

    #[test]
    fn existing_files_are_kept_as_bak() {
        let (_root, zed, ours) = dirs();
        fs::write(zed.join("settings.json"), "{\"theme\": \"Zed\"}").unwrap();
        fs::write(zed.join("keymap.json"), "[{\"bindings\": {}}]").unwrap();
        fs::write(ours.join("settings.json"), "// mine\n{\"theme\": \"Mine\"}").unwrap();

        let outcome = import(&zed, &ours, true);

        assert_eq!(
            fs::read_to_string(ours.join("settings.json.bak")).unwrap(),
            "// mine\n{\"theme\": \"Mine\"}"
        );
        assert_eq!(
            fs::read_to_string(ours.join("settings.json")).unwrap(),
            "{\"theme\": \"Zed\"}"
        );
        assert!(!ours.join("keymap.json.bak").exists());
        let ImportOutcome::Imported(files) = outcome else {
            panic!("expected an import");
        };
        assert!(files[0].backed_up && !files[1].backed_up);
    }

    #[test]
    fn keymap_is_copied() {
        let (_root, zed, ours) = dirs();
        let keymap = "// my keys\n[{\"bindings\": {\"ctrl-x ctrl-s\": \"workspace::Save\"}}]\n";
        fs::write(zed.join("keymap.json"), keymap).unwrap();
        fs::write(ours.join("keymap.json"), "[]").unwrap();

        import(&zed, &ours, true);

        assert_eq!(
            fs::read_to_string(ours.join("keymap.json")).unwrap(),
            keymap
        );
        assert_eq!(
            fs::read_to_string(ours.join("keymap.json.bak")).unwrap(),
            "[]"
        );
        assert!(!ours.join("settings.json").exists());
    }

    #[test]
    fn server_url_is_removed_and_other_keys_and_comments_kept() {
        let (_root, zed, ours) = dirs();
        fs::write(
            zed.join("settings.json"),
            "{\n  // my theme\n  \"theme\": \"One\",\n  \"server_url\": \"https://example.test\",\n  \"tab_size\": 2,\n  \"nightly\": { \"server_url\": \"https://x.test\", \"vim_mode\": true }\n}\n",
        )
        .unwrap();

        let outcome = import(&zed, &ours, true);
        let message = outcome.message().unwrap();

        let text = fs::read_to_string(ours.join("settings.json")).unwrap();
        assert!(!text.contains("server_url"), "{text}");
        assert!(!text.contains("example.test"), "{text}");
        assert!(text.contains("// my theme"), "{text}");
        let value: Value = settings_json::parse_json_with_comments(&text).unwrap();
        assert_eq!(value["theme"], "One");
        assert_eq!(value["tab_size"], 2);
        assert_eq!(value["nightly"]["vim_mode"], true);
        let ImportOutcome::Imported(files) = outcome else {
            panic!("expected an import");
        };
        assert!(files[0].removed_server_url);
        assert!(message.contains("server_url was not imported"), "{message}");
    }

    #[test]
    fn server_url_as_only_key_leaves_valid_settings() {
        let (text, removed) = remove_server_url("{ \"server_url\": \"https://a.test\" }").unwrap();

        assert!(removed);
        let value: Value = settings_json::parse_json_with_comments(&text).unwrap();
        assert!(value.as_object().unwrap().is_empty());
    }

    #[test]
    fn settings_without_server_url_are_unchanged() {
        let text = "// c\n{\"a\": 1,}\n";

        assert_eq!(remove_server_url(text).unwrap(), (text.to_string(), false));
    }

    #[test]
    fn server_url_inside_an_array_is_refused_and_nothing_is_written() {
        let (_root, zed, ours) = dirs();
        fs::write(
            zed.join("settings.json"),
            "{\"a\": [{\"server_url\": \"x\"}]}",
        )
        .unwrap();
        fs::write(zed.join("keymap.json"), "[]").unwrap();

        let result = block_on(import_zed_settings(&zed, &ours, async { true }));

        assert!(result.is_err());
        assert!(!ours.join("keymap.json").exists());
        assert!(!ours.join("settings.json").exists());
    }

    #[test]
    fn invalid_settings_are_refused_and_nothing_is_written() {
        let (_root, zed, ours) = dirs();
        fs::write(zed.join("settings.json"), "{ nope").unwrap();
        fs::write(zed.join("keymap.json"), "[]").unwrap();

        assert!(block_on(import_zed_settings(&zed, &ours, async { true })).is_err());
        assert!(!ours.join("keymap.json").exists());
    }

    #[test]
    fn absent_source_is_reported_and_nothing_changes() {
        let (_root, zed, ours) = dirs();
        fs::write(ours.join("settings.json"), "{}").unwrap();

        let outcome = import(&zed, &ours, true);

        assert_eq!(outcome, ImportOutcome::NotFound { zed_dir: zed });
        assert!(outcome.message().unwrap().contains("not found"));
        assert_eq!(
            fs::read_to_string(ours.join("settings.json")).unwrap(),
            "{}"
        );
        assert!(!ours.join("settings.json.bak").exists());
        assert!(!ours.join("keymap.json").exists());
    }

    #[test]
    fn missing_zed_directory_is_reported_as_not_found() {
        let (root, _zed, ours) = dirs();

        let outcome = import(&root.path().join("no-such-dir"), &ours, true);

        assert!(matches!(outcome, ImportOutcome::NotFound { .. }));
    }

    #[test]
    fn same_directory_is_refused() {
        let (_root, zed, _ours) = dirs();
        fs::write(zed.join("settings.json"), "{}").unwrap();

        assert!(block_on(import_zed_settings(&zed, &zed, async { true })).is_err());
    }

    #[test]
    fn second_import_keeps_the_original_in_the_first_bak() {
        let (_root, zed, ours) = dirs();
        fs::write(zed.join("settings.json"), "{\"theme\": \"Zed\"}").unwrap();
        fs::write(ours.join("settings.json"), "// original\n{}").unwrap();

        import(&zed, &ours, true);
        import(&zed, &ours, true);
        fs::write(zed.join("settings.json"), "{\"theme\": \"Zed 2\"}").unwrap();
        import(&zed, &ours, true);

        assert_eq!(
            fs::read_to_string(ours.join("settings.json.bak")).unwrap(),
            "// original\n{}"
        );
        assert_eq!(
            fs::read_to_string(ours.join("settings.json.bak.1")).unwrap(),
            "{\"theme\": \"Zed\"}"
        );
        assert_eq!(
            fs::read_to_string(ours.join("settings.json.bak.2")).unwrap(),
            "{\"theme\": \"Zed\"}"
        );
        assert_eq!(
            fs::read_to_string(ours.join("settings.json")).unwrap(),
            "{\"theme\": \"Zed 2\"}"
        );
    }

    #[test]
    fn invalid_keymap_is_refused_and_nothing_is_written() {
        let (_root, zed, ours) = dirs();
        fs::write(zed.join("settings.json"), "{\"a\": 1}").unwrap();
        fs::write(zed.join("keymap.json"), "[{ nope").unwrap();
        fs::write(ours.join("settings.json"), "{}").unwrap();

        assert!(block_on(import_zed_settings(&zed, &ours, async { true })).is_err());

        assert_eq!(
            fs::read_to_string(ours.join("settings.json")).unwrap(),
            "{}"
        );
        assert!(!ours.join("settings.json.bak").exists());
        assert!(!ours.join("keymap.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn destination_linked_to_the_zed_file_is_refused() {
        let (_root, zed, ours) = dirs();
        fs::write(zed.join("settings.json"), "{\"theme\": \"Zed\"}").unwrap();
        fs::write(zed.join("keymap.json"), "[]").unwrap();
        std::os::unix::fs::symlink(zed.join("keymap.json"), ours.join("keymap.json")).unwrap();

        let result = block_on(import_zed_settings(&zed, &ours, async { true }));

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(zed.join("keymap.json")).unwrap(), "[]");
        assert!(!ours.join("settings.json").exists());
        assert!(!ours.join("keymap.json.bak").exists());
    }

    #[test]
    fn failure_after_the_first_write_names_the_replaced_file() {
        let (_root, zed, ours) = dirs();
        fs::write(zed.join("settings.json"), "{}").unwrap();
        fs::write(zed.join("keymap.json"), "[]").unwrap();
        // A directory where the keymap should go makes the second write fail.
        fs::create_dir(ours.join("keymap.json")).unwrap();

        let error = block_on(import_zed_settings(&zed, &ours, async { true })).unwrap_err();

        let message = format!("{error:#}");
        assert!(
            message.contains("settings.json was already replaced"),
            "{message}"
        );
        assert_eq!(
            fs::read_to_string(ours.join("settings.json")).unwrap(),
            "{}"
        );
    }
}
