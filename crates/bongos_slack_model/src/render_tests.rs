use std::collections::HashMap;

use crate::api::ApiError;
use crate::render::{
    FixedOffset, MAX_POST_CHARS, Threads, UserNames, check_length, escape_for_post, message_link,
    text, too_long_message, transcript,
};
use crate::types::{ConversationId, Cursor, Message, Page, Ts, UserId};

fn names() -> UserNames {
    let mut names = UserNames::new();
    names.insert(&UserId("U1".to_string()), "alice");
    names.insert(&UserId("U2".to_string()), "bob");
    names
}

fn message(ts: &str, user: Option<&str>, body: &str) -> Message {
    Message {
        ts: Ts::new(ts),
        user: user.map(|user| UserId(user.to_string())),
        text: body.to_string(),
        thread_ts: None,
        reply_count: 0,
        subtype: None,
    }
}

#[test]
fn mentions_links_and_escapes_render_as_people_read_them() {
    let users = names();
    let input = "hi <@U1> and <@U9> in <#C123|general> <#C9> <!here> <!channel> <!everyone> \
                 see <https://x.com/a?b=1&amp;c=2|the docs> or <https://y.org> \
                 <https://z.org|https://z.org> <!subteam^S1|@devs> 1 &lt; 2 &amp;&amp; 3 &gt; 2 \
                 &amp;lt; and a < b";
    assert_eq!(
        text(input, &users),
        "hi @alice and @U9 in #general #C9 @here @channel @everyone \
         see the docs (https://x.com/a?b=1&c=2) or https://y.org \
         https://z.org @devs 1 < 2 && 3 > 2 &lt; and a < b"
    );
    // Unescaping happens once: what looks like an entity afterwards stays.
    assert_eq!(text("&amp;amp; &amp;gt;", &users), "&amp; &gt;");
    // A mention of a user written in the text by name is not a mention.
    assert_eq!(text("&lt;@U1&gt;", &users), "<@U1>");
    assert_eq!(text("<@U1|ally>", &UserNames::new()), "@ally");
    assert_eq!(text("", &users), "");
    assert_eq!(text("unclosed <@U1", &users), "unclosed <@U1");
    // Names are looked up from users, display name first.
    let user = |display: &str, real: &str| crate::api::User {
        id: UserId("U7".to_string()),
        name: "handle".to_string(),
        display_name: display.to_string(),
        real_name: real.to_string(),
    };
    for (user, expected) in [
        (user("Disp", "Real Name"), "@Disp"),
        (user("", "Real Name"), "@Real Name"),
        (user("", ""), "@handle"),
    ] {
        assert_eq!(text("<@U7>", &UserNames::from_users([&user])), expected);
    }
}

#[test]
fn posts_escape_three_characters_and_stop_at_the_limit() {
    assert_eq!(
        escape_for_post("a & b <c> &amp; \"q\" é"),
        "a &amp; b &lt;c&gt; &amp;amp; \"q\" é"
    );
    // What a post says reads back as typed.
    for typed in ["<@U1> & <!here> &lt;", "1 < 2 > 0", "&&&", "plain"] {
        assert_eq!(text(&escape_for_post(typed), &UserNames::new()), typed);
    }

    // The limit counts characters of the escaped text, not bytes.
    assert_eq!(MAX_POST_CHARS, 40_000);
    assert_eq!(check_length(&"é".repeat(40_000)), Ok(()));
    assert_eq!(check_length(""), Ok(()));
    assert_eq!(
        check_length(&"é".repeat(40_001)),
        Err(ApiError::TooLong { over: 1 })
    );
    let escaped = escape_for_post(&"&".repeat(10_000));
    assert_eq!(escaped.chars().count(), 50_000);
    assert_eq!(
        check_length(&escaped),
        Err(ApiError::TooLong { over: 10_000 })
    );
    assert_eq!(
        too_long_message(10_000),
        "10000 characters over Slack's limit; nothing was sent"
    );
}

