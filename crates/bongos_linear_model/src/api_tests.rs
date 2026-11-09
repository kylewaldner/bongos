use crate::api::*;

#[test]
fn state_kinds_round_trip_and_keep_unknown_values() {
    for name in [
        "triage",
        "backlog",
        "unstarted",
        "started",
        "completed",
        "canceled",
        "duplicate",
    ] {
        assert_eq!(StateKind::parse(name).as_str(), name);
        assert!(!matches!(StateKind::parse(name), StateKind::Other(_)));
    }
    assert_eq!(
        StateKind::parse("paused"),
        StateKind::Other("paused".into())
    );
    assert_eq!(StateKind::parse("paused").as_str(), "paused");
}

#[test]
fn duplicate_is_treated_like_canceled() {
    assert!(StateKind::Duplicate.is_dropped());
    assert!(StateKind::Canceled.is_dropped());
    assert!(!StateKind::Started.is_dropped());
    assert!(!StateKind::Other("paused".into()).is_dropped());
}

#[test]
fn errors_name_the_problem_and_nothing_else() {
    assert_eq!(
        ApiError::Unauthorized.to_string(),
        "Linear rejected the token"
    );
    assert_eq!(
        ApiError::Duplicate("abc".into()).to_string(),
        "abc already exists in Linear"
    );
    assert_eq!(
        ApiError::NotFound("SEQ-1".into()).to_string(),
        "SEQ-1 not found in Linear"
    );
}
