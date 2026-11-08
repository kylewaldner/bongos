mod adapter;
mod commands;
mod glue;
mod install;
mod installed;
mod intellij;
mod jdk;
mod license;
mod log;
mod manager;
mod manifest;
mod resolve;
mod status;
mod status_item;

use std::{path::PathBuf, sync::Arc};

pub use adapter::{INTELLIJ_DOWNLOAD_HINT, KotlinAdapter, server_command};
pub use commands::{reimport_project, restart_server, switch_server};
pub use glue::{attach_managers, attach_notices, fallback_notice_id};
use gpui::{App, Global, actions};
pub use install::{
    InstallCancelled, InstalledServer, PROGRESS_STEP, ProgressFn, ensure_installed, install,
    spawn_ensure_installed,
};
pub use installed::{MARKER as INSTALLED_MARKER, check_installed, server_name};
pub use intellij::{
    EULA_FILE, LICENSE_TIMEOUT, LicenseOutcome, UNLICENSED_EXIT_CODE, accept_eula,
    bundle_eula_hash, eula_accepted, eula_argument, eula_hash, license_check,
};
pub use jdk::{
    Detection, Jdk, JdkChoice, JdkInputs, JdkQuery, MenuEntry, SET_JDK_HOME_LABEL, Toolchain,
    default_candidate_patterns, detect, find_toolchain, set_jdk_home_entry,
};
use language::{LanguageName, LanguageRegistry};
pub use license::{
    EULA_TITLE, LICENSE_LOST_EXIT_CODES, LICENSE_LOST_SIGNALS, LICENSE_STATUS_TITLE, ShowText,
    is_license_lost_exit, is_license_lost_signal, license_activate, license_status, license_trial,
    review_intellij_eula,
};
pub use log::{KotlinLog, MAX_LOG_BYTES, StderrMirror};
pub use manager::{
    ADAPTER_NAME, CRASH_LOOP_REASON, CRASH_WINDOW, EXPIRED_REASON, Fallback, FallbackReason,
    KotlinEvent, KotlinHost, KotlinProjectManager, KotlinRegistry, KotlinWorktreeState,
    ServerStatus, fallback_message,
};
pub use manifest::{ArchiveKind, Manifest, ManifestEntry, OVERRIDE_VAR, current_platform};
pub use resolve::{
    Layer, LayerReport, Resolved, ResolvedSource, clear_project_override, layer_report,
    project_override, resolve, set_project_override,
};
use settings::{KotlinServerKind, RegisterSetting, Settings, SettingsContent};
pub use status::{Command, MenuItem, Snapshot, StatusLine, menu_items, status_line};
pub use status_item::{KotlinStatusItem, cancel_installs, show_logs};

actions!(
    kotlin,
    [
        /// Picks the Kotlin server for this project: kotlin-lsp, the IntelliJ server, or the
        /// setting's.
        SwitchServer,
        /// Restarts the Kotlin server and forgets any fallback and crash history.
        RestartServer,
        /// Reimports the project by restarting the Kotlin server, which imports Gradle on start.
        ReimportProject,
        /// Shows the JDK that imports Gradle projects, why it was chosen, and the server's own
        /// runtime.
        ShowJdk,
        /// Opens `kotlin.log`.
        ShowLogs,
        /// Opens or closes the Kotlin status menu in the status bar.
        ShowStatusMenu,
        /// Cancels the Kotlin server download in progress.
        CancelInstall,
        /// Shows the IntelliJ server's license status.
        LicenseStatus,
        /// Activates an IntelliJ server license in a terminal tab: JetBrains account, activation
        /// code or key, key file, or license server.
        LicenseActivate,
        /// Starts an IntelliJ server trial in a terminal tab.
        LicenseTrial,
        /// Shows the IntelliJ server's license agreement, with a toast to accept it.
        ReviewIntellijEula,
    ]
);

/// What the Kotlin actions need from startup: where things live and the pinned servers.
struct KotlinSetup {
    dirs: KotlinDirs,
    manifest: Arc<Manifest>,
}

impl Global for KotlinSetup {}

/// Everything `kotlin: show jdk` reads on the main thread. `text` does the file reads and can run
/// on any thread.
pub struct ShowJdkRequest {
    queries: Vec<JdkQuery>,
    manifest: Option<Arc<Manifest>>,
}

