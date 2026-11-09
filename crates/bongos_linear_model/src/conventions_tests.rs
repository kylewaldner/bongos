use bongos_work_model::rules::LINEAR_BRANCH_VECTOR;
use bongos_work_model::{ResultKind, TaskResult};

use crate::conventions::*;
use crate::test_support::*;

#[test]
fn the_branch_segment_matches_the_shared_vector() {
    for (identifier, template, slug, expected) in LINEAR_BRANCH_VECTOR {
        assert_eq!(
            branch_segment(identifier, template, slug),
            expected,
            "{template}"
        );
    }
    // Default template, `{linear}`, a path template.
    assert_eq!(
        branch_segment("SEQ-121", "{slug}", "proration-engine"),
        "seq-121-proration-engine"
    );
    assert_eq!(
        branch_segment("SEQ-121", "{linear}/{slug}", "proration-engine"),
        "seq-121/proration-engine"
    );
    assert_eq!(
        branch_segment("SEQ-121", "kyle/{slug}", "proration-engine"),
        "kyle/seq-121-proration-engine"
    );
}

#[test]
fn an_empty_template_or_a_trailing_slash_still_names_the_slug() {
    assert_eq!(branch_segment("SEQ-121", "", "engine"), "seq-121-engine");
    assert_eq!(branch_segment("SEQ-121", "  ", "engine"), "seq-121-engine");
    assert_eq!(
        branch_segment("SEQ-121", "kyle/", "engine"),
        "kyle/seq-121-engine"
    );
    assert_eq!(
        branch_segment("SEQ-121", "{linear}/", "engine"),
        "seq-121/engine"
    );
    // An empty slug leaves no trailing dash.
    assert_eq!(branch_segment("SEQ-121", "{slug}", ""), "seq-121");
    assert_eq!(branch_segment("SEQ-121", "kyle/{slug}", ""), "kyle/seq-121");
}

#[test]
fn branch_name_and_pr_title_need_a_synced_link() {
    let disk = Disk::new(vec![
        goal(synced(leaf("a"), "SEQ-121"), "Proration engine"),
        goal(hand_linked(leaf("hand"), "SEQ-5"), "Hand"),
        goal(
            pending(leaf("pend"), "0196f1c2-0000-4000-8000-000000000000"),
            "Pend",
        ),
        goal(leaf("plain"), "Plain"),
    ]);
    let tree = disk.tree();
    assert_eq!(
        pr_title(file(&tree, "a")).as_deref(),
        Some("SEQ-121 Proration engine")
    );
    assert_eq!(
        branch_name(file(&tree, "a"), "kyle/{slug}", "proration-engine").as_deref(),
        Some("kyle/seq-121-proration-engine")
    );
    for slug in ["hand", "pend", "plain"] {
        assert_eq!(pr_title(file(&tree, slug)), None, "{slug}");
        assert_eq!(
            branch_name(file(&tree, slug), "{slug}", "x"),
            None,
            "{slug}"
        );
        assert_eq!(context_text(&tree, &id(&tree, slug)), None, "{slug}");
    }
}

#[test]
fn the_context_text_names_the_ticket_and_the_title_rule() {
    let disk = Disk::new(vec![goal(synced(leaf("a"), "SEQ-121"), "Proration engine")]);
    let tree = disk.tree();
    assert_eq!(
        context_text(&tree, &id(&tree, "a")).unwrap(),
        "This task is Linear SEQ-121. Its branch name carries seq-121. Start the PR title with \
         `SEQ-121 ` (for example `SEQ-121 Proration engine`). Do not put other ticket ids or \
         closing words in the title."
    );
}

#[test]
fn a_group_gets_no_context_text() {
    let disk = Disk::new(vec![synced(group("g", vec![leaf("x")]), "SEQ-120")]);
    let tree = disk.tree();
    assert_eq!(context_text(&tree, &id(&tree, "g")), None);
}

#[test]
fn stacked_leaves_are_listed_for_the_combined_pr() {
    let disk = Disk::new(vec![
        synced(leaf("a"), "SEQ-121"),
        stacked_on(synced(leaf("b"), "SEQ-122"), "a"),
        stacked_on(synced(leaf("c"), "SEQ-123"), "b"),
        stacked_on(leaf("unlinked"), "a"),
        leaf("alone"),
    ]);
    let tree = disk.tree();
    assert_eq!(
        stacked_identifiers(&tree, &id(&tree, "a")),
        vec!["SEQ-122", "SEQ-123"]
    );
    assert_eq!(stacked_identifiers(&tree, &id(&tree, "b")), vec!["SEQ-123"]);
    let text = context_text(&tree, &id(&tree, "a")).unwrap();
    assert!(
        text.ends_with(
            "Leaves stacked on this one merge into its branch: SEQ-122, SEQ-123. Add one line \
             per ticket to the PR description: `Closes SEQ-122`."
        ),
        "{text}"
    );
    // The leaf at the top of the stack has nothing stacked on it.
    assert!(
        !context_text(&tree, &id(&tree, "c"))
            .unwrap()
            .contains("Closes")
    );
}

#[test]
fn a_warning_when_the_parent_pr_is_already_open() {
    let mut parent = synced(leaf("a"), "SEQ-121");
    file_mut(&mut parent).result = Some(TaskResult {
        kind: ResultKind::Pr,
        url: Some("https://github.com/acme/app/pull/42".to_string()),
        state: None,
        urls: Vec::new(),
        extra: Default::default(),
    });
    let mut branch_only = synced(leaf("d"), "SEQ-124");
    file_mut(&mut branch_only).result = Some(TaskResult {
        kind: ResultKind::Branch,
        url: None,
        state: None,
        urls: Vec::new(),
        extra: Default::default(),
    });
    let disk = Disk::new(vec![
        parent,
        stacked_on(synced(leaf("b"), "SEQ-123"), "a"),
        stacked_on(leaf("unlinked"), "a"),
        branch_only,
        stacked_on(synced(leaf("e"), "SEQ-125"), "d"),
    ]);
    let tree = disk.tree();
    assert_eq!(
        open_pr_warning(&tree, &id(&tree, "b")).as_deref(),
        Some(
            "PR https://github.com/acme/app/pull/42 for SEQ-121 is already open; add \
             `Closes SEQ-123` to its description"
        )
    );
    assert_eq!(open_pr_warning(&tree, &id(&tree, "unlinked")), None);
    assert_eq!(open_pr_warning(&tree, &id(&tree, "e")), None);
    assert_eq!(open_pr_warning(&tree, &id(&tree, "a")), None);
}
