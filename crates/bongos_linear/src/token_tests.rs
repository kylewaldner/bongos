//! Tests of the token modal and the credential store. The token is never printed: assertions
//! compare with `assert!` and carry messages that do not contain it.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use bongos_linear_model::fake::{Call, FakeLinear};
use bongos_linear_model::{ApiError, LinearApi};
use credentials_provider::CredentialsProvider;
use futures::FutureExt as _;
use gpui::{AsyncApp, Entity, TestAppContext, VisualTestContext};
use workspace::Workspace;

use crate::actions::{LastToast, OFF_MESSAGE};
use crate::actions_tests::{dispatch, set_enabled_in, toast_shown, workspace_with_actions};
use crate::token::{
    CREDENTIAL_URL, CREDENTIAL_USER, ENV_VAR, NO_TOKEN_MESSAGE, Token, TokenModal,
    install_client_factory, install_credentials_provider, install_env, with_token,
};

const SECRET: &str = "lin_api_TESTSECRET0123456789abcdef";

type Entries = HashMap<String, (String, Vec<u8>)>;

/// An in-memory credential store that counts its writes.
#[derive(Default)]
pub(crate) struct FakeCredentials {
    entries: Mutex<Entries>,
    writes: AtomicUsize,
}

impl FakeCredentials {
    fn stored(&self) -> Option<(String, Vec<u8>)> {
        self.entries.lock().unwrap().get(CREDENTIAL_URL).cloned()
    }

    fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }
}

type Local<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + 'a>>;

impl CredentialsProvider for FakeCredentials {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        _: &'a AsyncApp,
    ) -> Local<'a, Option<(String, Vec<u8>)>> {
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
            self.writes.fetch_add(1, Ordering::SeqCst);
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
            self.entries.lock().unwrap().remove(url);
            Ok(())
        }
        .boxed_local()
    }
}

struct Fixture {
    workspace: Entity<Workspace>,
    credentials: Arc<FakeCredentials>,
    fake: FakeLinear,
    /// How many clients the factory built, and whether each was built with the secret.
    built_with_secret: Arc<Mutex<Vec<bool>>>,
    env: Arc<Mutex<Option<String>>>,
}

async fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    let credentials = Arc::new(FakeCredentials::default());
    let fake = FakeLinear::new("acme");
    let built_with_secret = Arc::new(Mutex::new(Vec::new()));
    let env = Arc::new(Mutex::new(None::<String>));
    cx.update(|_, cx| {
        install_credentials_provider(credentials.clone(), cx);
        let (fake, built) = (fake.clone(), built_with_secret.clone());
        install_client_factory(
            Arc::new(move |token: &Token| {
                built.lock().unwrap().push(token.expose() == SECRET);
                Arc::new(fake.clone()) as Arc<dyn LinearApi>
            }),
            cx,
        );
        let env = env.clone();
        install_env(
            move |name| {
                (name == ENV_VAR)
                    .then(|| env.lock().unwrap().clone())
                    .flatten()
            },
            cx,
        );
    });
    set_enabled_in(cx, true);
    (
        Fixture {
            workspace,
            credentials,
            fake,
            built_with_secret,
            env,
        },
        cx,
    )
}

fn open_modal(fixture: &Fixture, cx: &mut VisualTestContext) -> Entity<TokenModal> {
    dispatch("linear::SetToken", cx);
    fixture
        .workspace
        .read_with(cx, |workspace, cx| workspace.active_modal::<TokenModal>(cx))
        .expect("the token modal is open")
}

fn enter_token(fixture: &Fixture, text: &str, cx: &mut VisualTestContext) {
    let modal = open_modal(fixture, cx);
    let editor = modal.read_with(cx, |modal, _| modal.editor().clone());
    editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
}

fn last_toast(cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| cx.global::<LastToast>().0.clone())
}

#[gpui::test]
async fn set_token_checks_once_and_stores(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    enter_token(&fixture, &format!("  {SECRET}\n"), cx);

    assert!(
        fixture.fake.calls() == vec![Call::Viewer],
        "exactly one viewer request"
    );
    assert!(*fixture.built_with_secret.lock().unwrap() == vec![true]);
    let (user, bytes) = fixture.credentials.stored().expect("the token is stored");
    assert!(user == CREDENTIAL_USER);
    assert!(
        bytes == SECRET.as_bytes(),
        "the stored bytes are the trimmed token"
    );
    assert!(fixture.credentials.writes.load(Ordering::SeqCst) == 1);
    assert!(last_toast(cx) == "Linear token saved (acme)");
    assert!(
        fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_modal::<TokenModal>(cx))
            .is_none(),
        "the modal closes"
    );
}

