use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use collections::HashSet;
use fs::FakeFs;
use futures::StreamExt;
use gpui::{App, Entity, TestAppContext, UpdateGlobal};
use language::{
    FakeLspAdapter, Language, LanguageConfig, LanguageMatcher, LanguageName, rust_lang,
};
use lsp::LanguageServerName;
use parking_lot::Mutex;
use project::{
    LanguageServerPolicy, PolicyWorktree, Project,
    lsp_store::{FormatTrigger, LspFormatTarget},
    register_language_server_policy,
};
use serde_json::json;
use settings::{SettingsStore, WorktreeId};
use util::{path, rel_path::rel_path};

use crate::init_test;

/// Refuses `refused_server` in every worktree that has a `refuse-marker` entry.
struct MarkerPolicy {
    refused_server: &'static str,
    seen_paths: Mutex<Vec<PathBuf>>,
}

impl LanguageServerPolicy for MarkerPolicy {
    fn allows(
        &self,
        worktree: &PolicyWorktree,
        _: &LanguageName,
        server: &LanguageServerName,
        _: &App,
    ) -> bool {
        self.seen_paths.lock().push(worktree.abs_path.to_path_buf());
        server.0 != self.refused_server || !(worktree.has_entry)(rel_path("refuse-marker"))
    }
}

/// In worktrees with a `switch-marker` entry, allows `old-server` until `switched` is set and
/// `new-server` after. Elsewhere only `old-server` runs.
struct SwitchPolicy {
    switched: AtomicBool,
}

impl LanguageServerPolicy for SwitchPolicy {
    fn allows(
        &self,
        worktree: &PolicyWorktree,
        _: &LanguageName,
        server: &LanguageServerName,
        _: &App,
    ) -> bool {
        let use_new =
            (worktree.has_entry)(rel_path("switch-marker")) && self.switched.load(Ordering::SeqCst);
        match server.0.as_ref() {
            "old-server" => !use_new,
            "new-server" => use_new,
            _ => true,
        }
    }
}

/// Names `self.0` as the formatting server everywhere.
struct FormatterPolicy(&'static str);

impl LanguageServerPolicy for FormatterPolicy {
    fn allows(
        &self,
        _: &PolicyWorktree,
        _: &LanguageName,
        _: &LanguageServerName,
        _: &App,
    ) -> bool {
        true
    }

    fn formatter_server(
        &self,
        _: &PolicyWorktree,
        _: &LanguageName,
        _: &App,
    ) -> Option<LanguageServerName> {
        Some(LanguageServerName::new_static(self.0))
    }
}

fn worktree_ids(project: &Entity<Project>, cx: &mut TestAppContext) -> (WorktreeId, WorktreeId) {
    project.read_with(cx, |project, cx| {
        let id_for = |root: &str| {
            project
                .worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().as_ref() == Path::new(root))
                .unwrap()
                .read(cx)
                .id()
        };
        (id_for(path!("/a")), id_for(path!("/b")))
    })
}

fn running_servers(
    project: &Entity<Project>,
    cx: &mut TestAppContext,
) -> Vec<(WorktreeId, String)> {
    project.read_with(cx, |project, cx| {
        let mut servers = project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .map(|(_, status)| (status.worktree.unwrap(), status.name.0.to_string()))
            .collect::<Vec<_>>();
        servers.sort();
        servers
    })
}

/// A fake server that records in `events` when it starts and when it is asked to shut down.
fn recording_adapter(name: &'static str, events: &Arc<Mutex<Vec<String>>>) -> FakeLspAdapter {
    let events = events.clone();
    FakeLspAdapter {
        name,
        initializer: Some(Box::new(move |server: &mut lsp::FakeLanguageServer| {
            events.lock().push(format!("{name} started"));
            let events = events.clone();
            server.set_request_handler::<lsp::request::Shutdown, _, _>(move |_, _| {
                events.lock().push(format!("{name} stopped"));
                async move { Ok(()) }
            });
        })),
        ..Default::default()
    }
}

fn toml_lang() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "TOML".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["toml".to_string()],
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    ))
}

