//! Which Kotlin server a project gets: the per-project override in `kotlin-servers.json` over the
//! merged `kotlin.server` setting (project, active profile, user, default). Every function takes the
//! state directory as an argument; the app passes `paths::state_dir()` once.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use gpui::App;
use serde::{Deserialize, Serialize};
use settings::{
    KotlinServerKind, KotlinSettingsContent, ProfileBase, Settings, SettingsLocation,
    SettingsStore, UserSettingsContentExt, WorktreeId,
};
use util::rel_path::RelPath;

use crate::KotlinSettings;

const FILE_NAME: &str = "kotlin-servers.json";
const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedSource {
    ProjectOverride,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub server: KotlinServerKind,
    pub source: ResolvedSource,
}

/// The layer that set a value, from highest to lowest precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Override,
    Project,
    Profile,
    Base,
    Default,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayerReport {
    pub server: KotlinServerKind,
    pub server_layer: Layer,
    pub jdk_home: Option<PathBuf>,
    pub jdk_home_layer: Layer,
}

#[derive(Serialize, Deserialize)]
struct OverrideFile {
    version: u32,
    projects: BTreeMap<String, OverrideEntry>,
}

#[derive(Serialize, Deserialize)]
struct OverrideEntry {
    server: KotlinServerKind,
    set: String,
}

impl Default for OverrideFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            projects: BTreeMap::new(),
        }
    }
}

fn file_path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

