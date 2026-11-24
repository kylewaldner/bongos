//! Tests of the read buffer state over the fake Slack. Every answer is scripted and ready, so
//! nothing waits and no clock is read.

use std::time::Duration;

use bongos_slack_model::fake::{Call, FakeSlack};
use bongos_slack_model::render::{FixedOffset, Transcript, UserNames};
use bongos_slack_model::types::{ConversationId, Cursor, Destination, Message, Page, Ts, UserId};
use futures::executor::block_on;

use crate::read_view::*;

const ZONE: FixedOffset = FixedOffset(0);

fn channel() -> ConversationId {
    ConversationId("C1".to_string())
}

fn ts(n: u64) -> Ts {
    Ts::new(format!("{}.000100", 1_730_000_000 + n))
}

fn message(n: u64, text: &str) -> Message {
    Message {
        ts: ts(n),
        user: Some(UserId("U1".to_string())),
        text: text.to_string(),
        thread_ts: None,
        reply_count: 0,
        subtype: None,
    }
}

fn parent(n: u64, replies: u32) -> Message {
    Message {
        thread_ts: Some(ts(n)),
        reply_count: replies,
        ..message(n, &format!("thread {n}"))
    }
}

fn reply(n: u64, thread: u64, text: &str) -> Message {
    Message {
        thread_ts: Some(ts(thread)),
        ..message(n, text)
    }
}

/// History pages answer newest first.
fn history_page(range: std::ops::RangeInclusive<u64>, older: bool) -> Page<Message> {
    Page {
        items: range
            .rev()
            .map(|n| message(n, &format!("message {n}")))
            .collect(),
        next: older.then(|| Cursor("older".to_string())),
    }
}

fn users() -> UserNames {
    let mut names = UserNames::new();
    names.insert(&UserId("U1".to_string()), "kyle");
    names
}

fn view() -> ReadView {
    ReadView::new(channel(), "#team".to_string())
}

fn lines(transcript: &Transcript) -> Vec<&str> {
    transcript.text.split('\n').collect()
}

#[test]
fn first_page_shows_fifty_oldest_first_with_the_header() {
    let fake = FakeSlack::new();
    fake.push_history(Ok(history_page(51..=100, true)));
    let mut view = view();
    block_on(view.load_first(&fake)).unwrap();
    let transcript = view.render(&users(), &ZONE);
    let lines = lines(&transcript);
    assert_eq!(
        lines[0],
        "Showing 50 messages (older messages exist: slack: load older messages)"
    );
    assert_eq!(view.title(), "Slack: #team");
    assert!(lines[2].ends_with("message 51"), "{}", lines[2]);
    assert!(lines.last().unwrap().ends_with("message 100"));
    assert_eq!(transcript.index.len(), lines.len());
    assert_eq!(
        fake.calls(),
        vec![Call::History {
            channel: channel(),
            latest: None,
            limit: 50
        }]
    );
}

#[test]
fn load_older_merges_with_no_duplicate_and_none_missing() {
    let fake = FakeSlack::new();
    fake.push_history(Ok(history_page(51..=100, true)));
    // The older page overlaps the oldest message shown, as Slack may answer.
    fake.push_history(Ok(history_page(1..=51, false)));
    let mut view = view();
    block_on(view.load_first(&fake)).unwrap();
    let before = view.render(&users(), &ZONE);
    let at = row_of(&before, &ts(70)).unwrap();
    block_on(view.load_older(&fake)).unwrap();
    let after = view.render(&users(), &ZONE);
    assert_eq!(
        lines(&after)[0],
        "Showing 100 messages (this is the start of the conversation)"
    );
    for n in 1..=100 {
        let shown = after
            .text
            .lines()
            .filter(|line| line.ends_with(&format!("message {n}")))
            .count();
        assert_eq!(shown, 1, "message {n}");
    }
    assert_eq!(
        fake.calls()[1],
        Call::History {
            channel: channel(),
            latest: Some(ts(51)),
            limit: 50
        }
    );
    // Point stays on the same message: its new row is found by timestamp.
    let moved = row_of(&after, &ts(70)).unwrap();
    assert_eq!(moved, at + 50);
    assert_eq!(after.at(moved).unwrap().0, ts(70));
    // Nothing older remains.
    assert_eq!(
        block_on(view.load_older(&fake)),
        Err(LoadError::NothingOlder)
    );
}

#[test]
fn a_thread_groups_its_replies() {
    let fake = FakeSlack::new();
    fake.push_history(Ok(Page {
        items: vec![message(3, "after"), parent(2, 2), message(1, "before")],
        next: None,
    }));
    fake.push_replies(Ok(Page {
        items: vec![
            parent(2, 2),
            reply(4, 2, "first reply"),
            reply(5, 2, "second reply"),
        ],
        next: None,
    }));
    let mut view = view();
    block_on(view.load_first(&fake)).unwrap();
    let transcript = view.render(&users(), &ZONE);
    let lines = lines(&transcript);
    let at = lines.iter().position(|l| l.ends_with("thread 2")).unwrap();
    assert!(lines[at + 1].starts_with("  > ") && lines[at + 1].ends_with("first reply"));
    assert!(lines[at + 2].starts_with("  > ") && lines[at + 2].ends_with("second reply"));
    assert!(lines[at + 3].ends_with("after"));
    // The index maps a reply to its own ts and the parent's thread.
    assert_eq!(transcript.at(at + 1), Some(&(ts(4), Some(ts(2)))));
    assert_eq!(
        reply_destination(&channel(), &transcript, at + 1),
        Some(Destination {
            conversation: channel(),
            thread: Some(ts(2))
        })
    );
    assert_eq!(thread_at(&transcript, at), Some(ts(2)));
}

