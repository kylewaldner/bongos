//! [`HttpSlack`]: the [`SlackApi`] over the application's shared HTTP client, and the
//! [`ClientFactory`] that builds one from a token.
//!
//! Every request is one `POST` to `https://slack.com/api/<method>`: the reads with a
//! form-encoded body, `chat.postMessage` with a JSON body. The token goes in a sensitive
//! `Authorization: Bearer` header. One exchange, from sending to the last byte of the response,
//! may take [`TIMEOUT`]; at most [`MAX_RESPONSE_BYTES`] of the body are accepted, and at most one
//! byte more is read. Redirects are not followed, so the header never goes to another host, and
//! nothing is ever retried.
//!
//! Error texts are redacted by construction: every error this module makes is a fixed text, and
//! the ones parsed from Slack's answers (`bongos_slack_model::wire`) keep only an error code that
//! matches `^[a-z_]{1,64}$`. The transport error, the url, the headers and the token never reach
//! an error, a `Debug` output or a log line.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bongos_slack_model::token::HIDDEN;
use bongos_slack_model::types::{Conversation, ConversationId, Cursor, Message, Page, Ts, UserId};
use bongos_slack_model::wire;
use bongos_slack_model::{
    ApiError, AuthInfo, PostRequest, PostedMessage, SlackApi, SlackToken, User,
};
use futures::future::{self, BoxFuture, Either};
use futures::{AsyncReadExt as _, FutureExt as _};
use gpui::{App, BackgroundExecutor, Global};
use http_client::http::{HeaderMap, HeaderValue, header};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, RedirectPolicy, Url};
use serde_json::json;

/// Where requests go: this followed by the method name.
pub const ENDPOINT: &str = "https://slack.com/api/";
/// A debug build sends to this loopback url instead when it is set (see [`resolve_endpoint`]).
pub const OVERRIDE_ENV: &str = "BONGOS_SLACK_API_URL";
/// How long one exchange, from sending to the last byte of the response, may take.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// The largest response body accepted.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// The longest wait a `Retry-After` header is believed for.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
/// The shortest wait after a 429, so a `Retry-After: 0` does not invite a burst.
pub const MIN_RETRY_AFTER: Duration = Duration::from_secs(1);

/// The `Other` code of an answer that tried to redirect.
pub const REDIRECT_CODE: &str = "redirect_refused";
/// The `Other` code of an answer over [`MAX_RESPONSE_BYTES`].
pub const TOO_LARGE_CODE: &str = "response_too_large";

/// Page size of the paged reads.
const PAGE_LIMIT: usize = 200;
/// The largest `limit` `conversations.history` takes.
const HISTORY_LIMIT_MAX: usize = 999;

/// The endpoint to use: `override_url` when this is a debug build and the url is
/// `http://127.0.0.1:<port>/...`, else [`ENDPOINT`]. A release build ignores the override. The
/// result ends with `/`, so a method name can be appended.
pub fn resolve_endpoint(override_url: Option<&str>, is_debug: bool) -> String {
    match override_url.and_then(loopback_url) {
        Some(url) if is_debug => {
            let mut text = url.to_string();
            if !text.ends_with('/') {
                text.push('/');
            }
            text
        }
        _ => ENDPOINT.to_string(),
    }
}

/// [`resolve_endpoint`] with the environment and this build's profile.
pub fn endpoint() -> String {
    resolve_endpoint(
        std::env::var(OVERRIDE_ENV).ok().as_deref(),
        cfg!(debug_assertions),
    )
}

/// The parsed [`Url`] when `text` is an `http` url to `127.0.0.1` with an explicit port, no
/// credentials, no query and no fragment.
fn loopback_url(text: &str) -> Option<Url> {
    let url = Url::parse(text.trim()).ok()?;
    let loopback = url.scheme() == "http"
        && url.host() == Some(http_client::Host::Ipv4([127, 0, 0, 1].into()))
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    loopback.then_some(url)
}

/// Builds the client that sends requests with a token. [`init_client`] installs the one over the
/// application's HTTP client; tests install one that returns a fake.
pub type ClientFactory = Arc<dyn Fn(&SlackToken) -> Arc<dyn SlackApi> + Send + Sync>;

struct InstalledClientFactory(ClientFactory);

impl Global for InstalledClientFactory {}

/// Installs the builder of the clients that talk to Slack.
pub fn install_client_factory(factory: ClientFactory, cx: &mut App) {
    cx.set_global(InstalledClientFactory(factory));
}

/// Installs the factory over the application's HTTP client. Call once the HTTP client is set.
/// Over gpui's placeholder client nothing is installed, so no request can be made.
pub fn init_client(cx: &mut App) {
    match http_factory(cx.http_client(), cx.background_executor().clone()) {
        Some(factory) => install_client_factory(factory, cx),
        None => log::warn!("no HTTP client is set, so the Slack client is off"),
    }
}

