//! Which JDK a Kotlin server imports Gradle projects with (decision D11), and the text of
//! `kotlin: show jdk`.
//!
//! Detection takes every input explicitly (setting, project root, `JAVA_HOME`, candidate locations),
//! so tests run on a temp tree and never see the real machine. Order: the `kotlin.jdk_home` setting;
//! a JDK that matches the project's Gradle toolchain; `JAVA_HOME`; then the highest version found
//! under the candidate locations. A toolchain no detected JDK satisfies is a failure; with no
//! toolchain and no JDK the server's bundled runtime does the import.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use gpui::{Action, App};
use settings::{KotlinServerKind, Settings as _, SettingsLocation, WorktreeId};
use util::rel_path::RelPath;

use crate::{
    KotlinDirs, KotlinSettings,
    installed::{server_name, version_dir},
    manifest::{Manifest, current_platform},
    resolve::resolve,
};

/// The label of the status menu entry that opens the settings file when no JDK fits.
pub const SET_JDK_HOME_LABEL: &str = "Set kotlin.jdk_home";

/// The start of the `Failed` text for an unmet toolchain.
pub const NO_JDK_REASON: &str = "no JDK for Gradle import";

/// Locations that hold one JDK per directory. A `*` matches any run of characters inside one path
/// component. The macOS entries are patterns only on that platform; elsewhere they match nothing.
pub fn default_candidate_patterns(home: Option<&Path>) -> Vec<String> {
    let mut patterns = Vec::new();
    if let Some(home) = home {
        patterns.push(format!("{}/.sdkman/candidates/java/*", home.display()));
    }
    patterns.extend(
        [
            "/usr/lib/jvm/*",
            "/opt/homebrew/opt/openjdk*/libexec/openjdk.jdk/Contents/Home",
            "/Library/Java/JavaVirtualMachines/*/Contents/Home",
        ]
        .map(String::from),
    );
    patterns
}

/// A menu entry: its label and the action it dispatches.
pub struct MenuEntry {
    pub label: &'static str,
    pub action: Box<dyn Action>,
}

/// The entry shown with an unmet toolchain: it opens the settings file.
pub fn set_jdk_home_entry() -> MenuEntry {
    MenuEntry {
        label: SET_JDK_HOME_LABEL,
        action: Box::new(zed_actions::OpenSettingsFile),
    }
}

/// Everything detection reads. Build it with [`JdkInputs::from_environment`] in the app and by hand
/// in tests.
#[derive(Clone, Debug)]
pub struct JdkInputs {
    /// The `kotlin.jdk_home` setting. A relative path resolves against `root`.
    pub setting: Option<PathBuf>,
    /// The worktree's `.bongos/settings.json`, read only to say where the setting came from.
    pub project_settings: Option<PathBuf>,
    /// The worktree root.
    pub root: PathBuf,
    /// The `JAVA_HOME` environment variable.
    pub java_home: Option<OsString>,
    /// Candidate location patterns, see [`default_candidate_patterns`].
    pub candidates: Vec<String>,
}

impl JdkInputs {
    pub fn from_environment(root: &Path, setting: Option<PathBuf>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self {
            setting,
            project_settings: Some(root.join(".bongos").join("settings.json")),
            root: root.to_path_buf(),
            java_home: std::env::var_os("JAVA_HOME"),
            candidates: default_candidate_patterns(home.as_deref()),
        }
    }
}

/// A JDK directory with what is known about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jdk {
    pub home: PathBuf,
    /// The `JAVA_VERSION` of its `release` file, as written there.
    pub version: Option<String>,
    /// The major version of `version`.
    pub major: Option<u32>,
    /// Why this JDK was chosen: "toolchain 21 in build.gradle.kts", "JAVA_HOME", ...
    pub reason: String,
}

