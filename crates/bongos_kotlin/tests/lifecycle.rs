//! The Kotlin lifecycle with a real project and the fake server as a real process. The project is
//! on the real file system, the server binary comes from `lsp.kotlin.binary.path`, and the manager
//! restarts it through the project's `LspStore`.
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
    KotlinAdapter, KotlinDirs, KotlinLog, KotlinProjectManager, KotlinRegistry, KotlinSettings,
    ServerStatus, attach_managers,
};
use gpui::{BorrowAppContext as _, Entity, TestAppContext};
use language::{Language, LanguageConfig, LanguageMatcher, LanguageName};
use project::{LspStoreEvent, Project};
use settings::{KotlinServerKind, Settings as _, SettingsStore, WorktreeId};

const FAKE_SERVER: &str = env!("CARGO_BIN_EXE_bongos-fake-kotlin-server");

struct Setup {
    project: Entity<Project>,
    manager: Entity<KotlinProjectManager>,
    worktree_id: WorktreeId,
    dirs: KotlinDirs,
    pids: PathBuf,
    _temp: tempfile::TempDir,
    _buffer: Entity<language::Buffer>,
    _handle: project::lsp_store::OpenLspBufferHandle,
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

    fn log(&self) -> String {
        fs::read_to_string(KotlinLog::new(&self.dirs.logs_dir).path()).unwrap_or_default()
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
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let pids = temp.path().join("pids");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&pids).unwrap();
    fs::write(root.join("Main.kt"), "fun main() {}\n").unwrap();
    let dirs = KotlinDirs {
        state_dir: temp.path().join("state"),
        logs_dir: temp.path().join("logs"),
        install_root: temp.path().join("languages"),
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
                "env": { "BONGOS_FAKE_LSP_PIDS": pids, "BONGOS_FAKE_LSP_MODE": mode },
            } } }
        });
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.set_user_settings(&settings.to_string(), cx).unwrap();
        });
    });

    let fs = Arc::new(::fs::RealFs::new(None, cx.executor()));
    let project = Project::test(fs, [root.as_path()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(kotlin_language());
    languages.register_lsp_adapter(
        LanguageName::new_static("Kotlin"),
        Arc::new(KotlinAdapter::new(dirs.clone())),
    );

    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    let manager = cx.update(|cx| {
        cx.global::<KotlinRegistry>()
            .manager(&root)
            .expect("the project's worktree has a manager")
    });
    let (buffer, handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(root.join("Main.kt"), cx)
        })
        .await
        .unwrap();
    Setup {
        project,
        manager,
        worktree_id,
        dirs,
        pids,
        _temp: temp,
        _buffer: buffer,
        _handle: handle,
    }
}

/// Runs the foreground tasks until `condition` holds, sleeping between rounds because the server
/// is a real process. Fails after 15 seconds.
fn wait_until(
    cx: &mut TestAppContext,
    what: &str,
    mut condition: impl FnMut(&mut TestAppContext) -> bool,
) {
    let started = Instant::now();
    loop {
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "timed out waiting for {what}"
        );
        // The project's clock is fake: moving it on lets throttled progress reports through.
        cx.executor().advance_clock(Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(20));
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
    wait_until(cx, "the first server to be ready", |cx| {
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
        wait_until(cx, "the replacement server to be ready", |cx| {
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
    wait_until(cx, "indexing to be logged", |_| {
        setup.log().contains("indexing 3/5 modules")
    });
    wait_until(cx, "the server to be ready again", |cx| {
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
    wait_until(cx, "the startup stderr event", |_| {
        !seen.borrow().is_empty()
    });
    let (name, worktree_id, text) = seen.borrow()[0].clone();
    assert_eq!(name, "kotlin");
    assert_eq!(worktree_id, setup.worktree_id);
    assert!(text.contains("fake server starting up"), "{text}");
    wait_until(cx, "the stderr line in kotlin.log", |_| {
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
