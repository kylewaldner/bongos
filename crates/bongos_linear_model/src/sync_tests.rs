use std::path::PathBuf;

use bongos_work_model::derive::{self, Shape};
use bongos_work_model::{Kind, TodoRole, layout};
use uuid::Uuid;

use crate::api::Issue;
use crate::subtickets::{Unoffered, begin_create, finish_create, record_offered};
use crate::sync::*;
use crate::test_support::*;

fn toml_of(disk: &Disk, tree: &bongos_work_model::store::WorkTree, slug: &str) -> PathBuf {
    task_toml(tree, slug)
        .strip_prefix(disk.root())
        .unwrap()
        .to_path_buf()
}

/// A ticket with these children, whose `parent` fields point back.
fn with_children(number: u32, children: &[u32]) -> Issue {
    let mut parent = ticket(number, None);
    parent.children = children
        .iter()
        .map(|child| ticket(*child, Some(&parent)))
        .collect();
    parent
}

#[test]
fn link_writes_the_link_record_and_the_title_in_one_file() {
    let disk = Disk::new(vec![goal(leaf("a"), "Old goal"), leaf("b")]);
    let tree = disk.tree();
    let before = disk.snapshot();
    let mut issue = ticket(121, None);
    issue.title = "Proration engine".to_string();
    let change = link(&tree, &ctx(), &id(&tree, "a"), &issue).unwrap();
    assert_eq!(change.writes.len(), 1);
    disk.apply(&change);
    assert_eq!(
        changed_paths(&before, &disk.snapshot()),
        vec![toml_of(&disk, &tree, "a")]
    );
    let tree = disk.tree();
    let linked = file(&tree, "a");
    assert_eq!(linked.goal, "Proration engine");
    assert_eq!(linked.linear.as_deref(), Some("SEQ-121"));
    let state = linked.linear_state.as_ref().unwrap();
    assert_eq!(state.issue_id, "uuid-SEQ-121");
    assert_eq!(state.team_id, "team");
    assert_eq!(state.description, "Description 121");
    assert_eq!(state.url, "https://linear.app/acme/issue/SEQ-121");
    assert!(state.started_sent.is_none() && state.offered.is_empty());
    assert_eq!(linked.history.last().unwrap().event, "linked to SEQ-121");
    assert!(is_synced(linked));
    assert!(file(&tree, "b").linear_state.is_none());
}

#[test]
fn link_refuses_what_would_break_a_link() {
    let parent = with_children(120, &[121, 122]);
    let disk = Disk::new(vec![
        synced(leaf("synced"), "SEQ-1"),
        pending(leaf("pend"), "0196f1c2-0000-4000-8000-000000000000"),
        synced(leaf("holder"), "SEQ-121"),
        hand_linked(leaf("hand"), "SEQ-122"),
        leaf("free"),
        synced(
            group("g", vec![group("sub", vec![leaf("one")]), leaf("two")]),
            "SEQ-120",
        ),
    ]);
    let tree = disk.tree();
    let one = &parent.children[0];
    // Already synced: unlink first.
    let refused = link(&tree, &ctx(), &id(&tree, "synced"), &ticket(9, None)).unwrap_err();
    assert!(refused.contains("already linked to SEQ-1"), "{refused}");
    // A create in flight.
    let refused = link(&tree, &ctx(), &id(&tree, "pend"), &ticket(9, None)).unwrap_err();
    assert!(refused.contains("being created"), "{refused}");
    // Another task links the ticket, by a synced or a hand-set link.
    let refused = link(&tree, &ctx(), &id(&tree, "free"), one).unwrap_err();
    assert_eq!(refused, "SEQ-121 is linked to p/holder");
    let refused = link(&tree, &ctx(), &id(&tree, "free"), &parent.children[1]).unwrap_err();
    assert_eq!(refused, "SEQ-122 is linked to p/hand");
    // A group may link a subticket of the ticket its parent group links: import and create
    // build that shape.
    assert!(
        link(
            &tree,
            &ctx(),
            &id(&tree, "sub"),
            &ticket(130, Some(&parent))
        )
        .is_ok()
    );
    // A leaf may too, and a leaf may link a ticket that has subtickets.
    assert!(
        link(
            &tree,
            &ctx(),
            &id(&tree, "one"),
            &ticket(131, Some(&parent))
        )
        .is_ok()
    );
    assert!(
        link(
            &tree,
            &ctx(),
            &id(&tree, "free"),
            &with_children(140, &[141])
        )
        .is_ok()
    );
    // A task relinks its own hand-set identifier.
    assert!(link(&tree, &ctx(), &id(&tree, "hand"), &parent.children[1]).is_ok());
}

