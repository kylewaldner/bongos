use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::*;
use crate::graphql::*;
use crate::ident::parse_identifier;

fn fixture(name: &str) -> Value {
    let text = match name {
        "issues_batch" => include_str!("../test-fixtures/issues_batch.json"),
        "children_page2" => include_str!("../test-fixtures/children_page2.json"),
        "team_states" => include_str!("../test-fixtures/team_states.json"),
        "recent" => include_str!("../test-fixtures/recent.json"),
        "viewer" => include_str!("../test-fixtures/viewer.json"),
        "issue_update" => include_str!("../test-fixtures/issue_update.json"),
        "issue_create" => include_str!("../test-fixtures/issue_create.json"),
        "error_rate_limited" => include_str!("../test-fixtures/error_rate_limited.json"),
        "error_unauthorized" => include_str!("../test-fixtures/error_unauthorized.json"),
        "error_not_found" => include_str!("../test-fixtures/error_not_found.json"),
        "error_duplicate" => include_str!("../test-fixtures/error_duplicate.json"),
        "error_server" => include_str!("../test-fixtures/error_server.json"),
        other => panic!("no fixture {other}"),
    };
    serde_json::from_str(text).unwrap()
}

fn ok() -> ResponseMeta {
    ResponseMeta {
        status: 200,
        ..Default::default()
    }
}

fn refs(count: usize) -> Vec<IssueRef> {
    (1..=count as u32)
        .map(|n| IssueRef::Identifier(parse_identifier(&format!("SEQ-{n}")).unwrap()))
        .collect()
}

fn query(request: &Request) -> &str {
    request.body["query"].as_str().unwrap()
}

#[test]
fn issues_body_has_one_alias_per_ticket_and_variables() {
    let requests = issues_requests(&refs(3), BATCH_BUDGET);
    assert_eq!(requests.len(), 1);
    let q = query(&requests[0]);
    for alias in [
        "i0: issue(id: $id0)",
        "i1: issue(id: $id1)",
        "i2: issue(id: $id2)",
    ] {
        assert!(q.contains(alias), "{alias}");
    }
    assert!(q.contains("children(first: 25)"));
    assert!(q.contains("pageInfo { hasNextPage endCursor }"));
    assert_eq!(
        requests[0].body["variables"],
        json!({"id0": "SEQ-1", "id1": "SEQ-2", "id2": "SEQ-3"})
    );
}

#[test]
fn batches_stay_under_the_complexity_limit() {
    for count in [0, 1, 2, 59, 60, 61, 119, 300, 1000] {
        let all = refs(count);
        let requests = issues_requests(&all, BATCH_BUDGET);
        let mut seen = 0;
        for request in &requests {
            assert!(
                request.complexity <= BATCH_BUDGET,
                "{count}: {}",
                request.complexity
            );
            assert!(request.complexity < COMPLEXITY_LIMIT);
            let n = request.body["variables"].as_object().unwrap().len();
            // The estimate is a function of the ticket count only.
            assert_eq!(
                request.complexity,
                (n as u32 * issue_cost_tenths(CHILDREN_FIRST_PAGE)).div_ceil(10)
            );
            seen += n;
        }
        assert_eq!(seen, count);
    }
}

#[test]
fn a_huge_budget_is_still_capped_by_the_limit() {
    for request in issues_requests(&refs(500), 1_000_000) {
        assert!(
            request.complexity < COMPLEXITY_LIMIT,
            "{}",
            request.complexity
        );
    }
    // One ticket always fits, however small the budget.
    assert_eq!(issues_requests(&refs(2), 1).len(), 2);
}

#[test]
fn budget_shrinks_when_the_server_reports_more() {
    assert_eq!(shrink_budget(8000, 4000, 4000), 8000);
    assert_eq!(shrink_budget(8000, 4000, 3000), 8000);
    assert_eq!(shrink_budget(8000, 4000, 8000), 4000);
    assert!(batch_size(shrink_budget(8000, 4000, 8000)) < batch_size(8000));
}

