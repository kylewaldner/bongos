use std::collections::HashMap;
use std::io::{self, Cursor, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::client::{CHAT_ID_ENV, Process, Timeouts, run};
use super::report::{AgentName, Event, ReportedState, parse, render_reply};
use super::translate::MAX_HOOK_INPUT_BYTES;
use crate::adapter::{CHANNEL_SOCKET_ENV, CHANNEL_TOKEN_ENV};

const CHAT: &str = "0196f3a0-7c2e-7d41-9b0a-3f5e6d7c8b9a";
const TOKEN: &str = "9f0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e";
const SESSION: &str = "8c1e4f8a-1b2c-4d3e-8f9a-0b1c2d3e4f5a";
const AT: u64 = 1_762_850_000_123;
const MARKER: &str = "PRIVATE-MARKER-7f3a9c";

/// The hook events that give a report, each with the event name the report carries.
const REPORTING_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session_start"),
    ("UserPromptSubmit", "prompt_submitted"),
    ("Stop", "turn_done"),
    ("Notification", "needs_input"),
    ("PostToolUse", "tool_done"),
];

const SILENT_EVENTS: [&str; 5] = [
    "PreToolUse",
    "PermissionRequest",
    "PreCompact",
    "SessionEnd",
    "SomeFutureEvent",
];

struct Fixture {
    _dir: TempDir,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("c.sock");
        Self { _dir: dir, socket }
    }

    fn env(&self) -> HashMap<String, String> {
        HashMap::from([
            (
                CHANNEL_SOCKET_ENV.to_string(),
                self.socket.to_string_lossy().into_owned(),
            ),
            (CHAT_ID_ENV.to_string(), CHAT.to_string()),
            (CHANNEL_TOKEN_ENV.to_string(), TOKEN.to_string()),
        ])
    }
}

/// The result of one reporter run: what it read of stdin and what it wrote to stdout and stderr.
struct Outcome {
    stdin_read: u64,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Outcome {
    fn assert_silent(&self) {
        assert!(self.stdout.is_empty(), "the reporter wrote to stdout");
        assert!(self.stderr.is_empty(), "the reporter wrote to stderr");
    }
}

fn run_reporter(mode: &str, args: &[&str], env: &HashMap<String, String>, stdin: &[u8]) -> Outcome {
    run_reporter_with(mode, args, env, stdin, Timeouts::default())
}

fn run_reporter_with(
    mode: &str,
    args: &[&str],
    env: &HashMap<String, String>,
    stdin: &[u8],
    timeouts: Timeouts,
) -> Outcome {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let mut stdin = Cursor::new(stdin);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let lookup = |name: &str| env.get(name).cloned();
    run(
        mode,
        &args,
        Process {
            env: &lookup,
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
            now_ms: &|| AT,
            timeouts,
        },
    );
    let outcome = Outcome {
        stdin_read: stdin.position(),
        stdout,
        stderr,
    };
    outcome.assert_silent();
    outcome
}

/// A listener that accepts one connection, reads its line and replies `{"ok":true}`; the thread
/// returns every byte it received.
fn serve_one(socket: &Path) -> JoinHandle<Vec<u8>> {
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let received = read_line(&mut stream);
        let _ = stream.write_all(render_reply(Ok(())).as_bytes());
        received
    })
}

fn read_line(stream: &mut UnixStream) -> Vec<u8> {
    let mut received = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = stream.read(&mut byte) {
        received.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    received
}

/// A bound listener nobody accepts on, checked afterwards for a connection.
fn idle_listener(socket: &Path) -> UnixListener {
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn assert_no_connection(listener: &UnixListener) {
    match listener.accept() {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) => panic!("accept failed: {error}"),
        Ok(_) => panic!("the reporter connected"),
    }
}

/// The one line received, checked to be one report line, parsed.
fn received_report(received: &[u8]) -> super::report::Report {
    assert_eq!(
        received.last(),
        Some(&b'\n'),
        "the line ends with a newline"
    );
    assert_eq!(
        received.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "exactly one line"
    );
    parse(received).unwrap()
}

fn hook_input(event: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "hook_event_name": event,
        "session_id": SESSION,
        "source": "startup",
        "notification_type": "permission_prompt",
    }))
    .unwrap()
}

