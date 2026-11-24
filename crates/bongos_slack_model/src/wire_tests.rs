use std::time::Duration;

use crate::api::ApiError;
use crate::types::{ConversationId, ConversationKind, Cursor, Ts, UserId};
use crate::wire::{
    DEFAULT_RETRY_AFTER, parse_auth_test, parse_chat_post_message, parse_conversations_history,
    parse_conversations_list, parse_conversations_replies, parse_users_info, parse_users_list,
};

const AUTH_TEST: &str = include_str!("../test-fixtures/auth_test.json");
const CONVERSATIONS_LIST: &str = include_str!("../test-fixtures/conversations_list.json");
const HISTORY: &str = include_str!("../test-fixtures/conversations_history.json");
const HISTORY_LAST_PAGE: &str =
    include_str!("../test-fixtures/conversations_history_last_page.json");
const REPLIES: &str = include_str!("../test-fixtures/conversations_replies.json");
const USERS_INFO: &str = include_str!("../test-fixtures/users_info.json");
const USERS_LIST: &str = include_str!("../test-fixtures/users_list.json");
const POST_MESSAGE: &str = include_str!("../test-fixtures/chat_post_message.json");
const MISSING_SCOPE: &str = include_str!("../test-fixtures/error_missing_scope.json");
const INVALID_AUTH: &str = include_str!("../test-fixtures/error_invalid_auth.json");
const RATELIMITED: &str = include_str!("../test-fixtures/error_ratelimited.json");

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn error_body(code: &str) -> String {
    format!(r#"{{"ok":false,"error":"{code}"}}"#)
}

fn error_of(body: &str) -> ApiError {
    parse_users_info(200, &[], body.as_bytes()).unwrap_err()
}

#[test]
fn every_documented_example_parses() {
    let scopes = headers(&[("X-OAuth-Scopes", "channels:history, chat:write,users:read")]);
    let auth = parse_auth_test(200, &scopes, AUTH_TEST.as_bytes()).unwrap();
    assert_eq!(auth.team, "Subarachnoid Workspace");
    assert_eq!(auth.team_id, "T12345678");
    assert_eq!(auth.url, "https://subarachnoid.slack.com/");
    assert_eq!(auth.user, "grace");
    assert_eq!(auth.user_id, UserId("W12345678".to_string()));
    assert_eq!(
        auth.scopes,
        ["channels:history", "chat:write", "users:read"]
    );
    assert!(
        parse_auth_test(200, &[], AUTH_TEST.as_bytes())
            .unwrap()
            .scopes
            .is_empty()
    );

    let list = parse_conversations_list(200, &[], CONVERSATIONS_LIST.as_bytes()).unwrap();
    assert_eq!(list.items.len(), 2);
    assert_eq!(list.items[0].id, ConversationId("C012AB3CD".to_string()));
    assert_eq!(list.items[0].name, "general");
    assert_eq!(list.items[0].kind, ConversationKind::Channel);
    assert_eq!(list.next, Some(Cursor("dGVhbTpDMDYxRkE1UEI=".to_string())));

    let history = parse_conversations_history(200, &[], HISTORY.as_bytes()).unwrap();
    assert_eq!(history.items.len(), 2);
    assert_eq!(history.items[1].ts, Ts::new("1512104434.000490"));
    assert_eq!(
        history.items[1].user,
        Some(UserId("U222BBB222".to_string()))
    );
    assert_eq!(
        history.items[1].text,
        "What, you want to smell my shoes better?"
    );
    assert_eq!(history.items[1].thread_ts, None);
    assert_eq!(
        history.next,
        Some(Cursor("bmV4dF90czoxNTEyMDg1ODYxMDAwNTQz".to_string()))
    );

    let replies = parse_conversations_replies(200, &[], REPLIES.as_bytes()).unwrap();
    assert_eq!(replies.items.len(), 4);
    assert_eq!(replies.items[0].reply_count, 3);
    assert_eq!(
        replies.items[0].thread_ts,
        Some(replies.items[0].ts.clone())
    );
    assert_eq!(
        replies.items[1].thread_ts,
        Some(Ts::new("1482960137.003543"))
    );
    assert!(replies.next.is_some());

    let user = parse_users_info(200, &[], USERS_INFO.as_bytes()).unwrap();
    assert_eq!(user.id, UserId("W012A3CDE".to_string()));
    assert_eq!(user.name, "spengler");
    assert_eq!(user.display_name, "spengler");
    assert_eq!(user.real_name, "Egon Spengler");

    let users = parse_users_list(200, &[], USERS_LIST.as_bytes()).unwrap();
    assert_eq!(users.items.len(), 2);
    assert_eq!(users.items[1].display_name, "Glinda the Fairly Good");
    assert_eq!(users.next, Some(Cursor("dXNlcjpVMEc5V0ZYTlo=".to_string())));

    let posted = parse_chat_post_message(200, &[], POST_MESSAGE.as_bytes()).unwrap();
    assert_eq!(posted.channel, ConversationId("C123ABC456".to_string()));
    assert_eq!(posted.ts, Ts::new("1503435956.000247"));
}

#[test]
fn every_error_code_maps_and_no_text_echoes_the_body() {
    assert_eq!(
        parse_users_info(200, &[], INVALID_AUTH.as_bytes()),
        Err(ApiError::Unauthorized {
            reason: "invalid_auth".to_string()
        })
    );
    for code in [
        "not_authed",
        "token_revoked",
        "token_expired",
        "account_inactive",
    ] {
        assert_eq!(
            error_of(&error_body(code)),
            ApiError::Unauthorized {
                reason: code.to_string()
            },
            "{code}"
        );
    }
    assert_eq!(
        parse_conversations_history(200, &[], MISSING_SCOPE.as_bytes()),
        Err(ApiError::MissingScope {
            needed: "channels:history".to_string()
        })
    );
    assert_eq!(
        error_of(&error_body("missing_scope")),
        ApiError::MissingScope {
            needed: "unknown".to_string()
        }
    );
    for (code, what) in [
        ("channel_not_found", "channel"),
        ("thread_not_found", "thread"),
        ("message_not_found", "message"),
        ("user_not_found", "user"),
    ] {
        assert_eq!(
            error_of(&error_body(code)),
            ApiError::NotFound {
                what: what.to_string()
            },
            "{code}"
        );
    }
    assert_eq!(
        error_of(&error_body("not_in_channel")),
        ApiError::NotInChannel
    );
    for code in [
        "request_admin",
        "admin_required",
        "admin_approval_required",
        "app_approval_required",
        "restricted_action",
    ] {
        assert_eq!(error_of(&error_body(code)), ApiError::NeedsAdmin, "{code}");
    }
    assert_eq!(
        error_of(&error_body("msg_too_long")),
        ApiError::TooLong { over: 0 }
    );

    // Unknown codes are kept only when they look like Slack codes.
    assert_eq!(
        error_of(&error_body("fatal_error")),
        ApiError::Other {
            code: "fatal_error".to_string()
        }
    );
    for bad in [
        "Has Spaces xoxp-123-secret",
        "xoxp-123-secret",
        "UPPER",
        &"a".repeat(65),
        "",
    ] {
        let error = error_of(&error_body(bad));
        assert_eq!(
            error,
            ApiError::Other {
                code: "unknown_error".to_string()
            },
            "{bad}"
        );
        assert!(!error.to_string().contains("xoxp"));
    }
    assert_eq!(
        error_of(r#"{"ok":false}"#),
        ApiError::Other {
            code: "unknown_error".to_string()
        }
    );

    // A body that is not JSON, or not the method's answer, is an error that holds no text of it.
    for body in ["<html>xoxp-123-secret</html>", "", "[]", r#"{"ok":true}"#] {
        let error = error_of(body);
        assert_eq!(
            error,
            ApiError::Other {
                code: "invalid_response".to_string()
            },
            "{body}"
        );
        assert!(!error.to_string().contains("xoxp"));
    }

    // Rate limits: the header in any case, else 30 s; the body does not matter.
    let retry = |value: &str| headers(&[("retry-AFTER", value)]);
    for (headers, expected) in [
        (retry("7"), Duration::from_secs(7)),
        (retry(" 12 "), Duration::from_secs(12)),
        (retry("soon"), DEFAULT_RETRY_AFTER),
        (retry("-3"), DEFAULT_RETRY_AFTER),
        (Vec::new(), DEFAULT_RETRY_AFTER),
    ] {
        assert_eq!(
            parse_users_info(429, &headers, RATELIMITED.as_bytes()),
            Err(ApiError::RateLimited {
                retry_after: expected
            })
        );
    }
    assert_eq!(DEFAULT_RETRY_AFTER, Duration::from_secs(30));
    assert_eq!(
        parse_users_info(429, &[], b""),
        Err(ApiError::RateLimited {
            retry_after: DEFAULT_RETRY_AFTER
        })
    );
    assert_eq!(
        parse_users_info(200, &retry("5"), RATELIMITED.as_bytes()),
        Err(ApiError::RateLimited {
            retry_after: Duration::from_secs(5)
        })
    );

    // Other statuses: a server error, whatever the body says.
    for status in [500, 502, 503, 400, 404, 301] {
        assert_eq!(
            parse_users_info(status, &[], INVALID_AUTH.as_bytes()),
            Err(ApiError::Server { status }),
            "{status}"
        );
    }
}

#[test]
fn an_empty_or_missing_cursor_ends_the_paging() {
    let last = parse_conversations_history(200, &[], HISTORY_LAST_PAGE.as_bytes()).unwrap();
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.next, None);

    let page = |extra: &str| {
        format!(r#"{{"ok":true,"messages":[{{"ts":"1.000001","text":"a"}}]{extra}}}"#)
    };
    for (extra, next) in [
        (r#","response_metadata":{"next_cursor":""}"#, None),
        (r#","response_metadata":{}"#, None),
        ("", None),
        (
            r#","has_more":false,"response_metadata":{"next_cursor":"abc"}"#,
            None,
        ),
        (
            r#","has_more":true,"response_metadata":{"next_cursor":"abc"}"#,
            Some("abc"),
        ),
        (r#","response_metadata":{"next_cursor":"abc"}"#, Some("abc")),
    ] {
        let parsed = parse_conversations_replies(200, &[], page(extra).as_bytes()).unwrap();
        assert_eq!(
            parsed.next,
            next.map(|cursor| Cursor(cursor.to_string())),
            "{extra}"
        );
    }

    let no_users = r#"{"ok":true,"members":[],"response_metadata":{"next_cursor":""}}"#;
    let parsed = parse_users_list(200, &[], no_users.as_bytes()).unwrap();
    assert!(parsed.items.is_empty());
    assert_eq!(parsed.next, None);
}
