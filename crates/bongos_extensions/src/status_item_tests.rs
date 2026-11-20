//! Tests of the build status line: its text per phase, the failure and success lines, and Show
//! log opening the build log read-only. The text is built by pure functions, so most tests need
//! no window.

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use bongos_extensions_model::{LanguageList, SourceUrl};
use editor::Editor;
use fs::Fs;
use gpui::{TestAppContext, VisualTestContext};
use project::Project;
use serde_json::json;
use workspace::{AppState, MultiWorkspace};

use super::{StatusLine, open_read_only, status_line};
use crate::installer::{Failure, Installed, Operation, Phase, RequestKey};

const COMMIT: &str = "1111111111111111111111111111111111111111";

fn list() -> LanguageList {
    LanguageList::parse(&format!(
        "[[extension]]\nid = \"swift\"\nname = \"Swift\"\ndescription = \"Swift.\"\nrepository = \"https://example.com/acme/swift-extension\"\ncommit = \"{COMMIT}\"\nfile_types = [\"swift\"]\n"
    ))
    .unwrap()
}

fn key(name: &str) -> RequestKey {
    RequestKey {
        url: SourceUrl::parse(&format!("https://example.com/acme/{name}-extension")).unwrap(),
        path: None,
        rev: None,
    }
}

fn operation(phase: Phase, started_secs: u64) -> Operation {
    Operation {
        key: key("swift"),
        id: Some("swift".into()),
        phase,
        started: UNIX_EPOCH + Duration::from_secs(started_secs),
    }
}

fn failure(message: &str) -> Failure {
    Failure {
        key: key("swift"),
        id: Some("swift".into()),
        message: message.into(),
        log_path: Some(PathBuf::from("/logs/swift.log")),
    }
}

fn working(operations: &[Operation], now_secs: u64) -> String {
    let line = status_line(
        operations,
        None,
        None,
        &list(),
        UNIX_EPOCH + Duration::from_secs(now_secs),
    );
    match line {
        Some(StatusLine::Working(text)) => text,
        other => panic!("expected a working line, got {other:?}"),
    }
}

#[test]
fn text_per_phase_with_elapsed_time() {
    let cases = [
        (Phase::Resolving, "Swift extension: resolving (7s)"),
        (Phase::Cloning, "Swift extension: cloning (7s)"),
        (Phase::Checking, "Swift extension: checking (7s)"),
        (
            Phase::Building {
                downloading_wasi_sdk: false,
            },
            "Swift extension: building (7s)",
        ),
        (Phase::Loading, "Swift extension: loading (7s)"),
    ];
    for (phase, expected) in cases {
        assert_eq!(working(&[operation(phase, 100)], 107), expected);
    }
    let long = working(&[operation(Phase::Cloning, 100)], 100 + 65);
    assert_eq!(long, "Swift extension: cloning (1m 05s)");
}

#[test]
fn the_wasi_sdk_download_is_named() {
    let text = working(
        &[operation(
            Phase::Building {
                downloading_wasi_sdk: true,
            },
            0,
        )],
        12,
    );
    assert_eq!(
        text,
        "Swift extension: building (downloading the WASI SDK from github.com) (12s)"
    );
}

#[test]
fn a_failure_shows_the_first_line_of_the_cause() {
    let failure = failure("the grammar did not compile\nerror: more detail\nand more");
    let line = status_line(&[], Some(&failure), None, &list(), UNIX_EPOCH).unwrap();
    assert_eq!(
        line,
        StatusLine::Failed("Swift extension failed to build: the grammar did not compile".into())
    );
}

#[test]
fn more_than_one_operation_shows_the_first_and_a_count() {
    let operations = [
        operation(
            Phase::Building {
                downloading_wasi_sdk: false,
            },
            0,
        ),
        operation(Phase::WaitingToBuild, 1),
        operation(Phase::Cloning, 2),
    ];
    assert_eq!(
        working(&operations, 10),
        "Swift extension: building (10s) (+2 more)"
    );
}

#[test]
fn success_clears_after_five_seconds() {
    let installed = Installed {
        id: "swift".into(),
        commit: COMMIT.into(),
        log_path: PathBuf::from("/logs/swift.log"),
        already_installed: false,
    };
    let at = UNIX_EPOCH + Duration::from_secs(1_000);
    let line = |after_millis: u64| {
        status_line(
            &[],
            None,
            Some((&installed, at)),
            &list(),
            at + Duration::from_millis(after_millis),
        )
    };
    assert_eq!(
        line(0),
        Some(StatusLine::Installed("Swift installed".into()))
    );
    assert_eq!(
        line(4_999),
        Some(StatusLine::Installed("Swift installed".into()))
    );
    assert_eq!(line(5_000), None);
}

#[gpui::test]
async fn show_log_opens_the_file_read_only(cx: &mut TestAppContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        app_state
    });
    cx.update(|cx| <dyn Fs>::set_global(app_state.fs.clone(), cx));
    app_state
        .fs
        .as_fake()
        .insert_tree("/logs", json!({"swift.log": "cloning\nbuilding\n"}))
        .await;
    let project = Project::test(app_state.fs.clone(), ["/logs".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let task = workspace.update_in(&mut cx, |workspace, window, cx| {
        open_read_only(workspace, PathBuf::from("/logs/swift.log"), window, cx)
    });
    task.await.unwrap();
    cx.run_until_parked();
    let read_only = workspace.read_with(&cx, |workspace, cx| {
        let editor = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<Editor>())
            .expect("the log opened in an editor");
        editor.read(cx).read_only(cx)
    });
    assert!(read_only);
}