#[test]
fn issues_response_parses_with_children_relations_and_unknown_kinds() {
    let wanted = refs(1)
        .into_iter()
        .chain([IssueRef::Identifier(parse_identifier("SEQ-999").unwrap())])
        .collect::<Vec<_>>();
    let parsed = parse_issues(&wanted, &fixture("issues_batch"), &ok(), 0).unwrap();
    // SEQ-999 is null with an entity-not-found error: left out, not an error.
    assert_eq!(parsed.len(), 1);
    let issue = &parsed[0].issue;
    assert_eq!(issue.identifier, "SEQ-120");
    assert_eq!(issue.team_id, "team-seq");
    assert_eq!(issue.state.kind, StateKind::Started);
    assert_eq!(issue.parent.as_ref().unwrap().identifier, "SEQ-100");
    assert_eq!(issue.blocks, vec!["0196f1c2-0000-4000-8000-000000000130"]);
    assert_eq!(issue.children.len(), 2);
    assert_eq!(issue.children[0].description, "");
    assert_eq!(
        issue.children[1].state.kind,
        StateKind::Other("paused".into())
    );
    assert_eq!(issue.children[1].state.position, 9.5);
    assert!(!issue.children_truncated);
    assert_eq!(parsed[0].next_children_cursor.as_deref(), Some("cursor-2"));
}

#[test]
fn children_next_page_body_uses_after() {
    let request = children_page_request("SEQ-120", 25, "cursor-2").unwrap();
    let q = query(&request);
    assert!(q.contains("children(first: 100, after: $after)"));
    assert!(q.contains("issue(id: $id)"));
    assert_eq!(
        request.body["variables"],
        json!({"id": "SEQ-120", "after": "cursor-2"})
    );
    assert!(request.complexity < COMPLEXITY_LIMIT);
}

#[test]
fn children_second_page_is_parsed_and_appended() {
    let wanted = refs(1);
    let mut parsed = parse_issues(&wanted, &fixture("issues_batch"), &ok(), 0).unwrap();
    let mut issue = parsed.remove(0).issue;
    let page = parse_children_page("SEQ-120", &fixture("children_page2"), &ok(), 0).unwrap();
    assert!(!page.has_next_page);
    let next = apply_children_page(&mut issue, page);
    assert_eq!(next, None);
    assert_eq!(issue.children.len(), 3);
    assert_eq!(issue.children[2].identifier, "SEQ-123");
    assert_eq!(issue.children[2].state.kind, StateKind::Completed);
    assert!(!issue.children_truncated);
}

fn page_of(count: usize, start: usize, has_next: bool) -> ChildrenPage {
    let mut issue = parse_issues(&refs(1), &fixture("issues_batch"), &ok(), 0)
        .unwrap()
        .remove(0)
        .issue;
    let template = issue.children.remove(0);
    ChildrenPage {
        children: (start..start + count)
            .map(|n| Issue {
                id: format!("child-{n}"),
                ..template.clone()
            })
            .collect(),
        has_next_page: has_next,
        end_cursor: Some(format!("c{}", start + count)),
    }
}

fn empty_issue() -> Issue {
    let mut issue = parse_issues(&refs(1), &fixture("issues_batch"), &ok(), 0)
        .unwrap()
        .remove(0)
        .issue;
    issue.children.clear();
    issue
}

#[test]
fn children_paging_stops_at_the_cap_and_marks_truncation() {
    let mut issue = empty_issue();
    let mut have = 0;
    let mut cursor = apply_children_page(&mut issue, page_of(CHILDREN_FIRST_PAGE, 0, true));
    have += CHILDREN_FIRST_PAGE;
    while cursor.is_some() {
        let first = next_children_first(have).unwrap();
        assert!(first <= CHILDREN_MORE_PAGE);
        cursor = apply_children_page(&mut issue, page_of(first, have, true));
        have += first;
    }
    assert_eq!(issue.children.len(), CHILDREN_CAP);
    assert!(issue.children_truncated);
    assert_eq!(next_children_first(CHILDREN_CAP), None);
    assert!(children_page_request("SEQ-1", CHILDREN_CAP, "c").is_none());
}

#[test]
fn exactly_the_cap_without_more_is_not_truncated() {
    let mut issue = empty_issue();
    assert_eq!(
        apply_children_page(&mut issue, page_of(CHILDREN_CAP, 0, false)),
        None
    );
    assert_eq!(issue.children.len(), CHILDREN_CAP);
    assert!(!issue.children_truncated);
}