#[test]
fn a_transcript_has_replies_under_their_parents() {
    let mut users = names();
    users.insert(&UserId("U5".to_string()), "carol");

    let mut parent = message(
        "1795477800.000100",
        Some("U1"),
        "deploy <#C1|incidents> failed\nsee <https://ex.com/r|runbook>",
    );
    parent.thread_ts = Some(parent.ts.clone());
    parent.reply_count = 2;
    let mut busy = message("1795511700.000400", Some("U3"), "all good");
    busy.thread_ts = Some(busy.ts.clone());
    busy.reply_count = 3;
    let mut quiet = message("1795511800.000500", Some("U5"), "one reply\r\n\r\nhere");
    quiet.thread_ts = Some(quiet.ts.clone());
    quiet.reply_count = 1;
    let reminder = message("1795477000.000050", None, "reminder");

    let reply = |ts: &str, user: &str, body: &str| Message {
        thread_ts: Some(parent.ts.clone()),
        ..message(ts, Some(user), body)
    };
    // As `conversations.replies` answers: the parent, then the replies in any order.
    let mut threads: Threads = HashMap::new();
    threads.insert(
        parent.ts.clone(),
        vec![
            parent.clone(),
            reply("1795511640.000300", "U1", "fixed"),
            reply("1795482300.000200", "U2", "on it &amp; checking <@U1>"),
        ],
    );

    // Pages of history, each newest first; the oldest page says more exist; one message
    // is in both pages.
    let pages = [
        Page {
            items: vec![quiet, busy, parent.clone()],
            next: Some(Cursor("c1".to_string())),
        },
        Page {
            items: vec![parent.clone(), reminder],
            next: Some(Cursor("c2".to_string())),
        },
    ];
    let result = transcript(&pages, &users, &threads, &FixedOffset(3600));
    let golden = "\
Showing 4 messages (older messages exist: slack: load older messages)

2026-11-24 00:36  bot  reminder
2026-11-24 00:50  alice  deploy #incidents failed
    see runbook (https://ex.com/r)
  > 2026-11-24 02:05  bob  on it & checking @alice
  > 2026-11-24 10:14  alice  fixed
2026-11-24 10:15  U3  all good
  > 3 replies (slack: read thread)
2026-11-24 10:16  carol  one reply

    here
  > 1 reply (slack: read thread)";
    assert_eq!(result.text, golden);

    let row = |ts: &str, thread: Option<&str>| Some((Ts::new(ts), thread.map(Ts::new)));
    let a = "1795477800.000100";
    let expected = vec![
        None,
        None,
        row("1795477000.000050", None),
        row(a, Some(a)),
        row(a, Some(a)),
        row("1795482300.000200", Some(a)),
        row("1795511640.000300", Some(a)),
        row("1795511700.000400", Some("1795511700.000400")),
        row("1795511700.000400", Some("1795511700.000400")),
        row("1795511800.000500", Some("1795511800.000500")),
        row("1795511800.000500", Some("1795511800.000500")),
        row("1795511800.000500", Some("1795511800.000500")),
        row("1795511800.000500", Some("1795511800.000500")),
    ];
    assert_eq!(result.text.lines().count(), expected.len());
    assert_eq!(result.index, expected);
    assert_eq!(
        result.at(5).map(|(ts, _)| ts.as_str()),
        Some("1795482300.000200")
    );
    assert_eq!(result.at(0), None);
    assert_eq!(result.at(99), None);

    // The start of the conversation, with a time zone west of UTC across midnight.
    let last = [Page {
        items: vec![message("1795477800.000100", Some("U2"), "first")],
        next: None,
    }];
    let result = transcript(&last, &users, &HashMap::new(), &FixedOffset(-8 * 3600));
    assert_eq!(
        result.text,
        "Showing 1 message (this is the start of the conversation)\n\n\
         2026-11-23 15:50  bob  first"
    );
    let empty = transcript(&[], &users, &HashMap::new(), &FixedOffset(0));
    assert_eq!(
        empty.text,
        "Showing 0 messages (this is the start of the conversation)\n"
    );
}

#[test]
fn a_link_to_a_reply_names_its_thread() {
    let channel = ConversationId("C0123".to_string());
    let ts = Ts::new("1730000000.000100");
    let thread = Ts::new("1729999000.000200");
    let reply = "https://acme.slack.com/archives/C0123/p1730000000000100\
                 ?thread_ts=1729999000.000200&cid=C0123";
    for url in [
        "https://acme.slack.com/",
        "https://acme.slack.com",
        "https://acme.slack.com//",
    ] {
        assert_eq!(message_link(url, &channel, &ts, Some(&thread)), reply);
        assert_eq!(
            message_link(url, &channel, &ts, None),
            "https://acme.slack.com/archives/C0123/p1730000000000100"
        );
        // The parent of a thread is linked as a plain message.
        assert_eq!(
            message_link(url, &channel, &ts, Some(&ts)),
            "https://acme.slack.com/archives/C0123/p1730000000000100"
        );
    }
}
