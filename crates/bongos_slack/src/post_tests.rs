//! Tests of `slack: post` and `slack: reply` over the fake Slack: what is sent, where, and that a
//! refused, declined or failed post is not sent (again). Every answer is scripted and ready, so
//! nothing waits and no clock is read.

use bongos_slack_model::fake::{Call, FakeSlack};
use bongos_slack_model::render::{FixedOffset, UserNames};
use bongos_slack_model::types::{ConversationId, Destination, Message, Page, Ts, UserId};
use bongos_slack_model::{ApiError, Origin, PostRequest, PostedMessage};
use gpui::TestAppContext;

use crate::post::{
    POST_MAY_NOT_HAVE_BEEN_SENT, PREVIEW_LINES, PostDraft, ShownThread, answer,
    destination_choices, escaped_text, send, thread_name,
};
use crate::read_buffer::ConversationEntry;
use crate::read_view::{ReadView, reply_destination, row_of, threads_shown};
use crate::service::SlackError;
use crate::service_tests::setup;

fn channel() -> ConversationId {
    ConversationId("C0BUILDS".to_string())
}

fn to_channel() -> Destination {
    Destination {
        conversation: channel(),
        thread: None,
    }
}

fn draft(destination: Destination, name: &str, typed: &str) -> PostDraft {
    PostDraft::new(destination, name.to_string(), typed.to_string()).unwrap()
}

fn posted() -> PostedMessage {
    PostedMessage {
        channel: channel(),
        ts: Ts::new("1730000000.000900"),
    }
}

fn post_requests(fake: &FakeSlack) -> Vec<PostRequest> {
    fake.calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Post(request) => Some(request),
            _ => None,
        })
        .collect()
}

fn ts(n: u64) -> Ts {
    Ts::new(format!("{}.000100", 1_730_000_000 + n))
}

fn message(n: u64, text: &str, thread: Option<u64>, replies: u32) -> Message {
    Message {
        ts: ts(n),
        user: Some(UserId("U1".to_string())),
        text: text.to_string(),
        thread_ts: thread.map(ts),
        reply_count: replies,
        subtype: None,
    }
}

#[gpui::test]
async fn region_is_sent_verbatim_with_only_markup_escaped(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Ok(posted()));
    let typed = "deploy & roll <back> \"now\" é *bold*\n  indented line\n";

    let draft = draft(to_channel(), "#team-builds", typed);
    let message = cx.update(|cx| send(&fixture.service, draft, cx)).await;

    assert_eq!(message, "Posted to #team-builds");
    assert_eq!(
        post_requests(&fixture.fake),
        vec![PostRequest {
            channel: channel(),
            text: "deploy &amp; roll &lt;back&gt; \"now\" é *bold*\n  indented line\n".to_string(),
            thread_ts: None,
        }]
    );
}

#[gpui::test]
async fn reply_carries_the_thread_ts(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Ok(posted()));
    fixture.fake.push_post(Ok(posted()));
    let mut view = ReadView::new(channel(), "#team-builds".to_string());
    view.apply_page(
        Page {
            items: vec![
                message(3, "after", None, 0),
                message(2, "thread 2", Some(2), 1),
                message(1, "lonely", None, 0),
            ],
            next: None,
        },
        vec![(
            ts(2),
            vec![
                message(2, "thread 2", Some(2), 1),
                message(4, "a reply", Some(2), 0),
            ],
        )],
    );
    let mut users = UserNames::new();
    users.insert(&UserId("U1".to_string()), "kyle");
    let transcript = view.render(&users, &FixedOffset(0));

    // On a reply: its thread. On a message with no replies yet: the message starts the thread.
    let on_reply = reply_destination(
        &channel(),
        &transcript,
        row_of(&transcript, &ts(4)).unwrap(),
    );
    let on_lonely = reply_destination(
        &channel(),
        &transcript,
        row_of(&transcript, &ts(1)).unwrap(),
    );
    for destination in [on_reply.unwrap(), on_lonely.unwrap()] {
        let draft = draft(destination, &thread_name("#team-builds"), "on it");
        let message = cx.update(|cx| send(&fixture.service, draft, cx)).await;
        assert_eq!(message, "Posted to a thread in #team-builds");
    }

    let threads: Vec<Option<Ts>> = post_requests(&fixture.fake)
        .into_iter()
        .map(|request| request.thread_ts)
        .collect();
    assert_eq!(threads, vec![Some(ts(2)), Some(ts(1))]);
}