#[test]
fn an_oversized_page_is_cut_at_the_cap() {
    let mut issue = empty_issue();
    apply_children_page(&mut issue, page_of(CHILDREN_CAP + 5, 0, false));
    assert_eq!(issue.children.len(), CHILDREN_CAP);
    assert!(issue.children_truncated);
}

#[test]
fn team_states_recent_viewer_and_mutations_parse() {
    let states = parse_team_states("team-seq", &fixture("team_states"), &ok(), 0).unwrap();
    assert_eq!(states.len(), 6);
    assert_eq!(states[5].kind, StateKind::Duplicate);

    let recent = parse_recent(&fixture("recent"), &ok(), 0).unwrap();
    assert_eq!(recent[0].identifier, "SEQ-121");

    assert_eq!(parse_viewer(&fixture("viewer"), &ok(), 0).unwrap(), "Acme");
    parse_set_state("SEQ-1", &fixture("issue_update"), &ok(), 0).unwrap();

    let id = Uuid::parse_str("3d2e0000-0000-4000-8000-000000000001").unwrap();
    let created = parse_create(id, &fixture("issue_create"), &ok(), 0).unwrap();
    assert_eq!(created.identifier, "SEQ-124");
    assert_eq!(created.state.kind, StateKind::Backlog);
}

#[test]
fn other_bodies_carry_the_documented_shapes() {
    let q = |r: Request| r.body["query"].as_str().unwrap().to_string();
    assert!(q(recent_request(500)).contains("assignedIssues(first: 100, orderBy: updatedAt)"));
    assert!(q(team_states_request("t")).contains("team(id: $id)"));
    let set = set_state_request("SEQ-1", "st-1");
    assert!(query(&set).contains("issueUpdate(id: $id, input: { stateId: $stateId })"));
    assert_eq!(
        set.body["variables"],
        json!({"id": "SEQ-1", "stateId": "st-1"})
    );
    assert!(q(viewer_request()).contains("viewer"));
    for request in [
        recent_request(100),
        team_states_request("t"),
        set,
        viewer_request(),
    ] {
        assert!(request.complexity < COMPLEXITY_LIMIT);
    }
}

#[test]
fn create_body_sends_the_client_id_and_omits_empty_fields() {
    let id = Uuid::parse_str("3d2e0000-0000-4000-8000-000000000001").unwrap();
    let full = create_request(&NewIssue {
        id,
        team_id: "team-seq".into(),
        parent_id: Some("SEQ-120".into()),
        title: "T".into(),
        description: "D".into(),
    });
    assert!(query(&full).contains("issueCreate(input: $input)"));
    assert_eq!(
        full.body["variables"]["input"],
        json!({"id": "3d2e0000-0000-4000-8000-000000000001", "teamId": "team-seq",
               "parentId": "SEQ-120", "title": "T", "description": "D"})
    );
    let bare = create_request(&NewIssue {
        id,
        team_id: "team-seq".into(),
        parent_id: None,
        title: "T".into(),
        description: String::new(),
    });
    let input = bare.body["variables"]["input"].as_object().unwrap();
    assert!(!input.contains_key("parentId") && !input.contains_key("description"));
}

#[test]
fn no_body_mentions_a_credential() {
    let id = Uuid::nil();
    let new = NewIssue {
        id,
        team_id: "t".into(),
        parent_id: None,
        title: "x".into(),
        description: "".into(),
    };
    for request in [
        issues_requests(&refs(2), BATCH_BUDGET).remove(0),
        recent_request(5),
        team_states_request("t"),
        set_state_request("a", "b"),
        create_request(&new),
        viewer_request(),
    ] {
        let text = request.body.to_string().to_ascii_lowercase();
        assert!(
            !text.contains("token") && !text.contains("authorization") && !text.contains("apikey")
        );
    }
}

// Errors ---------------------------------------------------------------------------------------

fn classify_fixture(name: &str, meta: ResponseMeta) -> ApiError {
    classify(&meta, &fixture(name), "SEQ-1", 1_000).unwrap_err()
}

