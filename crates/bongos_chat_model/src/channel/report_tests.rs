use serde_json::{Map, Value, json};

use super::report::{
    AgentName, ChatId, Detail, Event, MAX_CONNECTIONS, MAX_LINE_BYTES, ParseError, READ_TIMEOUT,
    ReplyError, Report, ReportedState, SessionId, SessionSource, Token, parse, parse_reply, render,
    render_reply,
};

const CHAT: &str = "0196f3a0-7c2e-7d41-9b0a-3f5e6d7c8b9a";
const TOKEN: &str = "9f0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e";
const SESSION: &str = "8c1e4f8a-1b2c-4d3e-8f9a-0b1c2d3e4f5a";
const AT: u64 = 1_762_850_000_123;

const WIRE_KEYS: [&str; 10] = [
    "v", "chat", "token", "at", "agent", "event", "session", "source", "state", "detail",
];

fn chat() -> ChatId {
    ChatId::new(CHAT).unwrap()
}

fn token() -> Token {
    Token::new(TOKEN).unwrap()
}

fn report(event: Event, session: Option<SessionId>) -> Report {
    Report {
        chat: chat(),
        token: token(),
        at: AT,
        agent: AgentName::claude(),
        event,
        session,
    }
}

fn every_event() -> Vec<Event> {
    let mut events = Vec::new();
    for source in SessionSource::ALL {
        events.push(Event::SessionStart { source });
    }
    events.push(Event::PromptSubmitted);
    events.push(Event::TurnDone);
    events.push(Event::NeedsInput { detail: None });
    events.push(Event::NeedsInput {
        detail: Detail::new("permission_prompt"),
    });
    events.push(Event::NeedsInput {
        detail: Detail::new("idle_prompt"),
    });
    events.push(Event::ToolDone);
    for state in ReportedState::ALL {
        events.push(Event::State { state });
    }
    events
}

fn every_session() -> Vec<Option<SessionId>> {
    vec![
        None,
        SessionId::new(SESSION),
        SessionId::new("thread_01-ABC"),
        SessionId::new(&"a".repeat(128)),
    ]
}

/// A valid request as a JSON object, to be changed by each refusal test.
fn valid_object() -> Map<String, Value> {
    let Value::Object(object) = json!({
        "v": 1, "chat": CHAT, "token": TOKEN, "at": AT, "agent": "claude",
        "event": "prompt_submitted", "session": SESSION, "source": null, "state": null,
        "detail": null,
    }) else {
        panic!("not an object");
    };
    object
}

fn parse_object(object: Map<String, Value>) -> Result<Report, ParseError> {
    parse(Value::Object(object).to_string().as_bytes())
}

fn with(key: &str, value: Value) -> Result<Report, ParseError> {
    let mut object = valid_object();
    object.insert(key.to_string(), value);
    parse_object(object)
}

fn without(key: &str) -> Result<Report, ParseError> {
    let mut object = valid_object();
    object.remove(key);
    parse_object(object)
}

#[test]
fn limits_match_the_wire_format() {
    assert_eq!(MAX_LINE_BYTES, 4096);
    assert_eq!(MAX_CONNECTIONS, 32);
    assert_eq!(READ_TIMEOUT.as_millis(), 2000);
}

#[test]
fn renders_the_documented_line() {
    let line = render(&report(Event::PromptSubmitted, SessionId::new(SESSION))).unwrap();
    assert_eq!(
        line,
        format!(
            "{{\"v\":1,\"chat\":\"{CHAT}\",\"token\":\"{TOKEN}\",\"at\":{AT},\"agent\":\"claude\",\
             \"event\":\"prompt_submitted\",\"session\":\"{SESSION}\",\"source\":null,\"state\":null,\
             \"detail\":null}}\n"
        )
    );
}

#[test]
fn renders_source_state_and_detail_for_their_events() {
    let line = render(&report(
        Event::SessionStart {
            source: SessionSource::Compact,
        },
        None,
    ))
    .unwrap();
    assert!(line.contains("\"event\":\"session_start\""));
    assert!(line.contains("\"source\":\"compact\""));
    assert!(line.contains("\"session\":null"));

    let line = render(&report(
        Event::State {
            state: ReportedState::Waiting,
        },
        None,
    ))
    .unwrap();
    assert!(
        line.contains("\"event\":\"state\",\"session\":null,\"source\":null,\"state\":\"waiting\"")
    );

    let line = render(&report(
        Event::NeedsInput {
            detail: Detail::new("permission_prompt"),
        },
        None,
    ))
    .unwrap();
    assert!(line.contains("\"detail\":\"permission_prompt\""));
}

