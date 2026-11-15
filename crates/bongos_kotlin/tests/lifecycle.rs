//! The Kotlin lifecycle with a real project and the fake server as a real process. The project is
//! on the real file system, the server binary comes from `lsp.kotlin.binary.path`, and the manager
//! restarts it through the project's `LspStore`.
//! The fake server is a stand-in for the real language server and never talks to the network.
#![cfg(unix)]
#![allow(clippy::disallowed_methods)]

use std::{
    cell::RefCell,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use bongos_kotlin::{
    FallbackReason, KotlinAdapter, KotlinDirs, KotlinEvent, KotlinLog, KotlinProjectManager,
    KotlinRegistry, KotlinSettings, ServerStatus, attach_managers, attach_notices,
    fallback_message, fallback_notice_id,
};
use editor::Editor;
use gpui::{BorrowAppContext as _, Entity, TestAppContext};
use language::{Language, LanguageConfig, LanguageMatcher, LanguageName};
use project::{LspStoreEvent, Project};
use settings::{KotlinServerKind, Settings as _, SettingsStore, WorktreeId};
use workspace::Workspace;

const FAKE_SERVER: &str = env!("CARGO_BIN_EXE_bongos-fake-kotlin-server");

struct Setup {
    project: Entity<Project>,
    manager: Entity<KotlinProjectManager>,
    /// The first worktree.
    worktree_id: WorktreeId,
    worktree_ids: Vec<WorktreeId>,
    dirs: KotlinDirs,
    pids: PathBuf,
    /// The fake server's progress gates (`BONGOS_FAKE_LSP_GATES`).
    gates: PathBuf,
    _temp: tempfile::TempDir,
    _buffers: Vec<Entity<language::Buffer>>,
    _handles: Vec<project::lsp_store::OpenLspBufferHandle>,
}

impl Setup {
    fn pid_files(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = fs::read_dir(&self.pids)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        pids.sort();
        pids
    }

    fn status(&self, cx: &mut TestAppContext) -> Option<ServerStatus> {
        self.manager
            .read_with(cx, |manager, _| manager.status(self.worktree_id).cloned())
    }

    /// Lets the fake server send the report of the progress step titled `title`.
    fn open_gate(&self, title: &str) {
        fs::write(self.gates.join(title), "").unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(KotlinLog::new(&self.dirs.logs_dir).path()).unwrap_or_default()
    }

    /// `wait_until` for a state a failed server never reaches: a failed status ends the wait at
    /// once, with the log, instead of at the give-up point.
    fn wait_until(
        &self,
        cx: &mut TestAppContext,
        what: &str,
        mut condition: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        wait_until(cx, what, |cx| {
            let status = self.status(cx);
            assert!(
                !matches!(
                    status,
                    Some(ServerStatus::Failed { .. } | ServerStatus::BothFailed { .. })
                ),
                "the server failed while waiting for {what}: {status:?}\n{}",
                self.log()
            );
            condition(cx)
        });
    }
}

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

/// Opens `Main.kt` in a project whose Kotlin server is `binary` run with `mode`.
async fn start(cx: &mut TestAppContext, binary: &Path, mode: &str) -> Setup {
    start_worktrees(cx, binary, mode, 1).await
}

/// The same with `count` worktrees in one project, each with an open `Main.kt`.
async fn start_worktrees(
    cx: &mut TestAppContext,
    binary: &Path,
    mode: &str,
    count: usize,
) -> Setup {
    let temp = tempfile::tempdir().unwrap();
    let pids = temp.path().join("pids");
    fs::create_dir_all(&pids).unwrap();
    let gates = temp.path().join("gates");
    fs::create_dir_all(&gates).unwrap();
    let roots: Vec<PathBuf> = (0..count)
        .map(|index| temp.path().join(format!("project{index}")))
        .collect();
    for root in &roots {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Main.kt"), "fun main() {}\n").unwrap();
    }
    let dirs = KotlinDirs {
        state_dir: temp.path().join("state"),
        logs_dir: temp.path().join("logs"),
        install_root: temp.path().join("languages"),
        config_dir: temp.path().join("config"),
    };

    cx.executor().allow_parking();
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        KotlinSettings::register(cx);
        attach_managers(dirs.clone(), cx);
        let settings = serde_json::json!({
            "lsp": { "kotlin": { "binary": {
                "path": binary,
                "env": {
                    "BONGOS_FAKE_LSP_PIDS": pids,
                    "BONGOS_FAKE_LSP_MODE": mode,
                    "BONGOS_FAKE_LSP_GATES": gates,
                },
            } } }
        });
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.set_user_settings(&settings.to_string(), cx).unwrap();
        });
    });

    let fs = Arc::new(::fs::RealFs::new(None, cx.executor()));
    let project = Project::test(fs, roots.iter().map(PathBuf::as_path), cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(kotlin_language());
    languages.register_lsp_adapter(
        LanguageName::new_static("Kotlin"),
        Arc::new(KotlinAdapter::new(
            dirs.clone(),
            Arc::new(bongos_kotlin::Manifest::embedded()),
        )),
    );

    let worktree_ids: Vec<WorktreeId> = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).id())
            .collect()
    });
    let manager = cx.update(|cx| {
        cx.global::<KotlinRegistry>()
            .manager(&roots[0])
            .expect("the project's worktree has a manager")
    });
    let mut buffers = Vec::new();
    let mut handles = Vec::new();
    for root in &roots {
        let (buffer, handle) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(root.join("Main.kt"), cx)
            })
            .await
            .unwrap();
        buffers.push(buffer);
        handles.push(handle);
    }
    Setup {
        project,
        manager,
        worktree_id: worktree_ids[0],
        worktree_ids,
        dirs,
        pids,
        gates,
        _temp: temp,
        _buffers: buffers,
        _handles: handles,
    }
}

