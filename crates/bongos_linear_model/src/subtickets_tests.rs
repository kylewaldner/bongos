use bongos_work_model::{ByKind, ProposalRecord, ProposalState, Status, TodoState};
use futures::executor::block_on;
use uuid::Uuid;

use crate::api::{ApiError, IssueRef, LinearApi, NewIssue};
use crate::fake::Call;
use crate::subtickets::*;
use crate::sync::is_synced;
use crate::test_support::*;

fn planned(mut task: bongos_work_model::store::Task, text: &str) -> bongos_work_model::store::Task {
    file_mut(&mut task).planned_result = Some(text.to_string());
    task
}

fn titles(missing: &[Missing]) -> Vec<&str> {
    missing.iter().map(|item| item.title.as_str()).collect()
}

#[test]
fn missing_lists_open_unlinked_tasks_in_child_order() {
    let disk = Disk::new(vec![synced(
        group(
            "g",
            vec![
                planned(created(leaf("a"), "2026-11-08T10:03:00Z"), "A PR"),
                stacked_on(created(leaf("b"), "2026-11-08T10:01:00Z"), "a"),
                created(leaf("c"), "2026-11-08T10:02:00Z"),
                created(synced(leaf("linked"), "SEQ-2"), "2026-11-08T10:00:00Z"),
                created(hand_linked(leaf("hand"), "SEQ-3"), "2026-11-08T10:00:00Z"),
                created(cancelled(leaf("dropped")), "2026-11-08T10:00:00Z"),
            ],
        ),
        "SEQ-1",
    )]);
    let tree = disk.tree();
    let g = id(&tree, "g");
    let all = missing(&tree, &g, Scope::Command).unwrap();
    // `b` is stacked on `a`, so `a` comes first; `c` is free and older than `a`.
    assert_eq!(titles(&all), vec!["c", "a", "b"]);
    assert!(
        all.iter()
            .all(|item| item.parent_task_id == g && !item.is_group)
    );
    // The description is the planned result, else empty.
    assert_eq!(all[1].description, "A PR");
    assert_eq!(all[0].description, "");

    // Declined leaves are left out of the offer after an apply, never of the command.
    let declined = record_offered(&tree, &ctx(), &g, &[], &[id(&tree, "c")]).unwrap();
    disk.apply(&declined);
    let tree = disk.tree();
    let offer = missing(&tree, &g, Scope::AfterApply).unwrap();
    assert_eq!(titles(&offer), vec!["a", "b"]);
    let command = missing(&tree, &g, Scope::Command).unwrap();
    assert_eq!(titles(&command), vec!["c", "a", "b"]);
}

#[test]
fn missing_nests_unlinked_groups_and_hangs_linked_subgroups_under_their_ticket() {
    let mut done = leaf("done-one");
    for todo in &mut file_mut(&mut done).todos {
        todo.state = TodoState::Done;
    }
    let disk = Disk::new(vec![synced(
        group(
            "g",
            vec![
                group("unlinked", vec![leaf("u1"), leaf("u2")]),
                group("finished", vec![done]),
                synced(group("sub", vec![leaf("s1")]), "SEQ-5"),
                pending(leaf("retry"), "0196f1c2-0000-4000-8000-000000000001"),
            ],
        ),
        "SEQ-1",
    )]);
    let tree = disk.tree();
    let items = missing(&tree, &id(&tree, "g"), Scope::Command).unwrap();
    let find = |title: &str| items.iter().position(|item| item.title == title).unwrap();
    assert_eq!(titles(&items), vec!["retry", "s1", "unlinked", "u1", "u2"]);
    // A parent comes before its children; a nested leaf hangs under the unlinked group.
    assert!(find("unlinked") < find("u1"));
    assert_eq!(items[find("u1")].parent_task_id, id(&tree, "unlinked"));
    assert!(items[find("unlinked")].is_group);
    // A linked subgroup's leaf hangs under its ticket; the finished group has nothing to create.
    assert_eq!(items[find("s1")].parent_task_id, id(&tree, "sub"));
    // A create in flight keeps its key, so the retry reuses it.
    assert_eq!(
        items[find("retry")].pending_id.as_deref(),
        Some("0196f1c2-0000-4000-8000-000000000001")
    );
    // Only a linked group can be asked.
    assert!(missing(&tree, &id(&tree, "unlinked"), Scope::Command).is_err());
}