#[test]
fn every_event_and_field_combination_round_trips() {
    let agents = [AgentName::claude(), AgentName::new("some-agent_2").unwrap()];
    for event in every_event() {
        for session in every_session() {
            for agent in &agents {
                for at in [0, AT, u64::MAX] {
                    let original = Report {
                        agent: agent.clone(),
                        at,
                        ..report(event.clone(), session.clone())
                    };
                    let line = render(&original).unwrap();
                    assert!(line.ends_with('\n'));
                    assert_eq!(line.matches('\n').count(), 1);
                    assert!(line.len() <= MAX_LINE_BYTES);
                    assert_eq!(parse(line.as_bytes()), Ok(original.clone()));
                    let trimmed = line.trim_end_matches('\n');
                    assert_eq!(parse(trimmed.as_bytes()), Ok(original));
                }
            }
        }
    }
}

#[test]
fn serde_round_trips_through_the_same_format() {
    for event in every_event() {
        let original = report(event, SessionId::new(SESSION));
        let text = serde_json::to_string(&original).unwrap();
        assert_eq!(format!("{text}\n"), render(&original).unwrap());
        let back: Report = serde_json::from_str(&text).unwrap();
        assert_eq!(back, original);
    }
}

#[test]
fn rendered_keys_are_exactly_the_wire_keys() {
    for event in every_event() {
        for session in every_session() {
            let line = render(&report(event.clone(), session)).unwrap();
            let Value::Object(object) = serde_json::from_str::<Value>(&line).unwrap() else {
                panic!("not an object");
            };
            let keys: Vec<&str> = object.keys().map(String::as_str).collect();
            let mut expected = WIRE_KEYS.to_vec();
            expected.sort_unstable();
            let mut sorted = keys.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, expected);
            // Every value is a number, a short string or null: nothing nested.
            for value in object.values() {
                assert!(value.is_null() || value.is_string() || value.is_number());
            }
        }
    }
}

#[test]
fn refuses_a_line_over_the_limit() {
    let mut object = valid_object();
    object.insert(
        "padding".to_string(),
        Value::String("x".repeat(MAX_LINE_BYTES)),
    );
    assert_eq!(parse_object(object), Err(ParseError::TooLarge));
    assert_eq!(
        parse(&vec![b' '; MAX_LINE_BYTES + 1]),
        Err(ParseError::TooLarge)
    );
    assert_eq!(ParseError::TooLarge.reply_error(), ReplyError::TooLarge);
}

#[test]
fn accepts_a_line_of_exactly_the_limit() {
    let line = render(&report(Event::TurnDone, None)).unwrap();
    let body = line.trim_end_matches('\n');
    // Pad with spaces inside the object so the line is MAX_LINE_BYTES long with its newline.
    let padding = " ".repeat(MAX_LINE_BYTES - line.len());
    let padded = format!("{{{padding}{}\n", &body[1..]);
    assert_eq!(padded.len(), MAX_LINE_BYTES);
    assert!(parse(padded.as_bytes()).is_ok());
}

#[test]
fn refuses_invalid_utf8() {
    let mut line = render(&report(Event::TurnDone, None)).unwrap().into_bytes();
    line.insert(10, 0xff);
    assert_eq!(parse(&line), Err(ParseError::NotUtf8));
}

#[test]
fn refuses_bad_json() {
    for line in [
        "",
        "\n",
        "{",
        "not json",
        "[1,2]",
        "null",
        "1",
        "{\"v\":1,}",
    ] {
        assert_eq!(parse(line.as_bytes()), Err(ParseError::Json), "{line:?}");
    }
}

#[test]
fn refuses_trailing_garbage() {
    let line = render(&report(Event::TurnDone, None)).unwrap();
    let body = line.trim_end_matches('\n');
    assert_eq!(parse(format!("{body}x").as_bytes()), Err(ParseError::Json));
    assert_eq!(
        parse(format!("{body}{body}").as_bytes()),
        Err(ParseError::Json)
    );
    assert_eq!(parse(format!("{body} 1").as_bytes()), Err(ParseError::Json));
    assert_eq!(
        parse(format!("{body}\n{body}\n").as_bytes()),
        Err(ParseError::NotOneLine)
    );
    assert_eq!(
        parse(format!("{body}\n\n").as_bytes()),
        Err(ParseError::NotOneLine)
    );
    // Trailing spaces are whitespace, not garbage.
    assert!(parse(format!("{body}  \n").as_bytes()).is_ok());
    assert!(parse(format!("{body}\r\n").as_bytes()).is_ok());
}