impl ShowJdkRequest {
    pub fn new(project: &project::Project, cx: &App) -> Self {
        let setup = cx.try_global::<KotlinSetup>();
        let queries = setup
            .map(|setup| {
                project
                    .visible_worktrees(cx)
                    .filter_map(|worktree| {
                        let worktree = worktree.read(cx);
                        worktree.is_local().then(|| {
                            jdk::query(&worktree.abs_path(), worktree.id(), &setup.dirs, cx)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            queries,
            manifest: setup.map(|setup| setup.manifest.clone()),
        }
    }

    /// The contents of the "Kotlin JDK" buffer.
    pub fn text(&self) -> String {
        jdk::report(&self.queries, self.manifest.as_deref())
    }
}

/// The title of the buffer `ShowJdk` opens.
pub const SHOW_JDK_TITLE: &str = "Kotlin JDK";

/// The merged `kotlin` settings. Read them with a `SettingsLocation` so a project's
/// `.bongos/settings.json` applies over the active profile and the user settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct KotlinSettings {
    pub server: KotlinServerKind,
    pub jdk_home: Option<PathBuf>,
    /// Whether bongos may download the IntelliJ server (decision D16).
    pub intellij_download: bool,
}

impl Settings for KotlinSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let kotlin = content
            .project
            .kotlin
            .clone()
            .expect("default.json defines `kotlin`");
        Self {
            server: kotlin.server.expect("default.json defines `kotlin.server`"),
            jdk_home: kotlin.jdk_home.map(PathBuf::from),
            intellij_download: kotlin
                .intellij
                .expect("default.json defines `kotlin.intellij`")
                .download
                .expect("default.json defines `kotlin.intellij.download`"),
        }
    }
}

/// Where bongos keeps Kotlin data. The app passes its `paths` values once; tests pass temp
/// directories, so no test touches the real home.
#[derive(Clone, Debug)]
pub struct KotlinDirs {
    /// Holds `kotlin-servers.json`.
    pub state_dir: PathBuf,
    /// Holds `kotlin.log`.
    pub logs_dir: PathBuf,
    /// Holds `<server>/<version>` for each installed server.
    pub install_root: PathBuf,
}

pub fn init(languages: &LanguageRegistry, dirs: KotlinDirs, cx: &mut App) {
    KotlinSettings::register(cx);
    let manifest = Manifest::load(cfg!(debug_assertions)).unwrap_or_else(|error| {
        ::log::error!("{error:#}; using the built-in Kotlin server manifest");
        Manifest::embedded()
    });
    let manifest = Arc::new(manifest);
    cx.set_global(KotlinSetup {
        dirs: dirs.clone(),
        manifest: manifest.clone(),
    });
    languages.register_lsp_adapter(
        LanguageName::new_static("Kotlin"),
        Arc::new(KotlinAdapter::new(dirs.clone(), manifest)),
    );
    attach_managers(dirs, cx);
    attach_notices(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext, TestAppContext};
    use settings::{
        ActiveSettingsProfileName, LocalSettingsKind, LocalSettingsPath, SettingsLocation,
        SettingsStore, WorktreeId,
    };
    use util::rel_path::RelPath;

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

    fn clear_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.clear_local_settings(worktree(), cx).unwrap();
            });
        });
    }

    fn set_project(cx: &mut TestAppContext, json: Option<&str>) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        worktree(),
                        LocalSettingsPath::InWorktree(RelPath::empty_arc()),
                        LocalSettingsKind::Settings,
                        json,
                        cx,
                    )
                    .unwrap();
            });
        });
    }

    fn set_profile(cx: &mut TestAppContext, name: &str) {
        cx.update(|cx| cx.set_global(ActiveSettingsProfileName(name.to_string())));
    }

    fn global_server(cx: &mut TestAppContext) -> KotlinServerKind {
        cx.update(|cx| KotlinSettings::get_global(cx).server)
    }

    fn project_server(cx: &mut TestAppContext) -> KotlinServerKind {
        cx.update(|cx| {
            KotlinSettings::get(
                Some(SettingsLocation {
                    worktree_id: worktree(),
                    path: RelPath::empty(),
                }),
                cx,
            )
            .server
        })
    }

    const USER_INTELLIJ: &str = r#"{ "kotlin": { "server": "intellij" } }"#;

    #[gpui::test]
    fn test_default_server_and_jdk(cx: &mut TestAppContext) {
        setup(cx);
        let settings = cx.update(|cx| KotlinSettings::get_global(cx).clone());
        assert_eq!(settings.server, KotlinServerKind::KotlinLsp);
        assert_eq!(settings.jdk_home, None);
    }

    fn project_intellij_download(cx: &mut TestAppContext) -> bool {
        cx.update(|cx| {
            KotlinSettings::get(
                Some(SettingsLocation {
                    worktree_id: worktree(),
                    path: RelPath::empty(),
                }),
                cx,
            )
            .intellij_download
        })
    }

    #[gpui::test]
    fn test_intellij_download_is_off_by_default_and_follows_the_layers(cx: &mut TestAppContext) {
        setup(cx);
        assert!(!cx.update(|cx| KotlinSettings::get_global(cx).intellij_download));

        set_user(cx, r#"{ "kotlin": { "intellij": { "download": true } } }"#);
        assert!(cx.update(|cx| KotlinSettings::get_global(cx).intellij_download));
        // Setting only `server` leaves the download choice alone.
        set_user(cx, r#"{ "kotlin": { "server": "intellij" } }"#);
        assert!(!cx.update(|cx| KotlinSettings::get_global(cx).intellij_download));

        set_user(
            cx,
            r#"{
                "kotlin": { "intellij": { "download": true } },
                "profiles": { "locked": { "settings": { "kotlin": { "intellij": { "download": false } } } } }
            }"#,
        );
        assert!(project_intellij_download(cx));
        set_profile(cx, "locked");
        assert!(!project_intellij_download(cx));
        set_project(
            cx,
            Some(r#"{ "kotlin": { "intellij": { "download": true } } }"#),
        );
        assert!(project_intellij_download(cx));
    }

    #[gpui::test]
    fn test_user_top_level_overrides_default(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{ "kotlin": { "server": "intellij", "jdk_home": "/opt/jdk-21" } }"#,
        );
        let settings = cx.update(|cx| KotlinSettings::get_global(cx).clone());
        assert_eq!(settings.server, KotlinServerKind::Intellij);
        assert_eq!(settings.jdk_home, Some(PathBuf::from("/opt/jdk-21")));
    }

    #[gpui::test]
    fn test_no_profiles_key(cx: &mut TestAppContext) {
        setup(cx);
        set_user(cx, USER_INTELLIJ);
        set_profile(cx, "work");
        assert_eq!(global_server(cx), KotlinServerKind::Intellij);
    }

    #[gpui::test]
    fn test_unknown_profile_leaves_base_settings(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "kotlin": { "server": "intellij" },
                "profiles": { "work": { "settings": { "kotlin": { "server": "kotlin-lsp" } } } }
            }"#,
        );
        set_profile(cx, "missing");
        assert_eq!(global_server(cx), KotlinServerKind::Intellij);
        set_profile(cx, "work");
        assert_eq!(global_server(cx), KotlinServerKind::KotlinLsp);
    }

    #[gpui::test]
    fn test_profile_over_user(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "kotlin": { "server": "kotlin-lsp", "jdk_home": "/opt/jdk-17" },
                "profiles": { "work": { "settings": { "kotlin": { "server": "intellij" } } } }
            }"#,
        );
        assert_eq!(global_server(cx), KotlinServerKind::KotlinLsp);
        set_profile(cx, "work");
        let settings = cx.update(|cx| KotlinSettings::get_global(cx).clone());
        assert_eq!(settings.server, KotlinServerKind::Intellij);
        assert_eq!(settings.jdk_home, Some(PathBuf::from("/opt/jdk-17")));
    }

    #[gpui::test]
    fn test_profile_with_default_base_ignores_user_top_level(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "kotlin": { "server": "intellij", "jdk_home": "/opt/jdk-17" },
                "profiles": { "clean": { "base": "default", "settings": {} } }
            }"#,
        );
        assert_eq!(global_server(cx), KotlinServerKind::Intellij);
        set_profile(cx, "clean");
        let settings = cx.update(|cx| KotlinSettings::get_global(cx).clone());
        assert_eq!(settings.server, KotlinServerKind::KotlinLsp);
        assert_eq!(settings.jdk_home, None);
    }

    #[gpui::test]
    fn test_project_overrides_every_layer(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "kotlin": { "server": "intellij" },
                "profiles": {
                    "work": { "settings": { "kotlin": { "server": "intellij" } } },
                    "clean": { "base": "default", "settings": {} }
                }
            }"#,
        );
        set_project(cx, Some(r#"{ "kotlin": { "server": "kotlin-lsp" } }"#));
        assert_eq!(project_server(cx), KotlinServerKind::KotlinLsp);

        set_profile(cx, "work");
        assert_eq!(project_server(cx), KotlinServerKind::KotlinLsp);
        assert_eq!(global_server(cx), KotlinServerKind::Intellij);

        set_profile(cx, "clean");
        set_project(cx, Some(r#"{ "kotlin": { "server": "intellij" } }"#));
        assert_eq!(project_server(cx), KotlinServerKind::Intellij);
        assert_eq!(global_server(cx), KotlinServerKind::KotlinLsp);

        clear_project(cx);
        assert_eq!(project_server(cx), KotlinServerKind::KotlinLsp);
    }

    #[gpui::test]
    fn test_project_without_kotlin_follows_active_profile(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "profiles": { "work": { "settings": { "kotlin": { "server": "intellij" } } } }
            }"#,
        );
        set_project(cx, Some("{}"));
        assert_eq!(project_server(cx), KotlinServerKind::KotlinLsp);
        set_profile(cx, "work");
        assert_eq!(project_server(cx), KotlinServerKind::Intellij);
    }
}