/// The Gradle toolchain a project declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolchain {
    pub version: u32,
    /// The file that declares it, relative to the worktree root.
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JdkChoice {
    /// The import runs with this JDK, passed to the server as `JAVA_HOME`.
    Found(Jdk),
    /// The project declares a toolchain and no detected JDK satisfies it.
    Unmet(Toolchain),
    /// No toolchain and no JDK: the server's own bundled runtime imports.
    Bundled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detection {
    pub choice: JdkChoice,
    /// What the project declares, whether or not it decided the choice.
    pub toolchain: Option<Toolchain>,
    /// Things the user should know, such as a `kotlin.jdk_home` that does not exist.
    pub notes: Vec<String>,
}

impl Detection {
    /// The `Failed` status text for an unmet toolchain.
    pub fn failure_reason(&self) -> Option<String> {
        match &self.choice {
            JdkChoice::Unmet(toolchain) => {
                Some(format!("{NO_JDK_REASON} (toolchain {})", toolchain.version))
            }
            _ => None,
        }
    }

    /// The menu entry to offer with the failure.
    pub fn menu_entry(&self) -> Option<MenuEntry> {
        self.failure_reason().map(|_| set_jdk_home_entry())
    }

    pub fn jdk(&self) -> Option<&Jdk> {
        match &self.choice {
            JdkChoice::Found(jdk) => Some(jdk),
            _ => None,
        }
    }
}

pub fn detect(inputs: &JdkInputs) -> Detection {
    let toolchain = find_toolchain(&inputs.root);
    let mut notes = Vec::new();
    let done = |choice, notes| Detection {
        choice,
        toolchain: toolchain.clone(),
        notes,
    };

    if let Some(setting) = &inputs.setting {
        let path = if setting.is_absolute() {
            setting.clone()
        } else {
            inputs.root.join(setting)
        };
        if path.is_dir() {
            let origin = if project_sets_jdk_home(inputs.project_settings.as_deref()) {
                "project settings"
            } else {
                "settings"
            };
            return done(
                JdkChoice::Found(describe(path, format!("kotlin.jdk_home in {origin}"))),
                notes,
            );
        }
        notes.push(format!(
            "kotlin.jdk_home {} is not a directory, so it is ignored",
            path.display()
        ));
    }

    let java_home = inputs
        .java_home
        .as_ref()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let java_home = java_home.filter(|path| is_jdk_dir(path));
    let mut candidates: Vec<PathBuf> = Vec::new();
    for pattern in &inputs.candidates {
        candidates.extend(
            expand_pattern(pattern)
                .into_iter()
                .filter(|p| is_jdk_dir(p)),
        );
    }

    if let Some(toolchain) = &toolchain {
        let reason = format!("toolchain {} in {}", toolchain.version, toolchain.source);
        let matching = java_home
            .iter()
            .chain(candidates.iter())
            .map(|path| describe(path.clone(), reason.clone()))
            .find(|jdk| jdk.major == Some(toolchain.version));
        return done(
            match matching {
                Some(jdk) => JdkChoice::Found(jdk),
                None => JdkChoice::Unmet(toolchain.clone()),
            },
            notes,
        );
    }

    if let Some(path) = java_home {
        return done(JdkChoice::Found(describe(path, "JAVA_HOME".into())), notes);
    }

    let best = candidates
        .into_iter()
        .map(|path| {
            let parent = path_parent_for_reason(&path);
            describe(path, format!("found in {parent}"))
        })
        // The first of the highest version wins; an unknown version ranks lowest.
        .fold(None::<Jdk>, |best, jdk| match best {
            Some(best) if best.major >= jdk.major => Some(best),
            _ => Some(jdk),
        });
    match best {
        Some(jdk) => done(JdkChoice::Found(jdk), notes),
        None => done(JdkChoice::Bundled, notes),
    }
}

/// The directory the candidate pattern matched in. A macOS home sits under
/// `<name>.jdk/Contents/Home`, so the name of the JDK is the better parent there.
fn path_parent_for_reason(path: &Path) -> String {
    let parent = if path.ends_with("Contents/Home") {
        path.ancestors().nth(2)
    } else {
        path.parent()
    };
    parent.unwrap_or(path).display().to_string()
}

fn project_sets_jdk_home(settings_file: Option<&Path>) -> bool {
    let Some(text) = settings_file.and_then(|path| fs::read_to_string(path).ok()) else {
        return false;
    };
    // The file is JSON with comments, so a search is more tolerant than a parse.
    text.lines()
        .any(|line| !line.trim_start().starts_with("//") && line.contains("\"jdk_home\""))
}

fn is_jdk_dir(path: &Path) -> bool {
    path.join("release").is_file() || path.join("bin").join("java").is_file()
}

fn describe(home: PathBuf, reason: String) -> Jdk {
    let version = fs::read_to_string(home.join("release"))
        .ok()
        .and_then(|text| release_version(&text));
    let major = version.as_deref().and_then(major_version);
    Jdk {
        home,
        version,
        major,
        reason,
    }
}

/// The `JAVA_VERSION` of a JDK's `release` file, without quotes.
pub fn release_version(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.trim().strip_prefix("JAVA_VERSION")?;
        let value = value.trim_start().strip_prefix('=')?;
        let value = value.trim().trim_matches('"').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// The major version of a Java version string: `21.0.2` is 21, `1.8.0_292` is 8, `17` is 17 and
/// `22-ea` is 22.
pub fn major_version(version: &str) -> Option<u32> {
    let leading_number = |text: &str| -> Option<u32> {
        let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    match version.strip_prefix("1.") {
        Some(rest) => leading_number(rest),
        None => leading_number(version),
    }
}

/// Paths matching `pattern`, sorted. Only `*` inside a component is special.
fn expand_pattern(pattern: &str) -> Vec<PathBuf> {
    use std::path::Component;

    let mut current: Vec<PathBuf> = vec![PathBuf::new()];
    for component in Path::new(pattern).components() {
        let mut next = Vec::new();
        match component {
            Component::Normal(name) if name.to_string_lossy().contains('*') => {
                let name = name.to_string_lossy();
                for dir in &current {
                    let Ok(entries) = fs::read_dir(dir) else {
                        continue;
                    };
                    let mut found: Vec<PathBuf> = entries
                        .flatten()
                        .filter(|entry| wildcard_match(&name, &entry.file_name().to_string_lossy()))
                        .map(|entry| entry.path())
                        .collect();
                    found.sort();
                    next.extend(found);
                }
            }
            other => {
                for dir in &current {
                    next.push(dir.join(other.as_os_str()));
                }
            }
        }
        current = next;
    }
    current.retain(|path| path.is_dir());
    current
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((prefix, rest)) => {
            let Some(text) = text.strip_prefix(prefix) else {
                return false;
            };
            (0..=text.len())
                .filter(|index| text.is_char_boundary(*index))
                .any(|index| wildcard_match(rest, &text[index..]))
        }
    }
}

const BUILD_FILES: [&str; 4] = [
    "build.gradle.kts",
    "settings.gradle.kts",
    "build.gradle",
    "settings.gradle",
];

/// The first Gradle toolchain the project declares: root build files, then `buildSrc/` and
/// `build-logic/`, then `gradle/gradle-daemon-jvm.properties`.
pub fn find_toolchain(root: &Path) -> Option<Toolchain> {
    let mut files: Vec<PathBuf> = build_files_in(root, false);
    for dir in ["buildSrc", "build-logic"] {
        let dir = root.join(dir);
        files.extend(build_files_in(&dir, true));
        if let Ok(entries) = fs::read_dir(&dir) {
            let mut subdirs: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            subdirs.sort();
            for subdir in subdirs {
                files.extend(build_files_in(&subdir, true));
            }
        }
    }
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        if let Some(version) = toolchain_in_build_file(&text) {
            return Some(Toolchain {
                version,
                source: relative_name(root, &file),
            });
        }
    }
    let properties = root.join("gradle").join("gradle-daemon-jvm.properties");
    let version = toolchain_in_properties(&fs::read_to_string(&properties).ok()?)?;
    Some(Toolchain {
        version,
        source: relative_name(root, &properties),
    })
}

fn relative_name(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The build files of one project directory, and with `convention` also the precompiled script
/// plugins under `src/main/{kotlin,groovy}`.
fn build_files_in(dir: &Path, convention: bool) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = BUILD_FILES
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.is_file())
        .collect();
    if convention {
        for language in ["kotlin", "groovy"] {
            let Ok(entries) = fs::read_dir(dir.join("src").join("main").join(language)) else {
                continue;
            };
            let mut scripts: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name().is_some_and(|name| {
                        let name = name.to_string_lossy();
                        name.ends_with(".gradle.kts") || name.ends_with(".gradle")
                    })
                })
                .collect();
            scripts.sort();
            files.extend(scripts);
        }
    }
    files
}