#[gpui::test]
async fn test_refused_server_starts_only_where_allowed(cx: &mut TestAppContext) {
    init_test(cx);
    let policy = Arc::new(MarkerPolicy {
        refused_server: "policy-server",
        seen_paths: Mutex::default(),
    });
    cx.update(|cx| register_language_server_policy(policy.clone(), cx));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/a"),
        json!({ "refuse-marker": "", "main.rs": "fn main() {}" }),
    )
    .await;
    fs.insert_tree(path!("/b"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/a").as_ref(), path!("/b").as_ref()], cx).await;
    let (worktree_a, worktree_b) = worktree_ids(&project, cx);
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "policy-server",
            ..Default::default()
        },
    );

    let (_buffer_a, _handle_a) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/a/main.rs"), cx)
        })
        .await
        .unwrap();
    let (_buffer_b, _handle_b) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/b/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let mut server = fake_servers.next().await.unwrap();
    let opened = server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await
        .text_document
        .uri;
    assert_eq!(
        opened,
        lsp::Uri::from_file_path(path!("/b/main.rs")).unwrap()
    );
    cx.run_until_parked();
    assert!(
        fake_servers.try_recv().is_err(),
        "the refused server must not start in worktree a"
    );
    assert_eq!(
        running_servers(&project, cx),
        vec![(worktree_b, "policy-server".to_string())]
    );
    assert_ne!(worktree_a, worktree_b);
    let seen_paths = policy.seen_paths.lock().clone();
    assert!(seen_paths.contains(&PathBuf::from(path!("/a"))));
    assert!(seen_paths.contains(&PathBuf::from(path!("/b"))));
}

#[gpui::test]
async fn test_policy_switch_stops_old_server_before_starting_new(cx: &mut TestAppContext) {
    init_test(cx);
    let policy = Arc::new(SwitchPolicy {
        switched: AtomicBool::new(false),
    });
    cx.update(|cx| register_language_server_policy(policy.clone(), cx));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/a"),
        json!({ "switch-marker": "", "main.rs": "fn main() {}", "lib.rs": "" }),
    )
    .await;
    fs.insert_tree(path!("/b"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/a").as_ref(), path!("/b").as_ref()], cx).await;
    let (worktree_a, worktree_b) = worktree_ids(&project, cx);
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());

    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let _old_servers =
        language_registry.register_fake_lsp("Rust", recording_adapter("old-server", &events));
    let mut new_servers =
        language_registry.register_fake_lsp("Rust", recording_adapter("new-server", &events));

    let (_buffer_a, _handle_a) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/a/main.rs"), cx)
        })
        .await
        .unwrap();
    let (_buffer_b, _handle_b) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/b/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        running_servers(&project, cx),
        vec![
            (worktree_a, "old-server".to_string()),
            (worktree_b, "old-server".to_string()),
        ]
    );
    events.lock().clear();

    let lib_buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/a/lib.rs"), cx)
        })
        .await
        .unwrap();
    policy.switched.store(true, Ordering::SeqCst);
    let (reapply, _lib_handle) = project.update(cx, |project, cx| {
        let reapply = project
            .lsp_store()
            .update(cx, |lsp_store, cx| lsp_store.reapply_server_policy(cx));
        // A buffer registered while the old server is still stopping must not start the new
        // server early.
        let handle = project.register_buffer_with_language_servers(&lib_buffer, cx);
        (reapply, handle)
    });
    reapply.await;
    let mut new_server = new_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_eq!(
        *events.lock(),
        vec![
            "old-server stopped".to_string(),
            "new-server started".to_string()
        ],
        "the old server must be stopped before the new one starts"
    );
    assert_eq!(
        running_servers(&project, cx),
        vec![
            (worktree_a, "new-server".to_string()),
            (worktree_b, "old-server".to_string()),
        ]
    );
    let mut opened = HashSet::default();
    for _ in 0..2 {
        let notification = new_server
            .receive_notification::<lsp::notification::DidOpenTextDocument>()
            .await;
        opened.insert(notification.text_document.uri);
    }
    assert!(opened.contains(&lsp::Uri::from_file_path(path!("/a/main.rs")).unwrap()));
    assert!(opened.contains(&lsp::Uri::from_file_path(path!("/a/lib.rs")).unwrap()));
}

