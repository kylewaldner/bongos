use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use crate::adapter_tests::{ARTIFACT, RUNTIME, SESSION, SOURCE, claude_caps, plan, strings};
use crate::*;

const NO_FILE_FLAG: &str = "2.1.0";

fn launch(kind: LaunchKind, session: Option<&str>, drop: &[&str]) -> LaunchSpec {
    ClaudeAdapter::new("claude").launch(&plan(Agent::Claude, kind, session), &claude_caps(drop))
}

fn rt(name: &str) -> String {
    format!("{RUNTIME}/{name}")
}

fn fork(native: bool) -> LaunchKind {
    LaunchKind::Fork {
        source_session: SOURCE.to_string(),
        native,
    }
}

/// The injection flags every line carries on the fixture's capabilities.
fn injection() -> Vec<String> {
    strings(&[
        "--append-system-prompt-file",
        &rt("system.md"),
        "--settings",
        &rt("claude-settings.json"),
        "--plugin-dir",
        &rt("plugin"),
        "--model",
        "haiku",
        "--verbose",
    ])
}

fn line(head: &[&str]) -> Vec<String> {
    let mut args = strings(head);
    args.extend(injection());
    args
}

#[test]
fn new_line() {
    let spec = launch(LaunchKind::New, Some(SESSION), &[]);
    assert_eq!(spec.program, "claude");
    assert_eq!(spec.cwd, std::path::PathBuf::from("/repos/billing/wt"));
    assert_eq!(
        spec.args,
        line(&["--session-id", SESSION, "--name", "Billing proration"])
    );
    assert!(spec.fallbacks.is_empty() && !spec.capture_session_id);
}

#[test]
fn resume_line() {
    let spec = launch(LaunchKind::Resume, Some(SESSION), &[]);
    // A resume keeps the conversation's name and id: neither flag is passed.
    assert_eq!(spec.args, line(&["--resume", SESSION]));
    assert!(spec.fallbacks.is_empty() && !spec.capture_session_id);
}

#[test]
fn fork_line() {
    let spec = launch(fork(true), Some(SESSION), &[]);
    assert_eq!(
        spec.args,
        line(&[
            "--resume",
            SOURCE,
            "--fork-session",
            "--session-id",
            SESSION,
            "--name",
            "Billing proration"
        ])
    );
    assert!(spec.fallbacks.is_empty() && !spec.capture_session_id);
}

#[test]
fn handoff_line_is_a_new_conversation_without_a_label() {
    let spec = launch(fork(false), Some(SESSION), &[]);
    assert_eq!(
        spec.args,
        line(&["--session-id", SESSION, "--name", "Billing proration"])
    );
    assert!(spec.fallbacks.is_empty());
}

#[test]
fn fork_fallback_after_an_error_exit_captures_the_id() {
    let mut forced = plan(Agent::Claude, fork(true), Some(SESSION));
    forced.force = vec![Fallback::CaptureSessionId];
    let spec = ClaudeAdapter::new("claude").launch(&forced, &claude_caps(&[]));
    assert_eq!(
        spec.args,
        line(&[
            "--resume",
            SOURCE,
            "--fork-session",
            "--name",
            "Billing proration"
        ])
    );
    assert_eq!(spec.fallbacks, vec![Fallback::CaptureSessionId]);
    assert!(spec.capture_session_id);
}

#[test]
fn no_session_id_flag_means_capture() {
    let spec = launch(LaunchKind::New, Some(SESSION), &["--session-id"]);
    assert!(!spec.args.contains(&"--session-id".to_string()));
    assert_eq!(spec.fallbacks, vec![Fallback::CaptureSessionId]);
    assert!(spec.capture_session_id);
    // A chat with no id yet is the same case.
    let spec = launch(LaunchKind::New, None, &[]);
    assert!(spec.capture_session_id);
}

