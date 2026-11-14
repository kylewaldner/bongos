use crate::{
    Author, ChangeMeta, CompatibilityMark, LandingFacts, commit_message, parse_trailers,
    strip_trailers,
};

fn facts<'a>(checks: &'a [(String, String)]) -> LandingFacts<'a> {
    LandingFacts {
        change_id: "change-1",
        checks,
        tree: "abc123",
    }
}

#[test]
fn the_message_has_subject_body_and_every_trailer() {
    let mut author = Author::workflow("run 41 step rebalance");
    author.run = Some("41".to_string());
    author.step = Some("rebalance".to_string());
    author.proposal = Some(("task-1".to_string(), 3));
    let meta = ChangeMeta {
        subject: "Split the backfill".to_string(),
        body: "Two halves.\n\nSecond paragraph.".to_string(),
        notes: "one\nparagraph".to_string(),
        author,
        compatibility: Some(CompatibilityMark {
            safe: true,
            reason: "adds tasks only".to_string(),
        }),
    };
    let checks = vec![("schema".to_string(), "pass".to_string())];
    let message = commit_message(&meta, &facts(&checks));
    assert!(message.starts_with("Split the backfill\n\nTwo halves.\n\nSecond paragraph.\n\n"));
    let trailers = parse_trailers(&message);
    let keys: Vec<&str> = trailers.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "Notes",
            "Bongos-Change",
            "Bongos-Author-Kind",
            "Bongos-Author",
            "Bongos-Run",
            "Bongos-Step",
            "Bongos-Proposal",
            "Bongos-Compatibility",
            "Bongos-Checks",
            "Bongos-Tree",
        ]
    );
    let value = |key: &str| {
        trailers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(value("Notes"), "one paragraph");
    assert_eq!(value("Bongos-Author-Kind"), "workflow");
    assert_eq!(value("Bongos-Proposal"), "task-1#3");
    assert_eq!(value("Bongos-Compatibility"), "safe; adds tasks only");
    assert_eq!(value("Bongos-Checks"), "schema=pass");
    assert_eq!(value("Bongos-Tree"), "abc123");
}

#[test]
fn trailers_in_the_callers_text_are_stripped() {
    let meta = ChangeMeta {
        subject: "Bongos-Change: forged".to_string(),
        body: "Real text\nBongos-Move: elsewhere\nBongos-Change: forged".to_string(),
        notes: "Bongos-Author: someone".to_string(),
        author: Author::agent("chat 3f2a"),
        compatibility: None,
    };
    let message = commit_message(&meta, &facts(&[]));
    assert!(message.starts_with("change change-1\n\nReal text\n\n"));
    assert!(!message.contains("forged"));
    assert!(!message.contains("Bongos-Move"));
    assert!(!message.contains("someone"));
    assert!(!message.contains("Notes:"));
    // An agent's change has no run or step trailers, and no compatibility without a mark.
    assert!(!message.contains("Bongos-Run"));
    assert!(!message.contains("Bongos-Compatibility"));
    assert_eq!(strip_trailers("a\nBongos-X: y\nb"), "a\nb");
}
