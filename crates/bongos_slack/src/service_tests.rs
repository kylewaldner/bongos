//! Tests of the Slack service over the fake Slack, an in-memory credential store and the test
//! executor's clock: every wait is virtual time, advanced by the test, never slept.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bongos_slack_model::fake::{Call, FakeSlack};
use bongos_slack_model::render::FixedOffset;
use bongos_slack_model::scopes::SCOPES;
use bongos_slack_model::types::{
    Conversation, ConversationId, ConversationKind, Message, Page, Ts, UserId,
};
use bongos_slack_model::{
    AllowedList, ApiError, Destination, Origin, PostedMessage, SlackApi, SlackToken, User,
};
use chrono::{DateTime, Utc};
use gpui::{AppContext as _, BorrowAppContext as _, Entity, TestAppContext};
use settings::{Settings as _, SettingsStore};

use crate::SlackSettings;
use crate::client::install_client_factory;
use crate::connection::{self, Connection};
use crate::connection_tests::{FakeCredentials, TOKEN, auth_info};
use crate::service::{
    Answer, MAY_NOT_HAVE_BEEN_SENT, NOT_CONNECTED_MESSAGE, OFF_MESSAGE, RECONNECT_MESSAGE, Read,
    SlackError, SlackService, SlackState,
};
use crate::status_item::{Shown, SlackStatusItem, StatusFacts, status_text};

/// One hour east of UTC, so the local time differs from UTC.
pub(crate) const ZONE: FixedOffset = FixedOffset(3600);

pub(crate) struct Fixture {
    _root: tempfile::TempDir,
    pub(crate) dir: PathBuf,
    credentials: Arc<FakeCredentials>,
    pub(crate) fake: FakeSlack,
    pub(crate) service: Entity<SlackService>,
}

pub(crate) fn set_enabled(enabled: bool, cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.slack.get_or_insert_default().enabled = Some(enabled)
            });
        });
    });
}

/// A connected service (unless `connected` is false) with `slack.enabled` on, the token in the
/// in-memory store and the fake as the client.
pub(crate) fn setup(connected: bool, cx: &mut TestAppContext) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("slack");
    if connected {
        let connection = Connection::new(
            &auth_info("T1", SCOPES),
            SlackToken::parse(TOKEN).unwrap().kind(),
            DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap(),
        );
        connection::write_connection(&dir, &connection).unwrap();
    }
    let credentials = Arc::new(FakeCredentials::default());
    if connected {
        credentials.store(TOKEN);
    }
    let fake = FakeSlack::new();
    let service = cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        SlackSettings::register(cx);
        connection::install(dir.clone(), cx);
        connection::install_credentials_provider(credentials.clone(), cx);
        let client = fake.clone();
        install_client_factory(
            Arc::new(move |_: &SlackToken| Arc::new(client.clone()) as Arc<dyn SlackApi>),
            cx,
        );
        let clock = cx
            .background_executor()
            .scheduler_executor()
            .scheduler()
            .clock();
        SlackService::install(clock, Arc::new(ZONE), cx)
    });
    set_enabled(true, cx);
    Fixture {
        _root: root,
        dir,
        credentials,
        fake,
        service,
    }
}

fn channel(id: &str) -> ConversationId {
    ConversationId(id.to_string())
}

fn history(id: &str) -> Read {
    Read::History {
        channel: channel(id),
        latest: None,
        limit: 50,
    }
}

fn page(text: &str) -> Page<Message> {
    Page {
        items: vec![Message {
            ts: Ts::new("1730000000.000100"),
            user: None,
            text: text.to_string(),
            thread_ts: None,
            reply_count: 0,
            subtype: None,
        }],
        next: None,
    }
}

fn limited(seconds: u64) -> ApiError {
    ApiError::RateLimited {
        retry_after: Duration::from_secs(seconds),
    }
}

fn call(
    fixture: &Fixture,
    read: Read,
    cx: &mut TestAppContext,
) -> gpui::Task<Result<Answer, SlackError>> {
    fixture
        .service
        .update(cx, |service, cx| service.call(read, cx))
}

fn shown(cx: &mut TestAppContext) -> Shown {
    cx.update(|cx| Shown::of(cx))
}

fn history_calls(fake: &FakeSlack) -> Vec<ConversationId> {
    fake.calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::History { channel, .. } => Some(channel),
            _ => None,
        })
        .collect()
}

/// Where the answer of a task lands once it finishes.
type Landing<T> = Rc<RefCell<Option<T>>>;

