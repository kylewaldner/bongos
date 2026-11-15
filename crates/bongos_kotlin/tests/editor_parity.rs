//! Diagnostics parity and "not supported by <server>" with `FakeLanguageServer`s.
//!
//! The server in each scenario is a fake that either pushes `textDocument/publishDiagnostics` or
//! answers `textDocument/diagnostic`. Both land in the same buffer and project state, which is what
//! the editor, the status bar and the diagnostics panel read; the panel's own type is private to its
//! crate, so the panel is checked through the two things it builds its excerpts from: the project's
//! diagnostic summaries (its list of files) and each file's diagnostics in range, grouped.
#![cfg(unix)]

use std::{
    ops::Range,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bongos_kotlin::{
    Feature, KotlinDirs, KotlinSettings, attach_managers, attach_unsupported_notices,
    not_supported_message, not_supported_notice_id, not_supported_text,
};
use editor::{
    Editor,
    actions::{FindAllReferences, GoToDefinition, GoToTypeDefinition, Hover, Rename},
};
use futures::StreamExt as _;
use gpui::{AnyWindowHandle, Entity, Focusable as _, TestAppContext, VisualTestContext};
use language::{
    DiagnosticSeverity, FakeLspAdapter, Language, LanguageConfig, LanguageMatcher, Point,
};
use lsp::{FakeLanguageServer, Uri};
use project::{DiagnosticSummary, FakeFs, Project, project_settings::ProjectSettings};
use serde_json::json;
use settings::{Settings as _, SettingsStore, WorktreeId};
use workspace::Workspace;

const FILE: &str = "fun main() {\n    val x: Int = \"a\"\n    val unused = 1\n}\n";
const URI_PATH: &str = "/dir/Main.kt";

fn kotlin_language() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "Kotlin".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["kt".into()],
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    ))
}

fn error() -> lsp::Diagnostic {
    lsp::Diagnostic {
        range: lsp::Range::new(lsp::Position::new(1, 17), lsp::Position::new(1, 20)),
        severity: Some(lsp::DiagnosticSeverity::ERROR),
        message: "Type mismatch: inferred type is String but Int was expected".into(),
        source: Some("kotlin".into()),
        ..Default::default()
    }
}

fn warning() -> lsp::Diagnostic {
    lsp::Diagnostic {
        range: lsp::Range::new(lsp::Position::new(2, 8), lsp::Position::new(2, 14)),
        severity: Some(lsp::DiagnosticSeverity::WARNING),
        message: "Variable 'unused' is never used".into(),
        source: Some("kotlin".into()),
        ..Default::default()
    }
}

/// What a diagnostic looks like to the user, whichever way it arrived.
#[derive(Clone, Debug, PartialEq)]
struct Shown {
    message: String,
    severity: DiagnosticSeverity,
    range: Range<Point>,
    primary: bool,
}

/// What the buffer, the status bar and the diagnostics panel read.
#[derive(Debug, PartialEq)]
struct Observed {
    /// The open buffer's diagnostics, in order.
    buffer: Vec<Shown>,
    /// The project total (the status bar counts).
    summary: DiagnosticSummary,
    /// The panel: the files it lists, with their summaries, and each file's groups.
    panel: Vec<(String, DiagnosticSummary, Vec<Vec<Shown>>)>,
}

struct Scene {
    window: AnyWindowHandle,
    project: Entity<Project>,
    workspace: Entity<Workspace>,
    editor: Entity<Editor>,
    server: FakeLanguageServer,
    /// What the server answers to `textDocument/diagnostic`.
    pulled: Arc<Mutex<Vec<lsp::Diagnostic>>>,
    /// How many `textDocument/diagnostic` requests the server received.
    pulls: Arc<AtomicUsize>,
    /// How many requests of each editor feature the server received.
    requests: Requests,
    worktree_id: WorktreeId,
}

/// Once per test: settings, the editor and what the app attaches to every project and editor.
fn init(cx: &mut TestAppContext, with_manager: bool) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        KotlinSettings::register(cx);
        workspace::AppState::test(cx);
        editor::init(cx);
        if with_manager {
            let dir = std::env::temp_dir().join("bongos-kotlin-editor-parity");
            let dirs = KotlinDirs {
                state_dir: dir.join("state"),
                logs_dir: dir.join("logs"),
                install_root: dir.join("languages"),
                config_dir: dir.join("config"),
            };
            attach_managers(dirs, cx);
        }
        attach_unsupported_notices(cx);
    });
}

