//! Tests of the Slack HTTP client over the fake HTTP client. No test reaches the network, and no
//! assertion message contains the token.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bongos_slack_model::types::{ConversationId, Ts};
use bongos_slack_model::{ApiError, PostRequest, SlackApi, SlackToken};
use futures::{AsyncReadExt as _, FutureExt as _};
use gpui::TestAppContext;
use http_client::http::HeaderMap;
use http_client::{AsyncBody, FakeHttpClient, HttpClient, RedirectPolicy, Response, Url};
use serde_json::{Value, json};

use crate::client::{
    ENDPOINT, HttpSlack, MAX_RESPONSE_BYTES, REDIRECT_CODE, TIMEOUT, TOO_LARGE_CODE, client,
    http_factory, install_client_factory, resolve_endpoint,
};

const SECRET: &str = "xoxp-1111-2222-TESTSECRET";

struct Seen {
    method: String,
    uri: String,
    headers: HeaderMap,
    redirects: Option<RedirectPolicy>,
    body: Vec<u8>,
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: value.to_string().into_bytes(),
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

/// A fake Slack that records each request and answers with `handler`.
fn server(
    handler: impl Fn() -> anyhow::Result<Reply> + Send + Sync + 'static,
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
            log.lock().unwrap().push(Seen {
                method: parts.method.to_string(),
                uri: parts.uri.to_string(),
                redirects: parts.extensions.get::<RedirectPolicy>().cloned(),
                headers: parts.headers,
                body: bytes,
            });
            let reply = handler()?;
            let mut response = Response::builder().status(reply.status);
            for (name, value) in reply.headers {
                response = response.header(name, value);
            }
            Ok(response.body(AsyncBody::from(reply.body))?)
        }
    });
    (http as Arc<dyn HttpClient>, seen)
}

fn token() -> SlackToken {
    SlackToken::parse(SECRET).unwrap()
}

fn slack(http: Arc<dyn HttpClient>, cx: &TestAppContext) -> HttpSlack {
    HttpSlack::new(http, cx.executor(), &token(), ENDPOINT.to_string())
}

fn auth_reply() -> Reply {
    Reply::json(json!({
        "ok": true,
        "team": "acme",
        "team_id": "T1",
        "url": "https://acme.slack.com/",
        "user": "kyle",
        "user_id": "U1",
    }))
}

#[gpui::test]
async fn a_request_carries_a_sensitive_bearer_header_and_the_method_url(cx: &mut TestAppContext) {
    let (http, seen) =
        server(|| Ok(auth_reply().header("x-oauth-scopes", " channels:history, chat:write ,")));
    let info = slack(http, cx).auth_test().await.unwrap();
    assert!(info.scopes == ["channels:history", "chat:write"]);
    assert!(info.team == "acme");

    let seen = seen.lock().unwrap();
    assert!(seen.len() == 1, "one request, no retry");
    assert!(seen[0].method == "POST");
    assert!(seen[0].uri == "https://slack.com/api/auth.test");
    assert!(seen[0].headers["content-type"] == "application/x-www-form-urlencoded");
    let authorization = &seen[0].headers["authorization"];
    assert!(
        authorization.to_str().ok() == Some(format!("Bearer {SECRET}").as_str()),
        "the token is sent as a bearer token"
    );
    assert!(
        authorization.is_sensitive(),
        "the header is marked sensitive"
    );
    assert!(
        !format!("{:?}", seen[0].headers).contains(SECRET),
        "the header map prints no token"
    );
}

#[gpui::test]
async fn reads_are_form_encoded_and_a_post_is_json_with_formatting_off(cx: &mut TestAppContext) {
    let (http, seen) = server(|| {
        Ok(Reply::json(
            json!({ "ok": true, "channel": "C1", "ts": "1.000100", "messages": [] }),
        ))
    });
    let client = slack(http, cx);
    client
        .history(ConversationId("C1".into()), Some(Ts::new("2.000000")), 50)
        .await
        .unwrap();
    let posted = client
        .post(PostRequest {
            channel: ConversationId("C1".into()),
            text: "*hi* <!channel>".into(),
            thread_ts: Some(Ts::new("1.000100")),
        })
        .await
        .unwrap();
    assert!(posted.ts == Ts::new("1.000100"));

    let seen = seen.lock().unwrap();
    assert!(seen[0].uri == "https://slack.com/api/conversations.history");
    assert!(seen[0].body == b"channel=C1&limit=50&latest=2.000000");
    assert!(seen[1].uri == "https://slack.com/api/chat.postMessage");
    assert!(seen[1].headers["content-type"] == "application/json; charset=utf-8");
    let body: Value = serde_json::from_slice(&seen[1].body).unwrap();
    assert!(
        body == json!({
            "channel": "C1",
            "text": "*hi* <!channel>",
            "thread_ts": "1.000100",
            "mrkdwn": false,
            "parse": "none",
            "unfurl_links": false,
            "unfurl_media": false,
        })
    );
}

#[test]
fn a_release_build_ignores_the_url_override() {
    let local = "http://127.0.0.1:8123/";
    assert!(resolve_endpoint(Some(local), true) == local);
    assert!(resolve_endpoint(Some("http://127.0.0.1:8123"), true) == local);
    assert!(resolve_endpoint(Some(local), false) == ENDPOINT);
    assert!(resolve_endpoint(None, false) == ENDPOINT);
    assert!(resolve_endpoint(None, true) == ENDPOINT);
}

