//! Tests of the HTTP client over the fake HTTP client. The token is never printed: assertions
//! carry messages that do not contain it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use bongos_linear_model::graphql::{BATCH_BUDGET, CHILDREN_CAP, batch_size, shrink_budget};
use bongos_linear_model::ident::parse_identifier;
use bongos_linear_model::{ApiError, IssueRef, LinearApi, NewIssue};
use futures::{AsyncReadExt as _, FutureExt as _};
use gpui::TestAppContext;
use http_client::http::HeaderMap;
use http_client::{AsyncBody, FakeHttpClient, HttpClient, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::client::{
    ENDPOINT, HttpLinear, MAX_RESPONSE_BYTES, TIMEOUT, is_loopback_url, resolve_endpoint,
};
use crate::token::Token;

const SECRET: &str = "lin_api_TESTSECRET0123456789abcdef";

struct Seen {
    method: String,
    uri: String,
    headers: HeaderMap,
    body: Value,
}

enum Body {
    Bytes(Vec<u8>),
    Raw(AsyncBody),
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Body,
}

impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: Body::Bytes(value.to_string().into_bytes()),
        }
    }

    fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }
}

/// A fake Linear: `handler` is given the index of the request and its JSON body.
fn server(
    handler: impl Fn(usize, &Value) -> anyhow::Result<Reply> + Send + Sync + 'static,
) -> (Arc<dyn HttpClient>, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(handler);
    let log = seen.clone();
    let http = FakeHttpClient::create(move |request| {
        let (log, handler) = (log.clone(), handler.clone());
        async move {
            let (parts, mut body) = request.into_parts();
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).await?;
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let index = {
                let mut seen = log.lock().unwrap();
                seen.push(Seen {
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    headers: parts.headers,
                    body: body.clone(),
                });
                seen.len() - 1
            };
            let reply = handler(index, &body)?;
            let mut response = Response::builder().status(reply.status);
            for (name, value) in reply.headers {
                response = response.header(name, value);
            }
            Ok(response.body(match reply.body {
                Body::Bytes(bytes) => AsyncBody::from(bytes),
                Body::Raw(body) => body,
            })?)
        }
    });
    (http as Arc<dyn HttpClient>, seen)
}

fn client(http: Arc<dyn HttpClient>, cx: &TestAppContext) -> HttpLinear {
    HttpLinear::new(
        http,
        cx.executor(),
        Token::new(SECRET).unwrap(),
        ENDPOINT.to_string(),
    )
}

fn viewer_reply() -> Reply {
    Reply::json(json!({ "data": { "viewer": { "organization": { "name": "acme" } } } }))
}

fn state() -> Value {
    json!({ "id": "s1", "name": "Todo", "type": "unstarted", "position": 1.0 })
}

fn child(index: usize) -> Value {
    json!({
        "id": format!("child-{index}"),
        "identifier": format!("SEQ-{}", index + 2),
        "title": "a subticket",
        "description": "",
        "url": "https://linear.app/acme/issue/SEQ-2",
        "team": { "id": "team" },
        "state": state(),
    })
}

fn children(range: std::ops::Range<usize>, next: Option<&str>) -> Value {
    json!({
        "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
        "nodes": range.map(child).collect::<Vec<_>>(),
    })
}

fn issue_with(children: Value) -> Value {
    let mut issue = child(0);
    issue["id"] = json!("parent-id");
    issue["identifier"] = json!("SEQ-1");
    issue["parent"] = Value::Null;
    issue["relations"] = json!({ "nodes": [] });
    issue["children"] = children;
    issue
}

fn refs(count: usize) -> Vec<IssueRef> {
    (1..=count)
        .map(|n| IssueRef::Identifier(parse_identifier(&format!("SEQ-{n}")).unwrap()))
        .collect()
}

fn id_count(body: &Value) -> usize {
    body["variables"].as_object().map_or(0, |v| v.len())
}

