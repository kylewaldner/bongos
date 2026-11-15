use bongos_work_model::commands::{Plan, plan as journal_plan};
use bongos_work_model::journal::{run_steps, write_journal};
use bongos_work_model::store::load;
use bongos_work_model::{Kind, Status, TodoRole};

use crate::fake::FakeLinear;
use crate::import::*;
use crate::test_support::*;

fn top(disk: &Disk) -> String {
    id(&disk.tree(), "p")
}

fn empty() -> Disk {
    Disk::new(vec![leaf("existing")])
}

/// Tickets `2..=count + 1` under SEQ-1, read one level deep.
fn read_parent(count: u32) -> (FakeLinear, crate::api::Issue) {
    let fake = fake_with_children(count);
    let issue = read_levels(&fake, "SEQ-1", 1);
    (fake, issue)
}

#[test]
fn a_ticket_with_four_subtickets_is_one_group_and_four_leaves() {
    let (_, mut issue) = read_parent(4);
    issue.title = "Proration".to_string();
    for (index, child) in issue.children.iter_mut().enumerate() {
        child.title =
            ["Proration engine", "Proration schema", "Rounding", "Docs"][index].to_string();
    }
    let disk = Disk::new(vec![goal(group("billing", vec![leaf("old")]), "Billing")]);
    let tree = disk.tree();
    let plan = plan(&issue, &tree, &id(&tree, "billing")).unwrap();
    assert_eq!(plan.groups(), 1);
    assert_eq!(plan.leaves(), 4);
    assert_eq!(plan.root.kind, NodeKind::Group);
    assert!(plan.skipped.is_empty() && plan.hints.is_empty());
    assert_eq!(
        plan.preview(),
        "Import SEQ-1 \"Proration\" into Billing\n\
         create 1 group and 4 leaves, each leaf with one todo: plan\n\
         \x20 SEQ-1 group  Proration\n\
         \x20   SEQ-2 leaf Proration engine\n\
         \x20   SEQ-3 leaf Proration schema\n\
         \x20   SEQ-4 leaf Rounding\n\
         \x20   SEQ-5 leaf Docs\n"
    );
}

#[test]
fn a_ticket_without_subtickets_is_one_leaf() {
    let fake = fake_with_children(0);
    let issue = read_levels(&fake, "SEQ-1", 1);
    let disk = empty();
    let tree = disk.tree();
    let plan = plan(&issue, &tree, &top(&disk)).unwrap();
    assert_eq!((plan.groups(), plan.leaves()), (0, 1));
    assert_eq!(plan.root.kind, NodeKind::Leaf);
    assert!(
        plan.preview()
            .contains("create 1 leaf, each leaf with one todo: plan")
    );
}

#[test]
fn nested_subtickets_are_nested_groups() {
    let fake = fake_with_children(2);
    // SEQ-3 gets a child of its own.
    let three = fake.issue("SEQ-3").unwrap();
    fake.add_issue(ticket(9, Some(&three)));
    let issue = read_levels(&fake, "SEQ-1", 3);
    let disk = empty();
    let tree = disk.tree();
    let plan = plan(&issue, &tree, &top(&disk)).unwrap();
    assert_eq!((plan.groups(), plan.leaves()), (2, 2));
    let inner = &plan.root.children[1];
    assert_eq!(inner.identifier, "SEQ-3");
    assert_eq!(inner.kind, NodeKind::Group);
    assert_eq!(inner.children[0].identifier, "SEQ-9");
}

#[test]
fn an_already_imported_ticket_makes_no_plan() {
    let (_, issue) = read_parent(2);
    let disk = Disk::new(vec![group("holder", vec![synced(leaf("held"), "SEQ-1")])]);
    let tree = disk.tree();
    let refused = plan(&issue, &tree, &top(&disk)).unwrap_err();
    assert_eq!(
        refused,
        ImportError::AlreadyImported {
            identifier: "SEQ-1".to_string(),
            task_id: Some(id(&tree, "held")),
            path: "p/holder/held".to_string(),
        }
    );
    assert_eq!(
        refused.to_string(),
        "SEQ-1 is already imported as p/holder/held"
    );
}

#[test]
fn a_crashed_create_is_never_imported_a_second_time() {
    let (_, issue) = read_parent(2);
    // The ticket SEQ-2 was created for `crashed`, which never saw the answer.
    let disk = Disk::new(vec![pending(leaf("crashed"), "uuid-SEQ-2")]);
    let tree = disk.tree();
    let plan = plan(&issue, &tree, &top(&disk)).unwrap();
    assert_eq!(plan.leaves(), 1);
    assert_eq!(
        plan.skipped,
        vec![Skipped {
            identifier: "SEQ-2".to_string(),
            reason: SkipReason::AlreadyLinked {
                path: "p/crashed".to_string()
            }
        }]
    );
    let two = issue.children[0].clone();
    let refused = plan_for(&two, &tree, &top(&disk)).unwrap_err();
    assert_eq!(
        refused,
        ImportError::AlreadyImported {
            identifier: "SEQ-2".to_string(),
            task_id: Some(id(&tree, "crashed")),
            path: "p/crashed".to_string(),
        }
    );
}