/// Runs `task` to its end in the background of the test; [`finished`] tells whether it has.
fn track<T: 'static>(task: gpui::Task<T>, cx: &mut TestAppContext) -> Landing<T> {
    let landing: Landing<T> = Rc::default();
    let target = landing.clone();
    cx.update(|cx| {
        cx.spawn(async move |_| {
            let result = task.await;
            *target.borrow_mut() = Some(result);
        })
        .detach()
    });
    landing
}

/// The answer of a tracked task if it has finished, without waiting.
fn finished<T>(landing: &Landing<T>) -> Option<T> {
    landing.borrow_mut().take()
}

#[gpui::test]
async fn not_connected_makes_no_call(cx: &mut TestAppContext) {
    let fixture = setup(false, cx);

    let error = call(&fixture, history("C1"), cx).await.unwrap_err();
    assert_eq!(error, SlackError::NotConnected);
    assert_eq!(error.to_string(), NOT_CONNECTED_MESSAGE);
    let error = fixture
        .service
        .update(cx, |service, cx| {
            service.post(
                Destination::parse("C1").unwrap(),
                "hello".to_string(),
                Origin::Kyle,
                cx,
            )
        })
        .await
        .unwrap_err();
    assert_eq!(error, SlackError::NotConnected);
    assert_eq!(shown(cx), Shown::Nothing);
    assert!(fixture.fake.calls().is_empty(), "a request was made");
    assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), 0);

    // Off: no request even with a connection.
    let off = setup(true, cx);
    set_enabled(false, cx);
    let error = call(&off, history("C1"), cx).await.unwrap_err();
    assert_eq!(error, SlackError::Off);
    assert_eq!(error.to_string(), OFF_MESSAGE);
    assert_eq!(shown(cx), Shown::Nothing);
    assert!(off.fake.calls().is_empty(), "a request was made while off");
    assert_eq!(off.credentials.reads.load(Ordering::SeqCst), 0);
}

#[gpui::test]
async fn rate_limit_of_ten_seconds_shows_waiting_then_repeats_once(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Err(limited(10)));
    fixture.fake.push_history(Ok(page("after the wait")));
    let item = cx.new(SlackStatusItem::new);

    let task = call(&fixture, history("C1"), cx);
    let task = track(task, cx);
    cx.run_until_parked();

    assert_eq!(history_calls(&fixture.fake).len(), 1);
    assert_eq!(shown(cx), Shown::Waiting(Duration::from_secs(10)));
    assert_eq!(shown(cx).text().as_deref(), Some("Slack: waiting"));
    assert_eq!(shown(cx).tooltip(), "Slack asked to wait, 10 s left");
    assert!(item.read_with(cx, |item, _| item.is_ticking()));

    cx.executor().advance_clock(Duration::from_secs(4));
    cx.run_until_parked();
    assert!(finished(&task).is_none(), "the read did not wait");
    assert_eq!(shown(cx).tooltip(), "Slack asked to wait, 6 s left");
    assert_eq!(history_calls(&fixture.fake).len(), 1);

    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    let answer = finished(&task).expect("the read finished after the wait");
    assert_eq!(answer, Ok(Answer::Messages(page("after the wait"))));
    assert_eq!(
        history_calls(&fixture.fake),
        vec![channel("C1"), channel("C1")]
    );
    assert_eq!(shown(cx), Shown::Workspace("Acme".to_string()));
    assert_eq!(shown(cx).text().as_deref(), Some("Slack: Acme"));
    assert!(
        !item.read_with(cx, |item, _| item.is_ticking()),
        "the repaint timer runs after the wait"
    );
}

#[gpui::test]
async fn wait_past_five_minutes_fails_with_local_retry_time(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Err(limited(400)));
    let start = cx.update(|cx| {
        cx.background_executor()
            .scheduler_executor()
            .scheduler()
            .clock()
            .utc_now()
    });
    let retry = start + chrono::Duration::seconds(400 + 3600);
    let expected = format!("Slack rate limit, try again at {}", retry.format("%H:%M"));

    let error = call(&fixture, history("C1"), cx).await.unwrap_err();
    assert_eq!(error.to_string(), expected);

    // Another read during that wait fails at once, without a request.
    let error = call(&fixture, history("C2"), cx).await.unwrap_err();
    assert_eq!(error.to_string(), expected);
    assert_eq!(history_calls(&fixture.fake), vec![channel("C1")]);
}

