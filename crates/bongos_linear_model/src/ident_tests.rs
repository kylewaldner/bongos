use uuid::Uuid;

use crate::api::IssueRef;
use crate::ident::*;

#[test]
fn identifiers_parse_and_normalize() {
    let id = parse_identifier(" seq-121 ").unwrap();
    assert_eq!(id.team_key(), "SEQ");
    assert_eq!(id.number(), 121);
    assert_eq!(id.to_string(), "SEQ-121");
    assert!(parse_identifier("A1-7").is_some());
}

#[test]
fn identifiers_reject_junk() {
    for bad in [
        "",
        "SEQ",
        "SEQ-",
        "-12",
        "SEQ-0",
        "SEQ-012",
        "SEQ-1a",
        "SEQ-+1",
        "1SEQ-1",
        "SE Q-1",
        "SEQ-1-2",
        "SEQ-99999999999",
        "ABCDEFGHIJK-1",
    ] {
        assert_eq!(parse_identifier(bad), None, "{bad}");
    }
}

#[test]
fn uuids_parse_hyphenated_only() {
    let text = "0196f1c2-0000-4000-8000-000000000120";
    assert_eq!(parse_uuid(text).unwrap().to_string(), text);
    assert_eq!(parse_uuid("0196f1c2000040008000000000000120"), None);
    assert_eq!(parse_uuid("SEQ-1"), None);
}

#[test]
fn urls_parse() {
    let url = parse_issue_url("https://linear.app/acme/issue/SEQ-121/parse-tickets").unwrap();
    assert_eq!(url.workspace, "acme");
    assert_eq!(url.identifier.to_string(), "SEQ-121");
    for ok in [
        "https://linear.app/acme/issue/seq-121",
        "https://linear.app/acme/issue/SEQ-121/",
        "https://linear.app/acme/issue/SEQ-121/slug?x=1#top",
        "  https://linear.app/acme/issue/SEQ-121/slug  ",
    ] {
        assert_eq!(
            parse_issue_url(ok).unwrap().identifier.number(),
            121,
            "{ok}"
        );
    }
}

#[test]
fn urls_reject_other_hosts_and_shapes() {
    for bad in [
        "http://linear.app/acme/issue/SEQ-121",
        "https://evil.com/acme/issue/SEQ-121",
        "https://linear.app.evil.com/acme/issue/SEQ-121",
        "https://linear.app/acme/project/SEQ-121",
        "https://linear.app/acme/issue/nope",
        "https://linear.app/issue/SEQ-121",
        "https://linear.app/acme/issue/SEQ-121/slug/extra",
    ] {
        assert_eq!(parse_issue_url(bad), None, "{bad}");
    }
}

#[test]
fn urls_are_built_from_identifiers() {
    let id = parse_identifier("SEQ-9").unwrap();
    assert_eq!(
        issue_url("acme", &id),
        "https://linear.app/acme/issue/SEQ-9"
    );
    assert_eq!(
        parse_issue_url(&issue_url("acme", &id)).unwrap().identifier,
        id
    );
}

#[test]
fn issue_refs_accept_identifier_uuid_or_url() {
    let uuid = Uuid::parse_str("0196f1c2-0000-4000-8000-000000000120").unwrap();
    assert_eq!(parse_issue_ref(&uuid.to_string()), Some(IssueRef::Id(uuid)));
    let by_id = parse_issue_ref("SEQ-5").unwrap();
    assert_eq!(by_id.as_arg(), "SEQ-5");
    assert_eq!(
        parse_issue_ref("https://linear.app/acme/issue/SEQ-5/x"),
        Some(by_id)
    );
    assert_eq!(parse_issue_ref("hello"), None);
}