#[gpui::test]
async fn test_policy_formatter_replaces_prettier_for_auto(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.all_languages.defaults.prettier =
                    Some(settings::PrettierSettingsContent {
                        allowed: Some(true),
                        ..Default::default()
                    });
            });
        });
        register_language_server_policy(Arc::new(FormatterPolicy("policy-formatter")), cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/a"), json!({ "main.rs": "one" }))
        .await;
    fs.insert_tree(path!("/b"), json!({})).await;
    let project = Project::test(fs, [path!("/a").as_ref(), path!("/b").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let formatting_adapter = |name: &'static str| FakeLspAdapter {
        name,
        capabilities: lsp::ServerCapabilities {
            document_formatting_provider: Some(lsp::OneOf::Left(true)),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut primary_servers =
        language_registry.register_fake_lsp("Rust", formatting_adapter("primary-formatter"));
    let mut policy_servers =
        language_registry.register_fake_lsp("Rust", formatting_adapter("policy-formatter"));

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/a/main.rs"), cx)
        })
        .await
        .unwrap();
    let primary_server = primary_servers.next().await.unwrap();
    let policy_server = policy_servers.next().await.unwrap();
    cx.run_until_parked();
    let edit = |text: &'static str| {
        move |_, _| async move {
            Ok(Some(vec![lsp::TextEdit::new(
                lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 3)),
                text.to_string(),
            )]))
        }
    };
    primary_server.set_request_handler::<lsp::request::Formatting, _, _>(edit(" primary"));
    policy_server.set_request_handler::<lsp::request::Formatting, _, _>(edit(" policy"));

    project
        .update(cx, |project, cx| {
            project.format(
                HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                true,
                FormatTrigger::Manual,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        buffer.read_with(cx, |buffer, _| buffer.text()),
        "one policy\n"
    );
}

#[gpui::test]
async fn test_policy_switch_still_opens_buffers_in_running_servers(cx: &mut TestAppContext) {
    init_test(cx);
    let policy = Arc::new(SwitchPolicy {
        switched: AtomicBool::new(false),
    });
    cx.update(|cx| register_language_server_policy(policy.clone(), cx));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/a"),
        json!({
            "switch-marker": "",
            "main.rs": "fn main() {}",
            "one.toml": "",
            "two.toml": "",
        }),
    )
    .await;
    fs.insert_tree(path!("/b"), json!({})).await;
    let project = Project::test(fs, [path!("/a").as_ref(), path!("/b").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    language_registry.add(toml_lang());

    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let _old_servers =
        language_registry.register_fake_lsp("Rust", recording_adapter("old-server", &events));
    let mut new_servers =
        language_registry.register_fake_lsp("Rust", recording_adapter("new-server", &events));
    let mut toml_servers =
        language_registry.register_fake_lsp("TOML", recording_adapter("toml-server", &events));

    let (_rust_buffer, _rust_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/a/main.rs"), cx)
        })
        .await
        .unwrap();
    let (_one_buffer, _one_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/a/one.toml"), cx)
        })
        .await
        .unwrap();
    let mut toml_server = toml_servers.next().await.unwrap();
    cx.run_until_parked();
    let opened = toml_server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    assert_eq!(
        opened.text_document.uri,
        lsp::Uri::from_file_path(path!("/a/one.toml")).unwrap()
    );
    events.lock().clear();

    let two_buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/a/two.toml"), cx)
        })
        .await
        .unwrap();
    policy.switched.store(true, Ordering::SeqCst);
    let (reapply, _two_handle) = project.update(cx, |project, cx| {
        let reapply = project
            .lsp_store()
            .update(cx, |lsp_store, cx| lsp_store.reapply_server_policy(cx));
        // The TOML server survives the switch, so a buffer registered while the old Rust server
        // is still stopping must still be opened in it.
        let handle = project.register_buffer_with_language_servers(&two_buffer, cx);
        (reapply, handle)
    });
    let opened = toml_server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    assert_eq!(
        opened.text_document.uri,
        lsp::Uri::from_file_path(path!("/a/two.toml")).unwrap()
    );
    reapply.await;
    let _new_server = new_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_eq!(
        *events.lock(),
        vec![
            "old-server stopped".to_string(),
            "new-server started".to_string()
        ],
        "the old server must be stopped before the new one starts, and the TOML server keeps running"
    );
}

#[gpui::test]
async fn test_policy_formatter_not_running_falls_back(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        register_language_server_policy(Arc::new(FormatterPolicy("missing-formatter")), cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/a"), json!({ "main.rs": "one" }))
        .await;
    fs.insert_tree(path!("/b"), json!({})).await;
    let project = Project::test(fs, [path!("/a").as_ref(), path!("/b").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut primary_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "primary-formatter",
            capabilities: lsp::ServerCapabilities {
                document_formatting_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/a/main.rs"), cx)
        })
        .await
        .unwrap();
    let primary_server = primary_servers.next().await.unwrap();
    cx.run_until_parked();
    primary_server.set_request_handler::<lsp::request::Formatting, _, _>(|_, _| async move {
        Ok(Some(vec![lsp::TextEdit::new(
            lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 3)),
            " primary".to_string(),
        )]))
    });

    project
        .update(cx, |project, cx| {
            project.format(
                HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                true,
                FormatTrigger::Manual,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        buffer.read_with(cx, |buffer, _| buffer.text()),
        "one primary\n"
    );
}
