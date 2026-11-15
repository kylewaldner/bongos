//! Java on the Kotlin server, with a real project, the manager attached as in the app, and fake
//! `kotlin` and `jdtls` servers that record when they start and stop.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use bongos_language_status::LanguageNotices;
use fs::FakeFs;
use gpui::{BorrowAppContext as _, Entity, TestAppContext};
use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
use project::Project;
use serde_json::json;
use settings::{KotlinServerKind, Settings as _, SettingsStore, WorktreeId};

use super::*;
use crate::{
    KotlinDirs, KotlinProjectManager, KotlinRegistry, KotlinSettings, attach_managers,
    manager::FallbackReason,
};

type Events = Arc<Mutex<Vec<String>>>;

struct Scene {
    project: Entity<Project>,
    manager: Entity<KotlinProjectManager>,
    worktree_id: WorktreeId,
    events: Events,
    _temp: tempfile::TempDir,
    _handles: Vec<project::lsp_store::OpenLspBufferHandle>,
}

impl Scene {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn clear_events(&self) {
        self.events.lock().unwrap().clear();
    }

    /// The running servers of the worktree, by name.
    fn running(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.project.read_with(cx, |project, cx| {
            let mut names: Vec<String> = project
                .lsp_store()
                .read(cx)
                .language_server_statuses()
                .filter(|(_, status)| status.worktree == Some(self.worktree_id))
                .map(|(_, status)| status.name.0.to_string())
                .collect();
            names.sort();
            names
        })
    }

    fn java_server(&self, cx: &mut TestAppContext) -> JavaServer {
        cx.update(|cx| JavaRoutes::server(self.worktree_id, cx))
    }

    fn mixed_notices(&self, cx: &mut TestAppContext) -> usize {
        cx.update(|cx| {
            LanguageNotices::list(cx)
                .read(cx)
                .notices()
                .iter()
                .filter(|notice| {
                    notice.worktree_id == self.worktree_id
                        && notice.language == JAVA_LANGUAGE
                        && notice.key == MIXED_JVM_NOTICE_KEY
                })
                .count()
        })
    }

    fn switch(&self, server: KotlinServerKind, cx: &mut TestAppContext) {
        self.manager
            .update(cx, |manager, cx| {
                manager.switch_server(self.worktree_id, Some(server), cx)
            })
            .unwrap();
        cx.run_until_parked();
    }
}

fn language(name: &'static str, suffix: &str) -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: name.into(),
            matcher: LanguageMatcher {
                path_suffixes: vec![suffix.into()],
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    ))
}

/// A fake server named `name` that records its start, its shutdown and what it opened.
fn recording(name: &'static str, events: &Events) -> FakeLspAdapter {
    let events = events.clone();
    FakeLspAdapter {
        name,
        initializer: Some(Box::new(move |server: &mut lsp::FakeLanguageServer| {
            events.lock().unwrap().push(format!("{name} started"));
            let stopped = events.clone();
            server.set_request_handler::<lsp::request::Shutdown, _, _>(move |_, _| {
                stopped.lock().unwrap().push(format!("{name} stopped"));
                async move { Ok(()) }
            });
            let opened = events.clone();
            server.handle_notification::<lsp::notification::DidOpenTextDocument, _>(
                move |params, _| {
                    let file = params
                        .text_document
                        .uri
                        .path()
                        .to_string()
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    opened.lock().unwrap().push(format!(
                        "{name} opened {file} as {}",
                        params.text_document.language_id
                    ));
                },
            );
        })),
        ..FakeLspAdapter::default()
    }
}