#[test]
fn a_url_override_that_is_not_loopback_is_ignored() {
    for other in [
        "http://example.com/",
        "http://example.com:8123/",
        "https://127.0.0.1:8123/",
        "http://127.0.0.1/",
        "http://127.0.0.1.evil.com:8123/",
        "http://127.0.0.1@evil.com:8123/",
        "http://user:pass@127.0.0.1:8123/",
        "http://127.0.0.1:8123@evil.com/",
        "http://localhost:8123/",
        "http://127.0.0.2:8123/",
        "http://[::1]:8123/",
        "http://127.0.0.1:8123/?token=1",
        "https://slack.com/api/",
        "not a url",
        "",
    ] {
        assert!(resolve_endpoint(Some(other), true) == ENDPOINT, "{other}");
    }
}

#[gpui::test]
async fn a_redirect_is_not_followed(cx: &mut TestAppContext) {
    let (http, seen) = server(|| {
        Ok(Reply::json(json!({}))
            .status(302)
            .header("location", "https://evil.example/steal"))
    });
    let error = slack(http, cx).auth_test().await.unwrap_err();
    assert!(
        error
            == ApiError::Other {
                code: REDIRECT_CODE.to_string()
            }
    );
    let seen = seen.lock().unwrap();
    assert!(seen.len() == 1, "the redirect was not followed");
    assert!(seen[0].redirects == Some(RedirectPolicy::NoFollow));
    assert!(!error.to_string().contains("evil"));
}

#[gpui::test]
async fn a_body_over_four_megabytes_is_refused(cx: &mut TestAppContext) {
    let padded = |len: usize| {
        let mut body = auth_reply().body;
        body.resize(len, b' ');
        body
    };

    let (http, _) = server(move || {
        Ok(Reply {
            status: 200,
            headers: Vec::new(),
            body: padded(MAX_RESPONSE_BYTES),
        })
    });
    assert!(
        slack(http, cx).auth_test().await.is_ok(),
        "the limit itself is read"
    );

    let (http, _) = server(move || {
        Ok(Reply {
            status: 200,
            headers: Vec::new(),
            body: padded(MAX_RESPONSE_BYTES + 1),
        })
    });
    let error = slack(http, cx).auth_test().await.unwrap_err();
    assert!(
        error
            == ApiError::Other {
                code: TOO_LARGE_CODE.to_string()
            }
    );
}

#[gpui::test]
async fn a_transport_error_text_holds_neither_the_token_nor_the_url(cx: &mut TestAppContext) {
    let (http, _) = server(|| {
        anyhow::bail!(
            "connection reset sending POST https://slack.com/api/auth.test with Authorization: Bearer {SECRET}"
        )
    });
    let error = slack(http, cx).auth_test().await.unwrap_err();
    assert!(error == ApiError::Network);
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains(SECRET), "the error text holds the token");
        assert!(!text.contains("slack.com"), "the error text holds the url");
        assert!(
            !text.contains("reset"),
            "the error text holds the transport error"
        );
    }
}

#[gpui::test]
async fn a_429_is_rate_limited_for_the_retry_after_seconds(cx: &mut TestAppContext) {
    let (http, seen) = server(|| {
        Ok(Reply::json(json!({ "ok": false, "error": "ratelimited" }))
            .status(429)
            .header("retry-after", "42"))
    });
    let error = slack(http, cx).users(None).await.unwrap_err();
    assert!(
        error
            == ApiError::RateLimited {
                retry_after: Duration::from_secs(42)
            }
    );
    assert!(seen.lock().unwrap().len() == 1, "no retry");
}

#[gpui::test]
async fn an_exchange_times_out_after_fifteen_seconds(cx: &mut TestAppContext) {
    let http = FakeHttpClient::create(|_| async {
        futures::future::pending::<anyhow::Result<Response<AsyncBody>>>().await
    });
    assert!(TIMEOUT == Duration::from_secs(15));
    let client = slack(http as Arc<dyn HttpClient>, cx);

    let mut task = cx.executor().spawn(client.auth_test());
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(14));
    cx.run_until_parked();
    assert!(
        (&mut task).now_or_never().is_none(),
        "still waiting at 14 s"
    );

    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(task.await == Err(ApiError::Network));
}

#[gpui::test]
fn debug_hides_the_token(cx: &mut TestAppContext) {
    let (http, _) = server(|| Ok(auth_reply()));
    let text = format!("{:?}", slack(http, cx));
    assert!(!text.contains(SECRET), "Debug holds the token");
    assert!(text.contains(ENDPOINT));
}

/// An HTTP client without a user agent, like gpui's placeholder.
struct Placeholder;

impl HttpClient for Placeholder {
    fn user_agent(&self) -> Option<&http_client::http::HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        _: http_client::Request<AsyncBody>,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        panic!("nothing is sent over the placeholder")
    }
}

#[gpui::test]
fn without_a_token_or_a_real_http_client_no_client_is_built(cx: &mut TestAppContext) {
    assert!(http_factory(Arc::new(Placeholder), cx.executor()).is_none());

    cx.update(|cx| {
        assert!(client(Some(&token()), cx).err() == Some(ApiError::NotConnected));
    });

    let (http, seen) = server(|| Ok(auth_reply()));
    let factory = http_factory(http, cx.executor()).expect("the fake has a user agent");
    cx.update(|cx| {
        install_client_factory(factory, cx);
        assert!(client(None, cx).err() == Some(ApiError::NotConnected));
        assert!(client(Some(&token()), cx).is_ok());
    });
    assert!(seen.lock().unwrap().is_empty(), "building sends nothing");
}
