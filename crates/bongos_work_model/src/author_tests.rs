use crate::{Author, AuthorKind};

#[test]
fn a_person_read_back_is_only_claimed() {
    let mut person = Author::person("Sam");
    person.run = Some("run-1".to_string());
    person.proposal = Some(("task-1".to_string(), 3));
    assert_eq!(person.kind(), AuthorKind::Person);

    let json = serde_json::to_string(&person).unwrap();
    assert!(json.contains(r#""kind":"person""#), "{json}");
    let back: Author = serde_json::from_str(&json).unwrap();
    assert_eq!(back.kind(), AuthorKind::PersonClaimed);
    assert_eq!(back.name, "Sam");
    assert_eq!(back.run.as_deref(), Some("run-1"));
    assert_eq!(back.proposal, Some(("task-1".to_string(), 3)));

    // A claimed person writes the same kind and stays claimed.
    let again: Author = serde_json::from_str(&serde_json::to_string(&back).unwrap()).unwrap();
    assert_eq!(again, back);
}

#[test]
fn hand_written_person_json_is_claimed() {
    let author: Author = serde_json::from_str(r#"{"kind":"person"}"#).unwrap();
    assert_eq!(author.kind(), AuthorKind::PersonClaimed);
    assert_eq!(author.name, "");
}

#[test]
fn other_kinds_round_trip() {
    for author in [
        Author::agent("planner"),
        Author::workflow("linear sync"),
        Author::healer("healer"),
        Author::unattributed("state store"),
    ] {
        let back: Author =
            serde_json::from_str(&serde_json::to_string(&author).unwrap()).unwrap();
        assert_eq!(back, author);
    }
}

#[test]
fn an_unknown_kind_is_refused() {
    assert!(serde_json::from_str::<Author>(r#"{"kind":"person-claimed"}"#).is_err());
}