/// Runs the foreground tasks until `condition` holds, sleeping between rounds because the server
/// is a real process. Fails after 60 seconds, a give-up point for a loaded machine.
fn wait_until(
    cx: &mut TestAppContext,
    what: &str,
    mut condition: impl FnMut(&mut TestAppContext) -> bool,
) {
    let started = Instant::now();
    let mut clock_moved_at = started;
    loop {
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
        // The project's clock is fake. Moving it on by the real time that passed lets throttled
        // progress reports through and keeps its timeouts (120 s for `initialize`) at real pace,
        // so none of them can fire inside this wait.
        let now = Instant::now();
        cx.executor().advance_clock(now - clock_moved_at);
        clock_moved_at = now;
    }
}

/// `kill -0` succeeds for a running process and for a zombie, so failing means reaped.
fn is_gone(pid: u32) -> bool {
    !std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[gpui::test]
async fn test_restart_leaves_one_live_pid_and_reaps_the_old_one(cx: &mut TestAppContext) {
    let setup = start(cx, Path::new(FAKE_SERVER), "").await;
    setup.wait_until(cx, "the first server to be ready", |cx| {
        setup.pid_files().len() == 1 && setup.status(cx) == Some(ServerStatus::Ready)
    });
    let first = setup.pid_files()[0];

    for round in 0..2 {
        let (server, previous) = if round == 0 {
            (Some(KotlinServerKind::Intellij), first)
        } else {
            (None, setup.pid_files()[0])
        };
        setup
            .manager
            .update(cx, |manager, cx| {
                manager.switch_server(setup.worktree_id, server, cx)
            })
            .unwrap();
        let mut most_at_once = 0;
        setup.wait_until(cx, "the replacement server to be ready", |cx| {
            let pids = setup.pid_files();
            most_at_once = most_at_once.max(pids.len());
            pids.len() == 1 && pids[0] != previous && setup.status(cx) == Some(ServerStatus::Ready)
        });
        assert!(most_at_once <= 1, "two servers ran at once");
        assert!(
            is_gone(previous),
            "the old server {previous} was not reaped"
        );
    }

    let log = setup.log();
    assert!(log.contains("kotlin-lsp -> intellij"), "{log}");
    assert!(log.contains("intellij -> kotlin-lsp"), "{log}");
}

#[gpui::test]
async fn test_progress_drives_indexing_then_ready(cx: &mut TestAppContext) {
    let setup = start(cx, Path::new(FAKE_SERVER), "progress").await;
    setup.open_gate("Importing");
    // `LspStore` drops a report that comes less than `SERVER_PROGRESS_THROTTLE_TIMEOUT` after its
    // begin on the project's fake clock. A slow test thread can handle both in one round, at one
    // fake instant, so the server holds the report until the begin has been handled and the clock
    // has moved past the throttle.
    setup.wait_until(cx, "indexing to begin", |cx| {
        setup.status(cx)
            == Some(ServerStatus::Indexing {
                label: "Indexing".into(),
            })
    });
    cx.executor()
        .advance_clock(project::SERVER_PROGRESS_THROTTLE_TIMEOUT);
    setup.open_gate("Indexing");
    setup.wait_until(cx, "indexing to be logged", |_| {
        setup.log().contains("indexing 3/5 modules")
    });
    setup.wait_until(cx, "the server to be ready again", |cx| {
        setup.status(cx) == Some(ServerStatus::Ready) && setup.pid_files().len() == 1
    });
    let log = setup.log();
    let states: Vec<&str> = log
        .lines()
        .filter_map(|line| line.split_once(" state ").map(|(_, state)| state))
        .collect();
    assert!(
        states.last().is_some_and(|state| state.ends_with(" ready")),
        "{log}"
    );
    assert!(
        states
            .iter()
            .any(|state| state.ends_with(" indexing indexing 3/5 modules")),
        "{log}"
    );
}

type StartupStderr = Rc<RefCell<Vec<(String, WorktreeId, String)>>>;

fn collect_startup_stderr(setup: &Setup, cx: &mut TestAppContext) -> StartupStderr {
    let seen = StartupStderr::default();
    let lsp_store = setup
        .project
        .read_with(cx, |project, _| project.lsp_store());
    cx.update(|cx| {
        let seen = seen.clone();
        cx.subscribe(&lsp_store, move |_, event: &LspStoreEvent, _| {
            if let LspStoreEvent::LanguageServerStartupStderr {
                name,
                worktree_id,
                text,
            } = event
            {
                seen.borrow_mut()
                    .push((name.to_string(), *worktree_id, text.clone()));
            }
        })
        .detach();
    });
    seen
}

/// The event has to be subscribed before the server starts, so this opens the buffer itself.
#[gpui::test]
async fn test_startup_stderr_event_and_log(cx: &mut TestAppContext) {
    let setup = start(cx, Path::new(FAKE_SERVER), "stderr,slow-init=300").await;
    // The server started inside `start`, so restart it with the subscription in place.
    let seen = collect_startup_stderr(&setup, cx);
    setup.manager.update(cx, |manager, cx| {
        manager.restart_server(setup.worktree_id, cx)
    });
    setup.wait_until(cx, "the startup stderr event", |_| {
        !seen.borrow().is_empty()
    });
    let (name, worktree_id, text) = seen.borrow()[0].clone();
    assert_eq!(name, "kotlin");
    assert_eq!(worktree_id, setup.worktree_id);
    assert!(text.contains("fake server starting up"), "{text}");
    setup.wait_until(cx, "the stderr line in kotlin.log", |_| {
        setup.log().contains("stderr fake server starting up")
    });
}

#[gpui::test]
async fn test_failed_start_reports_its_stderr(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fails.sh");
    fs::write(&script, "#!/bin/sh\necho boom >&2\nsleep 0.3\nexit 3\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let setup = start(cx, &script, "").await;
    wait_until(cx, "kotlin.log to hold the failed start's stderr", |_| {
        setup.log().contains("stderr boom")
    });
}

#[gpui::test]
async fn test_switching_one_worktree_leaves_the_others_server_alone(cx: &mut TestAppContext) {
    let setup = start_worktrees(cx, Path::new(FAKE_SERVER), "", 2).await;
    let (first, second) = (setup.worktree_ids[0], setup.worktree_ids[1]);
    let process_of = |cx: &mut TestAppContext, worktree_id: WorktreeId| -> Option<u32> {
        setup.project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .language_server_statuses()
                .find(|(_, status)| status.worktree == Some(worktree_id))
                .and_then(|(_, status)| status.process_id)
        })
    };
    setup.wait_until(cx, "both servers to be ready", |cx| {
        setup.pid_files().len() == 2
            && process_of(cx, first).is_some()
            && process_of(cx, second).is_some()
            && setup.status(cx) == Some(ServerStatus::Ready)
    });
    let (old_first, untouched) = (
        process_of(cx, first).unwrap(),
        process_of(cx, second).unwrap(),
    );
    assert_ne!(old_first, untouched);

    setup
        .manager
        .update(cx, |manager, cx| {
            manager.switch_server(first, Some(KotlinServerKind::Intellij), cx)
        })
        .unwrap();
    setup.wait_until(cx, "the first worktree's server to be replaced", |cx| {
        process_of(cx, first).is_some_and(|pid| pid != old_first) && setup.pid_files().len() == 2
    });
    assert!(
        is_gone(old_first),
        "the old server {old_first} was not reaped"
    );
    assert_eq!(process_of(cx, second), Some(untouched));
    assert!(
        setup.pid_files().contains(&untouched),
        "the other worktree's server was stopped"
    );
}