/// Opens `Main.kt` in an editor of a workspace, served by a fake server named like the real one,
/// with `capabilities` (pull is on when they carry a `diagnostic_provider`).
async fn open(cx: &mut TestAppContext, capabilities: lsp::ServerCapabilities) -> Scene {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/dir", json!({ "Main.kt": FILE, "notes.txt": "main()\n" }))
        .await;
    let project = Project::test(fs, ["/dir".as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(kotlin_language());

    let pulled = Arc::new(Mutex::new(Vec::new()));
    let pulls = Arc::new(AtomicUsize::new(0));
    let mut servers = languages.register_fake_lsp(
        "Kotlin",
        FakeLspAdapter {
            name: bongos_kotlin::ADAPTER_NAME,
            capabilities,
            initializer: Some(Box::new({
                let pulled = pulled.clone();
                let pulls = pulls.clone();
                move |server| {
                    let pulled = pulled.clone();
                    let pulls = pulls.clone();
                    server.set_request_handler::<lsp::request::DocumentDiagnosticRequest, _, _>(
                        move |_, _| {
                            pulls.fetch_add(1, Ordering::SeqCst);
                            let items = pulled.lock().unwrap().clone();
                            async move {
                                Ok(lsp::DocumentDiagnosticReportResult::Report(
                                    lsp::DocumentDiagnosticReport::Full(
                                        lsp::RelatedFullDocumentDiagnosticReport {
                                            related_documents: None,
                                            full_document_diagnostic_report:
                                                lsp::FullDocumentDiagnosticReport {
                                                    result_id: None,
                                                    items,
                                                },
                                        },
                                    ),
                                ))
                            }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    let _ = languages.language_for_name("Kotlin").await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });

    let window = {
        let project = project.clone();
        cx.add_window(|window, cx| Workspace::test_new(project, window, cx))
    };
    let workspace = window.root(cx).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(
                (worktree_id, util::rel_path::rel_path("Main.kt")),
                None,
                true,
                window,
                cx,
            )
        })
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    let server = servers.next().await.expect("the fake server starts");
    let requests = Requests::default();
    requests.install(&server);
    cx.run_until_parked();
    Scene {
        window: window.into(),
        project,
        workspace,
        editor,
        server,
        pulled,
        pulls,
        requests,
        worktree_id,
    }
}

/// Counters of the requests the stock editor actions send.
#[derive(Clone, Default)]
struct Requests {
    definition: Arc<AtomicUsize>,
    type_definition: Arc<AtomicUsize>,
    references: Arc<AtomicUsize>,
    hover: Arc<AtomicUsize>,
}

impl Requests {
    fn install(&self, server: &FakeLanguageServer) {
        let count = &self.definition;
        let count = count.clone();
        server.set_request_handler::<lsp::request::GotoDefinition, _, _>(move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(None) }
        });
        let count = self.type_definition.clone();
        server.set_request_handler::<lsp::request::GotoTypeDefinition, _, _>(move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(None) }
        });
        let count = self.references.clone();
        server.set_request_handler::<lsp::request::References, _, _>(move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(None) }
        });
        let count = self.hover.clone();
        server.set_request_handler::<lsp::request::HoverRequest, _, _>(move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(None) }
        });
    }

    fn of(&self, feature: Feature) -> Option<usize> {
        let count = match feature {
            Feature::Definition => &self.definition,
            Feature::TypeDefinition => &self.type_definition,
            Feature::References => &self.references,
            Feature::Hover => &self.hover,
            Feature::Rename => return None,
        };
        Some(count.load(Ordering::SeqCst))
    }
}

fn pull_capabilities() -> lsp::ServerCapabilities {
    lsp::ServerCapabilities {
        diagnostic_provider: Some(lsp::DiagnosticServerCapabilities::Options(
            lsp::DiagnosticOptions {
                identifier: Some("kotlin".into()),
                inter_file_dependencies: true,
                workspace_diagnostics: false,
                work_done_progress_options: Default::default(),
            },
        )),
        ..Default::default()
    }
}

impl Scene {
    fn publish(&self, diagnostics: Vec<lsp::Diagnostic>, cx: &mut TestAppContext) {
        let cx = &mut VisualTestContext::from_window(self.window, cx);
        self.server.notify::<lsp::notification::PublishDiagnostics>(
            lsp::PublishDiagnosticsParams {
                uri: Uri::from_file_path(URI_PATH).unwrap(),
                version: None,
                diagnostics,
            },
        );
        cx.run_until_parked();
    }

