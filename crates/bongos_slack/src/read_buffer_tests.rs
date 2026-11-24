//! Tests of the read buffer model and the conversation picker over the fake Slack, the service
//! and the test executor's virtual clock: nothing sleeps and no test reads the wall clock.

use std::time::Duration;

use bongos_slack_model::fake::Call;
use bongos_slack_model::types::{
    Conversation, ConversationId, ConversationKind, Cursor, Message, Page, Ts, UserId,
};
use bongos_slack_model::{ApiError, User};
use gpui::{AppContext as _, Entity, TestAppContext};
use language::{Buffer, Capability};

use crate::connection;
use crate::read_buffer::{
    ConversationEntry, ReadSession, SessionError, conversation_entries, link_label, read_thread,
};
use crate::read_view::{LoadError, MessageLink};
use crate::service::{LIST_MAX_AGE, SlackError};
use crate::service_tests::{Fixture, setup};

fn id(raw: &str) -> ConversationId {
    ConversationId(raw.to_string())
}

fn conversation(raw: &str, kind: ConversationKind, name: &str, user: Option<&str>) -> Conversation {
    Conversation {
        id: id(raw),
        kind,
        name: name.to_string(),
        user: user.map(|user| UserId(user.to_string())),
    }
}

fn conversations(items: Vec<Conversation>) -> Page<Conversation> {
    Page { items, next: None }
}

fn list_calls(fixture: &Fixture) -> usize {
    fixture
        .fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Conversations { .. }))
        .count()
}

fn ts(n: u64) -> Ts {
    Ts::new(format!("{}.000100", 1_730_000_000 + n))
}

fn message(n: u64) -> Message {
    Message {
        ts: ts(n),
        user: None,
        text: format!("message {n}"),
        thread_ts: None,
        reply_count: 0,
        subtype: None,
    }
}

/// A history page, newest first.
fn history_page(range: std::ops::RangeInclusive<u64>, older: bool) -> Page<Message> {
    Page {
        items: range.rev().map(message).collect(),
        next: older.then(|| Cursor("older".to_string())),
    }
}

fn open_session(fixture: &Fixture, cx: &mut TestAppContext) -> (Entity<Buffer>, Entity<ReadSession>) {
    let buffer = cx.new(|cx| Buffer::local("", cx));
    let session = cx.update(|cx| {
        ReadSession::open(
            fixture.service.clone(),
            buffer.clone(),
            id("C1"),
            "#general".to_string(),
            cx,
        )
    });
    (buffer, session)
}

fn text(buffer: &Entity<Buffer>, cx: &mut TestAppContext) -> String {
    buffer.read_with(cx, |buffer, _| buffer.text())
}

#[gpui::test]
async fn picker_lists_conversations_and_rereads_a_copy_older_than_five_minutes(
    cx: &mut TestAppContext,
) {
    let fixture = setup(true, cx);
    fixture.fake.push_conversations(Ok(conversations(vec![
        conversation("D1", ConversationKind::Dm, "", Some("U1")),
        conversation("C2", ConversationKind::Private, "secret", None),
        conversation("C1", ConversationKind::Channel, "general", None),
        conversation("G1", ConversationKind::GroupDm, "mpdm-ann--bob-1", None),
    ])));
    fixture.fake.push_user(Ok(User {
        id: UserId("U1".to_string()),
        name: "carol".to_string(),
        display_name: String::new(),
        real_name: String::new(),
    }));

    let entries = cx
        .update(|cx| conversation_entries(fixture.service.clone(), cx))
        .await
        .unwrap();
    let labels: Vec<&str> = entries.iter().map(|entry| entry.label.as_str()).collect();
    assert_eq!(labels, ["#general", "#secret", "@ann, @bob", "@carol"]);
    assert_eq!(list_calls(&fixture), 1);

    // A picker that opens within five minutes uses the copy.
    cx.executor()
        .advance_clock(LIST_MAX_AGE - Duration::from_secs(1));
    cx.update(|cx| conversation_entries(fixture.service.clone(), cx))
        .await
        .unwrap();
    assert_eq!(list_calls(&fixture), 1, "a fresh copy was read again");

    // Older than five minutes: the list is read again when the picker opens.
    cx.executor().advance_clock(Duration::from_secs(2));
    fixture.fake.push_conversations(Ok(conversations(vec![conversation(
        "C3",
        ConversationKind::Channel,
        "random",
        None,
    )])));
    let entries = cx
        .update(|cx| conversation_entries(fixture.service.clone(), cx))
        .await
        .unwrap();
    assert_eq!(list_calls(&fixture), 2);
    assert_eq!(
        entries,
        vec![ConversationEntry {
            label: "#random".to_string(),
            id: id("C3"),
        }]
    );
}