/// The client for `token`: [`ApiError::NotConnected`] without a token or without an installed
/// factory, so no request is possible then.
pub fn client(token: Option<&SlackToken>, cx: &App) -> Result<Arc<dyn SlackApi>, ApiError> {
    let token = token.ok_or(ApiError::NotConnected)?;
    let installed = cx
        .try_global::<InstalledClientFactory>()
        .ok_or(ApiError::NotConnected)?;
    Ok((installed.0)(token))
}

/// Whether `http` is gpui's placeholder client, which fails every request. That type is private
/// to gpui, so it is told apart by having no user agent: the application's client and the test
/// fake both have one.
pub fn is_placeholder_client(http: &dyn HttpClient) -> bool {
    http.user_agent().is_none()
}

/// A [`ClientFactory`] that builds [`HttpSlack`]s over `http`, or `None` when `http` is gpui's
/// placeholder client.
pub fn http_factory(
    http: Arc<dyn HttpClient>,
    executor: BackgroundExecutor,
) -> Option<ClientFactory> {
    if is_placeholder_client(&*http) {
        return None;
    }
    Some(Arc::new(move |token: &SlackToken| {
        Arc::new(HttpSlack::new(
            http.clone(),
            executor.clone(),
            token,
            endpoint(),
        )) as Arc<dyn SlackApi>
    }))
}

struct Inner {
    http: Arc<dyn HttpClient>,
    executor: BackgroundExecutor,
    /// Ends with `/`.
    endpoint: String,
    /// `Bearer <token>`, marked sensitive. `None` when the token cannot be a header value; then
    /// nothing is sent.
    authorization: Option<HeaderValue>,
}

/// The Slack Web API over HTTP. It holds no state between calls: each call owns its request.
/// `Debug` shows the endpoint only.
#[derive(Clone)]
pub struct HttpSlack {
    inner: Arc<Inner>,
}

impl fmt::Debug for HttpSlack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSlack")
            .field("endpoint", &self.inner.endpoint)
            .field("token", &HIDDEN)
            .finish()
    }
}

impl HttpSlack {
    pub fn new(
        http: Arc<dyn HttpClient>,
        executor: BackgroundExecutor,
        token: &SlackToken,
        endpoint: String,
    ) -> Self {
        let authorization = HeaderValue::from_str(&format!("Bearer {}", token.secret()))
            .ok()
            .map(|mut value| {
                value.set_sensitive(true);
                value
            });
        Self {
            inner: Arc::new(Inner {
                http,
                executor,
                endpoint,
                authorization,
            }),
        }
    }
}

/// What a request carries.
enum Body {
    Form(Vec<(&'static str, String)>),
    Json(serde_json::Value),
}

/// The status, the headers the parsers read and the body of an answer.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// The headers `wire` reads; no other header is kept.
const READ_HEADERS: &[&str] = &["retry-after", "x-oauth-scopes"];

fn read_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    READ_HEADERS
        .iter()
        .filter_map(|name| {
            let value = headers.get(*name)?.to_str().ok()?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

fn other(code: &str) -> ApiError {
    ApiError::Other {
        code: code.to_string(),
    }
}

/// One exchange without the time limit: send, refuse a redirect, read at most
/// [`MAX_RESPONSE_BYTES`] plus one byte.
async fn exchange_unbounded(inner: &Inner, method: &str, body: Body) -> Result<Answer, ApiError> {
    let Some(authorization) = inner.authorization.clone() else {
        return Err(ApiError::NotConnected);
    };
    let (content_type, bytes) = match body {
        Body::Form(pairs) => {
            let encoded = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(pairs.iter().map(|(name, value)| (*name, value.as_str())))
                .finish();
            ("application/x-www-form-urlencoded", encoded.into_bytes())
        }
        Body::Json(value) => (
            "application/json; charset=utf-8",
            serde_json::to_vec(&value).map_err(|_| other("invalid_request"))?,
        ),
    };
    let request = http_client::Request::builder()
        .method(Method::POST)
        .uri(format!("{}{method}", inner.endpoint))
        .header(header::CONTENT_TYPE, content_type)
        .header(header::AUTHORIZATION, authorization)
        .follow_redirects(RedirectPolicy::NoFollow)
        .body(AsyncBody::from(bytes))
        .map_err(|_| ApiError::Network)?;
    // The transport error may quote the url or the request; it is dropped.
    let response = inner
        .http
        .send(request)
        .await
        .map_err(|_| ApiError::Network)?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(other(REDIRECT_CODE));
    }
    let headers = read_headers(response.headers());
    let mut reader = response.into_body().take(MAX_RESPONSE_BYTES as u64 + 1);
    let mut body = Vec::new();
    reader
        .read_to_end(&mut body)
        .await
        .map_err(|_| ApiError::Network)?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(other(TOO_LARGE_CODE));
    }
    Ok(Answer {
        status,
        headers,
        body,
    })
}

/// One exchange within [`TIMEOUT`].
async fn exchange(inner: &Inner, method: &str, body: Body) -> Result<Answer, ApiError> {
    let work = exchange_unbounded(inner, method, body).boxed();
    match future::select(work, inner.executor.timer(TIMEOUT)).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(ApiError::Network),
    }
}

/// Keeps a `Retry-After` within [`MIN_RETRY_AFTER`] and [`MAX_RETRY_AFTER`].
fn clamp(error: ApiError) -> ApiError {
    match error {
        ApiError::RateLimited { retry_after } => ApiError::RateLimited {
            retry_after: retry_after.clamp(MIN_RETRY_AFTER, MAX_RETRY_AFTER),
        },
        other => other,
    }
}

/// Sends `body` to `method` and parses the answer with `parse`.
async fn call<T>(
    inner: &Inner,
    method: &str,
    body: Body,
    parse: fn(u16, &wire::Headers, &[u8]) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let answer = exchange(inner, method, body).await.map_err(clamp)?;
    parse(answer.status, &answer.headers, &answer.body).map_err(clamp)
}

fn paged(mut pairs: Vec<(&'static str, String)>, cursor: Option<Cursor>) -> Body {
    pairs.push(("limit", PAGE_LIMIT.to_string()));
    if let Some(cursor) = cursor {
        pairs.push(("cursor", cursor.0));
    }
    Body::Form(pairs)
}

impl SlackApi for HttpSlack {
    fn auth_test(&self) -> BoxFuture<'static, Result<AuthInfo, ApiError>> {
        let inner = self.inner.clone();
        // Slack documents `x-oauth-scopes` on every Web API response; `wire` reads the scopes
        // from it (not confirmed against a user token).
        async move {
            call(
                &inner,
                "auth.test",
                Body::Form(Vec::new()),
                wire::parse_auth_test,
            )
            .await
        }
        .boxed()
    }

    fn conversations(
        &self,
        cursor: Option<Cursor>,
    ) -> BoxFuture<'static, Result<Page<Conversation>, ApiError>> {
        let inner = self.inner.clone();
        async move {
            let body = paged(
                vec![
                    (
                        "types",
                        "public_channel,private_channel,mpim,im".to_string(),
                    ),
                    ("exclude_archived", "true".to_string()),
                ],
                cursor,
            );
            call(
                &inner,
                "conversations.list",
                body,
                wire::parse_conversations_list,
            )
            .await
        }
        .boxed()
    }