#[gpui::test]
async fn two_reads_during_one_wait_share_it(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Err(limited(10)));
    fixture.fake.push_history(Ok(page("one")));
    fixture.fake.push_history(Ok(page("two")));

    let first = call(&fixture, history("C1"), cx);
    let first = track(first, cx);
    cx.run_until_parked();
    let until = fixture
        .service
        .read_with(cx, |service, _| service.waiting_until())
        .expect("waiting");

    cx.executor().advance_clock(Duration::from_secs(3));
    let second = call(&fixture, history("C2"), cx);
    let second = track(second, cx);
    cx.run_until_parked();
    assert_eq!(
        history_calls(&fixture.fake).len(),
        1,
        "the second read did not queue"
    );
    assert_eq!(
        fixture
            .service
            .read_with(cx, |service, _| service.waiting_until()),
        Some(until),
        "the wait moved"
    );

    cx.executor().advance_clock(Duration::from_secs(7));
    cx.run_until_parked();
    assert!(finished(&first).expect("first finished").is_ok());
    assert!(finished(&second).expect("second finished").is_ok());
    let calls = history_calls(&fixture.fake);
    assert_eq!(calls.len(), 3);
    assert_eq!(calls.iter().filter(|id| **id == channel("C2")).count(), 1);
    assert!(
        fixture
            .service
            .read_with(cx, |service, _| service.waiting_until())
            .is_none()
    );
}

#[gpui::test]
async fn repeated_read_within_five_seconds_makes_one_call(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Ok(page("first")));
    fixture.fake.push_history(Ok(page("second")));

    let first = call(&fixture, history("C1"), cx).await.unwrap();
    cx.executor().advance_clock(Duration::from_secs(4));
    let again = call(&fixture, history("C1"), cx).await.unwrap();
    assert_eq!(first, again);
    assert_eq!(history_calls(&fixture.fake).len(), 1);

    cx.executor().advance_clock(Duration::from_secs(2));
    let later = call(&fixture, history("C1"), cx).await.unwrap();
    assert_eq!(later, Answer::Messages(page("second")));
    assert_eq!(history_calls(&fixture.fake).len(), 2);
}

fn posts(fake: &FakeSlack) -> Vec<String> {
    fake.calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Post(request) => Some(request.text),
            _ => None,
        })
        .collect()
}

fn posted() -> PostedMessage {
    PostedMessage {
        channel: channel("C1"),
        ts: Ts::new("1730000000.000200"),
    }
}

#[gpui::test]
async fn post_failing_with_network_error_is_not_repeated(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Err(ApiError::Network));
    fixture.fake.push_post(Ok(posted()));

    let error = fixture
        .service
        .update(cx, |service, cx| {
            service.post(
                Destination::parse("C1").unwrap(),
                "deploy done".to_string(),
                Origin::AttendedRun,
                cx,
            )
        })
        .await
        .unwrap_err();
    cx.run_until_parked();

    assert!(matches!(error, SlackError::PostMaybeNotSent { .. }));
    assert!(error.to_string().contains(MAY_NOT_HAVE_BEEN_SENT));
    assert_eq!(posts(&fixture.fake), vec!["deploy done".to_string()]);
}

async fn failed_post_error(cause: ApiError, cx: &mut TestAppContext) -> SlackError {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Err(cause));
    fixture.fake.push_post(Ok(posted()));
    let error = fixture
        .service
        .update(cx, |service, cx| {
            service.post(
                Destination::parse("C1").unwrap(),
                "deploy done".to_string(),
                Origin::AttendedRun,
                cx,
            )
        })
        .await
        .unwrap_err();
    cx.run_until_parked();
    assert_eq!(posts(&fixture.fake), vec!["deploy done".to_string()]);
    error
}

#[gpui::test]
async fn post_failing_with_server_error_is_not_repeated(cx: &mut TestAppContext) {
    let error = failed_post_error(ApiError::Server { status: 502 }, cx).await;
    assert!(matches!(error, SlackError::PostMaybeNotSent { .. }));
    assert!(error.to_string().contains(MAY_NOT_HAVE_BEEN_SENT));
}

#[gpui::test]
async fn post_with_unreadable_or_oversized_answer_may_not_have_been_sent(cx: &mut TestAppContext) {
    for code in ["invalid_response", crate::client::TOO_LARGE_CODE] {
        let error = failed_post_error(
            ApiError::Other {
                code: code.to_string(),
            },
            cx,
        )
        .await;
        assert!(
            matches!(error, SlackError::PostMaybeNotSent { .. }),
            "{code}"
        );
        assert!(error.to_string().contains(MAY_NOT_HAVE_BEEN_SENT));
    }
}