/// `jvmToolchain(21)`, `jvmToolchain 21` and `JavaLanguageVersion.of(21)` (digits may be quoted).
fn toolchain_in_build_file(text: &str) -> Option<u32> {
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with("//") || line.starts_with('*') || line.starts_with("/*") {
            continue;
        }
        for marker in ["jvmToolchain", "JavaLanguageVersion.of"] {
            let mut rest = line;
            while let Some(index) = rest.find(marker) {
                rest = &rest[index + marker.len()..];
                let argument = rest
                    .trim_start_matches([' ', '\t', '('])
                    .trim_start_matches(['"', '\'']);
                let digits: String = argument.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(version) = digits.parse() {
                    return Some(version);
                }
            }
        }
    }
    None
}

/// `toolchainVersion=21` in `gradle/gradle-daemon-jvm.properties`.
fn toolchain_in_properties(text: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_prefix("toolchainVersion")?;
        let value = value.trim_start().strip_prefix(['=', ':'])?;
        value.trim().parse().ok()
    })
}

/// The server runtime line of `kotlin: show jdk`: the `runtime` entry of `servers.toml` inside the
/// installed server, else `JAVA_HOME`.
pub fn server_runtime(
    server: KotlinServerKind,
    install_root: &Path,
    manifest: Option<&Manifest>,
    java_home: Option<&OsString>,
) -> String {
    let runtime = manifest
        .zip(current_platform())
        .and_then(|(manifest, platform)| manifest.entry(server, platform).ok())
        .and_then(|entry| {
            let runtime = entry.runtime.as_ref()?;
            Some(version_dir(install_root, server, &entry.version).join(runtime))
        });
    match (runtime, java_home) {
        (Some(path), _) => format!("{} (bundled with {})", path.display(), server_name(server)),
        (None, Some(java_home)) if !java_home.is_empty() => {
            format!("{} (JAVA_HOME)", PathBuf::from(java_home).display())
        }
        _ => "unknown (no runtime in servers.toml and JAVA_HOME is not set)".into(),
    }
}