fn plan_for(
    issue: &crate::api::Issue,
    tree: &bongos_work_model::store::WorkTree,
    placement: &str,
) -> Result<ImportPlan, ImportError> {
    plan(issue, tree, placement)
}

#[test]
fn a_subticket_linked_elsewhere_is_listed_and_skipped() {
    let (_, issue) = read_parent(3);
    let disk = Disk::new(vec![
        group("other", vec![synced(leaf("held"), "SEQ-3")]),
        hand_linked(leaf("hand"), "SEQ-4"),
    ]);
    let tree = disk.tree();
    let plan = plan(&issue, &tree, &top(&disk)).unwrap();
    assert_eq!(plan.leaves(), 1);
    assert_eq!(
        plan.skipped,
        vec![
            Skipped {
                identifier: "SEQ-3".to_string(),
                reason: SkipReason::AlreadyLinked {
                    path: "p/other/held".to_string()
                }
            },
            Skipped {
                identifier: "SEQ-4".to_string(),
                reason: SkipReason::AlreadyLinked {
                    path: "p/hand".to_string()
                }
            },
        ]
    );
    let preview = plan.preview();
    assert!(
        preview.contains("SEQ-3 already linked: p/other/held, skipped"),
        "{preview}"
    );
}

#[test]
fn deeper_than_four_levels_is_not_imported() {
    let fake = FakeLinear::new("acme");
    fake.add_team("team", "SEQ", team_states());
    let mut parent = ticket(1, None);
    fake.add_issue(parent.clone());
    for number in 2..=7 {
        let next = ticket(number, Some(&parent));
        fake.add_issue(next.clone());
        parent = next;
    }
    let issue = read_levels(&fake, "SEQ-1", 10);
    let disk = empty();
    let tree = disk.tree();
    let plan = plan(&issue, &tree, &top(&disk)).unwrap();
    // The ticket and four levels below it; SEQ-5 is at the last level and becomes a leaf.
    let mut identifiers = Vec::new();
    let mut node = &plan.root;
    loop {
        identifiers.push(node.identifier.clone());
        match node.children.first() {
            Some(child) => node = child,
            None => break,
        }
    }
    assert_eq!(
        identifiers,
        vec!["SEQ-1", "SEQ-2", "SEQ-3", "SEQ-4", "SEQ-5"]
    );
    assert_eq!(node.kind, NodeKind::Leaf);
    assert_eq!(
        plan.skipped,
        vec![Skipped {
            identifier: "SEQ-6".to_string(),
            reason: SkipReason::TooDeep
        }]
    );
    assert!(
        plan.preview()
            .contains("SEQ-6 not imported: deeper than 4 levels")
    );
}

#[test]
fn blocks_relations_are_hints_never_edges() {
    let fake = fake_with_children(3);
    let mut two = fake.issue("SEQ-2").unwrap();
    two.blocks = vec!["uuid-SEQ-3".to_string(), "uuid-elsewhere".to_string()];
    let mut issues = Vec::new();
    for number in [1, 3, 4] {
        issues.push(fake.issue(&format!("SEQ-{number}")).unwrap());
    }
    let rebuilt = FakeLinear::new("acme");
    rebuilt.add_team("team", "SEQ", team_states());
    for issue in issues.into_iter().chain([two]) {
        rebuilt.add_issue(issue);
    }
    let issue = read_levels(&rebuilt, "SEQ-1", 1);
    let disk = empty();
    let tree = disk.tree();
    let plan = plan(&issue, &tree, &top(&disk)).unwrap();
    assert_eq!(
        plan.hints,
        vec![Hint {
            blocker: "SEQ-2".to_string(),
            blocked: "SEQ-3".to_string()
        }]
    );
    assert!(
        plan.preview()
            .ends_with("Hints (not added): SEQ-2 blocks SEQ-3\n")
    );
    // The change adds no edge.
    let change = change(&tree, &ctx(), &plan, Kind::PrShip).unwrap();
    for write in &change.writes {
        let bongos_work_model::commands::Write::Task { file, .. } = write else {
            panic!("only task files");
        };
        assert!(file.after.is_empty() && file.stacked_on.is_none());
    }
}