#[test]
fn link_seeds_offered_with_the_applied_records_under_the_group() {
    let disk = Disk::new(vec![
        applied(
            group(
                "g",
                vec![
                    applied(
                        applied(group("inner", vec![leaf("x")]), 1, "agent"),
                        2,
                        "kyle",
                    ),
                    // A linked subgroup keeps its own record: not seeded into the outer group.
                    applied(
                        synced(group("owned", vec![leaf("y")]), "SEQ-130"),
                        7,
                        "kyle",
                    ),
                ],
            ),
            3,
            "kyle",
        ),
        applied(leaf("l"), 4, "kyle"),
        applied(group("elsewhere", vec![leaf("z")]), 5, "kyle"),
    ]);
    let tree = disk.tree();
    let change = link(&tree, &ctx(), &id(&tree, "g"), &ticket(120, None)).unwrap();
    let state = change.file().unwrap().linear_state.clone().unwrap();
    let mut seeded: Vec<_> = state
        .offered
        .iter()
        .map(|offered| (offered.root.clone(), offered.number, offered.hash.clone()))
        .collect();
    seeded.sort();
    assert_eq!(
        seeded,
        vec![
            (id(&tree, "g"), 3, "hash-3".to_string()),
            (id(&tree, "inner"), 1, "hash-1".to_string()),
            (id(&tree, "inner"), 2, "hash-2".to_string()),
        ]
    );
    // A leaf has no offered list.
    let leaf_change = link(&tree, &ctx(), &id(&tree, "l"), &ticket(121, None)).unwrap();
    assert!(
        leaf_change
            .file()
            .unwrap()
            .linear_state
            .as_ref()
            .unwrap()
            .offered
            .is_empty()
    );

    // After the link, those records are not detected as new offers.
    disk.apply(&change);
    let tree = disk.tree();
    let pending: Vec<_> = crate::subtickets::unoffered(&tree)
        .into_iter()
        .map(|offer| offer.number)
        .collect();
    // 7 belongs to the linked subgroup, which has no `offered` yet.
    assert_eq!(pending, vec![7]);
}

#[test]
fn unlink_keeps_the_goal_and_removes_the_link() {
    let disk = Disk::new(vec![goal(synced(leaf("a"), "SEQ-121"), "Kept title")]);
    let tree = disk.tree();
    let change = unlink(&tree, &ctx(), &id(&tree, "a")).unwrap();
    assert_eq!(change.writes.len(), 1);
    disk.apply(&change);
    let tree = disk.tree();
    let unlinked = file(&tree, "a");
    assert_eq!(unlinked.goal, "Kept title");
    assert!(unlinked.linear.is_none() && unlinked.linear_state.is_none());
    assert_eq!(
        unlinked.history.last().unwrap().event,
        "unlinked from SEQ-121"
    );
}

/// The tree for the refusal tests: unlinked, hand-set and pending tasks beside a linked group.
fn mixed() -> Disk {
    Disk::new(vec![
        leaf("plain"),
        hand_linked(leaf("hand"), "SEQ-5"),
        pending(leaf("pend"), "0196f1c2-0000-4000-8000-000000000000"),
        group("loose", vec![leaf("inside")]),
        synced(
            group("g", vec![plan_leaf("x"), synced(plan_leaf("y"), "SEQ-12")]),
            "SEQ-10",
        ),
    ])
}

