use std::io::{self, Cursor, Read};

use serde_json::{Map, Value, json};

use super::report::{
    AgentName, ChatId, Detail, Event, ReportedState, SessionId, SessionSource, Token, parse, render,
};
use super::translate::{
    ClaudeHookInput, MAX_HOOK_INPUT_BYTES, ReportIdentity, read_bounded, state_report,
    translate_claude_hook,
};

const CHAT: &str = "0196f3a0-7c2e-7d41-9b0a-3f5e6d7c8b9a";
const TOKEN: &str = "9f0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e";
const SESSION: &str = "8c1e4f8a-1b2c-4d3e-8f9a-0b1c2d3e4f5a";
const AT: u64 = 1_762_850_000_123;

/// Every hook event name the installed Claude Code was seen to fire, the ones configured but never
/// seen, and one made up.
const EVENT_NAMES: [&str; 14] = [
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "Notification",
    "PostToolUse",
    "PreToolUse",
    "PermissionRequest",
    "PreCompact",
    "PostCompact",
    "SessionEnd",
    "SubagentStop",
    "PostToolUseFailure",
    "PermissionDenied",
    "SomeFutureEvent",
];

/// The fields of hook input that hold conversation content (or data about the user's machine) and
/// must never reach a report.
const CONTENT_FIELDS: [&str; 13] = [
    "prompt",
    "last_assistant_message",
    "tool_input",
    "tool_response",
    "transcript_path",
    "cwd",
    "message",
    "compact_summary",
    "permission_suggestions",
    "title",
    "custom_instructions",
    "tool_name",
    "model",
];

/// The core of every privacy marker; searched for in either case.
const MARKER: &str = "Q9ZMARK";

fn identity() -> ReportIdentity {
    ReportIdentity {
        chat: ChatId::new(CHAT).unwrap(),
        token: Token::new(TOKEN).unwrap(),
        at: AT,
    }
}

fn translate(input: &Value) -> Option<super::report::Report> {
    translate_claude_hook(input.to_string().as_bytes(), identity())
}

fn event_of(input: &Value) -> Option<Event> {
    translate(input).map(|report| report.event)
}

fn common(event: &str) -> Map<String, Value> {
    let Value::Object(object) = json!({
        "session_id": SESSION,
        "transcript_path": "/home/someone/.claude/projects/-work-app/8c1e4f8a.jsonl",
        "cwd": "/home/someone/work/app",
        "hook_event_name": event,
    }) else {
        panic!("not an object");
    };
    object
}

fn hook(event: &str, extra: Value) -> Value {
    let mut object = common(event);
    if let Value::Object(extra) = extra {
        object.extend(extra);
    }
    Value::Object(object)
}

fn session_start(source: &str) -> Value {
    let mut extra = json!({ "source": source });
    if source == "compact" {
        extra["model"] = json!("some-model");
    }
    if source == "resume" || source == "fork" {
        extra["context_tokens"] = json!(48211);
        extra["estimated_cache_write_usd"] = json!(0.18);
        extra["prompt_cache_likely_expired"] = json!(true);
        extra["seconds_since_last_response"] = json!(912);
    }
    hook("SessionStart", extra)
}

#[test]
fn session_start_reports_each_source() {
    for source in SessionSource::ALL {
        let report = translate(&session_start(source.as_str())).unwrap();
        assert_eq!(report.event, Event::SessionStart { source });
        assert_eq!(report.session, SessionId::new(SESSION));
        assert_eq!(report.agent, AgentName::claude());
        assert_eq!(report.chat.as_str(), CHAT);
        assert_eq!(report.token, Token::new(TOKEN).unwrap());
        assert_eq!(report.at, AT);
    }
}

#[test]
fn session_start_without_a_source_is_startup() {
    let input = hook("SessionStart", json!({}));
    assert_eq!(
        event_of(&input),
        Some(Event::SessionStart {
            source: SessionSource::Startup
        })
    );
    let input = hook("SessionStart", json!({ "source": null }));
    assert_eq!(
        event_of(&input),
        Some(Event::SessionStart {
            source: SessionSource::Startup
        })
    );
}

#[test]
fn session_start_with_an_unknown_source_gives_no_report() {
    for source in [
        json!("reboot"),
        json!("Startup"),
        json!(1),
        json!({"kind": "startup"}),
    ] {
        assert_eq!(
            event_of(&hook("SessionStart", json!({ "source": source }))),
            None
        );
    }
}

#[test]
fn user_prompt_submit_is_prompt_submitted() {
    let input = hook(
        "UserPromptSubmit",
        json!({
            "permission_mode": "default",
            "prompt": "please refactor the parser",
            "prompt_id": "p-0001",
        }),
    );
    let report = translate(&input).unwrap();
    assert_eq!(report.event, Event::PromptSubmitted);
    assert_eq!(report.session, SessionId::new(SESSION));
}