#[test]
fn refuses_an_unknown_version() {
    for version in [
        json!(0),
        json!(2),
        json!("1"),
        json!(1.5),
        json!(-1),
        json!([1]),
    ] {
        let result = with("v", version.clone());
        assert_eq!(result, Err(ParseError::UnsupportedVersion), "{version}");
        assert_eq!(
            ParseError::UnsupportedVersion.reply_error(),
            ReplyError::BadMessage
        );
    }
    assert_eq!(without("v"), Err(ParseError::MissingField("v")));
    assert_eq!(with("v", Value::Null), Err(ParseError::MissingField("v")));
}

#[test]
fn refuses_a_missing_required_field() {
    for key in ["chat", "token", "at", "agent", "event"] {
        assert_eq!(without(key), Err(ParseError::MissingField(key)), "{key}");
        assert_eq!(
            with(key, Value::Null),
            Err(ParseError::MissingField(key)),
            "{key}"
        );
    }
}

#[test]
fn optional_fields_may_be_absent() {
    let mut object = valid_object();
    for key in ["session", "source", "state", "detail"] {
        object.remove(key);
    }
    let parsed = parse_object(object).unwrap();
    assert_eq!(parsed, report(Event::PromptSubmitted, None));
}

#[test]
fn refuses_wrongly_typed_required_fields() {
    assert_eq!(with("chat", json!(5)), Err(ParseError::Json));
    assert_eq!(with("at", json!("1")), Err(ParseError::Json));
    assert_eq!(with("at", json!(-1)), Err(ParseError::Json));
    assert_eq!(with("event", json!({"a": 1})), Err(ParseError::Json));
}

#[test]
fn refuses_an_unknown_event() {
    assert_eq!(
        with("event", json!("PromptSubmitted")),
        Err(ParseError::InvalidField("event"))
    );
    assert_eq!(
        with("event", json!("")),
        Err(ParseError::InvalidField("event"))
    );
}

#[test]
fn refuses_session_start_without_a_valid_source() {
    let mut object = valid_object();
    object.insert("event".to_string(), json!("session_start"));
    assert_eq!(
        parse_object(object.clone()),
        Err(ParseError::MissingField("source"))
    );
    object.insert("source".to_string(), json!("reboot"));
    assert_eq!(
        parse_object(object.clone()),
        Err(ParseError::InvalidField("source"))
    );
    object.insert("source".to_string(), json!("resume"));
    assert_eq!(
        parse_object(object).map(|report| report.event),
        Ok(Event::SessionStart {
            source: SessionSource::Resume
        })
    );
}

#[test]
fn refuses_a_state_event_without_a_valid_state() {
    let mut object = valid_object();
    object.insert("event".to_string(), json!("state"));
    assert_eq!(
        parse_object(object.clone()),
        Err(ParseError::MissingField("state"))
    );
    object.insert("state".to_string(), json!("busy"));
    assert_eq!(
        parse_object(object.clone()),
        Err(ParseError::InvalidField("state"))
    );
    object.insert("state".to_string(), json!("idle"));
    assert_eq!(
        parse_object(object).map(|report| report.event),
        Ok(Event::State {
            state: ReportedState::Idle
        })
    );
}

#[test]
fn refuses_bad_chat_token_and_agent() {
    let long = "a".repeat(129);
    for key in ["chat", "token"] {
        for value in [
            "",
            long.as_str(),
            "has space",
            "slash/es",
            "quote\"",
            "ünicode",
        ] {
            assert_eq!(
                with(key, json!(value)),
                Err(ParseError::InvalidField(key)),
                "{key} {value:?}"
            );
        }
        assert!(with(key, json!("a".repeat(128))).is_ok());
    }
    for value in ["", "Claude", "with space", &"a".repeat(33)] {
        assert_eq!(
            with("agent", json!(value)),
            Err(ParseError::InvalidField("agent")),
            "{value:?}"
        );
    }
}

#[test]
fn accepts_other_agents_names() {
    let parsed = with("agent", json!("codex")).unwrap();
    assert_eq!(parsed.agent.as_str(), "codex");
    assert!(with("agent", json!("a".repeat(32))).is_ok());
}

#[test]
fn ignores_unknown_fields() {
    let mut object = valid_object();
    object.insert("future".to_string(), json!({"nested": [1, 2, 3]}));
    object.insert("extra".to_string(), json!("text"));
    assert_eq!(
        parse_object(object),
        Ok(report(Event::PromptSubmitted, SessionId::new(SESSION)))
    );
}

#[test]
fn ignores_fields_that_do_not_belong_to_the_event() {
    let mut object = valid_object();
    object.insert("source".to_string(), json!("anything"));
    object.insert("state".to_string(), json!("anything"));
    object.insert("detail".to_string(), json!("permission_prompt"));
    assert_eq!(
        parse_object(object),
        Ok(report(Event::PromptSubmitted, SessionId::new(SESSION)))
    );
}