/// A license lost mid-session (the fake server's pinned `window/showMessage` signal): the project
/// falls back to kotlin-lsp once, shows one notice, and the open Kotlin file stays open, highlighted
/// by the same language and editable throughout. Both servers are the fake here, so the mode that
/// sends the signal runs again after the fallback; kotlin-lsp's signal is not matched.
#[gpui::test]
async fn test_a_lost_license_keeps_buffers_open_and_editable(cx: &mut TestAppContext) {
    let setup = start(cx, Path::new(FAKE_SERVER), "license-lost-after=200").await;
    cx.update(|cx| {
        workspace::AppState::test(cx);
        editor::init(cx);
        attach_notices(cx);
    });
    let project = setup.project.clone();
    let (workspace, cx) =
        cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(
                (setup.worktree_id, util::rel_path::rel_path("Main.kt")),
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
    setup.wait_until(cx, "kotlin-lsp to be ready", |cx| {
        setup.status(cx) == Some(ServerStatus::Ready)
    });

    // Every notice the manager asks the workspace to show.
    let notices = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let notices = notices.clone();
        cx.subscribe(&setup.manager, move |_, event: &KotlinEvent, _| {
            if let KotlinEvent::FellBack { message, .. } = event {
                notices.borrow_mut().push(message.clone());
            }
        })
        .detach();
    });
    setup
        .manager
        .update(cx, |manager, cx| {
            manager.switch_server(setup.worktree_id, Some(KotlinServerKind::Intellij), cx)
        })
        .unwrap();
    let fallback = |cx: &mut TestAppContext| {
        setup.manager.read_with(cx, |manager, _| {
            manager
                .worktree(setup.worktree_id)
                .and_then(|state| state.fallback.as_ref())
                .map(|fallback| fallback.kind)
        })
    };
    setup.wait_until(cx, "the fallback to kotlin-lsp to be ready", |cx| {
        fallback(cx) == Some(FallbackReason::LicenseLost)
            && setup.status(cx) == Some(ServerStatus::Ready)
            && setup.pid_files().len() == 1
    });
    // kotlin-lsp sends the same text again later; nothing retries IntelliJ.
    let settle = Instant::now();
    setup.wait_until(cx, "a while on kotlin-lsp", |_| {
        settle.elapsed() > Duration::from_millis(800)
    });
    let log = setup.log();
    assert_eq!(
        log.matches(" fallback ").count(),
        1,
        "one fallback line: {log}"
    );
    assert_eq!(log.matches("kotlin-lsp -> intellij").count(), 1, "{log}");
    assert_eq!(fallback(cx), Some(FallbackReason::LicenseLost));

    // One notice for the fallback, and the workspace shows it.
    assert_eq!(
        *notices.borrow(),
        [fallback_message(FallbackReason::LicenseLost)]
    );
    let notice = fallback_notice_id(setup.worktree_id);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.notification_ids().contains(&notice));
    });

    // The file is still the open item, still Kotlin, and takes edits.
    workspace.read_with(cx, |workspace, cx| {
        let active = workspace.active_item_as::<Editor>(cx).expect("an editor");
        assert_eq!(active.entity_id(), editor.entity_id());
    });
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("// still editable\n", window, cx)
    });
    editor.read_with(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().expect("one buffer");
        let buffer = buffer.read(cx);
        assert_eq!(buffer.text(), "// still editable\nfun main() {}\n");
        assert_eq!(
            buffer
                .language()
                .map(|language| language.name().to_string()),
            Some("Kotlin".to_string())
        );
    });
}