    /// Types in the editor, as the user fixing the file does: the editor asks the server to pull
    /// after its debounce.
    fn edit_and_wait_for_pull(&self, cx: &mut TestAppContext) {
        let cx = &mut VisualTestContext::from_window(self.window, cx);
        let before = self.pulls.load(Ordering::SeqCst);
        // At the end of the file, so the positions the server answers with stay right.
        let buffer = self
            .editor
            .read_with(cx, |editor, cx| editor.active_buffer(cx).unwrap());
        buffer.update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(end..end, "// edit\n")], None, cx);
        });
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(
            self.pulls.load(Ordering::SeqCst) > before,
            "the edit made the editor pull diagnostics"
        );
    }

    fn observe(&self, cx: &mut TestAppContext) -> Observed {
        let cx = &mut VisualTestContext::from_window(self.window, cx);
        cx.run_until_parked();
        let project = self.project.clone();
        let editor = self.editor.clone();
        cx.update(|_, cx| {
            let buffer = editor.read(cx).active_buffer(cx).expect("a buffer");
            let snapshot = buffer.read(cx).snapshot();
            let shown: Vec<(usize, Shown)> = snapshot
                .diagnostics_in_range::<_, Point>(Point::zero()..snapshot.max_point(), false)
                .map(|entry| {
                    (
                        entry.diagnostic.group_id,
                        Shown {
                            message: entry.diagnostic.message.to_string(),
                            severity: entry.diagnostic.severity,
                            range: entry.range,
                            primary: entry.diagnostic.is_primary,
                        },
                    )
                })
                .collect();
            let mut groups: Vec<(usize, Vec<Shown>)> = Vec::new();
            for (group, entry) in &shown {
                match groups.iter_mut().find(|(id, _)| id == group) {
                    Some((_, entries)) => entries.push(entry.clone()),
                    None => groups.push((*group, vec![entry.clone()])),
                }
            }
            let groups: Vec<Vec<Shown>> = groups.into_iter().map(|(_, entries)| entries).collect();
            let panel = project
                .read(cx)
                .diagnostic_summaries(false, cx)
                .map(|(path, _, summary)| {
                    (path.path.as_unix_str().to_string(), summary, groups.clone())
                })
                .collect();
            Observed {
                buffer: shown.into_iter().map(|(_, entry)| entry).collect(),
                summary: project.read(cx).diagnostic_summary(false, cx),
                panel,
            }
        })
    }
}

#[gpui::test]
async fn test_pushed_and_pulled_diagnostics_look_the_same_and_clear_alike(cx: &mut TestAppContext) {
    init(cx, false);
    // Pull stays on for Kotlin: the setting is the default.
    cx.update(|cx| {
        assert!(
            ProjectSettings::get_global(cx)
                .diagnostics
                .lsp_pull_diagnostics
                .enabled
        );
    });

    let pushing = open(cx, lsp::ServerCapabilities::default()).await;
    pushing.publish(vec![error(), warning()], cx);
    let pushed = pushing.observe(cx);
    pushing.publish(Vec::new(), cx);
    let pushed_cleared = pushing.observe(cx);
    assert_eq!(
        pushing.pulls.load(Ordering::SeqCst),
        0,
        "a push-only server is never asked to pull"
    );

    let pulling = open(cx, pull_capabilities()).await;
    *pulling.pulled.lock().unwrap() = vec![error(), warning()];
    pulling.edit_and_wait_for_pull(cx);
    let pulled = pulling.observe(cx);
    *pulling.pulled.lock().unwrap() = Vec::new();
    pulling.edit_and_wait_for_pull(cx);
    let pulled_cleared = pulling.observe(cx);

    assert_eq!(pushed.buffer.len(), 2, "{pushed:?}");
    assert_eq!(
        pushed.summary,
        DiagnosticSummary {
            error_count: 1,
            warning_count: 1
        }
    );
    assert_eq!(pushed.panel.len(), 1, "one file in the panel");
    assert_eq!(pushed.panel[0].0, "Main.kt");
    assert_eq!(pushed.panel[0].2.len(), 2, "two groups, one per diagnostic");
    assert_eq!(pulled.buffer, pushed.buffer, "equal buffer diagnostics");
    assert_eq!(pulled.summary, pushed.summary, "equal project summaries");
    assert_eq!(pulled.panel, pushed.panel, "equal panel entries");

    for cleared in [&pushed_cleared, &pulled_cleared] {
        assert_eq!(cleared.buffer, Vec::new());
        assert_eq!(cleared.summary, DiagnosticSummary::default());
        assert_eq!(cleared.panel, Vec::new());
    }
}