/// A project with `Main.kt` and `Util.java` open, under the user settings `user`.
async fn open(cx: &mut TestAppContext, user: serde_json::Value) -> Scene {
    let temp = tempfile::tempdir().unwrap();
    let dirs = KotlinDirs {
        state_dir: temp.path().join("state"),
        logs_dir: temp.path().join("logs"),
        install_root: temp.path().join("languages"),
        config_dir: temp.path().join("config"),
    };
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        KotlinSettings::register(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.set_user_settings(&user.to_string(), cx).unwrap();
        });
        attach_managers(dirs, cx);
        project::register_language_server_policy(Arc::new(JavaServerPolicy), cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/dir",
        json!({ "Main.kt": "fun main() {}\n", "Util.java": "class Util {}\n" }),
    )
    .await;
    let project = Project::test(fs, ["/dir".as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(language("Kotlin", "kt"));
    languages.add(language(JAVA_LANGUAGE, "java"));
    let events = Events::default();
    let _kotlin = languages.register_fake_lsp("Kotlin", recording(ADAPTER_NAME, &events));
    // The same adapter name for Java, served by the same fake server entry.
    languages.register_fake_lsp_adapter(JAVA_LANGUAGE, recording(ADAPTER_NAME, &events));
    let _jdtls = languages.register_fake_lsp(JAVA_LANGUAGE, recording(JDTLS_ADAPTER, &events));

    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    let manager = cx.update(|cx| {
        cx.global::<KotlinRegistry>()
            .manager(Path::new("/dir"))
            .expect("the worktree has a manager")
    });
    cx.run_until_parked();
    let mut handles = Vec::new();
    for file in ["/dir/Main.kt", "/dir/Util.java"] {
        let (_, handle) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(file, cx)
            })
            .await
            .unwrap();
        handles.push(handle);
        cx.run_until_parked();
    }
    Scene {
        project,
        manager,
        worktree_id,
        events,
        _temp: temp,
        _handles: handles,
    }
}

fn intellij() -> serde_json::Value {
    json!({ "kotlin": { "server": "intellij", "intellij": { "download": true } } })
}

/// The index of the first event equal to `event` at or after `from`.
fn position(events: &[String], event: &str, from: usize) -> usize {
    events[from..]
        .iter()
        .position(|candidate| candidate == event)
        .map(|index| index + from)
        .unwrap_or_else(|| panic!("no {event:?} after {from} in {events:?}"))
}

#[gpui::test]
async fn test_on_intellij_kotlin_and_java_share_one_server(cx: &mut TestAppContext) {
    let scene = open(cx, intellij()).await;
    assert_eq!(scene.java_server(cx), JavaServer::Kotlin);
    assert_eq!(scene.running(cx), ["kotlin"]);
    let events = scene.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "kotlin started")
            .count(),
        1,
        "{events:?}"
    );
    assert!(
        events.contains(&"kotlin opened Main.kt as kotlin".to_string()),
        "{events:?}"
    );
    assert!(
        events.contains(&"kotlin opened Util.java as java".to_string()),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|event| event.starts_with("jdtls")),
        "{events:?}"
    );
    // Never on IntelliJ.
    assert_eq!(scene.mixed_notices(cx), 0);
}

#[gpui::test]
async fn test_on_kotlin_lsp_java_gets_jdtls_and_kotlin_is_refused(cx: &mut TestAppContext) {
    let scene = open(cx, json!({ "kotlin": { "server": "kotlin-lsp" } })).await;
    assert_eq!(scene.java_server(cx), JavaServer::Jdtls);
    assert_eq!(scene.running(cx), ["jdtls", "kotlin"]);
    let events = scene.events();
    assert!(
        events.contains(&"jdtls opened Util.java as java".to_string()),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("kotlin opened Util.java")),
        "{events:?}"
    );
    cx.update(|cx| {
        let worktree = PolicyWorktree {
            id: scene.worktree_id,
            abs_path: Path::new("/dir"),
            has_entry: &|_| false,
        };
        let java = LanguageName::new_static(JAVA_LANGUAGE);
        let policy = JavaServerPolicy;
        assert!(!policy.allows(
            &worktree,
            &java,
            &LanguageServerName::new_static(ADAPTER_NAME),
            cx
        ));
        assert!(policy.allows(
            &worktree,
            &java,
            &LanguageServerName::new_static(JDTLS_ADAPTER),
            cx
        ));
    });
}

#[gpui::test]
async fn test_intellij_without_download_or_install_leaves_java_on_jdtls(cx: &mut TestAppContext) {
    let scene = open(cx, json!({ "kotlin": { "server": "intellij" } })).await;
    assert_eq!(scene.java_server(cx), JavaServer::Jdtls);
    assert!(scene.running(cx).contains(&"jdtls".to_string()));

    // Allowing the download is enough: Java moves to the Kotlin server.
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(&intellij().to_string(), cx)
                .unwrap();
        });
    });
    cx.run_until_parked();
    assert_eq!(scene.java_server(cx), JavaServer::Kotlin);
    assert_eq!(scene.running(cx), ["kotlin"]);
}

