//! The Codex lines are the design's `unverified` assumptions (Codex was not installed for the
//! spike); these tests pin the table's current values, so a later check changes data and tests
//! together.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use crate::adapter_tests::{ARTIFACT, RUNTIME, SESSION, SOURCE, codex_caps, plan, strings};
use crate::*;

const KEY: &str = "model_instructions_file";

fn adapter() -> CodexAdapter {
    CodexAdapter::new("codex")
}

fn launch(kind: LaunchKind, session: Option<&str>, drop: &[&str]) -> LaunchSpec {
    adapter().launch(&plan(Agent::Codex, kind, session), &codex_caps(drop))
}

fn instructions() -> String {
    format!("{KEY}=\"{RUNTIME}/system.md\"")
}

fn line(head: &[&str]) -> Vec<String> {
    let mut args = strings(head);
    args.extend(strings(&[
        "-c",
        &instructions(),
        "-m",
        "haiku",
        "--verbose",
    ]));
    args
}

fn fork(native: bool) -> LaunchKind {
    LaunchKind::Fork {
        source_session: SOURCE.to_string(),
        native,
    }
}

#[test]
fn new_line_unverified() {
    let spec = launch(LaunchKind::New, None, &[]);
    assert_eq!(spec.program, "codex");
    assert_eq!(spec.args, line(&[]));
    assert_eq!(spec.fallbacks, vec![Fallback::CaptureSessionId]);
    assert!(spec.capture_session_id);
}

#[test]
fn resume_line_unverified() {
    let spec = launch(LaunchKind::Resume, Some(SESSION), &[]);
    assert_eq!(spec.args, line(&["resume", SESSION]));
    assert!(spec.fallbacks.is_empty() && !spec.capture_session_id);
}

#[test]
fn fork_line_unverified() {
    let spec = launch(fork(true), None, &[]);
    assert_eq!(spec.args, line(&["fork", SOURCE]));
    assert!(spec.capture_session_id);
}

#[test]
fn handoff_line_unverified_is_a_new_conversation() {
    let spec = launch(fork(false), None, &[]);
    assert_eq!(spec.args, line(&[]));
    assert_eq!(spec.fallbacks, vec![Fallback::CaptureSessionId]);
}

#[test]
fn no_fork_subcommand_starts_a_handoff_unverified() {
    let spec = launch(fork(true), None, &["fork"]);
    assert_eq!(spec.args, line(&[]));
    assert_eq!(
        spec.fallbacks,
        vec![Fallback::ForkUnsupported, Fallback::CaptureSessionId]
    );
    assert!(!adapter().shape(&codex_caps(&["fork"])).native_fork);
    assert!(adapter().shape(&codex_caps(&[])).native_fork);
}

#[test]
fn resume_without_an_id_takes_the_latest_unverified() {
    let spec = launch(LaunchKind::Resume, None, &[]);
    assert_eq!(spec.args, line(&["resume", "--last"]));
    assert_eq!(spec.fallbacks, vec![Fallback::ResumedLatest]);
    assert!(!spec.capture_session_id);
}

#[test]
fn resume_without_an_id_or_last_restarts_unverified() {
    let spec = launch(LaunchKind::Resume, None, &["--last"]);
    assert_eq!(spec.args, line(&[]));
    assert!(spec.fallbacks.contains(&Fallback::RestartNoSession));
    assert!(spec.capture_session_id);
}

#[test]
fn no_instructions_key_sends_the_path_as_the_first_message_unverified() {
    let table = CodexTable {
        instructions_key: None,
        ..CODEX_TABLE
    };
    let adapter = CodexAdapter::with_table("codex", table);
    let spec = adapter.launch(
        &plan(Agent::Codex, LaunchKind::Resume, Some(SESSION)),
        &codex_caps(&[]),
    );
    let prompt = format!("Read {RUNTIME}/system.md before anything else");
    assert_eq!(
        spec.args,
        strings(&["resume", SESSION, "-m", "haiku", "--verbose", &prompt])
    );
    assert_eq!(spec.fallbacks, vec![Fallback::ContextAsFirstMessage]);
    // No config flag at all is the same case.
    let spec = launch(LaunchKind::Resume, Some(SESSION), &["-c", "--config"]);
    assert_eq!(spec.args.last().map(String::as_str), Some(prompt.as_str()));
    assert_eq!(spec.fallbacks, vec![Fallback::ContextAsFirstMessage]);
}

#[test]
fn model_flag_falls_back_to_the_long_name_then_omits() {
    let spec = launch(LaunchKind::Resume, Some(SESSION), &["-m"]);
    assert!(spec.args.windows(2).any(|w| w == ["--model", "haiku"]));
    let spec = launch(LaunchKind::Resume, Some(SESSION), &["-m", "--model"]);
    assert!(!spec.args.contains(&"haiku".to_string()));
    assert_eq!(spec.fallbacks, vec![Fallback::ModelOmitted]);
}

#[test]
fn the_instructions_value_is_a_quoted_toml_string_unverified() {
    let mut p = plan(Agent::Codex, LaunchKind::New, None);
    p.files.system_path = "/rt/a \"b\"\\c/codex-instructions.md".into();
    let spec = adapter().launch(&p, &codex_caps(&[]));
    assert!(spec.args.contains(&format!(
        "{KEY}=\"/rt/a \\\"b\\\"\\\\c/codex-instructions.md\""
    )));
}