/// Zed keeps a server's pushed set and its pulled set side by side under the one server id. A
/// server that sends the same diagnostics both ways therefore shows them twice (a known gap, left
/// as Zed has it: no real Kotlin server does both). What must hold is that nothing accumulates:
/// repeating a push or a pull replaces that transport's set, the panel lists the file once, and
/// clearing both transports clears everything.
#[gpui::test]
async fn test_a_server_that_pushes_and_pulls_keeps_one_set_per_transport(cx: &mut TestAppContext) {
    init(cx, false);
    let both = open(cx, pull_capabilities()).await;
    let counts = |scene: &Scene, cx: &mut TestAppContext| {
        let observed = scene.observe(cx);
        let summary = observed.summary;
        (
            summary.error_count,
            summary.warning_count,
            observed.buffer.len(),
            observed.panel.len(),
        )
    };

    both.publish(vec![error(), warning()], cx);
    assert_eq!(counts(&both, cx), (1, 1, 2, 1), "pushed set only");

    *both.pulled.lock().unwrap() = vec![error(), warning()];
    both.edit_and_wait_for_pull(cx);
    let observed = both.observe(cx);
    assert_eq!(
        observed.summary,
        DiagnosticSummary {
            error_count: 2,
            warning_count: 2
        },
        "the pulled set joins the pushed one: {observed:?}"
    );
    assert_eq!(observed.buffer.len(), 4);
    assert_eq!(observed.panel.len(), 1, "the panel lists the file once");
    assert_eq!(observed.panel[0].1, observed.summary);
    assert_eq!(observed.panel[0].2.len(), 4);

    // Repeating either transport replaces its set; the total does not grow.
    both.publish(vec![error(), warning()], cx);
    both.edit_and_wait_for_pull(cx);
    assert_eq!(counts(&both, cx), (2, 2, 4, 1), "repeats do not accumulate");

    // Clearing one transport leaves the other's set; clearing both leaves nothing.
    both.publish(Vec::new(), cx);
    assert_eq!(
        counts(&both, cx),
        (1, 1, 2, 1),
        "only the pulled set is left"
    );
    *both.pulled.lock().unwrap() = Vec::new();
    both.edit_and_wait_for_pull(cx);
    let cleared = both.observe(cx);
    assert_eq!(cleared.buffer, Vec::new());
    assert_eq!(cleared.summary, DiagnosticSummary::default());
    assert_eq!(cleared.panel, Vec::new());
}

const FEATURES: [Feature; 5] = [
    Feature::Definition,
    Feature::TypeDefinition,
    Feature::References,
    Feature::Rename,
    Feature::Hover,
];

fn all_but(rename: bool) -> lsp::ServerCapabilities {
    lsp::ServerCapabilities {
        definition_provider: Some(lsp::OneOf::Left(true)),
        type_definition_provider: Some(lsp::TypeDefinitionProviderCapability::Simple(true)),
        references_provider: Some(lsp::OneOf::Left(true)),
        hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
        rename_provider: rename.then_some(lsp::OneOf::Left(true)),
        ..Default::default()
    }
}

impl Scene {
    /// Focuses `editor` with the cursor in `main`, and draws so its action handlers are registered.
    fn focus(&self, editor: &Entity<Editor>, cx: &mut TestAppContext) {
        let cx = &mut VisualTestContext::from_window(self.window, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 5)..Point::new(0, 5)]);
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    /// Dispatches the stock editor action of `feature` to the focused editor.
    fn press(&self, feature: Feature, cx: &mut TestAppContext) {
        let cx = &mut VisualTestContext::from_window(self.window, cx);
        match feature {
            Feature::Definition => cx.dispatch_action(GoToDefinition),
            Feature::TypeDefinition => cx.dispatch_action(GoToTypeDefinition),
            Feature::References => cx.dispatch_action(FindAllReferences::default()),
            Feature::Rename => cx.dispatch_action(Rename),
            Feature::Hover => cx.dispatch_action(Hover),
        }
        // Past the hover delay and any request debounce.
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
    }

    fn toasts(&self, cx: &mut TestAppContext) -> Vec<Feature> {
        let ids = self
            .workspace
            .read_with(cx, |workspace, _| workspace.notification_ids());
        FEATURES
            .into_iter()
            .filter(|feature| ids.contains(&not_supported_notice_id(*feature)))
            .collect()
    }

    fn notification_count(&self, cx: &mut TestAppContext) -> usize {
        self.workspace
            .read_with(cx, |workspace, _| workspace.notification_ids().len())
    }

    fn rename_started(&self, cx: &mut TestAppContext) -> bool {
        self.editor
            .read_with(cx, |editor, _| editor.pending_rename().is_some())
    }

    fn text(&self, feature: Feature, cx: &mut TestAppContext) -> Option<String> {
        let editor = self.editor.clone();
        cx.update(|cx| not_supported_text(&editor, feature, cx))
    }
}