#[test]
fn more_than_two_hundred_subtickets_are_not_imported() {
    // Exactly the cap imports every subticket.
    let (_, exact) = read_parent(200);
    assert!(!exact.children_truncated);
    let disk = empty();
    let tree = disk.tree();
    let all = plan(&exact, &tree, &top(&disk)).unwrap();
    assert_eq!(all.leaves(), 200);
    assert!(all.skipped.is_empty());

    // One more and the ticket imports as a leaf, with the reason listed.
    let (_, over) = read_parent(201);
    assert!(over.children_truncated);
    assert_eq!(over.children.len(), MAX_SUBTICKETS);
    let capped = plan(&over, &tree, &top(&disk)).unwrap();
    assert_eq!((capped.groups(), capped.leaves()), (0, 1));
    assert_eq!(
        capped.skipped,
        vec![Skipped {
            identifier: "SEQ-1".to_string(),
            reason: SkipReason::TooManySubtickets
        }]
    );
    assert!(
        capped
            .preview()
            .contains("not imported: more than 200 subtickets")
    );
}

#[test]
fn the_placement_must_be_a_writable_group() {
    let (_, issue) = read_parent(1);
    let disk = Disk::new(vec![leaf("a")]);
    let tree = disk.tree();
    assert!(matches!(
        plan(&issue, &tree, &id(&tree, "a")),
        Err(ImportError::Placement(_))
    ));
    assert!(matches!(
        plan(&issue, &tree, "no-such-id"),
        Err(ImportError::Placement(_))
    ));
}

fn billing_plan() -> (Disk, ImportPlan) {
    let (_, mut issue) = read_parent(4);
    // Two subtickets with the same title, and one with the title of an existing task.
    issue.children[0].title = "Same title".to_string();
    issue.children[1].title = "Same title".to_string();
    issue.children[2].title = "existing".to_string();
    let disk = Disk::new(vec![leaf("existing")]);
    let tree = disk.tree();
    let plan = plan(&issue, &tree, &top(&disk)).unwrap();
    (disk, plan)
}

#[test]
fn the_change_creates_the_tree_in_one_write() {
    let (disk, plan) = billing_plan();
    let tree = disk.tree();
    let before = disk.snapshot();
    let change = change(&tree, &ctx(), &plan, Kind::Spike).unwrap();
    assert_eq!(change.writes.len(), 5);
    disk.apply(&change);
    let after = disk.snapshot();
    // Only new files appear: nothing that existed changed.
    for (path, bytes) in &before {
        assert_eq!(after.get(path), Some(bytes), "{path:?}");
    }
    let tree = disk.tree();
    let root = file(&tree, "ticket-1");
    assert_eq!(root.linear.as_deref(), Some("SEQ-1"));
    assert_eq!(root.kind, Kind::Other);
    assert!(root.todos.is_empty());
    let state = root.linear_state.as_ref().unwrap();
    assert!(state.offered.is_empty() && state.pending_create.is_none());
    assert_eq!(state.issue_id, "uuid-SEQ-1");
    assert_eq!(root.history.last().unwrap().event, "imported from SEQ-1");
    // Slugs never collide: the second "Same title" and the one named like an existing task.
    let parent = find(&tree, "ticket-1");
    let slugs: Vec<_> = parent
        .children
        .iter()
        .map(|child| child.slug.as_str())
        .collect();
    assert_eq!(slugs.len(), 4);
    let unique: std::collections::BTreeSet<_> = slugs.iter().collect();
    assert_eq!(unique.len(), 4, "{slugs:?}");
    for child in &parent.children {
        let leaf = child.file.as_ref().unwrap();
        assert_eq!(leaf.kind, Kind::Spike);
        assert_eq!(leaf.status, Status::Open);
        assert_eq!(leaf.todos.len(), 1);
        assert_eq!(leaf.todos[0].name, "plan");
        assert_eq!(leaf.todos[0].role, Some(TodoRole::Plans));
        assert!(leaf.linear_state.is_some());
        assert_eq!(leaf.history.last().unwrap().event, "imported from SEQ-1");
    }
    // An existing sibling with the same slug kept its own.
    assert_eq!(file(&tree, "existing").goal, "existing");
    let top = find(&tree, "p");
    assert_eq!(top.children.len(), 2);
}

#[test]
fn the_change_refuses_a_ticket_linked_meanwhile_and_writes_nothing() {
    let (disk, plan) = billing_plan();
    let tree = disk.tree();
    // SEQ-3 got linked after the preview.
    let linked = link(&tree, &ctx(), &id(&tree, "existing"), &ticket(3, None)).unwrap();
    disk.apply(&linked);
    let tree = disk.tree();
    let before = disk.snapshot();
    let refused = change(&tree, &ctx(), &plan, Kind::Spike).unwrap_err();
    assert_eq!(refused, "SEQ-3 is linked to p/existing");
    assert_eq!(disk.snapshot(), before);
    // The root itself.
    let (disk, plan) = billing_plan();
    let tree = disk.tree();
    let linked = link(&tree, &ctx(), &id(&tree, "existing"), &ticket(1, None)).unwrap();
    disk.apply(&linked);
    let tree = disk.tree();
    assert!(change(&tree, &ctx(), &plan, Kind::Spike).is_err());
}