#[gpui::test]
async fn a_request_is_one_post_with_a_sensitive_authorization_header(cx: &mut TestAppContext) {
    let (http, seen) = server(|_, _| Ok(viewer_reply()));
    let linear = client(http, cx);

    assert!(linear.viewer().await == Ok("acme".to_string()));

    let seen = seen.lock().unwrap();
    assert!(seen.len() == 1);
    assert!(seen[0].method == "POST");
    assert!(seen[0].uri == ENDPOINT);
    assert!(seen[0].headers["content-type"] == "application/json");
    let authorization = &seen[0].headers["authorization"];
    assert!(authorization == SECRET, "the key is sent as it is");
    assert!(
        authorization.is_sensitive(),
        "the header is marked sensitive"
    );
    assert!(
        !format!("{:?}", seen[0].headers).contains(SECRET),
        "the header map prints no key"
    );
    assert!(seen[0].body["query"].as_str().unwrap().contains("viewer"));
}

#[gpui::test]
async fn debug_redacts_the_token(cx: &mut TestAppContext) {
    let (http, _) = server(|_, _| Ok(viewer_reply()));
    let linear = client(http, cx);
    let text = format!("{linear:?}");
    assert!(!text.contains(SECRET));
    assert!(text.contains("<redacted>"));
    assert!(text.contains(ENDPOINT));
}

#[test]
fn the_url_override_is_for_debug_builds_and_loopback_only() {
    let local = "http://127.0.0.1:8123/graphql";
    assert!(resolve_endpoint(Some(local), true) == local);
    assert!(resolve_endpoint(Some(" http://127.0.0.1:1/ "), true) == "http://127.0.0.1:1/");
    // A release build ignores it.
    assert!(resolve_endpoint(Some(local), false) == ENDPOINT);
    assert!(resolve_endpoint(None, true) == ENDPOINT);
    assert!(resolve_endpoint(None, false) == ENDPOINT);
    for other in [
        "http://localhost:8123/graphql",
        "http://127.0.0.2:8123/",
        "http://127.0.0.1/",
        "https://127.0.0.1:8123/",
        "http://example.com:8123/",
        "http://127.0.0.1.example.com:8123/",
        "http://user:pass@127.0.0.1:8123/",
        "http://127.0.0.1:8123/?key=1",
        "http://[::1]:8123/",
        "https://api.linear.app/graphql",
        "not a url",
        "",
    ] {
        assert!(!is_loopback_url(other), "{other}");
        assert!(resolve_endpoint(Some(other), true) == ENDPOINT, "{other}");
    }
}

#[gpui::test]
async fn the_complexity_header_shrinks_the_next_batch(cx: &mut TestAppContext) {
    let ids = refs(100);
    let first = batch_size(BATCH_BUDGET);
    assert!(first < ids.len(), "the test needs more than one batch");

    // Without the header the batches are full: the rest follows the first.
    let (http, seen) = server(|_, _| Ok(Reply::json(json!({ "data": {} }))));
    client(http, cx).issues(ids.clone()).await.unwrap();
    let sizes: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| id_count(&s.body))
        .collect();
    assert!(sizes == vec![first, ids.len() - first]);

    // With an X-Complexity above the estimate, the next batch is smaller.
    let estimate =
        bongos_linear_model::graphql::issues_requests(&ids[..first], BATCH_BUDGET)[0].complexity;
    let observed = estimate * 3;
    let (http, seen) = server(move |index, _| {
        let reply = Reply::json(json!({ "data": {} }));
        Ok(if index == 0 {
            reply.header("X-Complexity", &observed.to_string())
        } else {
            reply
        })
    });
    client(http, cx).issues(ids.clone()).await.unwrap();
    let sizes: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| id_count(&s.body))
        .collect();
    let shrunk = batch_size(shrink_budget(BATCH_BUDGET, estimate, observed));
    assert!(shrunk < first);
    assert!(
        sizes[..2] == [first, shrunk],
        "the header was read: {sizes:?}"
    );
    assert!(sizes.iter().sum::<usize>() == ids.len());
}

