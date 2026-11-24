//! Tests of connect and disconnect over the fake Slack and an in-memory credential store, each in
//! its own temporary directory. No assertion message contains the token.

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use bongos_slack_model::fake::FakeSlack;
use bongos_slack_model::scopes::SCOPES;
use bongos_slack_model::types::{Conversation, ConversationId, Cursor, Message, Page, Ts, UserId};
use bongos_slack_model::{
    ApiError, AuthInfo, PostRequest, PostedMessage, SlackApi, SlackToken, TokenError, User,
};
use chrono::{DateTime, Utc};
use credentials_provider::CredentialsProvider;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui::{AsyncApp, TestAppContext};

use crate::allowed;
use crate::client::install_client_factory;
use crate::connection::{
    self, ALLOWED_FILE, CONNECTION_FILE, CREDENTIAL_URL, CREDENTIAL_USER, ConnectError,
    DisconnectError, StoredKind,
};

const TOKEN: &str = "xoxp-1111-2222-TESTSECRET";

type Local<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + 'a>>;

#[derive(Default)]
struct FakeCredentials {
    entries: Mutex<HashMap<String, (String, Vec<u8>)>>,
    fail_write: AtomicBool,
    fail_delete: AtomicBool,
    reads: AtomicUsize,
}

impl FakeCredentials {
    fn secret(&self) -> Option<(String, Vec<u8>)> {
        self.entries.lock().unwrap().get(CREDENTIAL_URL).cloned()
    }
}

impl CredentialsProvider for FakeCredentials {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        _: &'a AsyncApp,
    ) -> Local<'a, Option<(String, Vec<u8>)>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        async move { Ok(self.entries.lock().unwrap().get(url).cloned()) }.boxed_local()
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        _: &'a AsyncApp,
    ) -> Local<'a, ()> {
        async move {
            if self.fail_write.load(Ordering::SeqCst) {
                return Err(anyhow!("keychain locked"));
            }
            self.entries
                .lock()
                .unwrap()
                .insert(url.to_string(), (username.to_string(), password.to_vec()));
            Ok(())
        }
        .boxed_local()
    }

    fn delete_credentials<'a>(&'a self, url: &'a str, _: &'a AsyncApp) -> Local<'a, ()> {
        async move {
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(anyhow!("keychain locked"));
            }
            self.entries.lock().unwrap().remove(url);
            Ok(())
        }
        .boxed_local()
    }
}

/// An `auth.test` that waits for the gate before answering from the fake.
struct GatedSlack {
    fake: FakeSlack,
    gate: Mutex<Option<oneshot::Receiver<()>>>,
}

impl SlackApi for GatedSlack {
    fn auth_test(&self) -> BoxFuture<'static, Result<AuthInfo, ApiError>> {
        let gate = self.gate.lock().unwrap().take();
        let answer = self.fake.auth_test();
        async move {
            if let Some(gate) = gate {
                gate.await.ok();
            }
            answer.await
        }
        .boxed()
    }

    fn conversations(
        &self,
        cursor: Option<Cursor>,
    ) -> BoxFuture<'static, Result<Page<Conversation>, ApiError>> {
        self.fake.conversations(cursor)
    }

    fn history(
        &self,
        channel: ConversationId,
        latest: Option<Ts>,
        limit: usize,
    ) -> BoxFuture<'static, Result<Page<Message>, ApiError>> {
        self.fake.history(channel, latest, limit)
    }

    fn replies(
        &self,
        channel: ConversationId,
        thread_ts: Ts,
        cursor: Option<Cursor>,
    ) -> BoxFuture<'static, Result<Page<Message>, ApiError>> {
        self.fake.replies(channel, thread_ts, cursor)
    }

    fn user(&self, id: UserId) -> BoxFuture<'static, Result<User, ApiError>> {
        self.fake.user(id)
    }

    fn users(&self, cursor: Option<Cursor>) -> BoxFuture<'static, Result<Page<User>, ApiError>> {
        self.fake.users(cursor)
    }

    fn post(&self, request: PostRequest) -> BoxFuture<'static, Result<PostedMessage, ApiError>> {
        self.fake.post(request)
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    dir: PathBuf,
    credentials: Arc<FakeCredentials>,
    fake: FakeSlack,
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_000_000, 0).unwrap()
}

fn auth_info(team_id: &str, scopes: &[&str]) -> AuthInfo {
    AuthInfo {
        team: "Acme".to_string(),
        team_id: team_id.to_string(),
        url: "https://acme.slack.com/".to_string(),
        user: "kyle".to_string(),
        user_id: UserId("U0001".to_string()),
        scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
    }
}