#[gpui::test]
async fn reading_a_channel_opens_a_read_only_buffer_with_the_transcript(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Ok(history_page(1..=3, true)));
    let (buffer, session) = open_session(&fixture, cx);

    session
        .update(cx, |session, cx| session.load_first(cx))
        .await
        .unwrap();

    assert_eq!(session.read_with(cx, |session, _| session.title()), "Slack: #general");
    assert_eq!(buffer.read_with(cx, |buffer, _| buffer.capability()), Capability::ReadOnly);
    let shown = text(&buffer, cx);
    assert!(
        shown.starts_with("Showing 3 messages (older messages exist: slack: load older messages)\n"),
        "{shown}"
    );
    for n in 1..=3 {
        assert!(shown.contains(&format!("message {n}")), "{shown}");
    }
    assert_eq!(
        session.read_with(cx, |session, _| session.transcript().text.clone()),
        shown
    );
    let found = cx.update(|cx| ReadSession::for_buffer(&buffer, cx));
    assert_eq!(found.map(|found| found.entity_id()), Some(session.entity_id()));
}

#[gpui::test]
async fn wait_line_shows_from_the_service_and_clears_when_the_load_finishes(
    cx: &mut TestAppContext,
) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Err(ApiError::RateLimited {
        retry_after: Duration::from_secs(10),
    }));
    fixture.fake.push_history(Ok(history_page(1..=2, false)));
    let (buffer, session) = open_session(&fixture, cx);

    let load = session.update(cx, |session, cx| session.load_first(cx));
    let landing = cx.update(|cx| {
        let landing = std::rc::Rc::new(std::cell::RefCell::new(None));
        let target = landing.clone();
        cx.spawn(async move |_| *target.borrow_mut() = Some(load.await))
            .detach();
        landing
    });
    cx.run_until_parked();

    let waiting = text(&buffer, cx);
    assert_eq!(waiting.lines().nth(1), Some("Slack asked to wait 10 seconds"), "{waiting}");
    assert!(session.read_with(cx, |session, _| session.is_loading()));

    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();
    assert!(landing.borrow().as_ref().is_some_and(|result| result.is_ok()));
    let done = text(&buffer, cx);
    assert!(!done.contains("Slack asked to wait"), "{done}");
    assert!(done.contains("message 2"), "{done}");
    assert!(!session.read_with(cx, |session, _| session.is_loading()));
}

#[gpui::test]
async fn load_older_keeps_point_on_the_same_message(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Ok(history_page(3..=4, true)));
    fixture.fake.push_history(Ok(history_page(1..=2, false)));
    let (buffer, session) = open_session(&fixture, cx);
    session
        .update(cx, |session, cx| session.load_first(cx))
        .await
        .unwrap();

    let row_of_four = |cx: &mut TestAppContext| {
        text(&buffer, cx)
            .lines()
            .position(|line| line.contains("message 4"))
            .unwrap()
    };
    let before = row_of_four(cx);
    let moved = session
        .update(cx, |session, cx| session.load_older(before, cx))
        .await
        .unwrap();

    let after = row_of_four(cx);
    assert!(after > before, "older messages went above");
    assert_eq!(moved, Some(after));
    assert!(text(&buffer, cx).contains("message 1"));
    assert!(text(&buffer, cx).contains("this is the start of the conversation"));
    // The second request asked for messages older than the oldest shown.
    let latest = fixture.fake.calls().into_iter().filter_map(|call| match call {
        Call::History { latest, .. } => Some(latest),
        _ => None,
    });
    assert_eq!(latest.collect::<Vec<_>>(), vec![None, Some(ts(3))]);

    // Nothing is older now.
    let error = session
        .update(cx, |session, cx| session.load_older(after, cx))
        .await
        .unwrap_err();
    assert_eq!(error, SessionError::Load(LoadError::NothingOlder));
}