#[gpui::test]
async fn post_refused_with_a_slack_code_says_it_did_not_post(cx: &mut TestAppContext) {
    let error = failed_post_error(
        ApiError::Other {
            code: "is_archived".to_string(),
        },
        cx,
    )
    .await;
    assert!(matches!(error, SlackError::PostRefused { .. }));
}

#[gpui::test]
async fn posts_run_in_order_and_repeat_only_after_rate_limit(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Err(limited(5)));
    fixture.fake.push_post(Ok(posted()));
    fixture.fake.push_post(Ok(posted()));
    let post = |text: &str, cx: &mut TestAppContext| {
        let text = text.to_string();
        fixture.service.update(cx, |service, cx| {
            service.post(Destination::parse("C1").unwrap(), text, Origin::Kyle, cx)
        })
    };

    let first = post("one", cx);
    let first = track(first, cx);
    let second = post("two", cx);
    let second = track(second, cx);
    cx.run_until_parked();
    assert_eq!(posts(&fixture.fake), vec!["one".to_string()]);

    cx.executor().advance_clock(Duration::from_secs(5));
    cx.run_until_parked();
    assert!(finished(&first).expect("first finished").is_ok());
    assert!(finished(&second).expect("second finished").is_ok());
    assert_eq!(
        posts(&fixture.fake),
        vec!["one".to_string(), "one".to_string(), "two".to_string()]
    );
}

#[gpui::test]
async fn revoked_token_shows_reconnect(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Err(ApiError::Unauthorized {
        reason: "token_revoked".to_string(),
    }));

    let error = call(&fixture, history("C1"), cx).await.unwrap_err();
    cx.run_until_parked();

    assert_eq!(error.to_string(), RECONNECT_MESSAGE);
    assert_eq!(shown(cx), Shown::Reconnect);
    assert_eq!(shown(cx).text().as_deref(), Some("Slack: reconnect"));
    assert!(matches!(
        fixture
            .service
            .read_with(cx, |service, cx| service.state(cx)),
        SlackState::Reconnect(_)
    ));
    // Every later caller gets the same, with no request.
    let error = call(&fixture, history("C2"), cx).await.unwrap_err();
    assert_eq!(error, SlackError::Reconnect);
    assert_eq!(history_calls(&fixture.fake).len(), 1);
}

#[gpui::test]
async fn missing_secret_at_first_use_shows_reconnect(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.credentials.entries.lock().unwrap().clear();

    let error = call(&fixture, history("C1"), cx).await.unwrap_err();
    cx.run_until_parked();

    assert_eq!(error, SlackError::Reconnect);
    assert_eq!(shown(cx), Shown::Reconnect);
    assert!(fixture.fake.calls().is_empty());
}

#[gpui::test]
async fn status_text_never_holds_the_token(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Err(limited(30)));
    let _task = call(&fixture, history("C1"), cx);
    cx.run_until_parked();
    fs::write(
        fixture.dir.join(connection::ALLOWED_FILE),
        "[projects.\"p1\"]\nconversations = [\"C9\"]\n",
    )
    .unwrap();

    let waiting = cx.update(|cx| crate::status_item::current_status_text(Some("p1"), cx));
    assert!(waiting.contains("Workspace: Acme"));
    assert!(waiting.contains("Token: user token, slack token (hidden)"));
    assert!(waiting.contains("Allowed channels of project p1: C9"));
    assert!(waiting.contains("Slack asked to wait, 30 s left"));

    let state = fixture
        .service
        .read_with(cx, |service, cx| service.state(cx));
    let connection = match &state {
        SlackState::Connected(connection) => connection.clone(),
        other => panic!("not connected: {other:?}"),
    };
    let reconnect = SlackState::Reconnect(connection);
    let mut texts = vec![waiting, format!("{state:?}"), format!("{reconnect:?}")];
    for state in [&state, &reconnect, &SlackState::NotConnected] {
        texts.push(status_text(&StatusFacts {
            enabled: true,
            state,
            wait: None,
            project: None,
            allowed: &AllowedList::default(),
        }));
    }
    for text in texts {
        assert!(!text.contains(TOKEN), "the status shows the token");
        assert!(!text.contains("xoxp-"), "the status shows a token");
        assert!(!text.contains("TESTSECRET"), "the status shows the token");
    }
}

