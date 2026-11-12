mod adapter;
mod commands;
mod glue;
mod install;
mod installed;
mod intellij;
mod jdk;
mod license;
mod locks;
mod log;
mod manager;
mod manifest;
mod manifest_cache;
mod remote_manifest;
mod resolve;
mod status;
mod status_item;
mod unsupported;
mod updates;
mod versions;

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
pub use locks::{
    LockClock, LockGuard, LockRegistry, LockTimedOut, LockWaitCancelled, STATE_LOCK_LIMIT,
    state_lock_path, version_lock_path,
};
pub use log::{KotlinLog, MAX_LOG_BYTES, StderrMirror};
pub use manager::{
    ADAPTER_NAME, CRASH_LOOP_REASON, CRASH_WINDOW, EXPIRED_REASON, Fallback, FallbackReason,
    KotlinEvent, KotlinHost, KotlinProjectManager, KotlinRegistry, KotlinWorktreeState,
    ServerStatus, fallback_message,
};
pub use manifest::{ArchiveKind, Manifest, ManifestEntry, OVERRIDE_VAR, current_platform};
pub use manifest_cache::{
    BuildId, CachedManifest, Claim, FETCH_LEASE, FetchOutcome, FetchedManifest, KotlinLspBuilds,
    Lease, ManifestCache, ServerState, read_for_start,
};
pub use remote_manifest::sig_url;
pub use remote_manifest::{
    KEY_VAR as MANIFEST_KEY_VAR, RELEASE_KEYS, RemoteBuild, RemoteManifest, TrustedKey,
    compare_versions, is_allowed_archive_url, parse_version, trusted_keys, verify_signature,
};
pub use resolve::{
    Layer, LayerReport, Resolved, ResolvedSource, clear_project_override, layer_report,
    project_override, resolve, set_project_override,
};
use settings::{KotlinServerKind, RegisterSetting, Settings, SettingsContent};
pub use status::{Command, MenuItem, Snapshot, StatusLine, menu_items, status_line};
pub use status_item::{KotlinStatusItem, cancel_installs, show_logs};
pub use unsupported::{
    Feature, attach_unsupported_notices, not_supported_message, not_supported_notice_id,
    not_supported_text,
};
pub use updates::{
    FETCH_TIMEOUT, KotlinUpdates, MAX_AGE, Refresh, STARTUP_DELAY, UpdateSetup, UpdateTimings,
    WAKE_INTERVAL, WallClock,
};
pub use versions::{Resolution, resolve_build};

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
    /// Where bongos looks for the signed server manifest; empty means never. Read it only through
    /// `KotlinSettings::get_global` (see `manifest_url_for_fetch`), never with a `SettingsLocation`,
    /// so a project's `.bongos/settings.json` cannot direct a fetch.
    pub manifest_url: String,
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
            manifest_url: kotlin
                .manifest_url
                .expect("default.json defines `kotlin.manifest_url`"),
            intellij_download: kotlin
                .intellij
                .expect("default.json defines `kotlin.intellij`")
                .download
                .expect("default.json defines `kotlin.intellij.download`"),
        }
    }
}

impl KotlinSettings {
    /// The manifest URL to fetch, or `None` when the setting is empty (bongos never checks for
    /// updates) or names a URL that may not be fetched: unparsable, not `https`, or `http`
    /// anywhere but loopback in a debug build. A refused value is logged once per call.
    pub fn manifest_url_for_fetch(&self) -> Option<http_client::Url> {
        let text = self.manifest_url.trim();
        if text.is_empty() {
            return None;
        }
        let url = match http_client::Url::parse(text) {
            Ok(url) => url,
            Err(error) => {
                ::log::warn!("ignoring kotlin.manifest_url: it is not a URL ({error})");
                return None;
            }
        };
        if let Some(problem) = remote_manifest::manifest_url_problem(&url) {
            ::log::warn!("ignoring kotlin.manifest_url: {problem}");
            return None;
        }
        Some(url)
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
    KotlinUpdates::init(&dirs.install_root, &dirs.logs_dir, cx);
    attach_managers(dirs, cx);
    attach_notices(cx);
    attach_unsupported_notices(cx);
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

    fn global_manifest_url(cx: &mut TestAppContext) -> String {
        cx.update(|cx| KotlinSettings::get_global(cx).manifest_url.clone())
    }

    fn settings_with_manifest_url(url: &str) -> KotlinSettings {
        KotlinSettings {
            server: KotlinServerKind::KotlinLsp,
            jdk_home: None,
            manifest_url: url.to_string(),
            intellij_download: false,
        }
    }

    fn fetch_url(url: &str) -> Option<String> {
        let url = settings_with_manifest_url(url).manifest_url_for_fetch();
        url.map(|url| url.to_string())
    }

    #[gpui::test]
    fn test_manifest_url_is_empty_by_default_and_read_from_user_and_profile(
        cx: &mut TestAppContext,
    ) {
        setup(cx);
        assert_eq!(global_manifest_url(cx), "");
        assert_eq!(
            cx.update(|cx| KotlinSettings::get_global(cx).manifest_url_for_fetch()),
            None
        );

        set_user(
            cx,
            r#"{
                "kotlin": { "manifest_url": "https://example.com/servers.json" },
                "profiles": { "mirror": { "settings": { "kotlin": { "manifest_url": "https://mirror.example.com/servers.json" } } } }
            }"#,
        );
        assert_eq!(global_manifest_url(cx), "https://example.com/servers.json");
        set_profile(cx, "mirror");
        assert_eq!(
            global_manifest_url(cx),
            "https://mirror.example.com/servers.json"
        );
    }