#[test]
fn every_sync_command_refuses_an_unlinked_task() {
    let disk = mixed();
    let tree = disk.tree();
    let before = disk.snapshot();
    let issue = ticket(9, None);
    let summary = issue.summary();
    for slug in ["plain", "hand", "pend", "loose", "inside"] {
        let task = id(&tree, slug);
        let refusals = [
            ("unlink", unlink(&tree, &ctx(), &task)),
            ("dismiss", dismiss(&tree, &ctx(), &task, "SEQ-9")),
            ("accept", accept(&tree, &ctx(), &task, &issue, Kind::PrShip)),
            (
                "record_offered",
                record_offered(&tree, &ctx(), &task, &[], &[]),
            ),
            (
                "finish_create",
                finish_create(&tree, &ctx(), &task, &summary, ""),
            ),
        ];
        for (name, outcome) in refusals {
            assert!(outcome.is_err(), "{name} on {slug}: {outcome:?}");
        }
    }
    assert_eq!(disk.snapshot(), before);
}

#[test]
fn only_link_and_the_confirmed_pending_create_write_an_unlinked_task() {
    let disk = mixed();
    let tree = disk.tree();
    let uuid = Uuid::new_v4();
    // The pending-create write: confirmed, under a linked group, on a task with no link.
    let change = begin_create(&tree, &ctx(), &id(&tree, "x"), uuid, true).unwrap();
    assert_eq!(change.writes.len(), 1);
    let written = change.file().unwrap();
    assert!(written.linear.is_none());
    let state = written.linear_state.as_ref().unwrap();
    assert_eq!(state.pending_create, Some(uuid.hyphenated().to_string()));
    assert!(state.issue_id.is_empty() && state.offered.is_empty());
    // It is not a link: the goal stays editable and nothing treats it as linked.
    assert!(!is_synced(written));
    assert_eq!(bongos_work_model::rules::goal_locked(written), None);

    // Refused: unconfirmed, no linked group above, already linked, already pending.
    let x = id(&tree, "x");
    assert!(begin_create(&tree, &ctx(), &x, uuid, false).is_err());
    assert!(begin_create(&tree, &ctx(), &id(&tree, "plain"), uuid, true).is_err());
    assert!(begin_create(&tree, &ctx(), &id(&tree, "inside"), uuid, true).is_err());
    assert!(begin_create(&tree, &ctx(), &id(&tree, "y"), uuid, true).is_err());
    assert!(begin_create(&tree, &ctx(), &id(&tree, "pend"), uuid, true).is_err());
    assert!(begin_create(&tree, &ctx(), &id(&tree, "hand"), uuid, true).is_err());
}

#[test]
fn a_command_changes_only_its_own_task_file() {
    let disk = mixed();
    let tree = disk.tree();
    let g = id(&tree, "g");
    let y = id(&tree, "y");
    let unoffered = [Unoffered {
        group_id: g.clone(),
        root_id: id(&tree, "x"),
        number: 1,
        hash: "h".to_string(),
        by: "kyle".to_string(),
        by_person: true,
    }];
    let uuid = Uuid::new_v4();
    let commands = [
        (
            "link",
            link(&tree, &ctx(), &id(&tree, "plain"), &ticket(40, None)),
            "plain",
        ),
        ("unlink", unlink(&tree, &ctx(), &y), "y"),
        ("dismiss", dismiss(&tree, &ctx(), &g, "seq-77"), "g"),
        (
            "record_offered",
            record_offered(&tree, &ctx(), &g, &unoffered, &[id(&tree, "x")]),
            "g",
        ),
        (
            "begin_create",
            begin_create(&tree, &ctx(), &id(&tree, "x"), uuid, true),
            "x",
        ),
    ];
    for (name, outcome, slug) in commands {
        let change = outcome.unwrap_or_else(|refusal| panic!("{name}: {refusal}"));
        // A fresh copy of the same tree for each command, so the snapshots compare one write.
        let copy = mixed();
        let copy_tree = copy.tree();
        // The ids are the slugs', so the change applies to the copy by its directory layout.
        let rebased = rebase(&change, disk.root(), copy.root());
        let before = copy.snapshot();
        copy.apply(&rebased);
        assert_eq!(
            changed_paths(&before, &copy.snapshot()),
            vec![toml_of(&copy, &copy_tree, slug)],
            "{name}"
        );
    }
}

