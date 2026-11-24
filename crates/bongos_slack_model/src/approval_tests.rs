use crate::allowed::AllowedList;
use crate::approval::{Decision, Origin, PostSummary, decide, question_hash};
use crate::types::{ConversationId, Destination};

fn destination(text: &str) -> Destination {
    Destination::parse(text).unwrap()
}

#[test]
fn decide_follows_the_table() {
    let releases = destination("C0RELEASES");
    let releases_thread = destination("C0RELEASES:1730000000.000100");
    let other = destination("C0OTHER");
    let allowed = AllowedList::new([ConversationId("C0RELEASES".to_string())]);
    let empty = AllowedList::default();

    // (origin, allowed destination, other destination, empty list)
    let table = [
        (Origin::Kyle, Decision::Send, Decision::Send, Decision::Send),
        (
            Origin::AttendedRun,
            Decision::Send,
            Decision::Send,
            Decision::Send,
        ),
        (
            Origin::UnattendedRun,
            Decision::Send,
            Decision::Ask,
            Decision::Ask,
        ),
        (
            Origin::ChatAgent,
            Decision::Send,
            Decision::Ask,
            Decision::Ask,
        ),
    ];
    for (origin, on_allowed, on_other, on_empty) in table {
        assert_eq!(
            decide(origin, &releases, &allowed),
            on_allowed,
            "{origin:?}"
        );
        assert_eq!(
            decide(origin, &releases_thread, &allowed),
            on_allowed,
            "{origin:?} in a thread"
        );
        assert_eq!(decide(origin, &other, &allowed), on_other, "{origin:?}");
        assert_eq!(decide(origin, &releases, &empty), on_empty, "{origin:?}");
    }
    assert!(!empty.allows(&releases), "an empty list allows nothing");
}

#[test]
fn a_changed_text_changes_the_question_hash() {
    let releases = destination("C0RELEASES");
    let hash = question_hash(&releases, "shipping 1.2");
    assert_eq!(hash, question_hash(&releases, "shipping 1.2"));
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(hash, question_hash(&releases, "shipping 1.3"));
    assert_ne!(hash, question_hash(&destination("C0OTHER"), "shipping 1.2"));
    assert_ne!(
        hash,
        question_hash(&destination("C0RELEASES:1730000000.000100"), "shipping 1.2")
    );

    let summary = PostSummary {
        destination: "#releases",
        workspace: "Acme",
        user: "kyle",
        text: "shipping 1.2",
    };
    assert_eq!(
        summary.text(),
        "Post to #releases in Acme as kyle (12 characters):\nshipping 1.2"
    );
}