#[test]
fn the_eleventh_thread_shows_its_hint() {
    let fake = FakeSlack::new();
    // Newest first: threads 12 down to 2 (eleven of them).
    fake.push_history(Ok(Page {
        items: (2..=12).rev().map(|n| parent(n, 3)).collect(),
        next: None,
    }));
    for n in (3..=12).rev() {
        fake.push_replies(Ok(Page {
            items: vec![parent(n, 3), reply(n + 100, n, "a reply")],
            next: None,
        }));
    }
    let mut view = view();
    block_on(view.load_first(&fake)).unwrap();
    let fetched = fake
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::Replies { .. }))
        .count();
    assert_eq!(fetched, 10);
    let transcript = view.render(&users(), &ZONE);
    let hints: Vec<&str> = transcript
        .text
        .lines()
        .filter(|line| line.contains("(slack: read thread)"))
        .collect();
    assert_eq!(hints, vec!["  > 3 replies (slack: read thread)"]);
    // The hint row maps to the thread, so `slack: read thread` at point finds it.
    let row = row_of(&transcript, &ts(2)).unwrap() + 1;
    assert_eq!(thread_at(&transcript, row), Some(ts(2)));
}

#[test]
fn waiting_shows_and_clears() {
    let fake = FakeSlack::new();
    fake.push_history(Ok(history_page(1..=3, true)));
    fake.push_history(Ok(history_page(0..=0, false)));
    let mut view = view();
    block_on(view.load_first(&fake)).unwrap();
    view.set_waiting(Some(Duration::from_millis(29_500)));
    let waiting = view.render(&users(), &ZONE);
    assert_eq!(lines(&waiting)[1], "Slack asked to wait 30 seconds");
    assert_eq!(waiting.index[1], None);
    assert_eq!(waiting.index.len(), lines(&waiting).len());
    block_on(view.load_older(&fake)).unwrap();
    let done = view.render(&users(), &ZONE);
    assert!(!done.text.contains("asked to wait"));
}

#[test]
fn a_disconnected_buffer_refuses_to_load() {
    let fake = FakeSlack::new();
    fake.push_history(Ok(history_page(1..=3, true)));
    let mut view = view();
    block_on(view.load_first(&fake)).unwrap();
    view.disconnect();
    assert_eq!(
        block_on(view.load_older(&fake)),
        Err(LoadError::Disconnected)
    );
    assert_eq!(fake.calls().len(), 1);
    let transcript = view.render(&users(), &ZONE);
    assert_eq!(lines(&transcript)[1], DISCONNECTED_LINE);
    // The messages already shown stay.
    assert_eq!(view.len(), 3);
}

#[test]
fn nothing_in_a_message_is_opened_on_load() {
    let fake = FakeSlack::new();
    let hostile = "<https://evil.example/x|click me> <@U9> <!channel> <#C9|elsewhere>";
    fake.push_history(Ok(Page {
        items: vec![message(1, hostile)],
        next: None,
    }));
    let mut view = view();
    block_on(view.load_first(&fake)).unwrap();
    // Only the history page was asked for: no user, channel or thread lookup followed the text.
    assert_eq!(fake.calls().len(), 1);
    assert!(matches!(fake.calls()[0], Call::History { .. }));
    let transcript = view.render(&users(), &ZONE);
    assert!(
        transcript
            .text
            .contains("click me (https://evil.example/x)")
    );
    assert!(transcript.text.contains("@U9 @channel #elsewhere"));
}

#[test]
fn labels_and_message_links() {
    use bongos_slack_model::types::{Conversation, ConversationKind};
    let group = Conversation {
        id: ConversationId("G1".to_string()),
        kind: ConversationKind::GroupDm,
        name: "mpdm-ann--bob--kyle-1".to_string(),
        user: None,
    };
    assert_eq!(conversation_label(&group, &users()), "@ann, @bob, @kyle");
    let dm = Conversation {
        id: ConversationId("D1".to_string()),
        kind: ConversationKind::Dm,
        name: String::new(),
        user: Some(UserId("U1".to_string())),
    };
    assert_eq!(conversation_label(&dm, &users()), "@kyle");
    let link = MessageLink::parse(
        "https://acme.slack.com/archives/C1/p1730000005000100?thread_ts=1730000002.000100&cid=C1",
    )
    .unwrap();
    assert_eq!(link.conversation, channel());
    assert_eq!(link.ts, Ts::new("1730000005.000100"));
    assert_eq!(link.thread(), Ts::new("1730000002.000100"));
    assert_eq!(MessageLink::parse("https://acme.slack.com/other"), None);
}