#[test]
fn a_crash_between_create_and_write_makes_no_second_ticket() {
    let fake = fake_with_children(0);
    let disk = Disk::new(vec![synced(
        group("g", vec![planned(leaf("a"), "The first PR")]),
        "SEQ-1",
    )]);
    let tree = disk.tree();
    let g = id(&tree, "g");
    let item = missing(&tree, &g, Scope::Command).unwrap().remove(0);
    assert_eq!(item.pending_id, None);

    // Confirmed: the key is written first, then the request goes out.
    let key = Uuid::new_v4();
    disk.apply(&begin_create(&tree, &ctx(), &item.task_id, key, true).unwrap());
    let input = create_input(&disk.tree(), &item, key).unwrap();
    assert_eq!(input.parent_id.as_deref(), Some("uuid-SEQ-1"));
    assert_eq!(input.team_id, "team");
    assert_eq!(input.description, "The first PR");
    let created = block_on(fake.create(input)).unwrap();
    // The app dies here: the ticket exists, the task has no link yet.

    let tree = disk.tree();
    assert!(!is_synced(file(&tree, "a")));
    let retry = missing(&tree, &g, Scope::Command).unwrap().remove(0);
    assert_eq!(retry.pending_id, Some(key.hyphenated().to_string()));
    // Nothing treats it as linked meanwhile: the ticket is not a suggestion either.
    let read = block_on(fake.issues(vec![IssueRef::Identifier(
        crate::ident::parse_identifier("SEQ-1").unwrap(),
    )]))
    .unwrap()
    .remove(0);
    assert!(crate::sync::suggestions(&tree, &g, &read).is_empty());

    // The retry reuses the key; Linear answers Duplicate; the ticket is read by its UUID.
    let reuse: Uuid = retry.pending_id.as_deref().unwrap().parse().unwrap();
    assert_eq!(reuse, key);
    let again = block_on(fake.create(create_input(&tree, &retry, reuse).unwrap()));
    assert!(matches!(again, Err(ApiError::Duplicate(_))), "{again:?}");
    let found = block_on(fake.issues(vec![IssueRef::Id(reuse)]))
        .unwrap()
        .remove(0);
    assert_eq!(found.id, created.id);
    disk.apply(
        &finish_create(
            &tree,
            &ctx(),
            &retry.task_id,
            &found.summary(),
            &retry.description,
        )
        .unwrap(),
    );

    let tree = disk.tree();
    let linked = file(&tree, "a");
    assert!(is_synced(linked));
    assert_eq!(linked.linear.as_deref(), Some(created.identifier.as_str()));
    let state = linked.linear_state.as_ref().unwrap();
    assert_eq!(state.pending_create, None);
    assert_eq!(state.issue_id, key.hyphenated().to_string());
    assert_eq!(state.description, "The first PR");
    // One ticket under the group, two create attempts, and nothing left to create.
    let parent = block_on(fake.issues(vec![IssueRef::Identifier(
        crate::ident::parse_identifier("SEQ-1").unwrap(),
    )]))
    .unwrap()
    .remove(0);
    assert_eq!(parent.children.len(), 1);
    let creates = fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Create { .. }))
        .count();
    assert_eq!(creates, 2);
    assert!(missing(&tree, &g, Scope::Command).unwrap().is_empty());
}

#[test]
fn creating_walks_parents_before_children() {
    let fake = fake_with_children(0);
    let disk = Disk::new(vec![synced(
        group("g", vec![group("mid", vec![leaf("deep")]), leaf("flat")]),
        "SEQ-1",
    )]);
    let g = id(&disk.tree(), "g");
    let mut order = Vec::new();
    loop {
        let tree = disk.tree();
        let items = missing(&tree, &g, Scope::Command).unwrap();
        // The first item whose parent has a ticket.
        let Some(item) = items.first() else { break };
        let key = Uuid::new_v4();
        disk.apply(&begin_create(&tree, &ctx(), &item.task_id, key, true).unwrap());
        let tree = disk.tree();
        let input: NewIssue = create_input(&tree, item, key).unwrap();
        let summary = block_on(fake.create(input)).unwrap();
        disk.apply(
            &finish_create(&tree, &ctx(), &item.task_id, &summary, &item.description).unwrap(),
        );
        order.push(item.title.clone());
    }
    assert_eq!(order, vec!["flat", "mid", "deep"]);
    let tree = disk.tree();
    // `deep` hangs under `mid`'s ticket, which hangs under the group's.
    let mid = file(&tree, "mid").linear_state.clone().unwrap();
    let deep = fake
        .issue(file(&tree, "deep").linear.as_deref().unwrap())
        .unwrap();
    assert_eq!(deep.parent.unwrap().id, mid.issue_id);
    // A parent that has no ticket yet cannot be a parent.
    let disk = Disk::new(vec![synced(
        group("g", vec![group("mid", vec![leaf("deep")])]),
        "SEQ-1",
    )]);
    let tree = disk.tree();
    let items = missing(&tree, &id(&tree, "g"), Scope::Command).unwrap();
    assert!(create_input(&tree, &items[1], Uuid::new_v4()).is_err());
}