/// A temporary home with `home`, `data` and `config` directories; the Slack directory is in
/// `data`. The client factory answers from `fake` and the credential store is in memory.
fn setup(cx: &mut TestAppContext) -> Fixture {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().to_path_buf();
    for name in ["home", "data", "config"] {
        fs::create_dir_all(root.join(name)).unwrap();
    }
    let dir = root.join("data").join("slack");
    let credentials = Arc::new(FakeCredentials::default());
    let fake = FakeSlack::new();
    cx.update(|cx| {
        connection::install(dir.clone(), cx);
        connection::install_credentials_provider(credentials.clone(), cx);
        let fake = fake.clone();
        install_client_factory(
            Arc::new(move |_: &SlackToken| Arc::new(fake.clone()) as Arc<dyn SlackApi>),
            cx,
        );
    });
    Fixture {
        _root: root_dir,
        root,
        dir,
        credentials,
        fake,
    }
}

async fn connect(
    text: &str,
    cx: &mut TestAppContext,
) -> Result<connection::Connection, ConnectError> {
    cx.update(|cx| connection::connect(text, now(), cx)).await
}

async fn disconnect(cx: &mut TestAppContext) -> Result<(), DisconnectError> {
    cx.update(connection::disconnect).await
}

fn assert_nothing_stored(fixture: &Fixture, cx: &mut TestAppContext) {
    assert!(
        fixture.credentials.secret().is_none(),
        "a secret was stored"
    );
    assert!(!fixture.dir.join(CONNECTION_FILE).exists());
    assert!(cx.update(|cx| connection::current(cx)).is_none());
}

/// Every file under `root`.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[gpui::test]
async fn valid_token_stores_secret_and_file_without_token(cx: &mut TestAppContext) {
    let fixture = setup(cx);
    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));

    let connection = connect(TOKEN, cx).await.unwrap();

    let (user, secret) = fixture.credentials.secret().unwrap();
    assert_eq!(user, CREDENTIAL_USER);
    assert!(
        secret == TOKEN.as_bytes(),
        "the stored secret is not the token"
    );
    let text = fs::read_to_string(fixture.dir.join(CONNECTION_FILE)).unwrap();
    assert!(!text.contains(TOKEN), "connection.toml holds the token");
    assert!(!text.contains("xox"), "connection.toml holds a token");
    assert_eq!(connection.workspace, "Acme");
    assert_eq!(connection.team_id, "T1");
    assert_eq!(connection.token_kind, StoredKind::User);
    assert_eq!(connection.connected_at, now());
    assert_eq!(
        connection::read_connection(&fixture.dir),
        Some(connection.clone())
    );
    assert_eq!(cx.update(|cx| connection::current(cx)), Some(connection));
}

#[gpui::test]
async fn rejected_token_missing_scope_and_refresh_token_store_nothing(cx: &mut TestAppContext) {
    let fixture = setup(cx);

    fixture.fake.push_auth_test(Err(ApiError::Unauthorized {
        reason: "invalid_auth".to_string(),
    }));
    let error = connect(TOKEN, cx).await.unwrap_err();
    assert!(matches!(error, ConnectError::Check(_)));
    assert_nothing_stored(&fixture, cx);

    let lacking: Vec<&str> = SCOPES
        .iter()
        .copied()
        .filter(|scope| *scope != "chat:write")
        .collect();
    fixture.fake.push_auth_test(Ok(auth_info("T1", &lacking)));
    let error = connect(TOKEN, cx).await.unwrap_err();
    assert_eq!(error, ConnectError::MissingScopes(vec!["chat:write"]));
    assert!(error.to_string().contains("chat:write"));
    assert_nothing_stored(&fixture, cx);

    // No `x-oauth-scopes` header: the scopes are unknown, never all granted.
    fixture.fake.push_auth_test(Ok(auth_info("T1", &[])));
    let error = connect(TOKEN, cx).await.unwrap_err();
    assert_eq!(error, ConnectError::ScopesUnknown);
    assert_nothing_stored(&fixture, cx);

    let calls = fixture.fake.calls().len();
    let error = connect("xoxe-1-REFRESHSECRET", cx).await.unwrap_err();
    assert_eq!(error, ConnectError::Token(TokenError::Refresh));
    assert!(error.to_string().contains("this is a refresh token"));
    let error = connect("xapp-1-APPSECRET", cx).await.unwrap_err();
    assert_eq!(error, ConnectError::Token(TokenError::AppLevel));
    assert_eq!(
        fixture.fake.calls().len(),
        calls,
        "a refused kind made a request"
    );
    assert_nothing_stored(&fixture, cx);
}

#[gpui::test]
async fn failing_secret_store_writes_no_file(cx: &mut TestAppContext) {
    let fixture = setup(cx);
    fixture.credentials.fail_write.store(true, Ordering::SeqCst);
    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));

    let error = connect(TOKEN, cx).await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "No secret store is available (keychain locked); nothing was saved"
    );
    assert_nothing_stored(&fixture, cx);
    assert!(files_under(&fixture.root).is_empty());
}