fn post_to_c1(
    fixture: &Fixture,
    text: &str,
    cx: &mut TestAppContext,
) -> gpui::Task<Result<PostedMessage, SlackError>> {
    let text = text.to_string();
    fixture.service.update(cx, |service, cx| {
        service.post(Destination::parse("C1").unwrap(), text, Origin::Kyle, cx)
    })
}

fn user(id: &str) -> User {
    User {
        id: UserId(id.to_string()),
        name: id.to_lowercase(),
        display_name: String::new(),
        real_name: String::new(),
    }
}

fn user_calls(fake: &FakeSlack) -> usize {
    fake.calls()
        .into_iter()
        .filter(|call| matches!(call, Call::User { .. }))
        .count()
}

/// Disconnects and connects again, which changes the connection's generation.
async fn reconnect(fixture: &Fixture, cx: &mut TestAppContext) {
    cx.update(connection::disconnect).await.unwrap();
    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));
    let now = DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap();
    cx.update(|cx| connection::connect(TOKEN, now, cx))
        .await
        .unwrap();
    cx.run_until_parked();
}

#[gpui::test]
async fn second_longer_rate_limit_during_a_wait_extends_the_shared_wait(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Err(limited(10)));
    fixture.fake.push_history(Err(limited(60)));
    fixture.fake.push_history(Ok(page("one")));
    fixture.fake.push_history(Ok(page("two")));

    let first = track(call(&fixture, history("C1"), cx), cx);
    let second = track(call(&fixture, history("C2"), cx), cx);
    cx.run_until_parked();
    let until = fixture
        .service
        .read_with(cx, |service, _| service.waiting_until())
        .expect("waiting");
    let left = fixture
        .service
        .read_with(cx, |service, _| service.wait_left())
        .expect("waiting");
    assert_eq!(left, Duration::from_secs(60), "the longer limit won");
    assert_eq!(history_calls(&fixture.fake).len(), 2);

    // The first limit's ten seconds do not end the wait.
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();
    assert!(finished(&first).is_none());
    assert!(finished(&second).is_none());
    assert_eq!(
        fixture
            .service
            .read_with(cx, |service, _| service.waiting_until()),
        Some(until)
    );
    assert_eq!(history_calls(&fixture.fake).len(), 2);

    cx.executor().advance_clock(Duration::from_secs(50));
    cx.run_until_parked();
    assert!(finished(&first).expect("first finished").is_ok());
    assert!(finished(&second).expect("second finished").is_ok());
    assert_eq!(history_calls(&fixture.fake).len(), 4);
}

#[gpui::test]
async fn unauthorized_post_moves_to_reconnect_and_queued_post_makes_no_request(
    cx: &mut TestAppContext,
) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Err(ApiError::Unauthorized {
        reason: "token_revoked".to_string(),
    }));
    fixture.fake.push_post(Ok(posted()));

    let first = track(post_to_c1(&fixture, "one", cx), cx);
    let second = track(post_to_c1(&fixture, "two", cx), cx);
    cx.run_until_parked();

    assert_eq!(finished(&first), Some(Err(SlackError::Reconnect)));
    assert_eq!(finished(&second), Some(Err(SlackError::Reconnect)));
    assert_eq!(posts(&fixture.fake), vec!["one".to_string()]);
    assert_eq!(shown(cx), Shown::Reconnect);
    assert!(matches!(
        fixture
            .service
            .read_with(cx, |service, cx| service.state(cx)),
        SlackState::Reconnect(_)
    ));
    let later = post_to_c1(&fixture, "three", cx).await.unwrap_err();
    assert_eq!(later.to_string(), RECONNECT_MESSAGE);
    assert_eq!(posts(&fixture.fake), vec!["one".to_string()]);
}