#[test]
fn no_fork_flag_starts_a_handoff() {
    let spec = launch(fork(true), Some(SESSION), &["--fork-session"]);
    assert_eq!(
        spec.args,
        line(&["--session-id", SESSION, "--name", "Billing proration"])
    );
    assert_eq!(spec.fallbacks, vec![Fallback::ForkUnsupported]);
    assert!(
        !ClaudeAdapter::new("claude")
            .shape(&claude_caps(&["--fork-session"]))
            .native_fork
    );
}

#[test]
fn resume_without_a_session_starts_new() {
    let spec = launch(LaunchKind::Resume, None, &[]);
    assert!(!spec.args.contains(&"--resume".to_string()));
    assert!(spec.fallbacks.contains(&Fallback::RestartNoSession));
}

#[test]
fn system_prompt_falls_back_to_text_then_to_nothing() {
    let adapter = ClaudeAdapter::new("claude");
    let p = plan(Agent::Claude, LaunchKind::New, Some(SESSION));
    // Below the minimum version the file flag is not assumed.
    let old = adapter.capabilities_from_help(NO_FILE_FLAG, crate::adapter_tests::CLAUDE_HELP);
    let spec = adapter.launch(&p, &old);
    let at = spec
        .args
        .iter()
        .position(|a| a == "--append-system-prompt")
        .unwrap();
    assert_eq!(spec.args[at + 1], p.files.system_text);
    assert!(
        !spec
            .args
            .contains(&"--append-system-prompt-file".to_string())
    );
    assert_eq!(spec.fallbacks, vec![Fallback::SystemPromptAsText]);

    let none = adapter.capabilities_from_help(
        NO_FILE_FLAG,
        &crate::adapter_tests::help_without(
            crate::adapter_tests::CLAUDE_HELP,
            &["--append-system-prompt"],
        ),
    );
    let spec = adapter.launch(&p, &none);
    assert!(
        spec.args
            .iter()
            .all(|a| !a.starts_with("--append-system-prompt"))
    );
    assert_eq!(spec.fallbacks, vec![Fallback::SystemPromptUnavailable]);
}

#[test]
fn no_settings_puts_the_context_in_the_system_prompt() {
    let spec = launch(LaunchKind::New, Some(SESSION), &["--settings"]);
    assert!(!spec.args.contains(&"--settings".to_string()));
    assert_eq!(spec.fallbacks, vec![Fallback::ContextInSystemPrompt]);
    assert!(
        ClaudeAdapter::new("claude")
            .shape(&claude_caps(&["--settings"]))
            .context_in_system
    );
    assert!(
        !ClaudeAdapter::new("claude")
            .shape(&claude_caps(&[]))
            .context_in_system
    );
}

#[test]
fn no_plugin_dir_puts_the_skills_in_the_system_prompt() {
    let spec = launch(LaunchKind::New, Some(SESSION), &["--plugin-dir"]);
    assert!(!spec.args.contains(&"--plugin-dir".to_string()));
    assert_eq!(spec.fallbacks, vec![Fallback::SkillsAsText]);
    assert!(
        !ClaudeAdapter::new("claude")
            .shape(&claude_caps(&["--plugin-dir"]))
            .skills_as_plugin
    );
    // A type with no skills has nothing to fall back on.
    let mut p = plan(Agent::Claude, LaunchKind::New, Some(SESSION));
    p.files.has_skills = false;
    p.files.plugin_dir = None;
    let spec = ClaudeAdapter::new("claude").launch(&p, &claude_caps(&["--plugin-dir"]));
    assert!(spec.fallbacks.is_empty());
}

#[test]
fn no_name_flag_omits_the_name() {
    let spec = launch(LaunchKind::New, Some(SESSION), &["--name"]);
    assert!(!spec.args.contains(&"--name".to_string()));
    assert_eq!(spec.fallbacks, vec![Fallback::NameOmitted]);
}