    #[gpui::test]
    fn test_a_project_cannot_change_the_manifest_url(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{ "kotlin": { "manifest_url": "https://example.com/servers.json" } }"#,
        );
        set_project(
            cx,
            Some(r#"{ "kotlin": { "manifest_url": "https://evil.example.org/servers.json" } }"#),
        );
        // Only the global view is read for a fetch, and it never sees the project's value.
        assert_eq!(global_manifest_url(cx), "https://example.com/servers.json");
        // Positive control: the project's value did load, and a worktree-scoped read sees it.
        let project_url = cx.update(|cx| {
            KotlinSettings::get(
                Some(SettingsLocation {
                    worktree_id: worktree(),
                    path: RelPath::empty(),
                }),
                cx,
            )
            .manifest_url
            .clone()
        });
        assert_eq!(project_url, "https://evil.example.org/servers.json");
        clear_project(cx);
        assert_eq!(global_manifest_url(cx), "https://example.com/servers.json");
    }

    #[gpui::test]
    fn test_a_project_manifest_url_does_not_reach_the_global_settings(cx: &mut TestAppContext) {
        setup(cx);
        set_project(
            cx,
            Some(r#"{ "kotlin": { "manifest_url": "https://evil.example.org/servers.json" } }"#),
        );
        assert_eq!(global_manifest_url(cx), "");
        assert_eq!(
            cx.update(|cx| KotlinSettings::get_global(cx).manifest_url_for_fetch()),
            None
        );
    }

    #[test]
    fn test_manifest_url_for_fetch_rules() {
        assert_eq!(fetch_url(""), None);
        assert_eq!(fetch_url("   "), None);
        assert_eq!(fetch_url("not a url"), None);
        assert_eq!(fetch_url("ftp://example.com/servers.json"), None);
        assert_eq!(fetch_url("file:///tmp/servers.json"), None);
        assert_eq!(fetch_url("http://example.com/x"), None);
        assert_eq!(fetch_url("https://user:pw@example.com/servers.json"), None);
        assert_eq!(
            fetch_url("https://example.com/kotlin/servers.json").as_deref(),
            Some("https://example.com/kotlin/servers.json")
        );
        // Loopback over http is for debug builds only.
        let loopback = fetch_url("http://127.0.0.1:8123/servers.json");
        if cfg!(debug_assertions) {
            assert_eq!(
                loopback.as_deref(),
                Some("http://127.0.0.1:8123/servers.json")
            );
            assert!(fetch_url("http://localhost:8123/servers.json").is_some());
        } else {
            assert_eq!(loopback, None);
        }
        assert_eq!(fetch_url("http://[::1]:8123/servers.json"), None);
    }

    #[test]
    fn test_sig_url_derives_from_the_manifest_url() {
        let sig = |url: &str| {
            let url = settings_with_manifest_url(url).manifest_url_for_fetch();
            sig_url(&url.unwrap()).to_string()
        };
        assert_eq!(
            sig("https://example.com/servers.json"),
            "https://example.com/servers.json.sig"
        );
        assert_eq!(
            sig("https://example.com/a/b/servers.json"),
            "https://example.com/a/b/servers.json.sig"
        );
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