#[gpui::test]
async fn reconnect_drops_the_client_and_every_cache(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Ok(page("before")));
    fixture.fake.push_history(Ok(page("after")));
    fixture.fake.push_conversations(Ok(Page {
        items: vec![Conversation {
            id: channel("C1"),
            kind: ConversationKind::Channel,
            name: "general".to_string(),
            user: None,
        }],
        next: None,
    }));
    fixture.fake.push_conversations(Ok(Page {
        items: Vec::new(),
        next: None,
    }));
    fixture.fake.push_user(Ok(user("U1")));
    fixture.fake.push_user(Ok(user("U1")));

    call(&fixture, history("C1"), cx).await.unwrap();
    let list = fixture
        .service
        .update(cx, |service, cx| service.conversation_list(cx))
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    fixture
        .service
        .update(cx, |service, cx| service.user(UserId("U1".to_string()), cx))
        .await
        .unwrap();
    assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), 1);
    assert!(
        fixture
            .service
            .read_with(cx, |service, _| service.cached_conversations().is_some())
    );

    reconnect(&fixture, cx).await;

    assert!(
        fixture
            .service
            .read_with(cx, |service, _| service.cached_conversations().is_none()),
        "the conversation list of the old connection was kept"
    );
    let again = call(&fixture, history("C1"), cx).await.unwrap();
    assert_eq!(again, Answer::Messages(page("after")), "a read was reused");
    assert_eq!(
        fixture.credentials.reads.load(Ordering::SeqCst),
        2,
        "the old client was kept"
    );
    let list = fixture
        .service
        .update(cx, |service, cx| service.conversation_list(cx))
        .await
        .unwrap();
    assert!(list.is_empty());
    fixture
        .service
        .update(cx, |service, cx| service.user(UserId("U1".to_string()), cx))
        .await
        .unwrap();
    assert_eq!(user_calls(&fixture.fake), 2);
}

#[gpui::test]
async fn queued_post_keeps_its_own_five_minute_budget(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Err(limited(200)));
    fixture.fake.push_post(Ok(posted()));
    // The second post's wait ends 450 s after the first post began: past five minutes from
    // there, but within five minutes of its own turn.
    fixture.fake.push_post(Err(limited(250)));
    fixture.fake.push_post(Ok(posted()));

    let first = track(post_to_c1(&fixture, "one", cx), cx);
    let second = track(post_to_c1(&fixture, "two", cx), cx);
    cx.run_until_parked();
    assert_eq!(posts(&fixture.fake), vec!["one".to_string()]);

    cx.executor().advance_clock(Duration::from_secs(200));
    cx.run_until_parked();
    assert!(finished(&first).expect("first finished").is_ok());
    assert!(finished(&second).is_none(), "the second post is waiting");
    assert_eq!(
        posts(&fixture.fake),
        vec!["one".to_string(), "one".to_string(), "two".to_string()]
    );

    cx.executor().advance_clock(Duration::from_secs(250));
    cx.run_until_parked();
    assert!(finished(&second).expect("second finished").is_ok());
    assert_eq!(posts(&fixture.fake).len(), 4);
}

#[gpui::test]
async fn joined_read_checks_the_five_minute_rule_from_its_own_start(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Err(limited(200)));
    // At 200 s the repeat is limited again: the wait would end 450 s after the first caller
    // began, but within five minutes of the later caller's start.
    fixture.fake.push_history(Err(limited(250)));
    fixture.fake.push_history(Ok(page("late")));

    let first = track(call(&fixture, history("C1"), cx), cx);
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(190));
    let joined = track(call(&fixture, history("C1"), cx), cx);
    cx.run_until_parked();
    assert_eq!(history_calls(&fixture.fake).len(), 1, "the read was joined");

    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();
    assert!(matches!(
        finished(&first).expect("first finished"),
        Err(SlackError::RateLimit { .. })
    ));
    assert!(finished(&joined).is_none(), "the joined read is waiting");

    cx.executor().advance_clock(Duration::from_secs(250));
    cx.run_until_parked();
    assert_eq!(finished(&joined), Some(Ok(Answer::Messages(page("late")))));
    assert_eq!(history_calls(&fixture.fake).len(), 3);
}

#[gpui::test]
async fn user_drops_a_cached_user_after_a_generation_change(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_user(Ok(user("U1")));
    fixture.fake.push_user(Ok(user("U1")));
    let id = UserId("U1".to_string());
    let ask = |cx: &mut TestAppContext| {
        let id = id.clone();
        fixture
            .service
            .update(cx, |service, cx| service.user(id, cx))
    };

    ask(cx).await.unwrap();
    // Seen once: the session cache answers.
    ask(cx).await.unwrap();
    assert_eq!(user_calls(&fixture.fake), 1);

    cx.update(connection::disconnect).await.unwrap();
    cx.run_until_parked();
    assert_eq!(ask(cx).await.unwrap_err(), SlackError::NotConnected);
    assert_eq!(user_calls(&fixture.fake), 1);

    fixture.fake.push_auth_test(Ok(auth_info("T1", SCOPES)));
    let now = DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap();
    cx.update(|cx| connection::connect(TOKEN, now, cx))
        .await
        .unwrap();
    cx.run_until_parked();
    ask(cx).await.unwrap();
    assert_eq!(user_calls(&fixture.fake), 2, "the old user was kept");
}