/// `change` with every path moved from under `from` to under `to`.
fn rebase(
    change: &bongos_work_model::commands::Change,
    from: &std::path::Path,
    to: &std::path::Path,
) -> bongos_work_model::commands::Change {
    use bongos_work_model::commands::Write;
    let moved = |path: &std::path::Path| to.join(path.strip_prefix(from).unwrap());
    let mut rebased = change.clone();
    rebased.dir = moved(&change.dir);
    rebased.writes = change
        .writes
        .iter()
        .map(|write| match write {
            Write::Task { dir, file } => Write::Task {
                dir: moved(dir),
                file: file.clone(),
            },
            other => other.clone(),
        })
        .collect();
    rebased
}

fn suggestion_tree() -> Disk {
    Disk::new(vec![synced(
        group(
            "g",
            vec![
                synced(plan_leaf("linked"), "SEQ-121"),
                goal(plan_leaf("same-title"), "Ticket 123"),
                pending(plan_leaf("creating"), "uuid-SEQ-125"),
            ],
        ),
        "SEQ-120",
    )])
}

#[test]
fn suggestions_are_the_unlinked_undismissed_children() {
    let disk = suggestion_tree();
    let tree = disk.tree();
    let issue = with_children(120, &[121, 122, 123, 124, 125]);
    let g = id(&tree, "g");
    let found = suggestions(&tree, &g, &issue);
    // 121 is linked, 125 has a create in flight.
    let names: Vec<_> = found.iter().map(|s| s.identifier.as_str()).collect();
    assert_eq!(names, vec!["SEQ-122", "SEQ-123", "SEQ-124"]);

    // Dismissed ones never return, whatever the case.
    let change = dismiss(&tree, &ctx(), &g, "seq-122").unwrap();
    disk.apply(&change);
    let tree = disk.tree();
    let names: Vec<_> = suggestions(&tree, &g, &issue)
        .into_iter()
        .map(|s| s.identifier)
        .collect();
    assert_eq!(names, vec!["SEQ-123", "SEQ-124"]);
    assert!(dismiss(&tree, &ctx(), &g, "SEQ-122").is_err());
    assert_eq!(
        file(&tree, "g").linear_state.as_ref().unwrap().dismissed,
        vec!["SEQ-122"]
    );

    // A title equal to an unlinked leaf's goal is offered as "link to <leaf>".
    let matches = |number: u32| {
        let summary = ticket(number, None).summary();
        matching_leaf(&tree, &g, &summary).map(|task| task.slug.clone())
    };
    assert_eq!(matches(123).as_deref(), Some("same-title"));
    assert_eq!(matches(124), None);
    // Leaves are not groups: no suggestions for them.
    assert!(suggestions(&tree, &id(&tree, "linked"), &issue).is_empty());
}

#[test]
fn accepting_a_suggestion_adds_one_linked_leaf_with_a_plan_todo() {
    let disk = suggestion_tree();
    let tree = disk.tree();
    let g = id(&tree, "g");
    let mut issue = ticket(127, None);
    issue.title = "Handle refunds".to_string();
    issue.description = "Refund edge cases".to_string();
    let before = disk.snapshot();
    let change = accept(&tree, &ctx(), &g, &issue, Kind::Spike).unwrap();
    assert_eq!(change.writes.len(), 1);
    disk.apply(&change);
    let after = disk.snapshot();
    let changed = changed_paths(&before, &after);
    // The new task.toml and its prompts.txt; the group's file is untouched.
    assert_eq!(changed.len(), 2, "{changed:?}");
    assert!(
        changed
            .iter()
            .all(|path| path.starts_with("projects/p/tasks/g/tasks/handle-refunds"))
    );
    let tree = disk.tree();
    let accepted = file(&tree, "handle-refunds");
    assert_eq!(accepted.goal, "Handle refunds");
    assert_eq!(accepted.kind, Kind::Spike);
    assert_eq!(accepted.linear.as_deref(), Some("SEQ-127"));
    assert_eq!(
        accepted.linear_state.as_ref().unwrap().description,
        "Refund edge cases"
    );
    assert_eq!(accepted.todos.len(), 1);
    assert_eq!(accepted.todos[0].name, "plan");
    assert_eq!(accepted.todos[0].role, Some(TodoRole::Plans));
    assert_eq!(accepted.history.last().unwrap().event, "linked to SEQ-127");
    assert!(layout::task_file(&find(&tree, "handle-refunds").dir).exists());

    // It is linked now: a second accept of the same ticket, or a leaf group, is refused.
    assert!(accept(&tree, &ctx(), &g, &issue, Kind::Spike).is_err());
    assert!(
        accept(
            &tree,
            &ctx(),
            &id(&tree, "linked"),
            &ticket(9, None),
            Kind::Spike
        )
        .is_err()
    );
}