#[test]
fn stop_is_turn_done() {
    let input = hook(
        "Stop",
        json!({
            "background_tasks": [{"id": "t1", "description": "watch tests"}],
            "effort": "medium",
            "last_assistant_message": "Done. I changed three files.",
            "session_crons": [],
            "stop_hook_active": false,
            "prompt_id": "p-0001",
        }),
    );
    assert_eq!(event_of(&input), Some(Event::TurnDone));
}

#[test]
fn post_tool_use_is_tool_done() {
    let input = hook(
        "PostToolUse",
        json!({
            "tool_name": "Write",
            "tool_input": {"file_path": "/home/someone/work/app/a.txt", "content": "hello"},
            "tool_response": {"success": true, "filePath": "/home/someone/work/app/a.txt"},
            "tool_use_id": "toolu_01",
        }),
    );
    assert_eq!(event_of(&input), Some(Event::ToolDone));
}

#[test]
fn notification_is_needs_input_with_its_kind() {
    for kind in [
        "permission_prompt",
        "idle_prompt",
        "auth_success",
        "elicitation_dialog",
    ] {
        let input = hook(
            "Notification",
            json!({
                "message": "Claude needs your permission to use Bash",
                "title": "Permission needed",
                "notification_type": kind,
            }),
        );
        assert_eq!(
            event_of(&input),
            Some(Event::NeedsInput {
                detail: Detail::new(kind)
            })
        );
    }
}

#[test]
fn notification_without_a_valid_kind_has_no_detail() {
    let without = hook(
        "Notification",
        json!({ "message": "Claude is waiting for your input" }),
    );
    assert_eq!(event_of(&without), Some(Event::NeedsInput { detail: None }));
    for kind in [
        json!(null),
        json!(3),
        json!("Claude needs your permission"),
        json!("permission-prompt"),
        json!({"type": "idle_prompt"}),
    ] {
        let input = hook("Notification", json!({ "notification_type": kind }));
        assert_eq!(event_of(&input), Some(Event::NeedsInput { detail: None }));
    }
}

#[test]
fn events_without_a_report_give_none() {
    let inputs = [
        hook(
            "PreToolUse",
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": "ls"},
                "tool_use_id": "toolu_01",
                "effort": "medium",
                "permission_mode": "default",
            }),
        ),
        hook(
            "PermissionRequest",
            json!({
                "tool_name": "Write",
                "tool_input": {"file_path": "a.txt", "content": "x"},
                "permission_suggestions": [{"type": "addRules", "rules": [{"toolName": "Write"}]}],
            }),
        ),
        hook(
            "PreCompact",
            json!({ "trigger": "manual", "custom_instructions": "keep the plan" }),
        ),
        hook(
            "PostCompact",
            json!({ "trigger": "manual", "compact_summary": "We discussed the parser." }),
        ),
        hook(
            "SessionEnd",
            json!({ "reason": "other", "prompt_id": "p-0001" }),
        ),
        hook("SubagentStop", json!({})),
        hook("SomeFutureEvent", json!({ "source": "startup" })),
        hook("sessionstart", json!({})),
        hook("", json!({})),
    ];
    for input in inputs {
        assert_eq!(translate(&input), None, "{input}");
    }
}

#[test]
fn input_without_an_event_name_gives_none() {
    let mut object = common("Stop");
    object.remove("hook_event_name");
    assert_eq!(translate(&Value::Object(object.clone())), None);
    object.insert("hook_event_name".to_string(), json!(null));
    assert_eq!(translate(&Value::Object(object.clone())), None);
    object.insert("hook_event_name".to_string(), json!(["Stop"]));
    assert_eq!(translate(&Value::Object(object)), None);
}

#[test]
fn malformed_or_empty_input_gives_none() {
    for input in [
        &b""[..],
        b"\n",
        b"{",
        b"not json",
        b"[]",
        b"null",
        b"\"Stop\"",
        b"{\"hook_event_name\":\"Stop\"} trailing",
        b"\xff\xfe",
    ] {
        assert_eq!(translate_claude_hook(input, identity()), None, "{input:?}");
    }
}

#[test]
fn hostile_session_ids_are_dropped_and_the_report_kept() {
    let long = "a".repeat(300);
    for session in [
        json!("/home/someone/.claude/projects/x.jsonl"),
        json!(long),
        json!("bad\u{0}id"),
        json!("tab\there"),
        json!("line\nbreak"),
        json!(""),
        json!(17),
        json!({"id": SESSION}),
        json!(null),
    ] {
        let mut input = hook("Stop", json!({}));
        input["session_id"] = session.clone();
        let report = translate(&input).unwrap();
        assert_eq!(report.event, Event::TurnDone);
        assert_eq!(report.session, None, "{session}");
    }
    let mut input = hook("Stop", json!({}));
    input.as_object_mut().unwrap().remove("session_id");
    assert_eq!(translate(&input).unwrap().session, None);
}