#[test]
fn claude_hook_sends_one_line_for_every_reporting_event() {
    for (hook_event, event_name) in REPORTING_EVENTS {
        let fixture = Fixture::new();
        let server = serve_one(&fixture.socket);
        let input = hook_input(hook_event);
        let outcome = run_reporter("claude-hook", &[], &fixture.env(), &input);
        assert_eq!(outcome.stdin_read, input.len() as u64);
        let report = received_report(&server.join().unwrap());
        assert_eq!(report.event.name(), event_name, "{hook_event}");
        assert_eq!(report.chat.as_str(), CHAT);
        assert_eq!(report.token.as_str(), TOKEN);
        assert_eq!(report.at, AT);
        assert_eq!(report.agent, AgentName::claude());
        assert_eq!(report.session.as_ref().map(|s| s.as_str()), Some(SESSION));
    }
}

#[test]
fn claude_hook_sends_nothing_for_other_events() {
    for hook_event in SILENT_EVENTS {
        let fixture = Fixture::new();
        let listener = idle_listener(&fixture.socket);
        let input = hook_input(hook_event);
        let outcome = run_reporter("claude-hook", &[], &fixture.env(), &input);
        assert_eq!(outcome.stdin_read, input.len() as u64);
        assert_no_connection(&listener);
    }
}

#[test]
fn state_sends_one_line_for_each_state() {
    for state in ReportedState::ALL {
        let fixture = Fixture::new();
        let server = serve_one(&fixture.socket);
        let outcome = run_reporter("state", &[state.as_str()], &fixture.env(), b"{}");
        assert_eq!(outcome.stdin_read, 0, "state mode reads nothing");
        let report = received_report(&server.join().unwrap());
        assert_eq!(report.event, Event::State { state });
        assert_eq!(report.agent, AgentName::claude());
        assert_eq!(report.session, None);
        assert_eq!(report.at, AT);
    }
}

#[test]
fn state_takes_an_optional_agent_name() {
    let fixture = Fixture::new();
    let server = serve_one(&fixture.socket);
    run_reporter("state", &["waiting", "codex"], &fixture.env(), b"");
    let report = received_report(&server.join().unwrap());
    assert_eq!(report.agent.as_str(), "codex");
    assert_eq!(
        report.event,
        Event::State {
            state: ReportedState::Waiting
        }
    );
}

#[test]
fn state_uses_the_agent_from_the_environment() {
    let fixture = Fixture::new();
    let server = serve_one(&fixture.socket);
    let mut env = fixture.env();
    env.insert("BONGOS_CHAT_AGENT".to_string(), "codex".to_string());
    run_reporter("state", &["idle"], &env, b"");
    let report = received_report(&server.join().unwrap());
    assert_eq!(report.agent.as_str(), "codex");
}

#[test]
fn state_explicit_agent_beats_the_environment() {
    let fixture = Fixture::new();
    let server = serve_one(&fixture.socket);
    let mut env = fixture.env();
    env.insert("BONGOS_CHAT_AGENT".to_string(), "codex".to_string());
    run_reporter("state", &["idle", "gemini"], &env, b"");
    let report = received_report(&server.join().unwrap());
    assert_eq!(report.agent.as_str(), "gemini");
}

#[test]
fn state_invalid_environment_agent_falls_back_to_claude() {
    let fixture = Fixture::new();
    let server = serve_one(&fixture.socket);
    let mut env = fixture.env();
    env.insert("BONGOS_CHAT_AGENT".to_string(), "not valid!".to_string());
    run_reporter("state", &["idle"], &env, b"");
    let report = received_report(&server.join().unwrap());
    assert_eq!(report.agent, AgentName::claude());
}

#[test]
fn bad_mode_state_or_arguments_send_nothing() {
    let cases: [(&str, &[&str]); 7] = [
        ("state", &[]),
        ("state", &["busy"]),
        ("state", &["Idle"]),
        ("state", &["idle", "Not An Agent"]),
        ("state", &["idle", "codex", "extra"]),
        ("codex-hook", &[]),
        ("", &[]),
    ];
    for (mode, args) in cases {
        let fixture = Fixture::new();
        let listener = idle_listener(&fixture.socket);
        let outcome = run_reporter(mode, args, &fixture.env(), &hook_input("Stop"));
        assert_eq!(outcome.stdin_read, 0, "{mode} {args:?} reads nothing");
        assert_no_connection(&listener);
    }
}