#[test]
fn no_argv_carries_an_artifact_path() {
    for kind in [LaunchKind::New, LaunchKind::Resume, fork(true), fork(false)] {
        let spec = launch(kind, Some(SESSION), &[]);
        for arg in &spec.args {
            assert!(
                !arg.contains(ARTIFACT) && !arg.contains("SYSTEM-TEXT"),
                "{arg}"
            );
        }
        assert!(
            spec.args
                .iter()
                .any(|a| a.contains(&format!("{RUNTIME}/system.md")))
        );
    }
}

#[test]
fn login_probe_and_parse_unverified() {
    let adapter = adapter();
    assert_eq!(
        adapter.login_probe(&codex_caps(&[])),
        Some(strings(&["login", "status"]))
    );
    assert_eq!(adapter.login_probe(&codex_caps(&["login"])), None);
    assert_eq!(adapter.parse_login("Logged in", 0), Login::LoggedIn);
    assert_eq!(adapter.parse_login("", 0), Login::LoggedIn);
    assert_eq!(adapter.parse_login("Not logged in", 0), Login::LoggedOut);
    assert_eq!(adapter.parse_login("Logged in", 1), Login::LoggedOut);
}

const CHAT: &str = "0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000";
const OTHER_CHAT: &str = "0190aaaa-bbbb-7ccc-8ddd-eeeeffff1111";
const CWD: &str = "/repos/billing/wt";
const ID_A: &str = "aaaaaaaa-1111-4222-8333-444444444444";
const ID_B: &str = "bbbbbbbb-1111-4222-8333-444444444444";

/// Writes `rollout-<day>-<id>.jsonl` with a meta first line under today's date, then `extra` lines.
fn rollout(home: &TempDir, id: &str, cwd: &str, extra: &str, nested: bool) {
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y/%m/%d")
        .to_string();
    let dir = home.path().join(".codex/sessions").join(today);
    fs::create_dir_all(&dir).unwrap();
    let meta = if nested {
        format!(r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}"}}}}"#)
    } else {
        format!(r#"{{"id":"{id}","cwd":"{cwd}"}}"#)
    };
    fs::write(
        dir.join(format!("rollout-2026-11-08T10-00-00-{id}.jsonl")),
        format!("{meta}\n{extra}\n"),
    )
    .unwrap();
}

struct Capture {
    home: TempDir,
    since: SystemTime,
}

impl Capture {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            since: SystemTime::now() - Duration::from_secs(30),
        }
    }

    fn ask(&self, chat_id: &str, claimed: &[&str]) -> Option<String> {
        let claimed: BTreeSet<String> = claimed.iter().map(|id| id.to_string()).collect();
        adapter().capture_session_id(&CaptureQuery {
            chat_id,
            cwd: Path::new(CWD),
            since: self.since,
            claimed: &claimed,
            home: self.home.path(),
        })
    }
}

#[test]
fn capture_finds_the_session_of_the_cwd_unverified() {
    let c = Capture::new();
    assert_eq!(c.ask(CHAT, &[]), None);
    rollout(
        &c.home,
        "cccccccc-1111-4222-8333-444444444444",
        "/elsewhere",
        "",
        false,
    );
    assert_eq!(c.ask(CHAT, &[]), None);
    rollout(&c.home, ID_A, CWD, "", false);
    assert_eq!(c.ask(CHAT, &[]).as_deref(), Some(ID_A));
    // The meta record may sit under `payload`.
    let c = Capture::new();
    rollout(&c.home, ID_B, CWD, "", true);
    assert_eq!(c.ask(CHAT, &[]).as_deref(), Some(ID_B));
}

#[test]
fn capture_skips_claimed_ids_and_files_older_than_the_launch() {
    let c = Capture::new();
    rollout(&c.home, ID_A, CWD, "", false);
    assert_eq!(c.ask(CHAT, &[ID_A]), None);
    let late = Capture {
        since: SystemTime::now() + Duration::from_secs(3600),
        home: c.home,
    };
    assert_eq!(late.ask(CHAT, &[]), None);
}

#[test]
fn two_same_second_starts_are_told_apart_by_the_chat_id() {
    let c = Capture::new();
    rollout(
        &c.home,
        ID_A,
        CWD,
        &format!("instructions mention {CHAT}"),
        false,
    );
    rollout(
        &c.home,
        ID_B,
        CWD,
        &format!("instructions mention {OTHER_CHAT}"),
        false,
    );
    assert_eq!(c.ask(CHAT, &[]).as_deref(), Some(ID_A));
    assert_eq!(c.ask(OTHER_CHAT, &[]).as_deref(), Some(ID_B));
}

#[test]
fn two_same_second_starts_wait_when_the_files_do_not_say_which() {
    let c = Capture::new();
    rollout(&c.home, ID_A, CWD, "", false);
    rollout(&c.home, ID_B, CWD, "", false);
    // Ambiguous: neither chat may take either id yet.
    assert_eq!(c.ask(CHAT, &[]), None);
    assert_eq!(c.ask(OTHER_CHAT, &[]), None);
    // Once the first is claimed, the second has one candidate left.
    assert_eq!(c.ask(OTHER_CHAT, &[ID_A]).as_deref(), Some(ID_B));
}