#[gpui::test]
async fn disconnect_deletes_secret_allowed_and_connection(cx: &mut TestAppContext) {
    let fixture = setup(cx);
    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));
    connect(TOKEN, cx).await.unwrap();
    fs::write(
        fixture.dir.join(ALLOWED_FILE),
        "[projects.\"p1\"]\nconversations = [\"C1\"]\n",
    )
    .unwrap();
    assert!(allowed::load_in(&fixture.dir, "p1").contains(&ConversationId("C1".to_string())));

    disconnect(cx).await.unwrap();

    assert!(fixture.credentials.secret().is_none());
    assert!(!fixture.dir.join(ALLOWED_FILE).exists());
    assert!(!fixture.dir.join(CONNECTION_FILE).exists());
    assert!(cx.update(|cx| connection::current(cx)).is_none());
}

#[gpui::test]
async fn failed_secret_delete_keeps_connection(cx: &mut TestAppContext) {
    let fixture = setup(cx);
    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));
    let connection = connect(TOKEN, cx).await.unwrap();
    fs::write(fixture.dir.join(ALLOWED_FILE), "").unwrap();
    fixture
        .credentials
        .fail_delete
        .store(true, Ordering::SeqCst);

    let error = disconnect(cx).await.unwrap_err();

    assert!(error.to_string().contains("keychain locked"));
    assert!(fixture.credentials.secret().is_some());
    assert!(fixture.dir.join(ALLOWED_FILE).exists());
    assert!(fixture.dir.join(CONNECTION_FILE).exists());
    assert_eq!(cx.update(|cx| connection::current(cx)), Some(connection));
}

#[gpui::test]
async fn secret_without_file_is_not_connected_and_missing_allowed_is_empty(
    cx: &mut TestAppContext,
) {
    let fixture = setup(cx);
    fixture.credentials.entries.lock().unwrap().insert(
        CREDENTIAL_URL.to_string(),
        (CREDENTIAL_USER.to_string(), TOKEN.as_bytes().to_vec()),
    );

    cx.update(|cx| connection::install(fixture.dir.clone(), cx));

    assert!(cx.update(|cx| connection::current(cx)).is_none());
    assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), 0);
    assert!(allowed::load_in(&fixture.dir, "p1").is_empty());
    fs::create_dir_all(&fixture.dir).unwrap();
    fs::write(fixture.dir.join(ALLOWED_FILE), "not = [valid").unwrap();
    assert!(allowed::load_in(&fixture.dir, "p1").is_empty());

    // The next connect overwrites the leftover secret.
    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));
    connect("xoxb-3333-NEWSECRET", cx).await.unwrap();
    let (_, secret) = fixture.credentials.secret().unwrap();
    assert!(
        secret == b"xoxb-3333-NEWSECRET",
        "the secret was not overwritten"
    );
}

#[gpui::test]
async fn connect_leaves_no_token_in_home_data_or_config(cx: &mut TestAppContext) {
    let fixture = setup(cx);
    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));

    connect(TOKEN, cx).await.unwrap();

    let files = files_under(&fixture.root);
    assert!(!files.is_empty());
    for file in files {
        let bytes = fs::read(&file).unwrap();
        let holds_token = bytes
            .windows(TOKEN.len())
            .any(|window| window == TOKEN.as_bytes());
        assert!(!holds_token, "{} holds the token", file.display());
    }
}

#[gpui::test]
async fn second_connect_while_checking_is_refused_and_stale_temporaries_removed(
    cx: &mut TestAppContext,
) {
    let fixture = setup(cx);
    fs::create_dir_all(&fixture.dir).unwrap();
    let stale = fixture.dir.join(format!(".{CONNECTION_FILE}.999-0.tmp"));
    fs::write(&stale, "interrupted").unwrap();
    let (open_gate, gate) = oneshot::channel();
    let gated = Arc::new(GatedSlack {
        fake: fixture.fake.clone(),
        gate: Mutex::new(Some(gate)),
    });
    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));
    cx.update(|cx| {
        install_client_factory(
            Arc::new(move |_: &SlackToken| gated.clone() as Arc<dyn SlackApi>),
            cx,
        )
    });

    let first = cx.update(|cx| connection::connect(TOKEN, now(), cx));
    cx.run_until_parked();
    assert!(cx.update(|cx| connection::is_busy(cx)));

    let error = connect("xoxp-4444-OTHERSECRET", cx).await.unwrap_err();
    assert_eq!(error, ConnectError::AlreadyRunning);
    assert_eq!(error.to_string(), "Slack: a connect is already running");
    assert_eq!(disconnect(cx).await, Err(DisconnectError::AlreadyRunning));

    open_gate.send(()).unwrap();
    first.await.unwrap();

    assert!(!cx.update(|cx| connection::is_busy(cx)));
    assert!(!stale.exists(), "the interrupted temporary file was kept");
    let (_, secret) = fixture.credentials.secret().unwrap();
    assert!(
        secret == TOKEN.as_bytes(),
        "the second connect stored its token"
    );
    let names: Vec<String> = fs::read_dir(&fixture.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![CONNECTION_FILE.to_string()]);
}