/// What `kotlin: show jdk` needs for one worktree.
#[derive(Clone, Debug)]
pub struct JdkQuery {
    pub inputs: JdkInputs,
    pub server: KotlinServerKind,
    pub install_root: PathBuf,
}

/// The detection inputs of a worktree: its `kotlin.jdk_home` setting (project over profile over
/// user) and the process environment. Reads settings, so call it on the main thread; run
/// [`detect`] anywhere.
pub fn inputs(root: &Path, worktree_id: WorktreeId, cx: &App) -> JdkInputs {
    let setting = KotlinSettings::get(
        Some(SettingsLocation {
            worktree_id,
            path: RelPath::empty(),
        }),
        cx,
    )
    .jdk_home
    .clone();
    JdkInputs::from_environment(root, setting)
}

/// Reads the settings for a worktree on the main thread; run [`JdkQuery::report`] anywhere.
pub fn query(root: &Path, worktree_id: WorktreeId, dirs: &KotlinDirs, cx: &App) -> JdkQuery {
    let server = cx
        .try_global::<crate::KotlinRegistry>()
        .and_then(|registry| registry.manager(root))
        .and_then(|manager| manager.read(cx).worktree(worktree_id).map(|w| w.effective))
        .unwrap_or_else(|| resolve(&dirs.state_dir, root, worktree_id, cx).server);
    JdkQuery {
        inputs: inputs(root, worktree_id, cx),
        server,
        install_root: dirs.install_root.clone(),
    }
}

impl JdkQuery {
    pub fn detect(&self) -> Detection {
        detect(&self.inputs)
    }

    /// The text of one worktree's section of the "Kotlin JDK" buffer.
    pub fn report(&self, manifest: Option<&Manifest>) -> String {
        let detection = self.detect();
        let mut text = format!("{}\n", self.inputs.root.display());
        text.push_str(&import_section(&detection));
        text.push_str(&format!(
            "Server runtime ({}): {}\n",
            server_name(self.server),
            server_runtime(
                self.server,
                &self.install_root,
                manifest,
                self.inputs.java_home.as_ref(),
            )
        ));
        for note in &detection.notes {
            text.push_str(&format!("Note: {note}\n"));
        }
        text
    }
}

/// The import JDK lines: path, version and reason, or why there is none.
pub fn import_section(detection: &Detection) -> String {
    let declared = detection
        .toolchain
        .as_ref()
        .map(|toolchain| {
            format!(
                "Gradle toolchain: {} in {}\n",
                toolchain.version, toolchain.source
            )
        })
        .unwrap_or_default();
    match &detection.choice {
        JdkChoice::Found(jdk) => format!(
            "Import JDK: {}\nVersion: {}\nReason: {}\n{declared}",
            jdk.home.display(),
            jdk.version.as_deref().unwrap_or("unknown"),
            jdk.reason,
        ),
        JdkChoice::Unmet(_) => format!(
            "Import JDK: none\nReason: {}\n{declared}Fix: {SET_JDK_HOME_LABEL}\n",
            detection.failure_reason().unwrap_or_default()
        ),
        JdkChoice::Bundled => format!(
            "Import JDK: the bundled runtime of the server\nReason: no Gradle toolchain is declared and no JDK was found\n{declared}"
        ),
    }
}