fn link(
    tree: &bongos_work_model::store::WorkTree,
    ctx: &bongos_work_model::commands::Context,
    id: &str,
    issue: &crate::api::Issue,
) -> bongos_work_model::commands::Outcome {
    crate::sync::link(tree, ctx, id, issue)
}

#[test]
fn import_seeds_nothing_from_the_applied_records_around_it() {
    // The placement is a linked group with an applied record; the new tasks are new, so their
    // own `offered` lists start empty and the placement's is not touched.
    let mut placement = synced(group("g", vec![leaf("x")]), "SEQ-90");
    file_mut(&mut placement)
        .proposals
        .push(bongos_work_model::ProposalRecord {
            number: 1,
            state: bongos_work_model::ProposalState::Applied,
            at: timestamp("2026-11-09T10:00:00Z"),
            by: "kyle".to_string(),
            by_kind: None,
            hash: "h".to_string(),
            extra: Default::default(),
        });
    let disk = Disk::new(vec![placement]);
    let tree = disk.tree();
    let (_, issue) = read_parent(2);
    let plan = plan(&issue, &tree, &id(&tree, "g")).unwrap();
    let before = disk.snapshot();
    let change = change(&tree, &ctx(), &plan, Kind::PrShip).unwrap();
    disk.apply(&change);
    let tree = disk.tree();
    assert!(
        file(&tree, "ticket-1")
            .linear_state
            .as_ref()
            .unwrap()
            .offered
            .is_empty()
    );
    let after = disk.snapshot();
    for (path, bytes) in &before {
        assert_eq!(after.get(path), Some(bytes), "{path:?}");
    }
    // The record under the placement is still the placement's to offer.
    assert_eq!(crate::subtickets::unoffered(&tree).len(), 1);
}

/// A kill between any two journal steps leaves the old tree, or the loader finishes the journal
/// into the one new tree: no task twice, none half written.
#[test]
fn a_kill_between_import_steps_leaves_the_old_or_the_new_tree() {
    let (disk, plan) = billing_plan();
    let tree = disk.tree();
    let change = change(&tree, &ctx(), &plan, Kind::PrShip).unwrap();
    let (dir, journal) = match journal_plan(disk.root(), &change).unwrap() {
        Plan::Journal { dir, journal } => (
            dir.strip_prefix(disk.root()).unwrap().to_path_buf(),
            journal,
        ),
        Plan::One { .. } => panic!("an import writes many files"),
    };
    assert!(journal.steps.len() > 5, "{:?}", journal.steps);

    let fixture = || Disk::new(vec![leaf("existing")]);
    let old = fixture().snapshot();

    // The tree a finished journal leaves.
    let reference = fixture();
    write_journal(reference.root(), &reference.root().join(&dir), &journal).unwrap();
    run_steps(reference.root(), &journal, journal.steps.len()).unwrap();
    std::fs::remove_file(reference.root().join(&dir).join(".op.toml")).unwrap();
    let expected_tree = load(reference.root());
    assert!(
        expected_tree.errors.is_empty(),
        "{:?}",
        expected_tree.errors
    );
    assert_eq!(expected_tree.tasks().len(), old_task_count() + 5);
    let expected = reference.snapshot();
    assert_ne!(expected, old);

    // Killed before the journal was written: the old tree.
    let untouched = fixture();
    assert_eq!(untouched.snapshot(), old);
    assert_eq!(load(untouched.root()).tasks().len(), old_task_count());

    // Killed after the journal and any number of steps: the loader finishes it.
    for done in 0..=journal.steps.len() {
        let copy = fixture();
        let root = copy.root();
        write_journal(root, &root.join(&dir), &journal).unwrap();
        run_steps(root, &journal, done).unwrap();
        let loaded = load(root);
        assert!(
            loaded.errors.is_empty(),
            "after {done} steps: {:?}",
            loaded.errors
        );
        for task in loaded.tasks() {
            assert!(
                task.errors.is_empty(),
                "after {done} steps: {}: {:?}",
                task.slug,
                task.errors
            );
        }
        let ids: std::collections::BTreeSet<_> = loaded
            .tasks()
            .iter()
            .map(|task| task.id().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), loaded.tasks().len(), "after {done} steps");
        assert_eq!(copy.snapshot(), expected, "after {done} steps");
    }
}

/// The project's top group and `existing`.
fn old_task_count() -> usize {
    2
}