#[test]
fn the_refresh_diff_finds_titles_descriptions_and_missing_tickets() {
    let mut renamed = synced(plan_leaf("a"), "SEQ-1");
    state_mut(&mut renamed).description = "same".to_string();
    file_mut(&mut renamed).goal = "Old title".to_string();
    let mut same = synced(plan_leaf("b"), "SEQ-2");
    file_mut(&mut same).goal = "Ticket 2".to_string();
    state_mut(&mut same).description = "Description 2".to_string();
    let disk = Disk::new(vec![
        renamed,
        same,
        synced(plan_leaf("gone"), "SEQ-3"),
        synced(plan_leaf("out-of-scope"), "SEQ-4"),
        hand_linked(plan_leaf("hand"), "SEQ-5"),
        plan_leaf("plain"),
    ]);
    let tree = disk.tree();
    let mut one = ticket(1, None);
    one.title = "New title".to_string();
    one.description = "different".to_string();
    let issues = vec![one, ticket(2, None), ticket(4, None), ticket(5, None)];
    let scope: Vec<_> = ["a", "b", "gone", "hand", "plain"]
        .iter()
        .map(|slug| id(&tree, slug))
        .collect();
    let diff = refresh_diff(&tree, &scope, &issues);
    assert_eq!(diff.changes.len(), 1);
    let change = &diff.changes[0];
    assert_eq!(change.identifier, "SEQ-1");
    assert_eq!(
        change.title,
        Some(Renamed {
            old: "Old title".to_string(),
            new: "New title".to_string()
        })
    );
    assert_eq!(change.description.as_deref(), Some("different"));
    // The ticket Linear did not return is shown as not found, never unlinked.
    assert_eq!(diff.not_found, vec![id(&tree, "gone")]);
    assert!(diff.suggestions.is_empty());
    assert!(!diff.is_empty());
    // Nothing new: nothing to write.
    let quiet = refresh_diff(&tree, &[id(&tree, "b")], &issues);
    assert!(quiet.is_empty());
    assert!(refresh_change(&tree, &ctx(), &quiet).is_none());
}

#[test]
fn the_refresh_diff_lists_suggestions_for_a_group() {
    let disk = suggestion_tree();
    let tree = disk.tree();
    let g = id(&tree, "g");
    let mut issue = with_children(120, &[121, 122, 123]);
    issue.title = "g".to_string();
    // The group's own goal and description already match.
    issue.description = String::new();
    let diff = refresh_diff(&tree, std::slice::from_ref(&g), &[issue]);
    let names: Vec<_> = diff.suggestions[&g]
        .iter()
        .map(|summary| summary.identifier.as_str())
        .collect();
    assert_eq!(names, vec!["SEQ-122", "SEQ-123"]);
    assert!(diff.changes.is_empty());
}