#[test]
fn finish_create_needs_the_key_in_flight() {
    let disk = Disk::new(vec![synced(
        group("g", vec![leaf("a"), leaf("b")]),
        "SEQ-1",
    )]);
    let tree = disk.tree();
    let key = Uuid::new_v4();
    let a = id(&tree, "a");
    let summary = ticket(9, None).summary();
    // No create in flight.
    assert!(finish_create(&tree, &ctx(), &a, &summary, "").is_err());
    disk.apply(&begin_create(&tree, &ctx(), &a, key, true).unwrap());
    let tree = disk.tree();
    // Another ticket than the one being created.
    assert!(finish_create(&tree, &ctx(), &a, &summary, "").is_err());
    // A second begin is refused: the retry uses the key it has.
    assert!(begin_create(&tree, &ctx(), &a, Uuid::new_v4(), true).is_err());
}

#[test]
fn finish_create_seeds_a_group_offered_with_the_applied_proposals_under_it() {
    let mut mid = group("mid", vec![applied(leaf("deep"), 2, "kyle")]);
    applied_record(&mut mid, 1, ProposalState::Applied, "h1", "agent");
    applied_record(&mut mid, 3, ProposalState::Rejected, "h3", "kyle");
    let disk = Disk::new(vec![synced(group("g", vec![mid, leaf("flat")]), "SEQ-1")]);
    let tree = disk.tree();
    let (mid_id, flat_id) = (id(&tree, "mid"), id(&tree, "flat"));
    for (task, number) in [(&mid_id, 8), (&flat_id, 9)] {
        let key = Uuid::new_v4();
        let tree = disk.tree();
        disk.apply(&begin_create(&tree, &ctx(), task, key, true).unwrap());
        let tree = disk.tree();
        let mut summary = ticket(number, None).summary();
        summary.id = key.hyphenated().to_string();
        disk.apply(&finish_create(&tree, &ctx(), task, &summary, "").unwrap());
    }
    let tree = disk.tree();
    let offered = file(&tree, "mid").linear_state.clone().unwrap().offered;
    let seen: Vec<_> = offered
        .iter()
        .map(|item| (item.root.clone(), item.number, item.hash.clone()))
        .collect();
    // The applied proposals under the new group, none of the rejected one; they are not offered
    // again as tickets.
    assert_eq!(
        seen,
        vec![
            (mid_id, 1, "h1".to_string()),
            (id(&tree, "deep"), 2, "hash-2".to_string())
        ]
    );
    assert!(unoffered(&tree).is_empty());
    // A leaf gets no list.
    assert!(
        file(&tree, "flat")
            .linear_state
            .as_ref()
            .unwrap()
            .offered
            .is_empty()
    );
}

#[test]
fn finish_create_refuses_a_ticket_another_task_links() {
    let key = Uuid::new_v4();
    let disk = Disk::new(vec![synced(
        group("g", vec![leaf("a"), synced(leaf("other"), "SEQ-9")]),
        "SEQ-1",
    )]);
    let tree = disk.tree();
    let a = id(&tree, "a");
    disk.apply(&begin_create(&tree, &ctx(), &a, key, true).unwrap());
    let tree = disk.tree();
    let mut summary = ticket(9, None).summary();
    summary.id = key.hyphenated().to_string();
    let refused = finish_create(&tree, &ctx(), &a, &summary, "").unwrap_err();
    assert_eq!(refused, "SEQ-9 is linked to p/g/other");
    assert!(!is_synced(file(&tree, "a")));
}

fn who_applied(by: &str, by_kind: Option<ByKind>) -> bool {
    let disk = Disk::new(vec![synced(
        group("g", vec![applied_as(leaf("x"), 1, by, by_kind)]),
        "SEQ-1",
    )]);
    let found = unoffered(&disk.tree());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].by, by);
    found[0].by_person
}

#[test]
fn a_record_is_by_a_person_by_its_stored_kind_not_its_name() {
    // A person under any name.
    assert!(who_applied("alice", Some(ByKind::Person)));
    // An agent or a workflow, whatever it is called.
    assert!(!who_applied("kyle", Some(ByKind::Agent)));
    assert!(!who_applied("kyle", Some(ByKind::Workflow)));
    assert!(!who_applied("kyle", Some(ByKind::Unattributed)));
    assert!(!who_applied("agent", Some(ByKind::Healer)));
}