/// The key for a worktree root. A root that cannot be canonicalized (it no longer exists) keeps its
/// own spelling, so an entry for a deleted project can still be read and removed.
pub(crate) fn project_key(worktree_root: &Path) -> String {
    fs::canonicalize(worktree_root)
        .unwrap_or_else(|_| worktree_root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn load(state_dir: &Path) -> Result<OverrideFile> {
    let path = file_path(state_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(OverrideFile::default());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let file: OverrideFile =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if file.version != VERSION {
        bail!(
            "{} has unsupported version {}",
            path.display(),
            file.version
        );
    }
    Ok(file)
}

/// Writes the new contents next to the target; `commit_staged` renames it into place.
pub(crate) fn stage(path: &Path, contents: &[u8]) -> Result<PathBuf> {
    let staged = path.with_extension("json.tmp");
    let mut file =
        fs::File::create(&staged).with_context(|| format!("creating {}", staged.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(staged)
}

pub(crate) fn commit_staged(staged: &Path, path: &Path) -> Result<()> {
    fs::rename(staged, path).with_context(|| format!("replacing {}", path.display()))
}

fn store(state_dir: &Path, file: &OverrideFile) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = file_path(state_dir);
    let staged = stage(&path, &serde_json::to_vec_pretty(file)?)?;
    commit_staged(&staged, &path)
}

/// The override for a project, or `None` when there is none. An unreadable or corrupt file is an
/// error the caller reports; resolution itself treats it as no override.
pub fn project_override(
    state_dir: &Path,
    worktree_root: &Path,
) -> Result<Option<KotlinServerKind>> {
    let file = load(state_dir)?;
    Ok(file
        .projects
        .get(&project_key(worktree_root))
        .map(|entry| entry.server))
}

pub fn set_project_override(
    state_dir: &Path,
    worktree_root: &Path,
    server: KotlinServerKind,
) -> Result<()> {
    let mut file = load(state_dir)?;
    file.projects.insert(
        project_key(worktree_root),
        OverrideEntry {
            server,
            set: chrono::Utc::now().to_rfc3339(),
        },
    );
    store(state_dir, &file)
}

/// "Use setting": removes the entry, and the file when it was the last one.
pub fn clear_project_override(state_dir: &Path, worktree_root: &Path) -> Result<()> {
    let mut file = load(state_dir)?;
    if file.projects.remove(&project_key(worktree_root)).is_none() {
        return Ok(());
    }
    store(state_dir, &file)
}

pub(crate) fn settings_server(worktree_id: WorktreeId, cx: &App) -> KotlinServerKind {
    KotlinSettings::get(
        Some(SettingsLocation {
            worktree_id,
            path: RelPath::empty(),
        }),
        cx,
    )
    .server
}

pub fn resolve(
    state_dir: &Path,
    worktree_root: &Path,
    worktree_id: WorktreeId,
    cx: &App,
) -> Resolved {
    match project_override(state_dir, worktree_root) {
        Ok(Some(server)) => {
            return Resolved {
                server,
                source: ResolvedSource::ProjectOverride,
            };
        }
        Ok(None) => {}
        Err(error) => log::warn!("ignoring the Kotlin server override: {error:#}"),
    }
    Resolved {
        server: settings_server(worktree_id, cx),
        source: ResolvedSource::Settings,
    }
}

/// The layer whose own content sets the field (`sets`), by walking the layers the settings store
/// merges: project (the worktree root's `.bongos/settings.json`), the active profile, the user's
/// top level (skipped by a profile with `"base": "default"`), then the defaults.
fn settings_layer(
    worktree_id: WorktreeId,
    cx: &App,
    sets: impl Fn(&KotlinSettingsContent) -> bool,
) -> Layer {
    let store = cx.global::<SettingsStore>();
    let project_sets = store
        .local_settings(worktree_id)
        .filter(|(path, _)| path.is_empty())
        .any(|(_, content)| content.kotlin.as_ref().is_some_and(&sets));
    if project_sets {
        return Layer::Project;
    }
    let Some(user) = store.raw_user_settings() else {
        return Layer::Default;
    };
    let profile = user.for_profile(cx);
    if profile.is_some_and(|profile| profile.settings.project.kotlin.as_ref().is_some_and(&sets)) {
        return Layer::Profile;
    }
    let base_applies = profile.is_none_or(|profile| profile.base == ProfileBase::User);
    if base_applies && user.content.project.kotlin.as_ref().is_some_and(&sets) {
        return Layer::Base;
    }
    Layer::Default
}

/// Which layer set each Kotlin value, for `kotlin: show jdk` and the status menu.
pub fn layer_report(
    state_dir: &Path,
    worktree_root: &Path,
    worktree_id: WorktreeId,
    cx: &App,
) -> LayerReport {
    let resolved = resolve(state_dir, worktree_root, worktree_id, cx);
    let server_layer = match resolved.source {
        ResolvedSource::ProjectOverride => Layer::Override,
        ResolvedSource::Settings => {
            settings_layer(worktree_id, cx, |kotlin| kotlin.server.is_some())
        }
    };
    let settings = KotlinSettings::get(
        Some(SettingsLocation {
            worktree_id,
            path: RelPath::empty(),
        }),
        cx,
    );
    LayerReport {
        server: resolved.server,
        server_layer,
        jdk_home: settings.jdk_home.clone(),
        jdk_home_layer: settings_layer(worktree_id, cx, |kotlin| kotlin.jdk_home.is_some()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext, TestAppContext};
    use settings::{ActiveSettingsProfileName, LocalSettingsKind, LocalSettingsPath};

    fn worktree() -> WorktreeId {
        WorktreeId::from_usize(1)
    }

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            SettingsStore::observe_active_settings_profile_name(cx).detach();
            KotlinSettings::register(cx);
        });
    }

    fn set_user(cx: &mut TestAppContext, json: &str) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.set_user_settings(json, cx).unwrap();
            });
        });
    }

    fn set_project(cx: &mut TestAppContext, json: &str) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        worktree(),
                        LocalSettingsPath::InWorktree(RelPath::empty_arc()),
                        LocalSettingsKind::Settings,
                        Some(json),
                        cx,
                    )
                    .unwrap();
            });
        });
    }

    fn clear_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.clear_local_settings(worktree(), cx).unwrap();
            });
        });
    }

    fn set_profile(cx: &mut TestAppContext, name: &str) {
        cx.update(|cx| cx.set_global(ActiveSettingsProfileName(name.to_string())));
    }

    fn resolved(cx: &mut TestAppContext, state: &Path, root: &Path) -> Resolved {
        cx.update(|cx| resolve(state, root, worktree(), cx))
    }

    fn report(cx: &mut TestAppContext, state: &Path, root: &Path) -> LayerReport {
        cx.update(|cx| layer_report(state, root, worktree(), cx))
    }

    const USER: &str = r#"{
        "kotlin": { "server": "kotlin-lsp", "jdk_home": "/opt/jdk-17" },
        "profiles": {
            "work": { "settings": { "kotlin": { "server": "intellij" } } },
            "clean": { "base": "default", "settings": {} }
        }
    }"#;

    #[gpui::test]
    fn test_override_beats_project_profile_base_and_default(cx: &mut TestAppContext) {
        setup(cx);
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (state, root) = (state.path(), root.path());

        let default = resolved(cx, state, root);
        assert_eq!(default.server, KotlinServerKind::KotlinLsp);
        assert_eq!(report(cx, state, root).server_layer, Layer::Default);

        set_user(cx, USER);
        assert_eq!(report(cx, state, root).server_layer, Layer::Base);

        set_profile(cx, "work");
        let profile = resolved(cx, state, root);
        assert_eq!(profile.server, KotlinServerKind::Intellij);
        assert_eq!(report(cx, state, root).server_layer, Layer::Profile);

        set_project(cx, r#"{ "kotlin": { "server": "kotlin-lsp" } }"#);
        let project = resolved(cx, state, root);
        assert_eq!(project.server, KotlinServerKind::KotlinLsp);
        assert_eq!(project.source, ResolvedSource::Settings);
        assert_eq!(report(cx, state, root).server_layer, Layer::Project);

        set_project_override(state, root, KotlinServerKind::Intellij).unwrap();
        let overridden = resolved(cx, state, root);
        assert_eq!(overridden.server, KotlinServerKind::Intellij);
        assert_eq!(overridden.source, ResolvedSource::ProjectOverride);
        assert_eq!(report(cx, state, root).server_layer, Layer::Override);

        clear_project_override(state, root).unwrap();
        assert_eq!(
            resolved(cx, state, root).server,
            KotlinServerKind::KotlinLsp
        );
    }

    #[gpui::test]
    fn test_use_setting_removes_the_entry(cx: &mut TestAppContext) {
        setup(cx);
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        set_project_override(state.path(), root.path(), KotlinServerKind::Intellij).unwrap();
        set_project_override(state.path(), other.path(), KotlinServerKind::Intellij).unwrap();
        clear_project_override(state.path(), root.path()).unwrap();
        assert_eq!(project_override(state.path(), root.path()).unwrap(), None);
        assert_eq!(
            project_override(state.path(), other.path()).unwrap(),
            Some(KotlinServerKind::Intellij)
        );
        assert_eq!(
            resolved(cx, state.path(), root.path()).source,
            ResolvedSource::Settings
        );
        let text = fs::read_to_string(file_path(state.path())).unwrap();
        assert!(!text.contains(&project_key(root.path())));
    }

    #[gpui::test]
    fn test_profile_with_default_base_and_project_removal(cx: &mut TestAppContext) {
        setup(cx);
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        set_user(cx, USER);
        set_profile(cx, "clean");
        let default_based = report(cx, state.path(), root.path());
        assert_eq!(default_based.server_layer, Layer::Default);
        assert_eq!(default_based.jdk_home, None);
        assert_eq!(default_based.jdk_home_layer, Layer::Default);

        set_profile(cx, "none-such");
        let base = report(cx, state.path(), root.path());
        assert_eq!(base.jdk_home, Some(PathBuf::from("/opt/jdk-17")));
        assert_eq!(base.jdk_home_layer, Layer::Base);

        set_project(cx, r#"{ "kotlin": { "jdk_home": "/opt/jdk-21" } }"#);
        assert_eq!(
            report(cx, state.path(), root.path()).jdk_home_layer,
            Layer::Project
        );
        assert_eq!(
            report(cx, state.path(), root.path()).server_layer,
            Layer::Base
        );
        clear_project(cx);
        assert_eq!(
            report(cx, state.path(), root.path()).jdk_home_layer,
            Layer::Base
        );
    }

    #[test]
    fn test_keys_are_canonical_worktree_roots() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("project");
        fs::create_dir(&nested).unwrap();
        let alias = nested.join("..").join("project");
        set_project_override(state.path(), &alias, KotlinServerKind::Intellij).unwrap();
        assert_eq!(
            project_override(state.path(), &nested).unwrap(),
            Some(KotlinServerKind::Intellij)
        );
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&nested, &link).unwrap();
        assert_eq!(
            project_override(state.path(), &link).unwrap(),
            Some(KotlinServerKind::Intellij)
        );
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(file_path(state.path())).unwrap()).unwrap();
        assert_eq!(value["version"], 1);
        let keys: Vec<_> = value["projects"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [&fs::canonicalize(&nested)
                .unwrap()
                .to_string_lossy()
                .into_owned()]
        );
        assert_eq!(value["projects"][keys[0]]["server"], "intellij");
        assert!(
            chrono::DateTime::parse_from_rfc3339(
                value["projects"][keys[0]]["set"].as_str().unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn test_crash_mid_write_leaves_the_original_intact() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        set_project_override(state.path(), root.path(), KotlinServerKind::Intellij).unwrap();
        let path = file_path(state.path());
        let original = fs::read(&path).unwrap();

        let other = tempfile::tempdir().unwrap();
        let mut file = load(state.path()).unwrap();
        file.projects.insert(
            project_key(other.path()),
            OverrideEntry {
                server: KotlinServerKind::KotlinLsp,
                set: "2026-01-01T00:00:00+00:00".into(),
            },
        );
        // The process dies after staging and before the rename.
        let staged = stage(&path, &serde_json::to_vec_pretty(&file).unwrap()).unwrap();

        assert!(staged.exists());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            project_override(state.path(), root.path()).unwrap(),
            Some(KotlinServerKind::Intellij)
        );
        assert_eq!(project_override(state.path(), other.path()).unwrap(), None);

        set_project_override(state.path(), other.path(), KotlinServerKind::KotlinLsp).unwrap();
        assert!(!staged.exists());
        assert_eq!(
            project_override(state.path(), other.path()).unwrap(),
            Some(KotlinServerKind::KotlinLsp)
        );
        assert_eq!(
            project_override(state.path(), root.path()).unwrap(),
            Some(KotlinServerKind::Intellij)
        );
    }

    #[gpui::test]
    fn test_corrupt_or_foreign_file_is_no_override(cx: &mut TestAppContext) {
        setup(cx);
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(file_path(state.path()), "{ not json").unwrap();
        assert!(project_override(state.path(), root.path()).is_err());
        assert_eq!(
            resolved(cx, state.path(), root.path()).source,
            ResolvedSource::Settings
        );

        fs::write(file_path(state.path()), r#"{"version": 2, "projects": {}}"#).unwrap();
        assert!(project_override(state.path(), root.path()).is_err());
        assert!(
            set_project_override(state.path(), root.path(), KotlinServerKind::Intellij).is_err()
        );
        assert_eq!(
            resolved(cx, state.path(), root.path()).source,
            ResolvedSource::Settings
        );
    }
}