#[gpui::test]
async fn subtickets_are_paged_up_to_the_cap(cx: &mut TestAppContext) {
    let (http, seen) = server(|index, body| {
        let query = body["query"].as_str().unwrap_or("");
        Ok(match index {
            0 => Reply::json(json!({ "data": { "i0": issue_with(children(0..25, Some("c1"))) } })),
            1 => {
                assert!(query.contains("first: 100") && body["variables"]["after"] == "c1");
                Reply::json(
                    json!({ "data": { "issue": { "children": children(25..125, Some("c2")) } } }),
                )
            }
            2 => {
                assert!(query.contains("first: 75") && body["variables"]["after"] == "c2");
                Reply::json(
                    json!({ "data": { "issue": { "children": children(125..200, Some("c3")) } } }),
                )
            }
            _ => panic!("no request past the cap"),
        })
    });
    let issues = client(http, cx).issues(refs(1)).await.unwrap();
    assert!(seen.lock().unwrap().len() == 3);
    assert!(issues.len() == 1);
    assert!(issues[0].children.len() == CHILDREN_CAP);
    assert!(issues[0].children_truncated);
    assert!(issues[0].children[199].id == "child-199");
}

#[gpui::test]
async fn subtickets_that_fit_are_not_truncated(cx: &mut TestAppContext) {
    let (http, seen) = server(|index, _| {
        Ok(match index {
            0 => Reply::json(json!({ "data": { "i0": issue_with(children(0..25, Some("c1"))) } })),
            _ => {
                Reply::json(json!({ "data": { "issue": { "children": children(25..40, None) } } }))
            }
        })
    });
    let issues = client(http, cx).issues(refs(1)).await.unwrap();
    assert!(seen.lock().unwrap().len() == 2);
    assert!(issues[0].children.len() == 40);
    assert!(!issues[0].children_truncated);
}

#[gpui::test]
async fn a_page_that_never_ends_stops_when_it_brings_nothing(cx: &mut TestAppContext) {
    let (http, seen) = server(|index, _| {
        Ok(match index {
            0 => Reply::json(json!({ "data": { "i0": issue_with(children(0..25, Some("c1"))) } })),
            _ => Reply::json(
                json!({ "data": { "issue": { "children": children(0..0, Some("c1")) } } }),
            ),
        })
    });
    let issues = client(http, cx).issues(refs(1)).await.unwrap();
    assert!(seen.lock().unwrap().len() == 2);
    assert!(issues[0].children.len() == 25);
}

#[gpui::test]
async fn a_body_over_four_megabytes_is_refused(cx: &mut TestAppContext) {
    let padding = |len: usize| {
        let mut body = br#"{"data":{"viewer":{"organization":{"name":"acme"}}}}"#.to_vec();
        body.resize(len, b' ');
        body
    };

    // Exactly the limit is read.
    let (http, _) = server(move |_, _| {
        Ok(Reply {
            status: 200,
            headers: Vec::new(),
            body: Body::Bytes(padding(MAX_RESPONSE_BYTES)),
        })
    });
    assert!(client(http, cx).viewer().await == Ok("acme".to_string()));

    // One byte more is refused.
    let (http, _) = server(move |_, _| {
        Ok(Reply {
            status: 200,
            headers: Vec::new(),
            body: Body::Bytes(padding(MAX_RESPONSE_BYTES + 1)),
        })
    });
    let error = client(http, cx).viewer().await.unwrap_err();
    assert!(matches!(&error, ApiError::Server(text) if text.contains("4 MB")));
}

/// A body that never ends, and counts what was read from it.
struct Endless(Arc<AtomicUsize>);

impl futures::AsyncRead for Endless {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        buffer.fill(b' ');
        self.0.fetch_add(buffer.len(), Ordering::SeqCst);
        Poll::Ready(Ok(buffer.len()))
    }
}