#[test]
fn drops_an_invalid_session_and_keeps_the_report() {
    let long = "a".repeat(129);
    let invalid = [
        json!(""),
        json!(long),
        json!("/home/someone/.claude/projects/x.jsonl"),
        json!("has space"),
        json!("line\nbreak"),
        json!("bell\u{7}"),
        json!("ünicode"),
        json!(42),
        json!(true),
        json!({"id": SESSION}),
        json!([SESSION]),
        json!(null),
    ];
    for session in invalid {
        let parsed = with("session", session.clone());
        assert_eq!(
            parsed,
            Ok(report(Event::PromptSubmitted, None)),
            "{session}"
        );
    }
    let uppercase_uuid = "8C1E4F8A-1B2C-4D3E-8F9A-0B1C2D3E4F5A";
    assert_eq!(
        with("session", json!(uppercase_uuid))
            .unwrap()
            .session
            .map(|session| session.as_str().to_string()),
        Some(uppercase_uuid.to_string())
    );
}

#[test]
fn drops_an_invalid_detail_and_keeps_the_report() {
    let mut object = valid_object();
    object.insert("event".to_string(), json!("needs_input"));
    let long = "a".repeat(65);
    for detail in [
        json!("Permission prompt"),
        json!("permission-prompt"),
        json!("PERMISSION_PROMPT"),
        json!(""),
        json!(long),
        json!("Claude needs your permission to use Bash"),
        json!(7),
        json!({"kind": "idle_prompt"}),
    ] {
        object.insert("detail".to_string(), detail.clone());
        assert_eq!(
            parse_object(object.clone()).map(|report| report.event),
            Ok(Event::NeedsInput { detail: None }),
            "{detail}"
        );
    }
    object.insert("detail".to_string(), json!("a".repeat(64)));
    assert!(matches!(
        parse_object(object).map(|report| report.event),
        Ok(Event::NeedsInput { detail: Some(_) })
    ));
}

#[test]
fn validated_types_refuse_bad_text() {
    assert!(ChatId::new("").is_none());
    assert!(Token::new("a b").is_none());
    assert!(SessionId::new(&"a".repeat(129)).is_none());
    assert!(SessionId::new(SESSION).is_some());
    assert!(Detail::new("idle_prompt").is_some());
    assert!(Detail::new("idle prompt").is_none());
    assert!(AgentName::new("claude").is_some());
    assert_eq!(AgentName::claude().as_str(), AgentName::CLAUDE);
}

#[test]
fn token_debug_hides_the_token() {
    assert!(!format!("{:?}", token()).contains(TOKEN));
}

#[test]
fn every_parse_error_maps_to_a_reply() {
    let errors = [
        ParseError::TooLarge,
        ParseError::NotUtf8,
        ParseError::NotOneLine,
        ParseError::Json,
        ParseError::UnsupportedVersion,
        ParseError::MissingField("chat"),
        ParseError::InvalidField("chat"),
    ];
    for error in errors {
        let expected = if error == ParseError::TooLarge {
            ReplyError::TooLarge
        } else {
            ReplyError::BadMessage
        };
        assert_eq!(error.reply_error(), expected);
    }
}

#[test]
fn replies_render_and_parse() {
    assert_eq!(render_reply(Ok(())), "{\"ok\":true}\n");
    assert_eq!(parse_reply(b"{\"ok\":true}\n"), Some(Ok(())));
    let expected = [
        (ReplyError::UnknownChat, "unknown chat"),
        (ReplyError::BadToken, "bad token"),
        (ReplyError::BadMessage, "bad message"),
        (ReplyError::TooLarge, "too large"),
    ];
    for (error, wire) in expected {
        let line = render_reply(Err(error));
        assert_eq!(line, format!("{{\"ok\":false,\"error\":\"{wire}\"}}\n"));
        assert_eq!(parse_reply(line.as_bytes()), Some(Err(error)));
        assert_eq!(ReplyError::from_wire(wire), Some(error));
    }
}

#[test]
fn malformed_replies_parse_to_none() {
    for line in [
        "",
        "{}",
        "{\"ok\":false}",
        "{\"ok\":true,\"error\":\"bad token\"}",
        "{\"ok\":false,\"error\":\"other\"}",
        "{\"ok\":\"yes\"}",
        "{\"ok\":true} x",
    ] {
        assert_eq!(parse_reply(line.as_bytes()), None, "{line:?}");
    }
    assert_eq!(parse_reply(&vec![b' '; MAX_LINE_BYTES + 1]), None);
}