#[gpui::test]
async fn a_failed_check_stores_nothing_and_never_toasts_the_token(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;

    fixture.fake.fail_next(ApiError::Unauthorized);
    enter_token(&fixture, SECRET, cx);
    assert!(fixture.credentials.is_empty());
    assert!(fixture.credentials.writes.load(Ordering::SeqCst) == 0);
    assert!(last_toast(cx) == "Linear token not saved: Linear rejected the token");

    // An error that quotes what was sent is redacted before it reaches the toast.
    fixture
        .fake
        .fail_next(ApiError::Network(format!("bad header Bearer {SECRET}")));
    enter_token(&fixture, SECRET, cx);
    assert!(fixture.credentials.is_empty());
    let toast = last_toast(cx);
    assert!(!toast.contains(SECRET), "the toast holds no token");
    assert!(toast.contains("[redacted]"));
    assert!(fixture.fake.calls() == vec![Call::Viewer, Call::Viewer]);
}

#[gpui::test]
async fn an_empty_entry_makes_no_request(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    enter_token(&fixture, "   ", cx);
    assert!(fixture.fake.calls().is_empty());
    assert!(fixture.built_with_secret.lock().unwrap().is_empty());
    assert!(fixture.credentials.is_empty());
}

#[gpui::test]
async fn without_a_client_nothing_is_checked_or_stored(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    let credentials = Arc::new(FakeCredentials::default());
    cx.update(|_, cx| install_credentials_provider(credentials.clone(), cx));
    set_enabled_in(cx, true);
    let fixture = Fixture {
        workspace,
        credentials: credentials.clone(),
        fake: FakeLinear::new("acme"),
        built_with_secret: Default::default(),
        env: Default::default(),
    };
    enter_token(&fixture, SECRET, cx);
    assert!(credentials.is_empty());
    assert!(last_toast(cx).contains("not available"));
}

#[gpui::test]
async fn clear_token_removes_it(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    enter_token(&fixture, SECRET, cx);
    assert!(fixture.credentials.stored().is_some());

    dispatch("linear::ClearToken", cx);
    assert!(fixture.credentials.is_empty());
    assert!(last_toast(cx) == "Linear token removed");
    assert!(toast_shown(&fixture.workspace, cx));
}

async fn read_with_fixture(fixture: &Fixture, cx: &mut VisualTestContext) -> Option<Token> {
    let found = Arc::new(Mutex::new(None));
    let sink = found.clone();
    fixture.workspace.update(cx, |_, cx| {
        with_token(cx, move |_, token, _| *sink.lock().unwrap() = Some(token))
    });
    cx.run_until_parked();
    let token = found.lock().unwrap().take();
    token
}

#[gpui::test]
async fn the_store_wins_over_the_environment(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    *fixture.env.lock().unwrap() = Some("lin_api_FROMENV".to_string());

    // Only the environment: it is used.
    let token = read_with_fixture(&fixture, cx)
        .await
        .expect("the env token");
    assert!(token.expose() == "lin_api_FROMENV");

    // The store has one too: it wins.
    enter_token(&fixture, SECRET, cx);
    let token = read_with_fixture(&fixture, cx)
        .await
        .expect("the stored token");
    assert!(token.expose() == SECRET);

    // After clear, the environment is the fallback again; a blank one is no token.
    dispatch("linear::ClearToken", cx);
    let token = read_with_fixture(&fixture, cx)
        .await
        .expect("the env token again");
    assert!(token.expose() == "lin_api_FROMENV");
    *fixture.env.lock().unwrap() = Some("  ".to_string());
    assert!(read_with_fixture(&fixture, cx).await.is_none());
}

#[gpui::test]
async fn no_token_toasts_the_message_and_does_nothing_else(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    assert!(read_with_fixture(&fixture, cx).await.is_none());
    assert!(last_toast(cx) == NO_TOKEN_MESSAGE);
    assert!(NO_TOKEN_MESSAGE == "No Linear token: run linear: set token, or set LINEAR_API_KEY");
    assert!(toast_shown(&fixture.workspace, cx));
    assert!(fixture.fake.calls().is_empty());
}