/// The whole "Kotlin JDK" buffer for the given worktrees.
pub fn report(queries: &[JdkQuery], manifest: Option<&Manifest>) -> String {
    if queries.is_empty() {
        return "No Kotlin project is open.\n".into();
    }
    queries
        .iter()
        .map(|query| query.report(manifest))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn jdk(root: &Path, relative: &str, version: &str) -> PathBuf {
        write(
            root,
            &format!("{relative}/release"),
            &format!("IMPLEMENTOR=\"x\"\nJAVA_VERSION=\"{version}\"\nOS_NAME=\"Linux\"\n"),
        );
        root.join(relative)
    }

    struct Tree {
        dir: tempfile::TempDir,
    }

    impl Tree {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn project(&self) -> PathBuf {
            let project = self.path().join("project");
            fs::create_dir_all(&project).unwrap();
            project
        }

        fn inputs(&self) -> JdkInputs {
            let project = self.project();
            JdkInputs {
                setting: None,
                project_settings: Some(project.join(".bongos/settings.json")),
                root: project,
                java_home: None,
                candidates: vec![
                    format!("{}/home/.sdkman/candidates/java/*", self.path().display()),
                    format!("{}/usr/lib/jvm/*", self.path().display()),
                ],
            }
        }
    }

    fn found(detection: &Detection) -> &Jdk {
        detection
            .jdk()
            .unwrap_or_else(|| panic!("no JDK found: {detection:?}"))
    }

    #[test]
    fn test_release_and_major_versions() {
        assert_eq!(
            release_version("A=1\nJAVA_VERSION=\"21.0.2\"\n"),
            Some("21.0.2".into())
        );
        assert_eq!(release_version("JAVA_VERSION = 17\n"), Some("17".into()));
        assert_eq!(release_version("JAVA_VERSION=\"\"\n"), None);
        assert_eq!(release_version("JAVA_RUNTIME_VERSION=\"21\"\n"), None);
        assert_eq!(major_version("21.0.2"), Some(21));
        assert_eq!(major_version("1.8.0_292"), Some(8));
        assert_eq!(major_version("17"), Some(17));
        assert_eq!(major_version("22-ea"), Some(22));
        assert_eq!(major_version("x"), None);
    }

    #[test]
    fn test_setting_wins_over_everything() {
        let tree = Tree::new();
        let mut inputs = tree.inputs();
        write(
            &inputs.root,
            "build.gradle.kts",
            "kotlin { jvmToolchain(21) }",
        );
        let chosen = jdk(tree.path(), "chosen", "11.0.1");
        jdk(tree.path(), "usr/lib/jvm/java-21", "21.0.2");
        inputs.java_home = Some(tree.path().join("usr/lib/jvm/java-21").into());
        inputs.setting = Some(chosen.clone());

        let detection = detect(&inputs);
        let jdk = found(&detection);
        assert_eq!(jdk.home, chosen);
        assert_eq!(jdk.version.as_deref(), Some("11.0.1"));
        assert_eq!(jdk.reason, "kotlin.jdk_home in settings");
        // The toolchain is still reported, even though the setting decided.
        assert_eq!(detection.toolchain.unwrap().version, 21);
    }

    #[test]
    fn test_setting_names_project_settings_and_resolves_relative_to_the_root() {
        let tree = Tree::new();
        let mut inputs = tree.inputs();
        write(
            &inputs.root,
            ".bongos/settings.json",
            "{\n  // JDK\n  \"kotlin\": { \"jdk_home\": \"tools/jdk\" }\n}\n",
        );
        let home = jdk(&inputs.root, "tools/jdk", "17.0.9");
        inputs.setting = Some(PathBuf::from("tools/jdk"));

        let detection = detect(&inputs);
        let jdk = found(&detection);
        assert_eq!(jdk.home, home);
        assert_eq!(jdk.reason, "kotlin.jdk_home in project settings");
    }

    #[test]
    fn test_missing_setting_is_noted_and_skipped() {
        let tree = Tree::new();
        let mut inputs = tree.inputs();
        inputs.setting = Some(PathBuf::from("nowhere"));
        jdk(tree.path(), "usr/lib/jvm/java-17", "17.0.9");

        let detection = detect(&inputs);
        assert!(found(&detection).reason.starts_with("found in"));
        assert_eq!(detection.notes.len(), 1);
        assert!(detection.notes[0].contains("nowhere"));
    }

    #[test]
    fn test_toolchain_beats_java_home_and_candidates() {
        let tree = Tree::new();
        let mut inputs = tree.inputs();
        write(
            &inputs.root,
            "build.gradle.kts",
            "kotlin {\n    jvmToolchain(21)\n}\n",
        );
        let home17 = jdk(tree.path(), "usr/lib/jvm/java-17", "17.0.9");
        let home21 = jdk(tree.path(), "usr/lib/jvm/java-21", "21.0.2");
        inputs.java_home = Some(home17.into());

        let detection = detect(&inputs);
        let jdk = found(&detection);
        assert_eq!(jdk.home, home21);
        assert_eq!(jdk.reason, "toolchain 21 in build.gradle.kts");
        assert_eq!(jdk.version.as_deref(), Some("21.0.2"));
    }

    #[test]
    fn test_toolchain_can_be_met_by_java_home() {
        let tree = Tree::new();
        let mut inputs = tree.inputs();
        write(&inputs.root, "build.gradle.kts", "jvmToolchain(17)");
        let home17 = jdk(tree.path(), "elsewhere/jdk", "17.0.9");
        inputs.java_home = Some(home17.clone().into());
        let detection = detect(&inputs);
        assert_eq!(found(&detection).home, home17);
        assert_eq!(found(&detection).reason, "toolchain 17 in build.gradle.kts");
    }

    #[test]
    fn test_toolchain_file_shapes() {
        let shapes: &[(&str, &str, u32, &str)] = &[
            (
                "build.gradle.kts",
                "kotlin { jvmToolchain(21) }",
                21,
                "build.gradle.kts",
            ),
            (
                "build.gradle.kts",
                "java {\n  toolchain {\n    languageVersion.set(JavaLanguageVersion.of(17))\n  }\n}",
                17,
                "build.gradle.kts",
            ),
            (
                "build.gradle.kts",
                "java.toolchain.languageVersion = JavaLanguageVersion.of(\"11\")",
                11,
                "build.gradle.kts",
            ),
            (
                "settings.gradle.kts",
                "plugins { id(\"foojay\") }\njava { toolchain { languageVersion.set(JavaLanguageVersion.of(25)) } }",
                25,
                "settings.gradle.kts",
            ),
            (
                "build.gradle",
                "kotlin { jvmToolchain 17 }",
                17,
                "build.gradle",
            ),
            (
                "buildSrc/build.gradle.kts",
                "kotlin { jvmToolchain(8) }",
                8,
                "buildSrc/build.gradle.kts",
            ),
            (
                "buildSrc/src/main/kotlin/conventions.gradle.kts",
                "kotlin { jvmToolchain(21) }",
                21,
                "buildSrc/src/main/kotlin/conventions.gradle.kts",
            ),
            (
                "build-logic/build.gradle.kts",
                "kotlin { jvmToolchain(17) }",
                17,
                "build-logic/build.gradle.kts",
            ),
            (
                "build-logic/convention/build.gradle.kts",
                "kotlin { jvmToolchain(23) }",
                23,
                "build-logic/convention/build.gradle.kts",
            ),
            (
                "build-logic/settings.gradle.kts",
                "java.toolchain.languageVersion.set(JavaLanguageVersion.of(21))",
                21,
                "build-logic/settings.gradle.kts",
            ),
            (
                "gradle/gradle-daemon-jvm.properties",
                "#This file is generated by updateDaemonJvm\ntoolchainVendor=ADOPTIUM\ntoolchainVersion=21\n",
                21,
                "gradle/gradle-daemon-jvm.properties",
            ),
        ];
        for (file, text, version, source) in shapes {
            let tree = Tree::new();
            let project = tree.project();
            write(&project, file, text);
            let toolchain = find_toolchain(&project)
                .unwrap_or_else(|| panic!("no toolchain found in {file}: {text}"));
            assert_eq!(
                (toolchain.version, toolchain.source.as_str()),
                (*version, *source),
                "{file}"
            );
        }
    }

    #[test]
    fn test_toolchain_file_order_and_comments() {
        let tree = Tree::new();
        let project = tree.project();
        write(
            &project,
            "gradle/gradle-daemon-jvm.properties",
            "toolchainVersion=8\n",
        );
        write(
            &project,
            "buildSrc/build.gradle.kts",
            "kotlin { jvmToolchain(11) }",
        );
        assert_eq!(find_toolchain(&project).unwrap().version, 11);
        write(
            &project,
            "settings.gradle.kts",
            "// jvmToolchain(99)\nkotlin { jvmToolchain(17) }",
        );
        let toolchain = find_toolchain(&project).unwrap();
        assert_eq!(
            (toolchain.version, toolchain.source.as_str()),
            (17, "settings.gradle.kts")
        );
        write(
            &project,
            "build.gradle.kts",
            "kotlin { jvmToolchain(libs.versions.jdk.get().toInt()) }",
        );
        assert_eq!(find_toolchain(&project).unwrap().version, 17);
        assert_eq!(find_toolchain(tree.path()), None);
    }

    #[test]
    fn test_unmet_toolchain_fails_with_a_menu_entry() {
        let tree = Tree::new();
        let mut inputs = tree.inputs();
        write(
            &inputs.root,
            "build.gradle.kts",
            "kotlin { jvmToolchain(21) }",
        );
        // A JDK is there, but it is not 21; so is JAVA_HOME.
        let home17 = jdk(tree.path(), "usr/lib/jvm/java-17", "17.0.9");
        inputs.java_home = Some(home17.into());

        let detection = detect(&inputs);
        assert_eq!(
            detection.choice,
            JdkChoice::Unmet(Toolchain {
                version: 21,
                source: "build.gradle.kts".into()
            })
        );
        assert_eq!(
            detection.failure_reason().as_deref(),
            Some("no JDK for Gradle import (toolchain 21)")
        );
        assert_eq!(
            crate::ServerStatus::Failed {
                reason: detection.failure_reason().unwrap()
            }
            .log_text(),
            "failed: no JDK for Gradle import (toolchain 21)"
        );
        let entry = detection.menu_entry().expect("a menu entry");
        assert_eq!(entry.label, "Set kotlin.jdk_home");
        assert!(entry.action.partial_eq(&zed_actions::OpenSettingsFile));

        let section = import_section(&detection);
        assert!(section.contains("Import JDK: none"), "{section}");
        assert!(section.contains("toolchain 21"), "{section}");
    }

    #[test]
    fn test_no_toolchain_and_no_jdk_uses_the_bundled_runtime() {
        let tree = Tree::new();
        let mut inputs = tree.inputs();
        // A JAVA_HOME that is not a JDK does not count.
        inputs.java_home = Some(tree.path().join("not-a-jdk").into());
        let detection = detect(&inputs);
        assert_eq!(detection.choice, JdkChoice::Bundled);
        assert_eq!(detection.failure_reason(), None);
        assert!(detection.menu_entry().is_none());
        let section = import_section(&detection);
        assert!(
            section.contains("Import JDK: the bundled runtime of the server"),
            "{section}"
        );
    }

    #[test]
    fn test_java_home_beats_candidates_without_a_toolchain() {
        let tree = Tree::new();
        let mut inputs = tree.inputs();
        jdk(tree.path(), "usr/lib/jvm/java-21", "21.0.2");
        let home8 = jdk(tree.path(), "mine", "1.8.0_292");
        inputs.java_home = Some(home8.clone().into());
        let detection = detect(&inputs);
        let jdk = found(&detection);
        assert_eq!(jdk.home, home8);
        assert_eq!(jdk.reason, "JAVA_HOME");
        assert_eq!(jdk.major, Some(8));
    }

    #[test]
    fn test_candidates_pick_the_highest_version() {
        let tree = Tree::new();
        let inputs = tree.inputs();
        jdk(
            tree.path(),
            "home/.sdkman/candidates/java/17.0.9-tem",
            "17.0.9",
        );
        let sdk21 = jdk(
            tree.path(),
            "home/.sdkman/candidates/java/21.0.2-tem",
            "21.0.2",
        );
        jdk(tree.path(), "usr/lib/jvm/java-11-openjdk", "11.0.22");
        // A directory with no release file and no java binary is not a JDK.
        fs::create_dir_all(tree.path().join("usr/lib/jvm/empty")).unwrap();
        // The `current` link of SDKMAN points at a JDK that is also listed; the first wins a tie.
        let detection = detect(&inputs);
        let jdk = found(&detection);
        assert_eq!(jdk.home, sdk21);
        assert_eq!(jdk.major, Some(21));
        assert_eq!(
            jdk.reason,
            format!(
                "found in {}",
                tree.path().join("home/.sdkman/candidates/java").display()
            )
        );
    }

    #[test]
    fn test_usr_lib_jvm_is_searched() {
        let tree = Tree::new();
        let inputs = tree.inputs();
        let home = jdk(tree.path(), "usr/lib/jvm/java-17-openjdk-amd64", "17.0.9");
        let detection = detect(&inputs);
        assert_eq!(found(&detection).home, home);
        assert!(found(&detection).reason.ends_with("usr/lib/jvm"));
    }

    #[test]
    fn test_candidate_patterns_match_components_with_wildcards() {
        let tree = Tree::new();
        let brew_a = jdk(
            tree.path(),
            "opt/homebrew/opt/openjdk/libexec/openjdk.jdk/Contents/Home",
            "23.0.1",
        );
        let brew_b = jdk(
            tree.path(),
            "opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home",
            "17.0.9",
        );
        jdk(
            tree.path(),
            "opt/homebrew/opt/other/libexec/openjdk.jdk/Contents/Home",
            "99",
        );
        let found = expand_pattern(&format!(
            "{}/opt/homebrew/opt/openjdk*/libexec/openjdk.jdk/Contents/Home",
            tree.path().display()
        ));
        assert_eq!(found, vec![brew_a.clone(), brew_b]);

        let library = jdk(
            tree.path(),
            "Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home",
            "21.0.2",
        );
        assert_eq!(
            expand_pattern(&format!(
                "{}/Library/Java/JavaVirtualMachines/*/Contents/Home",
                tree.path().display()
            )),
            vec![library]
        );

        let mut inputs = tree.inputs();
        inputs.candidates = vec![format!(
            "{}/opt/homebrew/opt/openjdk*/libexec/openjdk.jdk/Contents/Home",
            tree.path().display()
        )];
        let detection = detect(&inputs);
        assert_eq!(detection.jdk().unwrap().home, brew_a);
        // The macOS reason names the JDK, not "Home".
        assert!(
            detection
                .jdk()
                .unwrap()
                .reason
                .ends_with("opt/homebrew/opt/openjdk/libexec/openjdk.jdk")
        );
    }

    #[test]
    fn test_default_candidate_patterns() {
        assert_eq!(
            default_candidate_patterns(Some(Path::new("/home/me"))),
            vec![
                "/home/me/.sdkman/candidates/java/*",
                "/usr/lib/jvm/*",
                "/opt/homebrew/opt/openjdk*/libexec/openjdk.jdk/Contents/Home",
                "/Library/Java/JavaVirtualMachines/*/Contents/Home",
            ]
        );
        assert_eq!(default_candidate_patterns(None).len(), 3);
    }

    #[test]
    fn test_wildcards() {
        assert!(wildcard_match("openjdk*", "openjdk"));
        assert!(wildcard_match("openjdk*", "openjdk@17"));
        assert!(!wildcard_match("openjdk*", "other"));
        assert!(wildcard_match("*-tem", "21.0.2-tem"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXcYYb"));
        assert!(wildcard_match("*", ""));
    }

    #[test]
    fn test_server_runtime_line() {
        let manifest = Manifest::embedded();
        let root = Path::new("/data/kotlin");
        let line = server_runtime(KotlinServerKind::KotlinLsp, root, Some(&manifest), None);
        if current_platform().is_some() {
            assert!(line.starts_with("/data/kotlin/kotlin-lsp/"), "{line}");
            assert!(line.contains("jbr"), "{line}");
            assert!(line.ends_with("(bundled with kotlin-lsp)"), "{line}");
        }
        let java_home = OsString::from("/opt/jdk");
        let line = server_runtime(KotlinServerKind::KotlinLsp, root, None, Some(&java_home));
        assert_eq!(line, "/opt/jdk (JAVA_HOME)");
        assert!(
            server_runtime(KotlinServerKind::KotlinLsp, root, None, None).starts_with("unknown")
        );
    }

    #[test]
    fn test_report_text() {
        let tree = Tree::new();
        let inputs = tree.inputs();
        write(
            &inputs.root,
            "build.gradle.kts",
            "kotlin { jvmToolchain(21) }",
        );
        jdk(tree.path(), "usr/lib/jvm/java-21", "21.0.2");
        let query = JdkQuery {
            inputs,
            server: KotlinServerKind::KotlinLsp,
            install_root: tree.path().join("kotlin"),
        };
        let text = report(&[query], Some(&Manifest::embedded()));
        assert!(text.contains("Version: 21.0.2"), "{text}");
        assert!(
            text.contains("Reason: toolchain 21 in build.gradle.kts"),
            "{text}"
        );
        assert!(text.contains("Server runtime (kotlin-lsp):"), "{text}");
        assert_eq!(report(&[], None), "No Kotlin project is open.\n");
    }
}