#[test]
fn missing_or_invalid_environment_reads_and_sends_nothing() {
    let names = [CHANNEL_SOCKET_ENV, CHAT_ID_ENV, CHANNEL_TOKEN_ENV];
    for name in names {
        for value in [None, Some(""), Some("not a valid id!")] {
            if name == CHANNEL_SOCKET_ENV && value == Some("not a valid id!") {
                // Any non-empty path is a socket path; covered by the refused-connection test.
                continue;
            }
            let fixture = Fixture::new();
            let listener = idle_listener(&fixture.socket);
            let mut env = fixture.env();
            match value {
                None => env.remove(name),
                Some(value) => env.insert(name.to_string(), value.to_string()),
            };
            for (mode, args) in [("claude-hook", &[][..]), ("state", &["idle"][..])] {
                let outcome = run_reporter(mode, args, &env, &hook_input("Stop"));
                assert_eq!(outcome.stdin_read, 0, "{name}={value:?} {mode}");
                assert_no_connection(&listener);
            }
        }
    }
}

#[test]
fn garbage_stdin_ends_cleanly_without_a_report() {
    let inputs: [&[u8]; 6] = [
        b"",
        b"not json",
        b"\xff\xfe\x00garbage",
        b"[1,2,3]",
        b"{\"hook_event_name\":",
        b"{\"hook_event_name\":42}",
    ];
    for input in inputs {
        let fixture = Fixture::new();
        let listener = idle_listener(&fixture.socket);
        let outcome = run_reporter("claude-hook", &[], &fixture.env(), input);
        assert_eq!(outcome.stdin_read, input.len() as u64);
        assert_no_connection(&listener);
    }
}

/// `{"hook_event_name":"Stop",...}` padded with spaces to `len` bytes.
fn padded_stop(len: usize) -> Vec<u8> {
    let mut input = hook_input("Stop");
    input.resize(len, b' ');
    input
}

#[test]
fn stdin_of_exactly_the_cap_is_read_whole() {
    let fixture = Fixture::new();
    let server = serve_one(&fixture.socket);
    let input = padded_stop(MAX_HOOK_INPUT_BYTES as usize);
    let outcome = run_reporter("claude-hook", &[], &fixture.env(), &input);
    assert_eq!(outcome.stdin_read, input.len() as u64);
    let report = received_report(&server.join().unwrap());
    assert_eq!(report.event, Event::TurnDone);
}

#[test]
fn stdin_over_the_cap_is_cut_and_drained() {
    let len = 20 * 1024 * 1024;

    // The object fits in the first 16 MiB, so the cut input still holds it.
    let fixture = Fixture::new();
    let server = serve_one(&fixture.socket);
    let input = padded_stop(len);
    let outcome = run_reporter("claude-hook", &[], &fixture.env(), &input);
    assert_eq!(outcome.stdin_read, len as u64, "the rest is drained");
    assert_eq!(
        received_report(&server.join().unwrap()).event,
        Event::TurnDone
    );

    // A prompt that runs past the cut leaves broken JSON: no report, still a clean end.
    let fixture = Fixture::new();
    let listener = idle_listener(&fixture.socket);
    let mut input = b"{\"hook_event_name\":\"Stop\",\"prompt\":\"".to_vec();
    input.resize(len - 2, b'a');
    input.extend_from_slice(b"\"}");
    let outcome = run_reporter("claude-hook", &[], &fixture.env(), &input);
    assert_eq!(outcome.stdin_read, len as u64, "the rest is drained");
    assert_no_connection(&listener);
}

#[test]
fn no_listener_at_the_path_ends_cleanly() {
    // Nothing at the path.
    let fixture = Fixture::new();
    run_reporter("state", &["idle"], &fixture.env(), b"");
    // `run` returned: its own timeouts end a stuck listener, and a hang ends in the test timeout.

    // A socket file whose listener is gone: the connection is refused.
    let fixture = Fixture::new();
    drop(UnixListener::bind(&fixture.socket).unwrap());
    assert!(fixture.socket.exists());
    run_reporter("state", &["idle"], &fixture.env(), b"");
    // `run` returned: its own timeouts end a stuck listener, and a hang ends in the test timeout.

    // A path that is not a socket at all.
    let fixture = Fixture::new();
    std::fs::write(&fixture.socket, b"not a socket").unwrap();
    run_reporter("claude-hook", &[], &fixture.env(), &hook_input("Stop"));
    // `run` returned: its own timeouts end a stuck listener, and a hang ends in the test timeout.
}