#[gpui::test]
async fn the_modal_is_masked_and_leaves_nothing_behind(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    let modal = open_modal(&fixture, cx);
    let editor = modal.read_with(cx, |modal, _| modal.editor().clone());
    editor.update_in(cx, |editor, window, cx| editor.set_text(SECRET, window, cx));
    let displayed = editor.update(cx, |editor, cx| editor.display_text(cx));
    assert!(
        !displayed.contains(SECRET),
        "the editor shows the token masked"
    );
    assert!(displayed.chars().count() == SECRET.chars().count());

    // Cancel clears the editor and stores nothing.
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();
    assert!(
        editor
            .read_with(cx, |editor, cx| editor.text(cx))
            .is_empty()
    );
    assert!(fixture.credentials.is_empty());
    assert!(fixture.fake.calls().is_empty());
}

#[gpui::test]
async fn token_commands_only_say_so_while_off(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    set_enabled_in(cx, false);
    // A stored token must survive a clear while the sync is off.
    fixture.credentials.entries.lock().unwrap().insert(
        CREDENTIAL_URL.to_string(),
        (CREDENTIAL_USER.to_string(), SECRET.as_bytes().to_vec()),
    );

    dispatch("linear::SetToken", cx);
    assert!(last_toast(cx) == OFF_MESSAGE);
    assert!(
        fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_modal::<TokenModal>(cx))
            .is_none(),
        "no modal opens"
    );
    dispatch("linear::ClearToken", cx);
    assert!(last_toast(cx) == OFF_MESSAGE);
    assert!(fixture.credentials.stored().is_some());
    assert!(fixture.fake.calls().is_empty());
}

/// Records every log line of the process.
struct CaptureLog(Mutex<Vec<String>>);

impl log::Log for CaptureLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.0
            .lock()
            .unwrap()
            .push(format!("{} {}", record.target(), record.args()));
    }

    fn flush(&self) {}
}

fn capture_log() -> &'static CaptureLog {
    static LOG: OnceLock<CaptureLog> = OnceLock::new();
    let capture = LOG.get_or_init(|| CaptureLog(Mutex::new(Vec::new())));
    // Another logger installed first only makes the capture empty, never wrong.
    if log::set_logger(capture).is_ok() {
        log::set_max_level(log::LevelFilter::Trace);
    }
    capture
}

fn files_holding(dir: &Path, needle: &[u8]) -> usize {
    let mut found = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found += files_holding(&path, needle);
        } else if std::fs::read(&path)
            .unwrap()
            .windows(needle.len())
            .any(|window| window == needle)
        {
            found += 1;
        }
    }
    found
}

#[gpui::test]
async fn the_token_is_in_the_store_only(cx: &mut TestAppContext) {
    let log = capture_log();
    // The temp home stands for `~/.bongos`, the chats runtime directory and the logs directory:
    // nothing in this feature writes there.
    let home = tempfile::tempdir().unwrap();
    let (fixture, cx) = fixture(cx).await;

    enter_token(&fixture, SECRET, cx);
    fixture
        .fake
        .fail_next(ApiError::Server(format!("echo {SECRET}")));
    enter_token(&fixture, SECRET, cx);
    dispatch("linear::ClearToken", cx);
    *fixture.env.lock().unwrap() = Some(SECRET.to_string());
    assert!(read_with_fixture(&fixture, cx).await.is_some());

    assert!(files_holding(home.path(), SECRET.as_bytes()) == 0);
    assert!(
        log.0
            .lock()
            .unwrap()
            .iter()
            .all(|line| !line.contains(SECRET)),
        "no log line holds the token"
    );

    // The one token-holding type redacts itself in `Debug`.
    let token = Token::new(SECRET).unwrap();
    assert!(!format!("{token:?}").contains(SECRET));
    assert!(format!("{token:?}") == "Token(<redacted>)");
    assert!(!format!("{:?}", Some(&token)).contains(SECRET));
    assert!(token.redact(&format!("a {SECRET} b {SECRET}")) == "a [redacted] b [redacted]");
}

#[test]
fn a_token_needs_content() {
    assert!(Token::new("").is_none());
    assert!(Token::new(" \n\t").is_none());
    assert!(Token::new(" abc ").unwrap().expose() == "abc");
}
