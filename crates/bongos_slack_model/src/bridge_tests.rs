use std::cell::RefCell;
use std::io;

use serde_json::{Map, Value, json};

use crate::bridge::{
    ArgError, Command, DEFAULT_COUNT, PostStatus, PostText, Reply, Request, UNKNOWN_STATUS,
    parse_args, run_client,
};

fn words(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_string).collect()
}

fn no_file(_: &str) -> io::Result<String> {
    Err(io::Error::new(io::ErrorKind::NotFound, "no file"))
}

#[test]
fn there_is_no_allow_connect_or_token_op() {
    for op in [
        "allow",
        "allow-channel",
        "disallow",
        "connect",
        "disconnect",
        "token",
        "set-token",
        "approve",
        "",
    ] {
        let line = vec![op.to_string(), "#team".to_string()];
        assert_eq!(parse_args(&line), Err(ArgError::UnknownOp), "op {op:?}");
        let sent = RefCell::new(0);
        let (output, status) = run_client(&line, &no_file, &|_| {
            *sent.borrow_mut() += 1;
            Ok(Value::Null)
        });
        assert_eq!(status, 2);
        assert_eq!(*sent.borrow(), 0, "no request is made for {op:?}");
        assert!(!output.contains("#team"));
    }
    for op in ["allow", "connect", "disconnect", "token", "allow_channel"] {
        let mut body = Map::new();
        body.insert("op".to_string(), json!(op));
        body.insert("channel".to_string(), json!("#team"));
        assert!(Request::from_body(&body).is_err(), "handler refuses {op:?}");
    }
}

#[test]
fn parses_every_op() {
    assert_eq!(
        parse_args(&words("read #team")),
        Ok(Command::Read {
            channel: "#team".to_string(),
            count: DEFAULT_COUNT,
            thread: None,
        })
    );
    assert_eq!(
        parse_args(&words("read #team --count 5 --thread 1730000000.000100")),
        Ok(Command::Read {
            channel: "#team".to_string(),
            count: 5,
            thread: Some("1730000000.000100".to_string()),
        })
    );
    assert_eq!(parse_args(&words("channels")), Ok(Command::Channels));
    assert_eq!(
        parse_args(&words("post #team --text-file note.md")),
        Ok(Command::Post {
            channel: "#team".to_string(),
            thread: None,
            text: PostText::File("note.md".to_string()),
        })
    );
    assert_eq!(
        parse_args(&words("post-status abc-1")),
        Ok(Command::PostStatus {
            id: "abc-1".to_string()
        })
    );
    assert_eq!(
        parse_args(&words("read #team --count 0")),
        Err(ArgError::BadCount)
    );
    assert_eq!(
        parse_args(&words("read #team --thread yesterday")),
        Err(ArgError::BadThread)
    );
    assert_eq!(parse_args(&words("post #team")), Err(ArgError::Text));
    assert_eq!(
        parse_args(&words("post #team --text a --text-file b")),
        Err(ArgError::Text)
    );
    assert_eq!(parse_args(&[]), Err(ArgError::Missing));
}

#[test]
fn request_bodies_round_trip() {
    let requests = [
        Request::Read {
            channel: "#team".to_string(),
            count: 3,
            thread: None,
        },
        Request::Channels,
        Request::Post {
            channel: "C0123".to_string(),
            thread: Some("1730000000.000100".to_string()),
            text: "hi\nthere".to_string(),
        },
        Request::PostStatus {
            id: "x-1".to_string(),
        },
    ];
    for request in requests {
        assert_eq!(Request::from_body(&request.to_body()), Ok(request));
    }
}

#[test]
fn client_reads_the_text_file_and_prints_the_reply() {
    let line = vec![
        "post".to_string(),
        "#team".to_string(),
        "--text-file".to_string(),
        "note.md".to_string(),
    ];
    let sent = RefCell::new(None);
    let (output, status) = run_client(
        &line,
        &|path| {
            assert_eq!(path, "note.md");
            Ok("the exact text\n".to_string())
        },
        &|body| {
            *sent.borrow_mut() = Some(body);
            Ok(Reply::ok("Posted to #team: link").to_value())
        },
    );
    assert_eq!((output.as_str(), status), ("Posted to #team: link", 0));
    let body = sent.borrow().clone().unwrap();
    assert_eq!(
        Request::from_body(&body),
        Ok(Request::Post {
            channel: "#team".to_string(),
            thread: None,
            text: "the exact text\n".to_string(),
        })
    );

    let (output, status) = run_client(&words("channels"), &no_file, &|_| {
        Ok(Reply::error("Slack is not connected").to_value())
    });
    assert_eq!((output.as_str(), status), ("Slack is not connected", 1));
}

#[test]
fn post_status_words() {
    assert_eq!(PostStatus::Pending.to_string(), "pending");
    assert_eq!(
        PostStatus::Posted {
            link: "https://x/p1".to_string()
        }
        .to_string(),
        "posted https://x/p1"
    );
    assert_eq!(PostStatus::Rejected.to_string(), "rejected");
    assert_eq!(
        PostStatus::Failed {
            reason: "channel_not_found".to_string()
        }
        .to_string(),
        "failed channel_not_found"
    );
    assert_eq!(PostStatus::Unknown.to_string(), UNKNOWN_STATUS);
    assert_eq!(
        UNKNOWN_STATUS,
        "unknown (bongos restarted; nothing was posted)"
    );
}