/// The ordering proof: the handlers run first and `cx.propagate()` reaches the stock action when
/// the server supports the feature. Each stock action does its work (a request, or the inline
/// rename editor) and no toast is shown.
#[gpui::test]
async fn test_a_capable_server_gets_the_stock_actions(cx: &mut TestAppContext) {
    init(cx, true);
    let scene = open(cx, all_but(true)).await;
    scene.focus(&scene.editor, cx);

    // Rename last: its inline editor suppresses hover.
    for feature in [
        Feature::Definition,
        Feature::TypeDefinition,
        Feature::References,
        Feature::Hover,
        Feature::Rename,
    ] {
        assert_eq!(scene.text(feature, cx), None, "{feature:?}");
        scene.press(feature, cx);
        if let Some(count) = scene.requests.of(feature) {
            assert!(count >= 1, "{feature:?} reached the server");
        }
    }
    assert!(scene.rename_started(cx), "the stock rename ran");
    assert_eq!(scene.toasts(cx), Vec::new());
}

#[gpui::test]
async fn test_a_missing_capability_shows_a_toast_and_skips_the_stock_action(
    cx: &mut TestAppContext,
) {
    init(cx, true);
    // Everything but rename.
    let scene = open(cx, all_but(false)).await;
    scene.focus(&scene.editor, cx);

    assert_eq!(
        scene.text(Feature::Rename, cx),
        Some("Rename is not supported by kotlin-lsp".to_string())
    );
    assert_eq!(
        not_supported_message(Feature::Rename, "kotlin-lsp"),
        "Rename is not supported by kotlin-lsp"
    );
    scene.press(Feature::Rename, cx);
    assert_eq!(scene.toasts(cx), vec![Feature::Rename]);
    assert!(!scene.rename_started(cx), "the stock rename did not run");

    // The other features are untouched: they run.
    for feature in [
        Feature::Definition,
        Feature::TypeDefinition,
        Feature::References,
        Feature::Hover,
    ] {
        scene.press(feature, cx);
        assert!(scene.requests.of(feature) >= Some(1), "{feature:?}");
    }
    assert_eq!(scene.toasts(cx), vec![Feature::Rename]);
}

#[gpui::test]
async fn test_each_unsupported_feature_shows_one_toast(cx: &mut TestAppContext) {
    init(cx, true);
    let scene = open(cx, lsp::ServerCapabilities::default()).await;
    scene.focus(&scene.editor, cx);
    let before = scene.notification_count(cx);

    // Pressing a key twice replaces the toast: one per feature, never a stack.
    for _ in 0..2 {
        for feature in FEATURES {
            scene.press(feature, cx);
        }
    }
    assert_eq!(scene.toasts(cx), FEATURES.to_vec());
    assert_eq!(scene.notification_count(cx), before + FEATURES.len());
    for feature in FEATURES {
        assert_eq!(scene.requests.of(feature).unwrap_or(0), 0, "{feature:?}");
    }
    assert!(!scene.rename_started(cx));
}

#[gpui::test]
async fn test_a_file_that_is_not_kotlin_is_left_alone(cx: &mut TestAppContext) {
    init(cx, true);
    let scene = open(cx, lsp::ServerCapabilities::default()).await;
    let notes = {
        let window = scene.window;
        let cx = &mut VisualTestContext::from_window(window, cx);
        let worktree_id = scene.worktree_id;
        scene
            .workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    (worktree_id, util::rel_path::rel_path("notes.txt")),
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap()
    };
    scene.focus(&notes, cx);
    for feature in FEATURES {
        let notes = notes.clone();
        assert_eq!(
            cx.update(|cx| not_supported_text(&notes, feature, cx)),
            None
        );
        scene.press(feature, cx);
    }
    assert_eq!(scene.toasts(cx), Vec::new());
}