#[test]
fn an_old_record_with_no_kind_is_by_a_person_when_its_name_is_kyle() {
    assert!(who_applied("kyle", None));
    assert!(!who_applied("agent", None));
    assert!(!who_applied("workflow:review", None));
}

fn applied_record(
    task: &mut bongos_work_model::store::Task,
    number: u32,
    state: ProposalState,
    hash: &str,
    by: &str,
) {
    file_mut(task).proposals.push(ProposalRecord {
        number,
        state,
        at: timestamp("2026-11-09T10:00:00Z"),
        by: by.to_string(),
        by_kind: None,
        hash: hash.to_string(),
        extra: Default::default(),
    });
}

#[test]
fn unoffered_finds_applied_proposals_under_a_linked_group() {
    let mut inner = group("inner", vec![leaf("x")]);
    applied_record(&mut inner, 1, ProposalState::Applied, "h1", "agent");
    applied_record(&mut inner, 2, ProposalState::Rejected, "h2", "kyle");
    let mut outside = group("outside", vec![leaf("y")]);
    applied_record(&mut outside, 1, ProposalState::Applied, "h9", "kyle");
    let mut top = synced(
        group(
            "g",
            vec![inner, group("between", vec![applied(leaf("z"), 4, "kyle")])],
        ),
        "SEQ-1",
    );
    applied_record(&mut top, 3, ProposalState::Applied, "h3", "kyle");
    let disk = Disk::new(vec![top, outside]);
    let tree = disk.tree();
    let g = id(&tree, "g");
    let mut found: Vec<_> = unoffered(&tree)
        .into_iter()
        .map(|offer| (offer.group_id, offer.root_id, offer.number, offer.by))
        .collect();
    found.sort();
    let mut expected = vec![
        (g.clone(), id(&tree, "inner"), 1, "agent".to_string()),
        (g.clone(), id(&tree, "g"), 3, "kyle".to_string()),
        (g, id(&tree, "z"), 4, "kyle".to_string()),
    ];
    expected.sort();
    // The rejected record and the group nobody linked are never offered.
    assert_eq!(found, expected);
}

#[test]
fn offered_is_recorded_whatever_the_answer_and_never_twice() {
    let mut top = synced(group("g", vec![leaf("a"), leaf("b")]), "SEQ-1");
    applied_record(&mut top, 1, ProposalState::Applied, "h1", "kyle");
    let disk = Disk::new(vec![top]);
    let tree = disk.tree();
    let g = id(&tree, "g");
    let offers = unoffered(&tree);
    assert_eq!(offers.len(), 1);
    let before = disk.snapshot();
    let change = record_offered(&tree, &ctx(), &g, &offers, &[id(&tree, "b")]).unwrap();
    assert_eq!(change.writes.len(), 1);
    disk.apply(&change);
    assert_eq!(changed_paths(&before, &disk.snapshot()).len(), 1);
    let tree = disk.tree();
    let state = file(&tree, "g").linear_state.clone().unwrap();
    assert_eq!(state.offered.len(), 1);
    assert_eq!(state.offered[0].root, g);
    assert_eq!(state.offered[0].number, 1);
    assert_eq!(state.offered[0].hash, "h1");
    assert_eq!(state.declined, vec![id(&tree, "b")]);
    assert!(unoffered(&tree).is_empty());
    // Recording again adds nothing.
    let again = record_offered(&tree, &ctx(), &g, &offers, &[id(&tree, "b")]).unwrap();
    let after = again.file().unwrap().linear_state.clone().unwrap();
    assert_eq!(after.offered, state.offered);
    assert_eq!(after.declined, state.declined);
    assert_eq!(file(&tree, "a").status, Status::Open);
}

#[test]
fn a_changed_proposal_file_is_offered_again() {
    let mut top = synced(group("g", vec![leaf("a")]), "SEQ-1");
    applied_record(&mut top, 1, ProposalState::Applied, "old", "kyle");
    state_mut(&mut top)
        .offered
        .push(bongos_work_model::LinearOffered {
            root: "id-g".to_string(),
            number: 1,
            hash: "old".to_string(),
            extra: Default::default(),
        });
    applied_record(&mut top, 2, ProposalState::Applied, "new", "kyle");
    let disk = Disk::new(vec![top]);
    let tree = disk.tree();
    let numbers: Vec<_> = unoffered(&tree)
        .into_iter()
        .map(|offer| offer.number)
        .collect();
    assert_eq!(numbers, vec![2]);
}