#[gpui::test]
async fn an_endless_body_is_never_buffered_unboundedly(cx: &mut TestAppContext) {
    let read = Arc::new(AtomicUsize::new(0));
    let counter = read.clone();
    let counted = Arc::new(Mutex::new(Some(counter)));
    let (http, _) = server(move |_, _| {
        let counter = counted.lock().unwrap().take().expect("one request");
        Ok(Reply {
            status: 200,
            headers: Vec::new(),
            body: Body::Raw(AsyncBody::from_reader(Endless(counter))),
        })
    });
    let error = client(http, cx).viewer().await.unwrap_err();
    assert!(matches!(error, ApiError::Server(_)));
    // `take` hands the reader buffers no larger than what is left of MAX + 1.
    assert!(read.load(Ordering::SeqCst) <= MAX_RESPONSE_BYTES + 1);
}

#[gpui::test]
async fn an_exchange_times_out_after_fifteen_seconds(cx: &mut TestAppContext) {
    let http = FakeHttpClient::create(|_| async {
        futures::future::pending::<anyhow::Result<Response<AsyncBody>>>().await
    });
    assert!(TIMEOUT == Duration::from_secs(15));
    let linear = client(http as Arc<dyn HttpClient>, cx);

    let mut task = cx.executor().spawn(linear.viewer());
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(14));
    cx.run_until_parked();
    assert!(
        (&mut task).now_or_never().is_none(),
        "still waiting at 14 s"
    );

    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let error = task.await.unwrap_err();
    assert!(matches!(&error, ApiError::Network(text) if text.contains("timed out")));
}

#[gpui::test]
async fn error_texts_hold_no_token_and_no_transport_detail(cx: &mut TestAppContext) {
    // A transport error that quotes everything it saw.
    let (http, _) = server(|_, _| {
        Err(anyhow::anyhow!(
            "connect failed: Authorization: {SECRET} for https://api.linear.app/graphql?key={SECRET}"
        ))
    });
    let error = client(http, cx).viewer().await.unwrap_err();
    assert!(error == ApiError::Network("the request failed".to_string()));

    // A GraphQL message that echoes the token, another key, and runs long.
    let (http, _) = server(|_, _| {
        let message = format!(
            "bad key {SECRET} and lin_api_OTHERKEY99 also \u{7}{}",
            "x".repeat(500)
        );
        Ok(Reply::json(
            json!({ "errors": [{ "message": message, "extensions": { "code": "INTERNAL" } }] }),
        ))
    });
    let error = client(http, cx).viewer().await.unwrap_err();
    let ApiError::Server(text) = &error else {
        panic!("a server error");
    };
    assert!(!text.contains(SECRET) && !text.contains("TESTSECRET"));
    assert!(!text.contains("OTHERKEY99") && !text.contains("lin_api_"));
    assert!(text.contains("[redacted]"));
    assert!(text.chars().count() <= 200 && !text.contains('\u{7}'));
    assert!(!error.to_string().contains(SECRET));

    // A body that is not JSON is not quoted either.
    let (http, _) = server(|_, _| {
        Ok(Reply {
            status: 200,
            headers: Vec::new(),
            body: Body::Bytes(format!("not json {SECRET}").into_bytes()),
        })
    });
    let error = client(http, cx).viewer().await.unwrap_err();
    assert!(error == ApiError::Server("malformed Linear response".to_string()));
}

#[gpui::test]
async fn statuses_map_to_errors(cx: &mut TestAppContext) {
    let (http, _) = server(|_, _| Ok(Reply::json(json!({})).status(401)));
    assert!(client(http, cx).viewer().await == Err(ApiError::Unauthorized));

    let (http, _) = server(|_, _| Ok(Reply::json(json!({})).status(502)));
    assert!(client(http, cx).viewer().await == Err(ApiError::Server("HTTP 502".into())));

    // A page that is not JSON.
    let (http, _) = server(|_, _| {
        Ok(Reply {
            status: 503,
            headers: Vec::new(),
            body: Body::Bytes(b"<html>busy</html>".to_vec()),
        })
    });
    assert!(client(http, cx).viewer().await == Err(ApiError::Server("HTTP 503".into())));
}