#[gpui::test]
async fn a_disconnected_buffer_refuses_to_load_more(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_history(Ok(history_page(3..=4, true)));
    let (buffer, session) = open_session(&fixture, cx);
    session
        .update(cx, |session, cx| session.load_first(cx))
        .await
        .unwrap();

    cx.update(connection::disconnect).await.unwrap();
    cx.run_until_parked();

    let shown = text(&buffer, cx);
    assert_eq!(
        shown.lines().nth(1),
        Some(crate::read_view::DISCONNECTED_LINE),
        "{shown}"
    );
    let calls = fixture.fake.calls().len();
    let error = session
        .update(cx, |session, cx| session.load_older(0, cx))
        .await
        .unwrap_err();
    assert_eq!(error, SessionError::Load(LoadError::Disconnected));
    assert_eq!(fixture.fake.calls().len(), calls, "a request was made");
}

#[gpui::test]
async fn read_thread_works_from_point_and_from_a_message_link(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    let parent = Message {
        thread_ts: Some(ts(2)),
        reply_count: 1,
        ..message(2)
    };
    fixture.fake.push_history(Ok(Page {
        items: vec![parent.clone(), message(1)],
        next: None,
    }));
    fixture.fake.push_replies(Ok(Page {
        items: vec![
            parent.clone(),
            Message {
                thread_ts: Some(ts(2)),
                ..message(3)
            },
        ],
        next: None,
    }));
    let (buffer, session) = open_session(&fixture, cx);
    session
        .update(cx, |session, cx| session.load_first(cx))
        .await
        .unwrap();

    // At point: the row of the parent maps to its thread.
    let row = text(&buffer, cx)
        .lines()
        .position(|line| line.contains("message 2"))
        .unwrap();
    assert_eq!(session.read_with(cx, |session, _| session.thread_at(row)), Some(ts(2)));
    let plain = text(&buffer, cx)
        .lines()
        .position(|line| line.contains("message 1"))
        .unwrap();
    assert_eq!(session.read_with(cx, |session, _| session.thread_at(plain)), Some(ts(1)));

    // From a message link: parsed without any request, then the replies are read.
    let link = MessageLink::parse("https://acme.slack.com/archives/C1/p1730000002000100").unwrap();
    assert_eq!(link.thread(), ts(2));
    fixture.fake.push_replies(Ok(Page {
        items: vec![
            parent,
            Message {
                thread_ts: Some(ts(2)),
                ..message(3)
            },
        ],
        next: None,
    }));
    let transcript = cx
        .update(|cx| read_thread(fixture.service.clone(), link.conversation.clone(), link.thread(), cx))
        .await
        .unwrap();
    assert!(transcript.text.contains("message 2"), "{}", transcript.text);
    assert!(transcript.text.contains("message 3"), "{}", transcript.text);
    let entries = vec![ConversationEntry {
        label: "#general".to_string(),
        id: id("C1"),
    }];
    assert_eq!(link_label(&link, &entries), "#general");
    assert_eq!(
        link_label(&MessageLink::parse("https://acme.slack.com/archives/C9/p1730000002000100").unwrap(), &entries),
        "C9"
    );
}

#[gpui::test]
async fn reads_make_no_request_while_slack_is_off_or_not_connected(cx: &mut TestAppContext) {
    let fixture = setup(false, cx);
    let (_, session) = open_session(&fixture, cx);
    let error = session
        .update(cx, |session, cx| session.load_first(cx))
        .await
        .unwrap_err();
    assert_eq!(error, SessionError::Slack(SlackError::NotConnected));
    assert!(fixture.fake.calls().is_empty());
}