#[test]
fn model_is_passed_only_when_listed() {
    let spec = launch(LaunchKind::New, Some(SESSION), &["--model"]);
    assert!(!spec.args.contains(&"haiku".to_string()));
    assert_eq!(spec.fallbacks, vec![Fallback::ModelOmitted]);
    let mut p = plan(Agent::Claude, LaunchKind::New, Some(SESSION));
    p.model = None;
    let spec = ClaudeAdapter::new("claude").launch(&p, &claude_caps(&[]));
    assert!(!spec.args.contains(&"--model".to_string()));
}

#[test]
fn no_argv_carries_an_artifact_path() {
    // In file mode every path argument is in the chat's runtime directory, and the system text,
    // which could hold artifact paths, is not on the command line at all.
    for kind in [LaunchKind::New, LaunchKind::Resume, fork(true), fork(false)] {
        let spec = launch(kind, Some(SESSION), &[]);
        for arg in &spec.args {
            assert!(
                !arg.contains(ARTIFACT) && !arg.contains("SYSTEM-TEXT"),
                "{arg}"
            );
            if arg.starts_with('/') {
                assert!(arg.starts_with(RUNTIME), "{arg}");
            }
        }
    }
}

#[test]
fn login_probe_and_parse() {
    let adapter = ClaudeAdapter::new("claude");
    assert_eq!(
        adapter.login_probe(&claude_caps(&[])),
        Some(strings(&["auth", "status"]))
    );
    assert_eq!(adapter.login_probe(&claude_caps(&["auth"])), None);
    let parse = |output: &str, status: i32| adapter.parse_login(output, status);
    assert_eq!(
        parse(r#"{"loggedIn": true, "email": "x@example.com"}"#, 0),
        Login::LoggedIn
    );
    assert_eq!(parse(r#"{"loggedIn": false}"#, 0), Login::LoggedOut);
    assert_eq!(parse(r#"{"loggedIn": "true"}"#, 0), Login::LoggedOut);
    assert_eq!(parse(r#"{"loggedIn": true}"#, 1), Login::LoggedOut);
    assert_eq!(parse("not json", 0), Login::LoggedOut);
    assert_eq!(parse("", 0), Login::LoggedOut);
}

#[test]
fn the_project_slug_replaces_slashes_and_dots() {
    assert_eq!(
        project_slug(Path::new("/run/media/a.b/c")),
        "-run-media-a-b-c"
    );
}

fn transcript(home: &TempDir, cwd: &str, id: &str) {
    let dir = home
        .path()
        .join(".claude/projects")
        .join(project_slug(Path::new(cwd)));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{id}.jsonl")), "{}\n").unwrap();
}

#[test]
fn a_fork_id_is_the_newest_unclaimed_transcript_since_launch() {
    let home = TempDir::new().unwrap();
    let adapter = ClaudeAdapter::new("claude");
    let cwd = "/repos/billing/wt";
    let claimed = std::collections::BTreeSet::from([SOURCE.to_string()]);
    let query = |since, claimed| CaptureQuery {
        chat_id: "0190aaaa",
        cwd: Path::new(cwd),
        since,
        claimed,
        home: home.path(),
    };
    let before = SystemTime::now() - Duration::from_secs(60);
    assert_eq!(adapter.capture_session_id(&query(before, &claimed)), None);

    transcript(&home, cwd, SOURCE);
    transcript(&home, "/elsewhere", "99999999-2222-4333-8444-555555555555");
    fs::write(
        home.path()
            .join(".claude/projects")
            .join(project_slug(Path::new(cwd)))
            .join("notes.txt"),
        "x",
    )
    .unwrap();
    // The source is claimed and another directory does not count.
    assert_eq!(adapter.capture_session_id(&query(before, &claimed)), None);

    transcript(&home, cwd, SESSION);
    assert_eq!(
        adapter
            .capture_session_id(&query(before, &claimed))
            .as_deref(),
        Some(SESSION)
    );
    // Files made before the launch do not count.
    let after = SystemTime::now() + Duration::from_secs(3600);
    assert_eq!(adapter.capture_session_id(&query(after, &claimed)), None);
}