#[test]
fn threads_of_read_buffers_are_post_destinations() {
    let mut view = ReadView::new(channel(), "#team-builds".to_string());
    view.apply_page(
        Page {
            items: vec![
                message(2, "thread 2", Some(2), 1),
                message(1, "plain", None, 0),
            ],
            next: None,
        },
        vec![(
            ts(2),
            vec![
                message(2, "thread 2", Some(2), 1),
                message(4, "a reply", Some(2), 0),
            ],
        )],
    );
    let transcript = view.render(&UserNames::new(), &FixedOffset(0));
    let shown: Vec<ShownThread> = threads_shown(&transcript)
        .into_iter()
        .map(|(thread, line)| ShownThread {
            conversation_label: "#team-builds".to_string(),
            conversation: channel(),
            thread,
            line,
        })
        .collect();
    let entries = vec![ConversationEntry {
        label: "#team-builds".to_string(),
        id: channel(),
    }];

    let choices = destination_choices(&entries, &shown);

    assert_eq!(choices.len(), 2);
    assert_eq!(choices[0].destination, to_channel());
    assert_eq!(choices[0].name, "#team-builds");
    assert_eq!(
        choices[1].destination,
        Destination {
            conversation: channel(),
            thread: Some(ts(2)),
        }
    );
    assert_eq!(choices[1].name, "a thread in #team-builds");
    assert!(choices[1].label.starts_with("thread in #team-builds: "));
    assert!(choices[1].label.contains("thread 2"));
}

#[gpui::test]
async fn text_over_the_limit_sends_nothing(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Ok(posted()));
    // 8,001 `&` escape to 40,005 characters: five over.
    let typed = "&".repeat(8_001);

    assert_eq!(
        escaped_text(&typed),
        Err("5 characters over Slack's limit; nothing was sent".to_string())
    );
    assert!(PostDraft::new(to_channel(), "#team-builds".to_string(), typed.clone()).is_err());
    // The service refuses it too, before any request.
    let error = fixture
        .service
        .update(cx, |service, cx| {
            service.post(
                to_channel(),
                crate::post::escaped_text(&"a".repeat(40_000)).unwrap() + "&",
                Origin::Kyle,
                cx,
            )
        })
        .await
        .unwrap_err();
    assert_eq!(error, SlackError::TooLong { over: 1 });
    assert_eq!(
        error.to_string(),
        "1 characters over Slack's limit; nothing was sent"
    );
    assert!(fixture.fake.calls().is_empty(), "nothing was sent");
}

#[gpui::test]
async fn answering_no_sends_nothing(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Ok(posted()));
    let draft = draft(to_channel(), "#team-builds", "ship it");

    let sent = cx.update(|cx| answer(&fixture.service, draft, false, cx));
    cx.run_until_parked();

    assert!(sent.is_none());
    assert!(fixture.fake.calls().is_empty(), "nothing was sent");
}

#[gpui::test]
async fn failed_post_says_it_may_not_have_been_sent_and_is_not_repeated(cx: &mut TestAppContext) {
    let fixture = setup(true, cx);
    fixture.fake.push_post(Err(ApiError::Network));
    fixture.fake.push_post(Ok(posted()));
    let draft = draft(to_channel(), "#team-builds", "ship it");

    let task = cx.update(|cx| answer(&fixture.service, draft, true, cx));
    let message = task.expect("yes sends").await;
    cx.run_until_parked();

    assert!(message.contains(POST_MAY_NOT_HAVE_BEEN_SENT), "{message}");
    assert!(message.contains("#team-builds"), "{message}");
    assert_eq!(post_requests(&fixture.fake).len(), 1, "exactly one call");
}

#[test]
fn confirmation_shows_the_first_lines_only() {
    let typed = (1..=8)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let draft = draft(to_channel(), "#team-builds", &typed);

    let question = draft.question("Acme", "kyle");

    let lines: Vec<&str> = question.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "Post to #team-builds in Acme as kyle ({} characters):",
            typed.chars().count()
        )
    );
    assert_eq!(lines[1], "line 1");
    assert_eq!(lines[PREVIEW_LINES], format!("line {PREVIEW_LINES}"));
    assert_eq!(lines[PREVIEW_LINES + 1], "… and 3 more lines");
    assert_eq!(lines[PREVIEW_LINES + 2], "Send it?");
    assert_eq!(lines.len(), PREVIEW_LINES + 3);
}