    fn history(
        &self,
        channel: ConversationId,
        latest: Option<Ts>,
        limit: usize,
    ) -> BoxFuture<'static, Result<Page<Message>, ApiError>> {
        let inner = self.inner.clone();
        async move {
            let mut pairs = vec![
                ("channel", channel.0),
                ("limit", limit.clamp(1, HISTORY_LIMIT_MAX).to_string()),
            ];
            if let Some(latest) = latest {
                pairs.push(("latest", latest.as_str().to_string()));
            }
            call(
                &inner,
                "conversations.history",
                Body::Form(pairs),
                wire::parse_conversations_history,
            )
            .await
        }
        .boxed()
    }

    fn replies(
        &self,
        channel: ConversationId,
        thread_ts: Ts,
        cursor: Option<Cursor>,
    ) -> BoxFuture<'static, Result<Page<Message>, ApiError>> {
        let inner = self.inner.clone();
        async move {
            let body = paged(
                vec![
                    ("channel", channel.0),
                    ("ts", thread_ts.as_str().to_string()),
                ],
                cursor,
            );
            call(
                &inner,
                "conversations.replies",
                body,
                wire::parse_conversations_replies,
            )
            .await
        }
        .boxed()
    }

    fn user(&self, id: UserId) -> BoxFuture<'static, Result<User, ApiError>> {
        let inner = self.inner.clone();
        async move {
            call(
                &inner,
                "users.info",
                Body::Form(vec![("user", id.0)]),
                wire::parse_users_info,
            )
            .await
        }
        .boxed()
    }

    fn users(&self, cursor: Option<Cursor>) -> BoxFuture<'static, Result<Page<User>, ApiError>> {
        let inner = self.inner.clone();
        async move {
            call(
                &inner,
                "users.list",
                paged(Vec::new(), cursor),
                wire::parse_users_list,
            )
            .await
        }
        .boxed()
    }

    fn post(&self, request: PostRequest) -> BoxFuture<'static, Result<PostedMessage, ApiError>> {
        let inner = self.inner.clone();
        async move {
            let mut body = json!({
                "channel": request.channel.0,
                "text": request.text,
                "mrkdwn": false,
                "parse": "none",
                "unfurl_links": false,
                "unfurl_media": false,
            });
            if let Some(thread_ts) = request.thread_ts {
                body["thread_ts"] = json!(thread_ts.as_str());
            }
            call(
                &inner,
                "chat.postMessage",
                Body::Json(body),
                wire::parse_chat_post_message,
            )
            .await
        }
        .boxed()
    }
}
