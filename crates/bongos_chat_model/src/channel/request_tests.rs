use super::report::{ChatId, MAX_LINE_BYTES, ParseError, Token};
use super::request::{Body, Request, RequestKind, Value, head_credentials, is_request, render};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// The JSON escapes of `e` and `a` (a backslash, `u` and four hex digits), built so no tool or
/// reader decodes them in the source.
const ESCAPED_E: &str = concat!("\\", "u", "0065");
const ESCAPED_A: &str = concat!("\\", "u", "0061");

fn request_line(body: Body) -> String {
    render(&Request {
        chat: ChatId::new("chat-a").unwrap(),
        token: Token::new(TOKEN).unwrap(),
        kind: RequestKind::new("echo").unwrap(),
        body,
    })
    .unwrap()
}

/// `{"pad":"<a × pad>",` followed by `rest`.
fn padded(pad: usize, rest: &str) -> String {
    format!("{{\"pad\":\"{}\",{rest}", "a".repeat(pad))
}

#[test]
fn a_rendered_request_is_a_request_whatever_its_body() {
    assert!(is_request(request_line(Body::new()).as_bytes()));
    let big = Body::from_iter([(
        "pad".to_string(),
        Value::from("a".repeat(3 * MAX_LINE_BYTES)),
    )]);
    assert!(is_request(request_line(big).as_bytes()));
    let report = format!("{{\"v\":1,\"chat\":\"chat-a\",\"token\":\"{TOKEN}\",\"at\":1}}\n");
    assert!(!is_request(report.as_bytes()));
}

#[test]
fn an_escaped_quote_in_a_key_does_not_end_it() {
    let cases = [
        ("a quote then request", r#"{"a\"request":1}"#, false),
        ("request after a quote", r#"{"\"request":1}"#, false),
        ("a quote inside request", r#"{"req\"uest":1}"#, false),
        (
            "an escaped backslash ends the key",
            r#"{"x\\":1,"request":"k"}"#,
            true,
        ),
    ];
    for (what, line, expected) in cases {
        assert_eq!(is_request(line.as_bytes()), expected, "{what}");
    }
    // `requ` + the JSON escape of `e` + `st`: the key is matched as written, not decoded.
    let escaped_key = format!(r#"{{"requ{ESCAPED_E}st":"k"}}"#);
    assert!(!is_request(escaped_key.as_bytes()), "{escaped_key}");
}

#[test]
fn a_nested_request_key_does_not_count() {
    for line in [
        r#"{"body":{"request":"k"}}"#,
        r#"{"list":[{"request":"k"}]}"#,
        r#"{"list":["request"],"x":1}"#,
        r#"{"value":"request","x":1}"#,
    ] {
        assert!(!is_request(line.as_bytes()), "{line}");
    }
    assert!(is_request(br#"{"body":{"request":"k"},"request":"k"}"#));
}

#[test]
fn a_duplicate_request_key_still_marks_a_request() {
    assert!(is_request(br#"{"request":"a","request":"b"}"#));
}

#[test]
fn only_a_top_level_object_can_be_a_request() {
    for line in [
        r#"["request":"k"]"#,
        r#"[{"request":"k"}]"#,
        r#""request":"k""#,
        r#"{}{"request":"k"}"#,
        r#"{"a":1}"request":"k""#,
        "",
        "   ",
    ] {
        assert!(!is_request(line.as_bytes()), "{line:?}");
    }
    assert!(is_request(b" \t\r\n{\"request\":\"k\"}"));
}

#[test]
fn a_request_key_must_end_within_the_first_4_kib() {
    // `{"pad":"` is 8 bytes and `",` 2, so the key's opening quote is at `pad + 10` and its colon
    // at `pad + 19`.
    let colon_at = |pad: usize| pad + 19;
    let last_pad = MAX_LINE_BYTES - 1 - 19;
    let line = padded(last_pad, r#""request":"k"}"#);
    assert_eq!(line.as_bytes()[colon_at(last_pad)], b':');
    assert!(
        is_request(line.as_bytes()),
        "the colon is the last byte looked at"
    );

    let line = padded(last_pad + 1, r#""request":"k"}"#);
    assert_eq!(colon_at(last_pad + 1), MAX_LINE_BYTES);
    assert!(!is_request(line.as_bytes()), "the colon is one byte past");

    // The key itself straddles the boundary.
    let split = MAX_LINE_BYTES - 14;
    let line = padded(split, r#""request":"k"}"#);
    assert!(split + 10 < MAX_LINE_BYTES && colon_at(split) > MAX_LINE_BYTES);
    assert!(!is_request(line.as_bytes()), "a key cut by the boundary");
}

#[test]
fn credentials_come_from_the_first_4_kib() {
    let big = Body::from_iter([(
        "pad".to_string(),
        Value::from("a".repeat(3 * MAX_LINE_BYTES)),
    )]);
    let (chat, token) = head_credentials(request_line(big).as_bytes()).unwrap();
    assert_eq!(chat.as_str(), "chat-a");
    assert_eq!(token.as_str(), TOKEN);

    // An escaped character decodes as the full parse decodes it.
    let escaped = format!(r#"{{"chat":"ch{ESCAPED_A}t-a","token":"{TOKEN}","request":"k"}}"#);
    let (chat, _) = head_credentials(escaped.as_bytes()).unwrap();
    assert_eq!(chat.as_str(), "chat-a");
}

#[test]
fn credentials_missing_from_the_first_4_kib_are_refused() {
    let late = padded(
        MAX_LINE_BYTES,
        &format!(r#""request":"k","chat":"chat-a","token":"{TOKEN}"}}"#),
    );
    assert_eq!(
        head_credentials(late.as_bytes()),
        Err(ParseError::MissingField("chat"))
    );

    // The token's value is cut by the boundary.
    let prefix = r#"{"request":"k","chat":"chat-a","pad":""#;
    let pad = MAX_LINE_BYTES - prefix.len() - 2 - "\"token\":\"".len() - 4;
    let cut = format!(r#"{prefix}{}","token":"{TOKEN}"}}"#, "a".repeat(pad));
    assert_eq!(
        head_credentials(cut.as_bytes()),
        Err(ParseError::InvalidField("token"))
    );

    let nested = format!(r#"{{"body":{{"chat":"chat-a","token":"{TOKEN}"}},"request":"k"}}"#);
    assert_eq!(
        head_credentials(nested.as_bytes()),
        Err(ParseError::MissingField("chat"))
    );
}

#[test]
fn bad_or_duplicate_credentials_are_refused() {
    let cases = [
        (
            "a duplicate chat",
            format!(r#"{{"chat":"chat-a","chat":"chat-b","token":"{TOKEN}"}}"#),
            ParseError::Json,
        ),
        (
            "a chat that is not a string",
            format!(r#"{{"chat":7,"token":"{TOKEN}"}}"#),
            ParseError::InvalidField("chat"),
        ),
        (
            "an invalid chat",
            format!(r#"{{"chat":"a b","token":"{TOKEN}"}}"#),
            ParseError::InvalidField("chat"),
        ),
        (
            "no token",
            r#"{"chat":"chat-a"}"#.to_string(),
            ParseError::MissingField("token"),
        ),
    ];
    for (what, line, error) in cases {
        assert_eq!(head_credentials(line.as_bytes()), Err(error), "{what}");
    }
}