#[test]
fn a_listener_that_never_accepts_is_left_within_the_bound() {
    let fixture = Fixture::new();
    let _listener = UnixListener::bind(&fixture.socket).unwrap();
    run_reporter("state", &["running"], &fixture.env(), b"");
    // `run` returned: its own timeouts end a stuck listener, and a hang ends in the test timeout.
}

#[test]
fn a_listener_that_never_replies_is_left_within_the_bound() {
    let fixture = Fixture::new();
    let listener = UnixListener::bind(&fixture.socket).unwrap();
    let (release, released) = mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let received = read_line(&mut stream);
        // Hold the connection open without replying until the test is done.
        let _ = released.recv();
        received
    });
    run_reporter("state", &["idle"], &fixture.env(), b"");
    // `run` returned: its own timeouts end a stuck listener, and a hang ends in the test timeout.
    release.send(()).unwrap();
    received_report(&server.join().unwrap());
}

#[test]
fn a_listener_with_a_full_backlog_is_left_within_the_bound() {
    let fixture = Fixture::new();
    let listener = UnixListener::bind(&fixture.socket).unwrap();
    // Shrink the backlog to nothing and fill it with one connection, so the next connect blocks.
    // SAFETY: `listen` on a socket this test owns; Linux lets a listening socket change its backlog.
    let result = unsafe { libc::listen(listener.as_raw_fd(), 0) };
    assert_eq!(result, 0);
    let _queued = UnixStream::connect(&fixture.socket).unwrap();
    let timeouts = Timeouts {
        connect: Duration::from_millis(200),
        reply: Duration::from_millis(200),
    };
    run_reporter_with("state", &["idle"], &fixture.env(), b"", timeouts);
    // `run` returned: its own timeouts end a stuck listener, and a hang ends in the test timeout.
}

#[test]
fn a_refusal_and_trailing_garbage_are_ignored() {
    let fixture = Fixture::new();
    let listener = UnixListener::bind(&fixture.socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let received = read_line(&mut stream);
        let _ = stream.write_all(b"{\"ok\":false,\"error\":\"bad token\"}\nmore garbage");
        received
    });
    run_reporter("state", &["idle"], &fixture.env(), b"");
    // `run` returned: its own timeouts end a stuck listener, and a hang ends in the test timeout.
    received_report(&server.join().unwrap());
}

/// Hook input with the marker in every field that holds conversation content.
fn marked_hook_input(event: &str) -> Vec<u8> {
    let marked = format!("{MARKER} said this");
    let object: Value = json!({
        "hook_event_name": event,
        "session_id": SESSION,
        "source": "startup",
        "notification_type": "permission_prompt",
        "prompt": marked,
        "message": marked,
        "last_assistant_message": marked,
        "transcript_path": format!("/home/someone/{MARKER}/transcript.jsonl"),
        "cwd": format!("/work/{MARKER}"),
        "tool_input": { "command": marked, "file_path": format!("/x/{MARKER}") },
        "tool_response": { "stdout": marked, "nested": [marked, { "deep": marked }] },
        "compact_summary": marked,
        MARKER: MARKER,
    });
    serde_json::to_vec(&object).unwrap()
}

#[test]
fn no_hook_content_reaches_the_socket() {
    for (hook_event, _) in REPORTING_EVENTS {
        let fixture = Fixture::new();
        let server = serve_one(&fixture.socket);
        let input = marked_hook_input(hook_event);
        assert!(String::from_utf8_lossy(&input).contains(MARKER));
        run_reporter("claude-hook", &[], &fixture.env(), &input);
        let received = server.join().unwrap();
        received_report(&received);
        assert!(
            !String::from_utf8_lossy(&received).contains(MARKER),
            "{hook_event} leaked content"
        );
    }

    // A marker in the fields that are kept is refused by their shape checks.
    let fixture = Fixture::new();
    let server = serve_one(&fixture.socket);
    let input = serde_json::to_vec(&json!({
        "hook_event_name": "Notification",
        "session_id": format!("{MARKER} with spaces"),
        "notification_type": format!("{MARKER} needs you"),
    }))
    .unwrap();
    run_reporter("claude-hook", &[], &fixture.env(), &input);
    let received = server.join().unwrap();
    let report = received_report(&received);
    assert_eq!(report.session, None);
    assert_eq!(report.event, Event::NeedsInput { detail: None });
    assert!(!String::from_utf8_lossy(&received).contains(MARKER));
}