#[test]
fn every_report_renders_and_parses_back() {
    let mut inputs: Vec<Value> = SessionSource::ALL
        .iter()
        .map(|source| session_start(source.as_str()))
        .collect();
    for event in ["UserPromptSubmit", "Stop", "Notification", "PostToolUse"] {
        inputs.push(hook(event, json!({ "notification_type": "idle_prompt" })));
    }
    for input in inputs {
        let report = translate(&input).unwrap();
        let line = render(&report).unwrap();
        assert_eq!(parse(line.as_bytes()), Ok(report));
    }
}

#[test]
fn state_report_names_the_state_and_reads_nothing() {
    for state in ReportedState::ALL {
        let report = state_report(identity(), AgentName::new("some-agent").unwrap(), state);
        assert_eq!(report.event, Event::State { state });
        assert_eq!(report.session, None);
        assert_eq!(report.agent.as_str(), "some-agent");
        let line = render(&report).unwrap();
        assert!(line.contains(&format!("\"state\":\"{}\"", state.as_str())));
        assert_eq!(parse(line.as_bytes()), Ok(report));
    }
}

#[test]
fn hook_input_parse_skips_unknown_fields() {
    let input = hook(
        "Stop",
        json!({ "anything": {"deep": [1, {"x": "y"}]}, "number": 1.5 }),
    );
    assert!(ClaudeHookInput::parse(input.to_string().as_bytes()).is_some());
}

/// A marker that is not a valid id, kind or source, so it must be dropped wherever it is placed.
fn marker(place: &str) -> String {
    format!("{MARKER} {place} /home/someone/secret \"quoted\" \u{1b}[31m end")
}

/// A marker shaped like a valid id, for fields that are never read at all.
fn plain_marker(place: &str) -> String {
    format!("{MARKER}_{place}")
}

/// Hook input for `event` with a marker in every content field, a nested copy inside objects and
/// arrays, and a marker used as a key.
fn input_with_content_markers(event: &str) -> Value {
    let mut object = common(event);
    object.insert("source".to_string(), json!("startup"));
    object.insert("notification_type".to_string(), json!("permission_prompt"));
    for field in CONTENT_FIELDS {
        object.insert(field.to_string(), json!(marker(field)));
    }
    object.insert(
        "tool_input".to_string(),
        json!({
            "command": marker("tool_input.command"),
            "content": plain_marker("tool_input_content"),
            "nested": {"deeper": [marker("tool_input.nested"), {"x": plain_marker("nested")}]},
        }),
    );
    object.insert(
        "tool_response".to_string(),
        json!({ "stdout": marker("tool_response.stdout"), "session_id": plain_marker("inner") }),
    );
    object.insert(
        "permission_suggestions".to_string(),
        json!([{ "type": "addRules", "rules": [{ "toolName": marker("rules") }] }]),
    );
    object.insert(
        "background_tasks".to_string(),
        json!([{ "description": marker("background_tasks") }]),
    );
    object.insert("prompt_id".to_string(), json!(plain_marker("prompt_id")));
    object.insert(plain_marker("key"), json!(marker("value_under_marker_key")));
    object.insert(
        "nested_ids".to_string(),
        json!({ "session_id": plain_marker("nested_session"), "hook_event_name": "Stop" }),
    );
    Value::Object(object)
}

fn assert_no_marker(rendered: &str, context: &str) {
    let lowercase = rendered.to_lowercase();
    assert!(
        !lowercase.contains(&MARKER.to_lowercase()),
        "marker leaked ({context}): {rendered}"
    );
    for word in ["secret", "someone", "quoted", "\\u001b"] {
        assert!(
            !lowercase.contains(word),
            "{word} leaked ({context}): {rendered}"
        );
    }
}

/// Translates `input`, renders the report if there is one and asserts no marker byte is in it.
/// Returns whether a report was made, so callers can check the test was not vacuous.
fn translate_and_check(input: &Value, context: &str) -> bool {
    let bytes = input.to_string();
    assert!(bytes.contains(MARKER), "the input must hold the marker");
    match translate_claude_hook(bytes.as_bytes(), identity()) {
        Some(report) => {
            let rendered = render(&report).unwrap();
            assert_no_marker(&rendered, context);
            assert_no_marker(&format!("{report:?}"), context);
            true
        }
        None => false,
    }
}