#[gpui::test]
async fn test_a_switch_stops_the_old_java_server_before_the_new_one_starts(
    cx: &mut TestAppContext,
) {
    let scene = open(
        cx,
        json!({ "kotlin": { "intellij": { "download": true } } }),
    )
    .await;
    assert_eq!(scene.running(cx), ["jdtls", "kotlin"]);

    // kotlin-lsp to IntelliJ: jdtls and kotlin-lsp are both gone before the shared server starts.
    scene.clear_events();
    scene.switch(KotlinServerKind::Intellij, cx);
    let events = scene.events();
    let started = position(&events, "kotlin started", 0);
    assert!(
        position(&events, "jdtls stopped", 0) < started,
        "{events:?}"
    );
    assert!(
        position(&events, "kotlin stopped", 0) < started,
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "kotlin started")
            .count(),
        1,
        "{events:?}"
    );
    assert!(!events.contains(&"jdtls started".to_string()), "{events:?}");
    assert!(
        position(&events, "kotlin opened Util.java as java", started) > started,
        "{events:?}"
    );
    assert_eq!(scene.java_server(cx), JavaServer::Kotlin);
    assert_eq!(scene.running(cx), ["kotlin"]);

    // Back to kotlin-lsp: the shared server stops before jdtls or kotlin-lsp starts.
    scene.clear_events();
    scene.switch(KotlinServerKind::KotlinLsp, cx);
    let events = scene.events();
    let stopped = position(&events, "kotlin stopped", 0);
    assert!(
        stopped < position(&events, "jdtls started", 0),
        "{events:?}"
    );
    assert!(
        stopped < position(&events, "kotlin started", 0),
        "{events:?}"
    );
    assert!(
        events.contains(&"jdtls opened Util.java as java".to_string()),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("kotlin opened Util.java")),
        "{events:?}"
    );
    assert_eq!(scene.java_server(cx), JavaServer::Jdtls);
    assert_eq!(scene.running(cx), ["jdtls", "kotlin"]);
}

#[gpui::test]
async fn test_a_fallback_from_intellij_moves_java_to_jdtls(cx: &mut TestAppContext) {
    let scene = open(cx, intellij()).await;
    assert_eq!(scene.running(cx), ["kotlin"]);
    scene.clear_events();

    let fell_back = scene.manager.update(cx, |manager, cx| {
        manager.intellij_unavailable(
            scene.worktree_id,
            FallbackReason::Unlicensed,
            "license status: unlicensed",
            cx,
        )
    });
    assert!(fell_back);
    // The answer is cached before anything is stopped or started.
    assert_eq!(scene.java_server(cx), JavaServer::Jdtls);
    cx.run_until_parked();

    let events = scene.events();
    let stopped = position(&events, "kotlin stopped", 0);
    assert!(
        stopped < position(&events, "jdtls started", 0),
        "{events:?}"
    );
    assert!(
        stopped < position(&events, "kotlin started", 0),
        "{events:?}"
    );
    assert!(
        events.contains(&"jdtls opened Util.java as java".to_string()),
        "{events:?}"
    );
    assert_eq!(scene.running(cx), ["jdtls", "kotlin"]);
    // A fallback runs kotlin-lsp, so the two servers are mixed now.
    assert_eq!(scene.mixed_notices(cx), 1);
}

#[gpui::test]
async fn test_the_mixed_notice_shows_once_per_worktree_and_only_on_kotlin_lsp(
    cx: &mut TestAppContext,
) {
    let scene = open(
        cx,
        json!({ "kotlin": { "intellij": { "download": true } } }),
    )
    .await;
    assert_eq!(scene.running(cx), ["jdtls", "kotlin"]);
    assert_eq!(scene.mixed_notices(cx), 1);
    let notice = cx.update(|cx| LanguageNotices::list(cx).read(cx).notices()[0].clone());
    assert_eq!(notice.text, MIXED_JVM_NOTICE);

    // Dismissed, it stays away when both servers start again in this session.
    cx.update(|cx| {
        LanguageNotices::list(cx).update(cx, |list, cx| list.dismiss(&notice, cx));
    });
    scene.manager.update(cx, |manager, cx| {
        manager.restart_server(scene.worktree_id, cx)
    });
    cx.run_until_parked();
    assert_eq!(scene.running(cx), ["jdtls", "kotlin"]);
    assert_eq!(scene.mixed_notices(cx), 0);

    // On IntelliJ nothing is mixed, and coming back does not show it a second time.
    scene.switch(KotlinServerKind::Intellij, cx);
    assert_eq!(scene.mixed_notices(cx), 0);
    scene.switch(KotlinServerKind::KotlinLsp, cx);
    assert_eq!(scene.running(cx), ["jdtls", "kotlin"]);
    assert_eq!(scene.mixed_notices(cx), 0);
}