#[test]
fn the_refresh_write_records_the_old_and_new_title_and_skips_changed_links() {
    let mut a = synced(plan_leaf("a"), "SEQ-1");
    file_mut(&mut a).goal = "Old title".to_string();
    let mut b = synced(plan_leaf("b"), "SEQ-2");
    file_mut(&mut b).goal = "Old b".to_string();
    let disk = Disk::new(vec![a, b, plan_leaf("c")]);
    let tree = disk.tree();
    let mut one = ticket(1, None);
    one.title = "New title".to_string();
    one.description = String::new();
    let mut two = ticket(2, None);
    two.title = "New b".to_string();
    two.description = String::new();
    let scope = vec![id(&tree, "a"), id(&tree, "b")];
    let diff = refresh_diff(&tree, &scope, &[one, two]);
    assert_eq!(diff.changes.len(), 2);

    // Between the diff and the write, `b` was unlinked.
    let unlinked = unlink(&tree, &ctx(), &id(&tree, "b")).unwrap();
    disk.apply(&unlinked);
    let tree = disk.tree();
    let before = disk.snapshot();
    let linear = bongos_work_model::commands::Context::new("linear", ctx().at);
    let change = refresh_change(&tree, &linear, &diff).unwrap();
    assert_eq!(change.writes.len(), 1);
    disk.apply(&change);
    assert_eq!(
        changed_paths(&before, &disk.snapshot()),
        vec![toml_of(&disk, &tree, "a")]
    );
    let tree = disk.tree();
    let a = file(&tree, "a");
    assert_eq!(a.goal, "New title");
    let entry = a.history.last().unwrap();
    assert_eq!(entry.event, "title from Linear");
    assert_eq!(
        entry.detail.as_deref(),
        Some("\"Old title\" -> \"New title\"")
    );
    assert_eq!(entry.by, "linear");
    assert_eq!(file(&tree, "b").goal, "Old b");
}

#[test]
fn an_imported_nested_group_can_be_relinked_after_unlink() {
    let fake = fake_with_children(2);
    let three = fake.issue("SEQ-3").unwrap();
    fake.add_issue(ticket(9, Some(&three)));
    let issue = read_levels(&fake, "SEQ-1", 3);
    let disk = Disk::new(vec![leaf("existing")]);
    let tree = disk.tree();
    let plan = crate::import::plan(&issue, &tree, &id(&tree, "p")).unwrap();
    disk.apply(&crate::import::change(&tree, &ctx(), &plan, Kind::PrShip).unwrap());
    let tree = disk.tree();
    let nested = tree
        .tasks()
        .into_iter()
        .find(|task| task.file.as_ref().and_then(|f| f.linear.as_deref()) == Some("SEQ-3"))
        .expect("the nested group");
    assert_eq!(derive::shape(nested, &tree), Shape::Group);
    let nested_id = nested.id().unwrap().to_string();
    disk.apply(&unlink(&tree, &ctx(), &nested_id).unwrap());
    let tree = disk.tree();
    let subticket = fake.issue("SEQ-3").unwrap();
    assert_eq!(subticket.parent.as_ref().unwrap().identifier, "SEQ-1");
    disk.apply(&link(&tree, &ctx(), &nested_id, &subticket).unwrap());
    let tree = disk.tree();
    let relinked = tree.task_by_id(&nested_id).unwrap();
    assert_eq!(link_of(relinked).unwrap().0, "SEQ-3");
}

#[test]
fn a_create_in_flight_counts_as_linked() {
    let key = "0196f1c2-0000-4000-8000-000000000000";
    let disk = Disk::new(vec![
        pending(leaf("pend"), key),
        leaf("free"),
        synced(group("g", vec![leaf("x")]), "SEQ-1"),
    ]);
    let tree = disk.tree();
    let found = linked_task(&tree, key, "SEQ-5", None).unwrap();
    assert_eq!(found.slug, "pend");
    assert!(linked_task(&tree, key, "SEQ-5", Some(&id(&tree, "pend"))).is_none());
    // The ticket the crashed create made is not linkable a second time.
    let mut made = ticket(5, None);
    made.id = key.to_string();
    let refused = link(&tree, &ctx(), &id(&tree, "free"), &made).unwrap_err();
    assert_eq!(refused, "SEQ-5 is linked to p/pend");
}