#[test]
fn no_content_field_reaches_a_report() {
    let reported = [
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "Notification",
        "PostToolUse",
    ];
    for event in EVENT_NAMES {
        let made = translate_and_check(&input_with_content_markers(event), event);
        assert_eq!(made, reported.contains(&event), "{event}");
    }
}

#[test]
fn a_marker_in_a_read_field_is_dropped_when_it_is_not_a_valid_value() {
    let reported = [
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "Notification",
        "PostToolUse",
    ];
    for event in EVENT_NAMES {
        // In session_id: the report is kept, without a session.
        let mut input = input_with_content_markers(event);
        input["session_id"] = json!(marker("session_id"));
        let made = translate_and_check(&input, &format!("{event} session_id"));
        assert_eq!(made, reported.contains(&event), "{event}");

        // In notification_type: the report is kept, without a detail.
        let mut input = input_with_content_markers(event);
        input["notification_type"] = json!(marker("notification_type"));
        let made = translate_and_check(&input, &format!("{event} notification_type"));
        assert_eq!(made, reported.contains(&event), "{event}");

        // In source: a session start gives no report; the others ignore it.
        let mut input = input_with_content_markers(event);
        input["source"] = json!(marker("source"));
        let made = translate_and_check(&input, &format!("{event} source"));
        assert_eq!(
            made,
            reported.contains(&event) && event != "SessionStart",
            "{event}"
        );

        // In hook_event_name: never a report.
        let mut input = input_with_content_markers(event);
        input["hook_event_name"] = json!(marker("hook_event_name"));
        assert!(!translate_and_check(
            &input,
            &format!("{event} hook_event_name")
        ));

        // As nested objects in the read fields: dropped.
        let mut input = input_with_content_markers(event);
        input["session_id"] = json!({ "id": marker("nested_session_id") });
        input["notification_type"] = json!([marker("nested_notification_type")]);
        let made = translate_and_check(&input, &format!("{event} nested read fields"));
        assert_eq!(made, reported.contains(&event), "{event}");
    }
}

#[test]
fn a_valid_looking_marker_in_a_content_field_never_moves_into_a_read_field() {
    let mut input = hook("Notification", json!({}));
    let object = input.as_object_mut().unwrap();
    object.remove("session_id");
    for field in CONTENT_FIELDS {
        object.insert(field.to_string(), json!(plain_marker(field)));
    }
    let report = translate(&input).unwrap();
    assert_eq!(report.session, None);
    assert_eq!(report.event, Event::NeedsInput { detail: None });
    assert_no_marker(&render(&report).unwrap(), "valid-looking markers");
}

/// A reader that yields `remaining` bytes of `x` and counts what was read from it.
struct CountingReader {
    remaining: u64,
    read: u64,
}

impl Read for CountingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = buffer
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        buffer[..count].fill(b'x');
        self.remaining -= count as u64;
        self.read += count as u64;
        Ok(count)
    }
}

#[test]
fn read_bounded_keeps_at_most_the_limit_and_drains_the_rest() {
    let total = MAX_HOOK_INPUT_BYTES + 4 * 1024 * 1024;
    let mut reader = CountingReader {
        remaining: total,
        read: 0,
    };
    let input = read_bounded(&mut reader).unwrap();
    assert_eq!(input.len() as u64, MAX_HOOK_INPUT_BYTES);
    assert_eq!(reader.read, total);
    assert_eq!(reader.remaining, 0);
}

#[test]
fn read_bounded_keeps_short_input_whole() {
    let text = hook("Stop", json!({})).to_string();
    let input = read_bounded(&mut Cursor::new(text.clone().into_bytes())).unwrap();
    assert_eq!(input, text.into_bytes());
    assert!(read_bounded(&mut io::empty()).unwrap().is_empty());
}

#[test]
fn input_cut_at_the_limit_gives_no_report() {
    let head = format!("{{\"hook_event_name\":\"Stop\",\"session_id\":\"{SESSION}\",\"prompt\":\"");
    let mut reader = Cursor::new(head.into_bytes())
        .chain(io::repeat(b'a').take(MAX_HOOK_INPUT_BYTES))
        .chain(Cursor::new(b"\"}".to_vec()));
    let input = read_bounded(&mut reader).unwrap();
    assert_eq!(input.len() as u64, MAX_HOOK_INPUT_BYTES);
    assert_eq!(translate_claude_hook(&input, identity()), None);
}

#[test]
fn large_content_under_the_limit_still_reports() {
    let input = hook(
        "Stop",
        json!({ "last_assistant_message": "a".repeat(1024 * 1024) }),
    );
    let bytes = read_bounded(&mut Cursor::new(input.to_string().into_bytes())).unwrap();
    let report = translate_claude_hook(&bytes, identity()).unwrap();
    assert_eq!(report.event, Event::TurnDone);
    assert!(render(&report).unwrap().len() < 1024);
}