/// A real process that crashes: the first exit restarts it, the second within a minute leaves
/// kotlin-lsp failed, with no third start.
#[gpui::test]
async fn test_a_crashing_server_restarts_once_then_fails(cx: &mut TestAppContext) {
    let setup = start(cx, Path::new(FAKE_SERVER), "crash-after=300").await;
    let crash_loop = Some(ServerStatus::Failed {
        reason: bongos_kotlin::CRASH_LOOP_REASON.into(),
    });
    wait_until(cx, "the crash loop", |cx| setup.status(cx) == crash_loop);
    let settle = Instant::now();
    wait_until(cx, "a while after the crash loop", |_| {
        settle.elapsed() > Duration::from_millis(800)
    });
    assert_eq!(setup.status(cx), crash_loop);
    let log = setup.log();
    assert_eq!(log.matches("crash: restarting once").count(), 1, "{log}");
    assert_eq!(log.matches("crash loop: ").count(), 1, "{log}");
    assert_eq!(
        log.matches("exit status: 1").count(),
        2,
        "no third start: {log}"
    );
}

/// A server binary that cannot be spawned: `LspStore`'s start-failed event reaches the manager,
/// which shows kotlin-lsp failed with the reason.
#[gpui::test]
async fn test_a_spawn_error_reaches_the_manager(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let setup = start(cx, &temp.path().join("no-such-server"), "").await;
    wait_until(cx, "the failed start", |cx| {
        matches!(
            setup.status(cx),
            Some(ServerStatus::Failed { reason }) if reason.starts_with("failed to start: ")
        )
    });
    assert!(setup.log().contains("start failed: "), "{}", setup.log());
}