#[gpui::test]
async fn the_rate_limit_reset_is_read_in_milliseconds_or_seconds(cx: &mut TestAppContext) {
    for (header, expected) in [
        ("1700000000000", 1_700_000_000_000),
        // Plainly seconds: scaled up.
        ("1700000000", 1_700_000_000_000),
    ] {
        let (http, _) = server(move |_, _| {
            Ok(Reply::json(json!({}))
                .status(429)
                .header("X-RateLimit-Requests-Reset", header))
        });
        assert!(
            client(http, cx).viewer().await
                == Err(ApiError::RateLimited {
                    reset_at: Some(expected)
                })
        );
    }
}

#[gpui::test]
async fn a_token_that_cannot_be_a_header_sends_nothing(cx: &mut TestAppContext) {
    let (http, seen) = server(|_, _| Ok(viewer_reply()));
    let linear = HttpLinear::new(
        http,
        cx.executor(),
        Token::new("lin_api_bad\u{1}token").unwrap(),
        ENDPOINT.to_string(),
    );
    assert!(linear.viewer().await == Err(ApiError::Unauthorized));
    assert!(seen.lock().unwrap().is_empty());
}

#[gpui::test]
async fn writes_go_through_the_same_path(cx: &mut TestAppContext) {
    let (http, seen) = server(|index, _| {
        Ok(match index {
            0 => Reply::json(json!({ "data": { "issueUpdate": { "success": true } } })),
            _ => Reply::json(json!({ "errors": [{ "message": "identifier must be unique",
                "extensions": { "code": "INVALID_INPUT" } }] })),
        })
    });
    let linear = client(http, cx);
    assert!(linear.set_state("issue-1", "state-1").await == Ok(()));
    let id = Uuid::from_u128(7);
    let created = linear
        .create(NewIssue {
            id,
            team_id: "team".into(),
            parent_id: None,
            title: "a ticket".into(),
            description: String::new(),
        })
        .await;
    assert!(matches!(created, Err(ApiError::Duplicate(_))));
    assert!(seen.lock().unwrap().len() == 2);
}

#[gpui::test]
async fn set_token_checks_the_token_with_one_request_over_http(cx: &mut TestAppContext) {
    use crate::actions_tests::{dispatch, set_enabled_in, workspace_with_actions};
    use crate::token::{TokenModal, install_credentials_provider};
    use crate::token_tests::FakeCredentials;

    let (workspace, cx) = workspace_with_actions(cx).await;
    let credentials = Arc::new(FakeCredentials::default());
    let (http, seen) = server(|_, _| Ok(viewer_reply()));
    cx.update(|_, cx| {
        install_credentials_provider(credentials.clone(), cx);
        cx.set_http_client(http);
        crate::init_client(cx);
    });

    // While the sync is off the command opens nothing and sends nothing.
    set_enabled_in(cx, false);
    dispatch("linear::SetToken", cx);
    assert!(seen.lock().unwrap().is_empty());

    set_enabled_in(cx, true);
    dispatch("linear::SetToken", cx);
    let modal = workspace
        .read_with(cx, |workspace, cx| workspace.active_modal::<TokenModal>(cx))
        .expect("the token modal is open");
    let editor = modal.read_with(cx, |modal, _| modal.editor().clone());
    editor.update_in(cx, |editor, window, cx| editor.set_text(SECRET, window, cx));
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    let seen = seen.lock().unwrap();
    assert!(seen.len() == 1, "exactly one request");
    assert!(seen[0].uri == ENDPOINT);
    assert!(seen[0].headers["authorization"].is_sensitive());
    assert!(credentials.stored().is_some(), "accepted, so stored");
}
