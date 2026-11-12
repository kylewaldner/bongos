//! Tests of the `wait` built-in with a fake `gh` script, so nothing reaches GitHub.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use gpui::TestAppContext;
use tempfile::TempDir;

use crate::builtin_wait::{WaitCondition, WaitOutcome, WaitRequest, wait};

/// A temporary directory holding a fake `gh` that prints `stdout` and exits with `code`.
fn fake_gh(stdout: &str, code: i32) -> (TempDir, PathBuf) {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|path| path != std::path::Path::new("/tmp"));
    let temp = match base {
        Some(base) => TempDir::new_in(base).unwrap(),
        None => TempDir::new().unwrap(),
    };
    let script = temp.path().join("gh");
    fs::write(
        &script,
        format!("#!/bin/sh\ncat <<'EOF'\n{stdout}\nEOF\nexit {code}\n"),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    (temp, script)
}

fn request(condition: WaitCondition, gh: PathBuf) -> WaitRequest {
    WaitRequest {
        condition,
        pr: "42".to_string(),
        command: Vec::new(),
        merge_is_ship: false,
        worktree: std::env::temp_dir(),
        gh,
        elapsed: Duration::ZERO,
        give_up: Some(Duration::from_millis(5)),
    }
}

fn timed_out(mut request: WaitRequest) -> WaitRequest {
    request.elapsed = Duration::from_millis(5);
    request
}

fn is_timed_out(outcome: &WaitOutcome) -> bool {
    matches!(outcome, WaitOutcome::TimedOut(reason) if reason == "wait timed out")
}

#[gpui::test]
async fn ci_green_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let green = r#"[{"bucket":"pass"},{"bucket":"skipping"}]"#;
    let pending = r#"[{"bucket":"pass"},{"bucket":"pending"}]"#;
    let red = r#"[{"bucket":"pass"},{"bucket":"fail"}]"#;
    let (_a, gh) = fake_gh(green, 0);
    assert_eq!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Satisfied
    );
    let (_b, gh) = fake_gh(pending, 8);
    assert_eq!(
        wait(&request(WaitCondition::CiGreen, gh.clone())).await,
        WaitOutcome::Waiting
    );
    assert!(is_timed_out(
        &wait(&timed_out(request(WaitCondition::CiGreen, gh))).await
    ));
    let (_c, gh) = fake_gh(red, 1);
    assert!(matches!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Failed(reason) if reason.contains("failed")
    ));
    // Unreadable output falls back to the exit code.
    let (_d, gh) = fake_gh("", 8);
    assert_eq!(
        wait(&request(WaitCondition::CiGreen, gh)).await,
        WaitOutcome::Waiting
    );
}

#[gpui::test]
async fn pr_merged_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_a, gh) = fake_gh(r#"{"state":"MERGED"}"#, 0);
    assert_eq!(
        wait(&request(WaitCondition::PrMerged, gh)).await,
        WaitOutcome::Satisfied
    );
    let (_b, gh) = fake_gh(r#"{"state":"OPEN"}"#, 0);
    assert_eq!(
        wait(&request(WaitCondition::PrMerged, gh.clone())).await,
        WaitOutcome::Waiting
    );
    assert!(is_timed_out(
        &wait(&timed_out(request(WaitCondition::PrMerged, gh))).await
    ));
    let (_c, gh) = fake_gh(r#"{"state":"CLOSED"}"#, 0);
    assert!(matches!(
        wait(&request(WaitCondition::PrMerged, gh)).await,
        WaitOutcome::Failed(reason) if reason.contains("closed")
    ));
}

#[gpui::test]
async fn command_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let gh = PathBuf::from("gh");
    let mut ok = request(WaitCondition::Command, gh.clone());
    ok.command = vec!["true".to_string()];
    assert_eq!(wait(&ok).await, WaitOutcome::Satisfied);
    let mut no = request(WaitCondition::Command, gh);
    no.command = vec!["false".to_string()];
    assert_eq!(wait(&no).await, WaitOutcome::Waiting);
    assert!(is_timed_out(&wait(&timed_out(no)).await));
}

#[gpui::test]
async fn deploy_cases(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let gh = PathBuf::from("gh");
    let mut ship = request(WaitCondition::Deploy, gh.clone());
    ship.merge_is_ship = true;
    assert_eq!(wait(&ship).await, WaitOutcome::Satisfied);
    let gate = request(WaitCondition::Deploy, gh.clone());
    assert_eq!(
        wait(&gate).await,
        WaitOutcome::OpenGate("Confirm deploy of 42?".to_string())
    );
    let mut with_command = request(WaitCondition::Deploy, gh);
    with_command.command = vec!["false".to_string()];
    assert_eq!(wait(&with_command).await, WaitOutcome::Waiting);
    assert!(is_timed_out(&wait(&timed_out(with_command.clone())).await));
    with_command.command = vec!["true".to_string()];
    assert_eq!(wait(&with_command).await, WaitOutcome::Satisfied);
}