#[test]
fn rate_limit_by_graphql_code_uses_headers_for_the_reset() {
    let meta = ResponseMeta::from_headers(400, |name| match name {
        "x-ratelimit-requests-reset" => Some("1700000060000".into()),
        _ => None,
    });
    assert_eq!(
        classify_fixture("error_rate_limited", meta),
        ApiError::RateLimited {
            reset_at: Some(1_700_000_060_000)
        }
    );
    assert_eq!(
        classify_fixture("error_rate_limited", ok()),
        ApiError::RateLimited { reset_at: None }
    );
}

#[test]
fn rate_limit_by_http_429_honors_retry_after() {
    let meta = ResponseMeta::from_headers(429, |name| match name {
        "retry-after" => Some("30".into()),
        _ => None,
    });
    assert_eq!(
        classify(&meta, &json!({}), "x", 1_000),
        Err(ApiError::RateLimited {
            reset_at: Some(31_000)
        })
    );
    // The later of the reset header and retry-after wins.
    let both = ResponseMeta {
        status: 429,
        retry_after_secs: Some(1),
        requests_reset_ms: Some(90_000),
        complexity: None,
    };
    assert_eq!(both.reset_at(1_000), Some(90_000));
}

#[test]
fn unauthorized_by_status_or_code() {
    assert_eq!(
        classify_fixture(
            "error_unauthorized",
            ResponseMeta {
                status: 400,
                ..Default::default()
            }
        ),
        ApiError::Unauthorized
    );
    assert_eq!(
        classify(
            &ResponseMeta {
                status: 401,
                ..Default::default()
            },
            &json!({}),
            "x",
            0
        ),
        Err(ApiError::Unauthorized)
    );
}

#[test]
fn not_found_names_the_context() {
    assert_eq!(
        classify_fixture("error_not_found", ok()),
        ApiError::NotFound("SEQ-1".into())
    );
    assert_eq!(
        parse_children_page("SEQ-1", &fixture("error_not_found"), &ok(), 0),
        Err(ApiError::NotFound("SEQ-1".into()))
    );
    assert_eq!(
        parse_team_states("team-x", &json!({"data": {"team": null}}), &ok(), 0),
        Err(ApiError::NotFound("team-x".into()))
    );
}

#[test]
fn duplicate_create_is_reported_with_the_id() {
    let id = Uuid::parse_str("3d2e0000-0000-4000-8000-000000000001").unwrap();
    assert_eq!(
        parse_create(id, &fixture("error_duplicate"), &ok(), 0),
        Err(ApiError::Duplicate(
            "3d2e0000-0000-4000-8000-000000000001".into()
        ))
    );
}

#[test]
fn server_errors_and_malformed_bodies() {
    assert_eq!(
        classify_fixture("error_server", ok()),
        ApiError::Server("Internal server error".into())
    );
    assert_eq!(
        classify(
            &ResponseMeta {
                status: 502,
                ..Default::default()
            },
            &json!({}),
            "x",
            0
        ),
        Err(ApiError::Server("HTTP 502".into()))
    );
    assert!(matches!(
        parse_viewer(&json!({"data": {}}), &ok(), 0),
        Err(ApiError::Server(_))
    ));
    assert!(matches!(
        parse_issues(&refs(1), &json!({"data": {"i0": {"id": "x"}}}), &ok(), 0),
        Err(ApiError::Server(_))
    ));
}

#[test]
fn issues_batch_surfaces_real_errors_but_not_missing_tickets() {
    assert_eq!(
        parse_issues(&refs(1), &fixture("error_rate_limited"), &ok(), 0),
        Err(ApiError::RateLimited { reset_at: None })
    );
    assert_eq!(
        parse_issues(&refs(1), &fixture("error_unauthorized"), &ok(), 0),
        Err(ApiError::Unauthorized)
    );
}

#[test]
fn headers_parse_case_by_lowercase_name() {
    let meta = ResponseMeta::from_headers(200, |name| match name {
        "x-complexity" => Some(" 421 ".into()),
        "retry-after" => Some("soon".into()),
        _ => None,
    });
    assert_eq!(meta.complexity, Some(421));
    assert_eq!(meta.retry_after_secs, None);
}
