use futures::executor::block_on;
use uuid::Uuid;

use crate::api::*;
use crate::fake::{Call, FakeLinear};
use crate::graphql::CHILDREN_CAP;
use crate::ident::parse_identifier;

fn state(id: &str, kind: StateKind, position: f64) -> WorkflowState {
    WorkflowState {
        id: id.into(),
        name: id.into(),
        kind,
        position,
    }
}

fn issue(n: u32, parent: Option<&Issue>) -> Issue {
    Issue {
        id: format!("id-{n}"),
        identifier: format!("SEQ-{n}"),
        title: format!("Ticket {n}"),
        description: String::new(),
        url: format!("https://linear.app/acme/issue/SEQ-{n}"),
        team_id: "team".into(),
        state: state("todo", StateKind::Unstarted, 1.0),
        parent: parent.map(Issue::summary),
        children: Vec::new(),
        children_truncated: false,
        blocks: Vec::new(),
    }
}

fn setup(children: u32) -> FakeLinear {
    let fake = FakeLinear::new("acme");
    fake.add_team(
        "team",
        "SEQ",
        vec![
            state("backlog", StateKind::Backlog, 0.0),
            state("todo", StateKind::Unstarted, 1.0),
            state("started", StateKind::Started, 2.0),
        ],
    );
    let parent = issue(1, None);
    fake.add_issue(parent.clone());
    for n in 0..children {
        fake.add_issue(issue(100 + n, Some(&parent)));
    }
    fake
}

fn by_ident(text: &str) -> IssueRef {
    IssueRef::Identifier(parse_identifier(text).unwrap())
}

#[test]
fn reads_children_across_pages_in_order() {
    let fake = setup(60);
    let read = block_on(fake.issues(vec![by_ident("SEQ-1"), by_ident("SEQ-404")])).unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].children.len(), 60);
    assert_eq!(read[0].children[59].identifier, "SEQ-159");
    assert!(!read[0].children_truncated);
}

#[test]
fn pages_use_cursors_like_the_real_api() {
    let fake = setup(7);
    let first = fake.children_page("id-1", 3, None);
    assert_eq!(first.children.len(), 3);
    assert!(first.has_next_page);
    let second = fake.children_page("id-1", 3, first.end_cursor.as_deref());
    assert_eq!(second.children[0].identifier, "SEQ-103");
    let third = fake.children_page("id-1", 3, second.end_cursor.as_deref());
    assert_eq!(third.children.len(), 1);
    assert!(!third.has_next_page);
    fake.set_page_size(2);
    let read = block_on(fake.issues(vec![by_ident("SEQ-1")])).unwrap();
    assert_eq!(read[0].children.len(), 7);
}

#[test]
fn the_cap_truncates_like_the_client_does() {
    let fake = setup(CHILDREN_CAP as u32 + 30);
    let read = block_on(fake.issues(vec![by_ident("SEQ-1")])).unwrap();
    assert_eq!(read[0].children.len(), CHILDREN_CAP);
    assert!(read[0].children_truncated);

    let exact = setup(CHILDREN_CAP as u32);
    let read = block_on(exact.issues(vec![by_ident("SEQ-1")])).unwrap();
    assert_eq!(read[0].children.len(), CHILDREN_CAP);
    assert!(!read[0].children_truncated);
}

#[test]
fn set_state_changes_the_ticket_and_unknown_ids_fail() {
    let fake = setup(0);
    block_on(fake.set_state("id-1", "started")).unwrap();
    assert_eq!(fake.issue("SEQ-1").unwrap().state.kind, StateKind::Started);
    assert_eq!(
        block_on(fake.set_state("nope", "started")),
        Err(ApiError::NotFound("nope".into()))
    );
    assert_eq!(
        block_on(fake.set_state("id-1", "nope")),
        Err(ApiError::NotFound("nope".into()))
    );
}

#[test]
fn create_is_idempotent_by_id() {
    let fake = setup(0);
    let id = Uuid::parse_str("3d2e0000-0000-4000-8000-000000000001").unwrap();
    let input = NewIssue {
        id,
        team_id: "team".into(),
        parent_id: Some("SEQ-1".into()),
        title: "Sub".into(),
        description: String::new(),
    };
    let created = block_on(fake.create(input.clone())).unwrap();
    assert_eq!(created.identifier, "SEQ-2");
    assert_eq!(created.state.kind, StateKind::Backlog);
    assert_eq!(created.url, "https://linear.app/acme/issue/SEQ-2");
    assert_eq!(
        block_on(fake.create(input)),
        Err(ApiError::Duplicate(
            "3d2e0000-0000-4000-8000-000000000001".into()
        ))
    );
    let read = block_on(fake.issues(vec![by_ident("SEQ-1")])).unwrap();
    assert_eq!(read[0].children.len(), 1);
}

#[test]
fn failures_are_injected_once_and_calls_are_recorded() {
    let fake = setup(0);
    fake.fail_next(ApiError::RateLimited { reset_at: Some(5) });
    assert_eq!(
        block_on(fake.viewer()),
        Err(ApiError::RateLimited { reset_at: Some(5) })
    );
    assert_eq!(block_on(fake.viewer()), Ok("acme".to_string()));
    assert_eq!(block_on(fake.team_states("team")).unwrap().len(), 3);
    assert_eq!(
        block_on(fake.team_states("x")),
        Err(ApiError::NotFound("x".into()))
    );
    assert_eq!(block_on(fake.recent(5)).unwrap()[0].identifier, "SEQ-1");
    assert_eq!(
        fake.calls(),
        vec![
            Call::Viewer,
            Call::Viewer,
            Call::TeamStates("team".into()),
            Call::TeamStates("x".into()),
            Call::Recent(5)
        ]
    );
    fake.clear_calls();
    assert!(fake.calls().is_empty());
}
