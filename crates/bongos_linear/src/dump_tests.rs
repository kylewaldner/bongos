//! Tests of `linear: dump state`.

use bongos_linear_model::ApiError;
use gpui::TestAppContext;

use crate::actions::{LastToast, OFF_MESSAGE};
use crate::actions_tests::{dispatch, set_enabled_in};
use crate::dump::{DUMP_FILE, install_logs_dir, render, write_into};
use crate::sync::{self, ReadState, Snapshot};
use crate::token_tests::{SECRET, enter_token, fixture, last_toast};

fn snapshot() -> Snapshot {
    Snapshot {
        enabled: true,
        token_present: false,
        requests_allowed: 3,
        requests_refused: 2,
        linked: vec!["task-a".into(), "task-b".into()],
        reads: vec![
            (
                "task-a".into(),
                ReadState::Fresh {
                    at: 1_700_000_000_000,
                },
            ),
            ("task-b".into(), ReadState::RateLimited),
        ],
    }
}

#[test]
fn the_dump_lists_the_state_and_no_token() {
    let text = render(&snapshot());
    assert!(text.contains("enabled: true\n"));
    assert!(text.contains("token present: false\n"));
    assert!(text.contains("requests allowed: 3\n"));
    assert!(text.contains("requests refused: 2\n"));
    assert!(text.contains("task-a: fresh at 1700000000000\n"));
    assert!(text.contains("task-b: rate limited\n"));
    let mut with_token = snapshot();
    with_token.token_present = true;
    assert!(render(&with_token).contains("token present: true\n"));
}

#[test]
fn the_dump_is_written_to_the_given_directory() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("logs");
    let path = write_into(&dir, "text").unwrap();
    assert!(path == dir.join(DUMP_FILE));
    assert!(std::fs::read_to_string(path).unwrap() == "text");
}

#[gpui::test]
async fn the_command_writes_the_file_and_never_the_token(cx: &mut TestAppContext) {
    let home = tempfile::tempdir().unwrap();
    let logs = home.path().join("logs");
    let (fixture, cx) = fixture(cx).await;
    cx.update(|_, cx| install_logs_dir(logs.clone(), cx));

    // No token yet.
    dispatch("linear::DumpState", cx);
    let dump = std::fs::read_to_string(logs.join(DUMP_FILE)).unwrap();
    assert!(dump.contains("enabled: true"));
    assert!(dump.contains("token present: false"));
    assert!(last_toast(cx).starts_with("Linear state written to "));

    // A token, and a read of a linked task that failed.
    enter_token(&fixture, SECRET, cx);
    cx.update(|_, cx| {
        let sync = sync::global(cx).unwrap();
        sync.update(cx, |sync, _| {
            sync.set_linked(["task-a".to_string()]);
            sync.record_read("task-a", &Err::<(), _>(ApiError::Network("x".into())), 5);
        });
    });
    dispatch("linear::DumpState", cx);
    let dump = std::fs::read_to_string(logs.join(DUMP_FILE)).unwrap();
    assert!(dump.contains("token present: true"));
    assert!(dump.contains("task-a: unreachable"));
    assert!(!dump.contains(SECRET));
    assert!(!dump.contains("lin_api_"));
    assert!(
        !dump.contains(&SECRET.len().to_string()),
        "not even the length"
    );
}

#[gpui::test]
async fn while_off_the_command_only_says_so(cx: &mut TestAppContext) {
    let home = tempfile::tempdir().unwrap();
    let logs = home.path().join("logs");
    let (_fixture, cx) = fixture(cx).await;
    cx.update(|_, cx| install_logs_dir(logs.clone(), cx));
    set_enabled_in(cx, false);
    dispatch("linear::DumpState", cx);
    assert!(!logs.exists());
    cx.update(|_, cx| assert!(cx.global::<LastToast>().0 == OFF_MESSAGE));
}
